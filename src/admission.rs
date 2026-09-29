//! Atomic per-key request/token/budget admission.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDate, Utc};
use dashmap::DashMap;

use crate::db::{self, ModelRow, Pool, VirtualKeyRow};
use crate::registry::{Registry, Resolved, Snapshot};
use crate::types::{InternalRequest, Prices, ProxyError, TokenUsage};

const MINUTE_WINDOW: Duration = Duration::from_secs(60);
const DEFAULT_OUTPUT_RESERVATION: u64 = 8_192;
pub const REJECTION_REASONS: [&str; 7] = [
    "rpm",
    "tpm",
    "daily_budget",
    "monthly_budget",
    "concurrency_global",
    "concurrency_key",
    "concurrency_route",
];

#[derive(Clone)]
pub struct AdmissionController {
    keys: Arc<DashMap<String, Arc<KeyAdmission>>>,
    next_id: Arc<AtomicU64>,
    metrics: Arc<AdmissionMetrics>,
    concurrency: Arc<parking_lot::Mutex<ConcurrencyState>>,
    global_concurrency_limit: u64,
}

impl Default for AdmissionController {
    fn default() -> Self {
        Self::new(crate::config::DEFAULT_MAX_INFLIGHT_INFERENCES)
    }
}

impl AdmissionController {
    pub fn new(global_concurrency_limit: u64) -> Self {
        assert!(
            global_concurrency_limit > 0,
            "global concurrency limit must be positive"
        );
        Self {
            keys: Arc::new(DashMap::new()),
            next_id: Arc::new(AtomicU64::new(0)),
            metrics: Arc::new(AdmissionMetrics::default()),
            concurrency: Arc::new(parking_lot::Mutex::new(ConcurrencyState::default())),
            global_concurrency_limit,
        }
    }
}

#[derive(Default)]
struct AdmissionMetrics {
    active_reservations: AtomicU64,
    active_reserved_tokens: AtomicU64,
    reservations_total: AtomicU64,
    reserved_tokens_total: AtomicU64,
    reconciled_complete_total: AtomicU64,
    reconciled_incomplete_total: AtomicU64,
    dropped_total: AtomicU64,
    reconciled_tokens_total: AtomicU64,
    reduced_tokens_total: AtomicU64,
    increased_tokens_total: AtomicU64,
    inflight_inferences: AtomicU64,
    rejected: [AtomicU64; REJECTION_REASONS.len()],
    reservation_started_at: DashMap<u64, Instant>,
}

#[derive(Debug, Clone, Default)]
pub struct AdmissionMetricsSnapshot {
    pub active_reservations: u64,
    pub active_reserved_tokens: u64,
    pub reservations_total: u64,
    pub reserved_tokens_total: u64,
    pub reconciled_complete_total: u64,
    pub reconciled_incomplete_total: u64,
    pub dropped_total: u64,
    pub reconciled_tokens_total: u64,
    pub reduced_tokens_total: u64,
    pub increased_tokens_total: u64,
    pub inflight_inferences: u64,
    pub rejected: [u64; REJECTION_REASONS.len()],
    /// Expose age without imposing an undocumented stale-reservation threshold.
    pub oldest_reservation_age_secs: u64,
}

#[derive(Default)]
struct ConcurrencyState {
    global: u64,
    keys: HashMap<String, u64>,
    routes: HashMap<String, u64>,
}

#[derive(Clone, Copy)]
enum RejectionReason {
    Rpm = 0,
    Tpm = 1,
    DailyBudget = 2,
    MonthlyBudget = 3,
    GlobalConcurrency = 4,
    KeyConcurrency = 5,
    RouteConcurrency = 6,
}

struct AdmissionFailure {
    reason: RejectionReason,
    error: ProxyError,
}

impl AdmissionMetrics {
    fn record_rejection(&self, reason: RejectionReason) {
        self.rejected[reason as usize].fetch_add(1, Ordering::Relaxed);
    }

    fn reservation_started(&self, id: u64, estimate: u64) {
        self.reservations_total.fetch_add(1, Ordering::Relaxed);
        self.reserved_tokens_total
            .fetch_add(estimate, Ordering::Relaxed);
        self.active_reservations.fetch_add(1, Ordering::Relaxed);
        self.active_reserved_tokens
            .fetch_add(estimate, Ordering::Relaxed);
        self.reservation_started_at.insert(id, Instant::now());
    }

    fn reservation_finished(&self, id: u64, estimate: u64) {
        self.active_reservations.fetch_sub(1, Ordering::Relaxed);
        self.active_reserved_tokens
            .fetch_sub(estimate, Ordering::Relaxed);
        self.reservation_started_at.remove(&id);
    }

    fn snapshot(&self) -> AdmissionMetricsSnapshot {
        let now = Instant::now();
        let mut oldest_age = Duration::ZERO;
        for started_at in self.reservation_started_at.iter() {
            oldest_age = oldest_age.max(now.saturating_duration_since(*started_at));
        }
        AdmissionMetricsSnapshot {
            active_reservations: self.active_reservations.load(Ordering::Relaxed),
            active_reserved_tokens: self.active_reserved_tokens.load(Ordering::Relaxed),
            reservations_total: self.reservations_total.load(Ordering::Relaxed),
            reserved_tokens_total: self.reserved_tokens_total.load(Ordering::Relaxed),
            reconciled_complete_total: self.reconciled_complete_total.load(Ordering::Relaxed),
            reconciled_incomplete_total: self.reconciled_incomplete_total.load(Ordering::Relaxed),
            dropped_total: self.dropped_total.load(Ordering::Relaxed),
            reconciled_tokens_total: self.reconciled_tokens_total.load(Ordering::Relaxed),
            reduced_tokens_total: self.reduced_tokens_total.load(Ordering::Relaxed),
            increased_tokens_total: self.increased_tokens_total.load(Ordering::Relaxed),
            inflight_inferences: self.inflight_inferences.load(Ordering::Relaxed),
            rejected: std::array::from_fn(|index| self.rejected[index].load(Ordering::Relaxed)),
            oldest_reservation_age_secs: oldest_age.as_secs(),
        }
    }
}

pub struct ConcurrencyReservation {
    state: Arc<parking_lot::Mutex<ConcurrencyState>>,
    metrics: Arc<AdmissionMetrics>,
    key_id: Option<String>,
    route_id: Option<String>,
}

impl Drop for ConcurrencyReservation {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        state.global = state.global.saturating_sub(1);
        decrement_active(&mut state.keys, self.key_id.as_deref());
        decrement_active(&mut state.routes, self.route_id.as_deref());
        self.metrics
            .inflight_inferences
            .fetch_sub(1, Ordering::Relaxed);
    }
}

fn decrement_active(counts: &mut HashMap<String, u64>, id: Option<&str>) {
    if let Some(id) = id {
        if let Some(active) = counts.get_mut(id) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                counts.remove(id);
            }
        }
    }
}

struct KeyAdmission {
    initialized: AtomicBool,
    init_lock: tokio::sync::Mutex<()>,
    ledger: parking_lot::Mutex<KeyLedger>,
}

impl Default for KeyAdmission {
    fn default() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            init_lock: tokio::sync::Mutex::new(()),
            ledger: parking_lot::Mutex::new(KeyLedger::default()),
        }
    }
}

#[derive(Default)]
struct KeyLedger {
    minute: VecDeque<MinuteUse>,
    active: HashMap<u64, ActiveReservation>,
    daily_day: Option<NaiveDate>,
    daily_spend: f64,
    daily_has_unknown_settled_cost: bool,
    monthly_key: Option<(i32, u32)>,
    monthly_spend: f64,
    monthly_has_unknown_settled_cost: bool,
}

struct MinuteUse {
    at: Instant,
    // Unknown persisted usage blocks TPM admission for the rest of this window.
    tokens: Option<u64>,
}

struct ActiveReservation {
    at: Instant,
    tokens: u64,
    cost: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub struct AdmissionEstimate {
    pub tokens: u64,
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AdmissionBudgetPeriodSnapshot {
    pub settled_spend_usd: f64,
    pub active_reserved_usd: f64,
    pub has_unknown_active_cost: bool,
    pub has_unknown_settled_cost: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AdmissionBudgetSnapshot {
    pub daily: AdmissionBudgetPeriodSnapshot,
    pub monthly: AdmissionBudgetPeriodSnapshot,
}

pub struct AdmissionReservation {
    entry: Arc<KeyAdmission>,
    // Instrument the existing reservation lifetime without changing ledger policy.
    metrics: Arc<AdmissionMetrics>,
    id: u64,
    estimate_tokens: u64,
    estimated_cost: Option<f64>,
    settled: bool,
}

impl AdmissionReservation {
    pub(crate) fn conservative_cost_estimate(&self) -> Option<f64> {
        self.estimated_cost
    }

    /// Settle the request count and retain its conservative token estimate.
    pub(crate) fn reconcile_incomplete_at(self, now_wall: chrono::DateTime<Utc>) {
        self.reconcile_at(&TokenUsage::default(), None, now_wall);
    }

    pub fn reconcile(self, usage: &TokenUsage, actual_cost: Option<f64>) {
        self.reconcile_at(usage, actual_cost, Utc::now());
    }

    pub(crate) fn reconcile_at(
        mut self,
        usage: &TokenUsage,
        actual_cost: Option<f64>,
        now_wall: chrono::DateTime<Utc>,
    ) {
        let actual_tokens = match (usage.input, usage.output) {
            (Some(input), Some(output)) => Some(input.saturating_add(output)),
            _ => None,
        };
        let actual_cost = actual_tokens.and(actual_cost);
        self.entry.ledger.lock().reconcile(
            self.id,
            actual_tokens,
            actual_cost,
            now_wall,
            Instant::now(),
        );
        self.metrics
            .reservation_finished(self.id, self.estimate_tokens);
        if let Some(actual_tokens) = actual_tokens {
            self.metrics
                .reconciled_complete_total
                .fetch_add(1, Ordering::Relaxed);
            self.metrics
                .reconciled_tokens_total
                .fetch_add(actual_tokens, Ordering::Relaxed);
            if actual_tokens < self.estimate_tokens {
                self.metrics
                    .reduced_tokens_total
                    .fetch_add(self.estimate_tokens - actual_tokens, Ordering::Relaxed);
            } else if actual_tokens > self.estimate_tokens {
                self.metrics
                    .increased_tokens_total
                    .fetch_add(actual_tokens - self.estimate_tokens, Ordering::Relaxed);
            }
        } else {
            self.metrics
                .reconciled_incomplete_total
                .fetch_add(1, Ordering::Relaxed);
        }
        self.settled = true;
    }
}

impl Drop for AdmissionReservation {
    fn drop(&mut self) {
        if !self.settled {
            self.entry.ledger.lock().cancel(self.id);
            self.metrics
                .reservation_finished(self.id, self.estimate_tokens);
            self.metrics.dropped_total.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl AdmissionController {
    pub fn metrics_snapshot(&self) -> AdmissionMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Snapshot route concurrency without reserving a slot or recording a rejection.
    pub fn route_capacity_available(&self, route_id: &str, limit: Option<i64>) -> bool {
        let Some(limit) = limit.filter(|limit| *limit > 0) else {
            return true;
        };
        self.concurrency
            .lock()
            .routes
            .get(route_id)
            .copied()
            .unwrap_or_default()
            < limit as u64
    }

    /// Number of currently admitted inference requests for one virtual key.
    pub fn key_inflight(&self, key_id: &str) -> u64 {
        self.concurrency
            .lock()
            .keys
            .get(key_id)
            .copied()
            .unwrap_or_default()
    }

    /// Client-safe aggregate view of settled spend and active budget reservations.
    pub async fn budget_snapshot(
        &self,
        pool: &Pool,
        key_id: &str,
    ) -> anyhow::Result<AdmissionBudgetSnapshot> {
        let entry = self.entry(key_id);
        self.ensure_initialized(pool, key_id, &entry).await?;
        let snapshot = entry
            .ledger
            .lock()
            .budget_snapshot(Utc::now(), Instant::now());
        Ok(snapshot)
    }

    pub fn reserve_concurrency(
        &self,
        key: Option<(&str, Option<i64>)>,
        route: Option<(&str, Option<i64>)>,
    ) -> Result<ConcurrencyReservation, ProxyError> {
        let mut state = self.concurrency.lock();
        if state.global >= self.global_concurrency_limit {
            self.metrics
                .record_rejection(RejectionReason::GlobalConcurrency);
            return Err(ProxyError::rate_limited(
                "local concurrency limit reached; retry after 1 second",
                Some(1),
            ));
        }
        if let Some((key_id, Some(limit))) = key.filter(|(_, limit)| limit.unwrap_or_default() > 0)
        {
            if state.keys.get(key_id).copied().unwrap_or_default() >= limit as u64 {
                self.metrics
                    .record_rejection(RejectionReason::KeyConcurrency);
                return Err(ProxyError::rate_limited(
                    "virtual key concurrency limit reached; retry after 1 second",
                    Some(1),
                ));
            }
        }
        if let Some((route_id, Some(limit))) =
            route.filter(|(_, limit)| limit.unwrap_or_default() > 0)
        {
            if state.routes.get(route_id).copied().unwrap_or_default() >= limit as u64 {
                self.metrics
                    .record_rejection(RejectionReason::RouteConcurrency);
                return Err(ProxyError::rate_limited(
                    "Route concurrency limit reached; retry after 1 second",
                    Some(1),
                ));
            }
        }

        state.global += 1;
        let key_id = key.map(|(id, _)| id.to_string());
        let route_id = route.map(|(id, _)| id.to_string());
        if let Some(id) = &key_id {
            *state.keys.entry(id.clone()).or_default() += 1;
        }
        if let Some(id) = &route_id {
            *state.routes.entry(id.clone()).or_default() += 1;
        }
        self.metrics
            .inflight_inferences
            .fetch_add(1, Ordering::Relaxed);
        Ok(ConcurrencyReservation {
            state: self.concurrency.clone(),
            metrics: self.metrics.clone(),
            key_id,
            route_id,
        })
    }

    fn entry(&self, key_id: &str) -> Arc<KeyAdmission> {
        self.keys
            .entry(key_id.to_string())
            .or_insert_with(|| Arc::new(KeyAdmission::default()))
            .clone()
    }

    async fn ensure_initialized(
        &self,
        pool: &Pool,
        key_id: &str,
        entry: &Arc<KeyAdmission>,
    ) -> anyhow::Result<()> {
        self.ensure_initialized_at(pool, key_id, entry, Utc::now(), Instant::now())
            .await
    }

    async fn ensure_initialized_at(
        &self,
        pool: &Pool,
        key_id: &str,
        entry: &Arc<KeyAdmission>,
        wall_now: chrono::DateTime<Utc>,
        instant_now: Instant,
    ) -> anyhow::Result<()> {
        if entry.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        let _guard = entry.init_lock.lock().await;
        if entry.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        let minute_since = (wall_now - chrono::Duration::seconds(60)).to_rfc3339();
        let daily_since = wall_now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|start| start.and_utc())
            .unwrap_or(wall_now)
            .to_rfc3339();
        let monthly_since = wall_now
            .date_naive()
            .with_day(1)
            .and_then(|first| first.and_hms_opt(0, 0, 0))
            .map(|start| start.and_utc())
            .unwrap_or(wall_now)
            .to_rfc3339();

        let (minute, daily, monthly) = tokio::try_join!(
            db::key_usage_entries_since(pool, key_id, &minute_since),
            db::key_admission_budget_spend_since(pool, key_id, &daily_since),
            db::key_admission_budget_spend_since(pool, key_id, &monthly_since),
        )?;

        let mut ledger = entry.ledger.lock();
        ledger.roll_periods(wall_now);
        for (ts, tokens) in minute {
            let Some(at) = db::parse_dt(&ts) else {
                continue;
            };
            let age = wall_now
                .signed_duration_since(at)
                .to_std()
                .unwrap_or_default()
                .min(MINUTE_WINDOW);
            ledger.minute.push_back(MinuteUse {
                at: instant_now.checked_sub(age).unwrap_or(instant_now),
                // Keep persisted unknown token counts unknown.
                tokens: tokens.map(|tokens| tokens.max(0) as u64),
            });
        }
        ledger.daily_spend = daily.max(0.0);
        ledger.monthly_spend = monthly.max(0.0);
        ledger.prune(instant_now);
        entry.initialized.store(true, Ordering::Release);
        Ok(())
    }

    pub async fn reserve(
        &self,
        pool: &Pool,
        snapshot: &Snapshot,
        key: &VirtualKeyRow,
        req: &InternalRequest,
    ) -> Result<AdmissionReservation, ProxyError> {
        let estimate = estimate_request(snapshot, key, req)?;
        let entry = self.entry(&key.id);
        self.ensure_initialized(pool, &key.id, &entry)
            .await
            .map_err(|error| {
                tracing::warn!(%error, key_id = %key.id, "could not initialize key admission state");
                ProxyError::unavailable("admission state temporarily unavailable")
            })?;
        self.reserve_initialized(entry, key, estimate)
    }

    pub async fn check_current(&self, pool: &Pool, key: &VirtualKeyRow) -> Result<(), ProxyError> {
        let entry = self.entry(&key.id);
        self.ensure_initialized(pool, &key.id, &entry)
            .await
            .map_err(|error| {
                tracing::warn!(%error, key_id = %key.id, "could not initialize key admission state");
                ProxyError::unavailable("admission state temporarily unavailable")
            })?;
        let result = entry
            .ledger
            .lock()
            .check_current(key, Utc::now(), Instant::now());
        match result {
            Ok(()) => Ok(()),
            Err(failure) => {
                self.metrics.record_rejection(failure.reason);
                Err(failure.error)
            }
        }
    }

    fn reserve_initialized(
        &self,
        entry: Arc<KeyAdmission>,
        key: &VirtualKeyRow,
        estimate: AdmissionEstimate,
    ) -> Result<AdmissionReservation, ProxyError> {
        let id = self
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if let Err(failure) =
            entry
                .ledger
                .lock()
                .reserve(id, key, estimate, Utc::now(), Instant::now())
        {
            self.metrics.record_rejection(failure.reason);
            return Err(failure.error);
        }
        self.metrics.reservation_started(id, estimate.tokens);
        Ok(AdmissionReservation {
            entry,
            metrics: self.metrics.clone(),
            id,
            estimate_tokens: estimate.tokens,
            estimated_cost: estimate.cost,
            settled: false,
        })
    }
}

impl KeyLedger {
    fn roll_periods(&mut self, now: chrono::DateTime<Utc>) {
        let day = now.date_naive();
        if self.daily_day != Some(day) {
            self.daily_day = Some(day);
            self.daily_spend = 0.0;
            self.daily_has_unknown_settled_cost = false;
        }
        let month = (now.year(), now.month());
        if self.monthly_key != Some(month) {
            self.monthly_key = Some(month);
            self.monthly_spend = 0.0;
            self.monthly_has_unknown_settled_cost = false;
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .minute
            .front()
            .map(|entry| now.saturating_duration_since(entry.at) >= MINUTE_WINDOW)
            .unwrap_or(false)
        {
            self.minute.pop_front();
        }
    }

    fn current(&mut self, now_wall: chrono::DateTime<Utc>, now: Instant) -> (u64, u64, f64, f64) {
        self.roll_periods(now_wall);
        self.prune(now);
        let mut requests = self.minute.len() as u64;
        let mut tokens = self.minute.iter().fold(0_u64, |total, entry| {
            entry
                .tokens
                .map_or(u64::MAX, |tokens| total.saturating_add(tokens))
        });
        let mut daily = self.daily_spend;
        let mut monthly = self.monthly_spend;

        // Usage rows are timestamped at completion, so active reservations belong
        // to the period currently being admitted, regardless of start time.
        for active in self.active.values() {
            if now.saturating_duration_since(active.at) < MINUTE_WINDOW {
                requests = requests.saturating_add(1);
                tokens = tokens.saturating_add(active.tokens);
            }
            if let Some(cost) = active.cost {
                daily += cost;
                monthly += cost;
            }
        }
        (requests, tokens, daily, monthly)
    }

    fn budget_snapshot(
        &mut self,
        now_wall: chrono::DateTime<Utc>,
        now: Instant,
    ) -> AdmissionBudgetSnapshot {
        self.roll_periods(now_wall);
        self.prune(now);
        let mut snapshot = AdmissionBudgetSnapshot {
            daily: AdmissionBudgetPeriodSnapshot {
                settled_spend_usd: self.daily_spend,
                has_unknown_settled_cost: self.daily_has_unknown_settled_cost,
                ..Default::default()
            },
            monthly: AdmissionBudgetPeriodSnapshot {
                settled_spend_usd: self.monthly_spend,
                has_unknown_settled_cost: self.monthly_has_unknown_settled_cost,
                ..Default::default()
            },
        };

        // Match admission: any active reservation may settle into this period.
        for active in self.active.values() {
            match active.cost {
                Some(cost) => {
                    snapshot.daily.active_reserved_usd += cost;
                    snapshot.monthly.active_reserved_usd += cost;
                }
                None => {
                    snapshot.daily.has_unknown_active_cost = true;
                    snapshot.monthly.has_unknown_active_cost = true;
                }
            }
        }
        snapshot
    }

    fn check_current(
        &mut self,
        key: &VirtualKeyRow,
        now_wall: chrono::DateTime<Utc>,
        now: Instant,
    ) -> Result<(), AdmissionFailure> {
        let (requests, tokens, daily, monthly) = self.current(now_wall, now);
        if let Some(rpm) = key.rpm_limit.filter(|value| *value > 0) {
            if requests >= rpm as u64 {
                return Err(AdmissionFailure {
                    reason: RejectionReason::Rpm,
                    error: ProxyError::rate_limited(
                        format!("rate limit exceeded: {rpm} requests per minute"),
                        Some(60),
                    ),
                });
            }
        }
        if let Some(tpm) = key.tpm_limit.filter(|value| *value > 0) {
            if tokens >= tpm as u64 {
                return Err(AdmissionFailure {
                    reason: RejectionReason::Tpm,
                    error: ProxyError::rate_limited(
                        format!("token rate limit exceeded: {tpm} tokens per minute"),
                        Some(60),
                    ),
                });
            }
        }
        if let Some(limit) = key.daily_budget.filter(|value| *value > 0.0) {
            if daily >= limit {
                return Err(AdmissionFailure {
                    reason: RejectionReason::DailyBudget,
                    error: ProxyError::budget_exceeded(format!(
                        "daily budget exceeded (USD {} of USD {}); resets at 00:00 UTC",
                        crate::cost::format_usd(daily),
                        crate::cost::format_usd(limit),
                    )),
                });
            }
        }
        if let Some(limit) = key.monthly_budget.filter(|value| *value > 0.0) {
            if monthly >= limit {
                return Err(AdmissionFailure {
                    reason: RejectionReason::MonthlyBudget,
                    error: ProxyError::budget_exceeded(format!(
                        "monthly budget exceeded (USD {} of USD {}); resets on the 1st",
                        crate::cost::format_usd(monthly),
                        crate::cost::format_usd(limit),
                    )),
                });
            }
        }
        Ok(())
    }

    fn reserve(
        &mut self,
        id: u64,
        key: &VirtualKeyRow,
        estimate: AdmissionEstimate,
        now_wall: chrono::DateTime<Utc>,
        now: Instant,
    ) -> Result<(), AdmissionFailure> {
        let (requests, tokens, daily, monthly) = self.current(now_wall, now);

        if let Some(rpm) = key.rpm_limit.filter(|value| *value > 0) {
            if requests.saturating_add(1) > rpm as u64 {
                return Err(AdmissionFailure {
                    reason: RejectionReason::Rpm,
                    error: ProxyError::rate_limited(
                        format!("rate limit exceeded: {rpm} requests per minute"),
                        Some(60),
                    ),
                });
            }
        }
        if let Some(tpm) = key.tpm_limit.filter(|value| *value > 0) {
            if tokens.saturating_add(estimate.tokens) > tpm as u64 {
                return Err(AdmissionFailure {
                    reason: RejectionReason::Tpm,
                    error: ProxyError::rate_limited(
                        format!(
                            "token rate limit exceeded: reserving {} tokens would exceed {tpm} tokens per minute",
                            estimate.tokens
                        ),
                        Some(60),
                    ),
                });
            }
        }
        if let Some(cost) = estimate.cost {
            if let Some(limit) = key.daily_budget.filter(|value| *value > 0.0) {
                if daily + cost > limit {
                    return Err(AdmissionFailure {
                        reason: RejectionReason::DailyBudget,
                        error: ProxyError::budget_exceeded(format!(
                            "daily budget would be exceeded (USD {} reserved/spent + USD {} request > USD {}); resets at 00:00 UTC",
                            crate::cost::format_usd(daily),
                            crate::cost::format_usd(cost),
                            crate::cost::format_usd(limit),
                        )),
                    });
                }
            }
            if let Some(limit) = key.monthly_budget.filter(|value| *value > 0.0) {
                if monthly + cost > limit {
                    return Err(AdmissionFailure {
                        reason: RejectionReason::MonthlyBudget,
                        error: ProxyError::budget_exceeded(format!(
                            "monthly budget would be exceeded (USD {} reserved/spent + USD {} request > USD {}); resets on the 1st",
                            crate::cost::format_usd(monthly),
                            crate::cost::format_usd(cost),
                            crate::cost::format_usd(limit),
                        )),
                    });
                }
            }
        }

        self.active.insert(
            id,
            ActiveReservation {
                at: now,
                tokens: estimate.tokens,
                cost: estimate.cost,
            },
        );
        Ok(())
    }

    fn reconcile(
        &mut self,
        id: u64,
        actual_tokens: Option<u64>,
        actual_cost: Option<f64>,
        now_wall: chrono::DateTime<Utc>,
        now: Instant,
    ) {
        self.roll_periods(now_wall);
        self.prune(now);
        let Some(active) = self.active.remove(&id) else {
            return;
        };

        let tokens = actual_tokens.unwrap_or(active.tokens);
        if now.saturating_duration_since(active.at) < MINUTE_WINDOW {
            self.minute.push_back(MinuteUse {
                at: active.at,
                tokens: Some(tokens),
            });
        }

        // roll_periods() selected the completion period, matching the usage row timestamp.
        if actual_cost.is_none() {
            self.daily_has_unknown_settled_cost = true;
            self.monthly_has_unknown_settled_cost = true;
        }
        let cost = actual_cost.or(active.cost);
        if let Some(cost) = cost {
            self.daily_spend += cost;
            self.monthly_spend += cost;
        }
    }

    fn cancel(&mut self, id: u64) {
        self.active.remove(&id);
    }
}

fn estimated_input_tokens(req: &InternalRequest) -> u64 {
    let tool_chars: u64 = req
        .tools
        .iter()
        .map(|tool| {
            tool.name.len() as u64
                + tool.description.as_deref().map(str::len).unwrap_or(0) as u64
                + tool.parameters.to_string().len() as u64
                + 32
        })
        .sum();
    req.approx_input_tokens()
        .saturating_add(tool_chars.div_ceil(4))
        .max(1)
}

fn output_reservation(req: &InternalRequest, model: &ModelRow) -> u64 {
    let model_max = model
        .max_output_tokens
        .filter(|value| *value > 0)
        .map(|value| value as u64);
    match (req.params.max_tokens.map(u64::from), model_max) {
        (Some(requested), Some(maximum)) => requested.min(maximum),
        (Some(requested), None) => requested,
        (None, Some(maximum)) => maximum,
        (None, None) => DEFAULT_OUTPUT_RESERVATION,
    }
}

fn conservative_cost(prices: &Prices, input: u64, output: u64) -> Option<f64> {
    if !prices.is_configured() {
        return None;
    }
    let input_base = match prices.input_per_1m {
        Some(price) => price,
        None if input == 0 => 0.0,
        None => return None,
    };
    let input_rate = input_base
        .max(prices.cached_per_1m.unwrap_or(input_base))
        .max(prices.cache_write_per_1m.unwrap_or(input_base));
    let output_base = match prices.output_per_1m {
        Some(price) => price,
        None if output == 0 => 0.0,
        None => return None,
    };
    let output_rate = output_base.max(prices.thinking_per_1m.unwrap_or(output_base));
    Some((input as f64 * input_rate + output as f64 * output_rate) / 1_000_000.0)
}

pub fn estimate_request(
    snapshot: &Snapshot,
    key: &VirtualKeyRow,
    req: &InternalRequest,
) -> Result<AdmissionEstimate, ProxyError> {
    let resolved = Registry::resolve_in(snapshot, &req.requested_model).ok_or_else(|| {
        ProxyError::not_found(format!(
            "model '{}' is not configured. Use GET /v1/models to list available models.",
            req.requested_model
        ))
    })?;
    let allowed_providers = key.allowed_providers();
    let input = estimated_input_tokens(req);

    let mut models = Vec::new();
    let mut seen = HashSet::new();
    match resolved {
        Resolved::Single {
            provider_id,
            model_id,
        } => {
            if !allowed_providers.is_empty() && !allowed_providers.contains(&provider_id) {
                return Err(ProxyError::new(
                    crate::types::ErrorKind::Forbidden,
                    "this key is not allowed to use the resolved provider",
                ));
            }
            if let Some(model) = snapshot.models.get(&model_id) {
                models.push(model.clone());
            }
        }
        Resolved::Route { targets, .. } => {
            for target in &targets {
                if !allowed_providers.is_empty() && !allowed_providers.contains(&target.provider.id)
                {
                    continue;
                }
                if seen.insert(target.model.id.clone()) {
                    models.push(target.model.clone());
                }
            }
            if models.is_empty() {
                return Err(ProxyError::new(
                    crate::types::ErrorKind::Forbidden,
                    "this key is not allowed to use any provider in the resolved route",
                ));
            }
        }
    }

    let mut tokens = 0u64;
    let mut max_cost = 0.0f64;
    let mut cost_known = true;
    for model in models {
        let output = output_reservation(req, &model);
        tokens = tokens.max(input.saturating_add(output));
        match conservative_cost(&model.prices(), input, output) {
            Some(cost) => max_cost = max_cost.max(cost),
            None => cost_known = false,
        }
    }

    Ok(AdmissionEstimate {
        tokens: tokens.max(input),
        cost: cost_known.then_some(max_cost),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> VirtualKeyRow {
        VirtualKeyRow {
            id: "key".into(),
            key_hash: "hash".into(),
            name: "test".into(),
            owner: String::new(),
            tag: String::new(),
            allowed_models: "[\"*\"]".into(),
            allowed_providers: "[]".into(),
            rpm_limit: None,
            tpm_limit: None,
            max_concurrent_requests: None,
            daily_budget: None,
            monthly_budget: None,
            expires_at: None,
            status: "active".into(),
            allowed_ips: "[]".into(),
            body_logging: 0,
            created_at: String::new(),
            revoked_at: None,
        }
    }

    fn initialized_controller() -> (AdmissionController, Arc<KeyAdmission>) {
        let controller = AdmissionController::default();
        let entry = controller.entry("key");
        entry.initialized.store(true, Ordering::Release);
        (controller, entry)
    }

    fn burst(
        controller: AdmissionController,
        entry: Arc<KeyAdmission>,
        key: VirtualKeyRow,
        estimate: AdmissionEstimate,
        count: usize,
    ) -> Vec<AdmissionReservation> {
        let barrier = Arc::new(std::sync::Barrier::new(count + 1));
        let mut handles = Vec::new();
        for _ in 0..count {
            let controller = controller.clone();
            let entry = entry.clone();
            let key = key.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                controller.reserve_initialized(entry, &key, estimate)
            }));
        }
        barrier.wait();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap().ok())
            .collect()
    }

    fn concurrency_burst(
        controller: AdmissionController,
        key: Option<(&'static str, Option<i64>)>,
        route: Option<(&'static str, Option<i64>)>,
        count: usize,
    ) -> Vec<ConcurrencyReservation> {
        let barrier = Arc::new(std::sync::Barrier::new(count + 1));
        let mut handles = Vec::new();
        for _ in 0..count {
            let controller = controller.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                controller.reserve_concurrency(key, route)
            }));
        }
        barrier.wait();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap().ok())
            .collect()
    }

    fn request(model: &str) -> InternalRequest {
        InternalRequest {
            requested_model: model.into(),
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            tool_choice_name: None,
            params: crate::types::SamplingParams {
                max_tokens: Some(50),
                ..Default::default()
            },
            stream: false,
            include_usage: false,
            thinking: None,
            extra: Default::default(),
            raw_body: None,
        }
    }

    fn snapshot_with_prices(prices: Prices) -> Snapshot {
        let mut snapshot = Snapshot::default();
        let model = ModelRow {
            id: "model".into(),
            provider_id: "provider".into(),
            upstream_id: "priced".into(),
            display_name: "Priced".into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: Some(50),
            capabilities: "{}".into(),
            prices: serde_json::to_string(&prices).unwrap(),
            parameters: "{}".into(),
            thinking_map: "{}".into(),
            extra_request: "{}".into(),
            discovery: "{}".into(),
            created_at: String::new(),
            opaque_state_plugin: String::new(),
        };
        snapshot.models.insert(model.id.clone(), model);
        snapshot
    }

    #[test]
    fn estimate_is_unknown_with_input_known_output_unknown() {
        let snapshot = snapshot_with_prices(Prices {
            input_per_1m: Some(1.0),
            output_per_1m: None,
            ..Default::default()
        });
        let estimate = estimate_request(&snapshot, &key(), &request("priced")).unwrap();
        assert!(estimate.cost.is_none());
    }

    #[test]
    fn estimate_is_unknown_with_output_known_input_unknown() {
        let snapshot = snapshot_with_prices(Prices {
            input_per_1m: None,
            output_per_1m: Some(2.0),
            ..Default::default()
        });
        let estimate = estimate_request(&snapshot, &key(), &request("priced")).unwrap();
        assert!(estimate.cost.is_none());
    }

    #[test]
    fn concurrent_rpm_burst_admits_only_capacity() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.rpm_limit = Some(4);
        let reservations = burst(
            controller,
            entry,
            key,
            AdmissionEstimate {
                tokens: 1,
                cost: Some(0.0),
            },
            32,
        );
        assert_eq!(reservations.len(), 4);
    }

    #[test]
    fn concurrent_tpm_burst_reserves_estimated_tokens_atomically() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.tpm_limit = Some(100);
        let reservations = burst(
            controller,
            entry,
            key,
            AdmissionEstimate {
                tokens: 30,
                cost: Some(0.0),
            },
            16,
        );
        assert_eq!(reservations.len(), 3);
    }

    #[tokio::test]
    async fn active_budget_reservations_follow_completion_period_across_utc_rollover() {
        let before_midnight = chrono::DateTime::parse_from_rfc3339("2026-01-31T23:59:59Z")
            .unwrap()
            .with_timezone(&Utc);
        let after_midnight = chrono::DateTime::parse_from_rfc3339("2026-02-01T00:00:02Z")
            .unwrap()
            .with_timezone(&Utc);
        let before_instant = Instant::now();
        let after_instant = before_instant + Duration::from_secs(3);
        let estimate = AdmissionEstimate {
            tokens: 1,
            cost: Some(0.8),
        };
        let mut key = key();
        key.daily_budget = Some(1.0);
        key.monthly_budget = Some(1.0);

        let mut ledger = KeyLedger::default();
        assert!(ledger
            .reserve(1, &key, estimate, before_midnight, before_instant)
            .is_ok());

        let active = ledger.budget_snapshot(after_midnight, after_instant);
        assert_eq!(active.daily.active_reserved_usd, 0.8);
        assert_eq!(active.monthly.active_reserved_usd, 0.8);
        assert!(ledger
            .reserve(
                2,
                &key,
                AdmissionEstimate {
                    tokens: 1,
                    cost: Some(0.8),
                },
                after_midnight,
                after_instant,
            )
            .is_err());

        ledger.reconcile(1, Some(1), Some(0.8), after_midnight, after_instant);
        let settled = ledger.budget_snapshot(after_midnight, after_instant);
        assert_eq!(settled.daily.settled_spend_usd, 0.8);
        assert_eq!(settled.monthly.settled_spend_usd, 0.8);
        assert!(ledger
            .reserve(
                3,
                &key,
                AdmissionEstimate {
                    tokens: 1,
                    cost: Some(0.3),
                },
                after_midnight,
                after_instant,
            )
            .is_err());

        let root = std::env::temp_dir().join(format!(
            "kinetix-admission-rollover-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::connect(&format!("sqlite://{}", root.join("kinetix.db").display()))
            .await
            .unwrap();
        db::migrate(&pool).await.unwrap();
        let completion_ts = after_midnight.to_rfc3339();
        sqlx::query(
            "INSERT INTO usage_logs
             (id, request_id, ts, key_id, client_format, requested_model, status, status_code,
              input_tokens, output_tokens, cost_usd, cost_known)
             VALUES ('rollover', 'rollover', ?, 'key', 'openai', 'model', 'success', 200, 1, 1, 0.8, 1)",
        )
        .bind(&completion_ts)
        .execute(&pool)
        .await
        .unwrap();

        let day_start = "2026-02-01T00:00:00+00:00";
        assert_eq!(
            db::key_admission_budget_spend_since(&pool, "key", day_start)
                .await
                .unwrap(),
            settled.daily.settled_spend_usd
        );
        let persisted_usage =
            db::client_usage_summary(&pool, "key", day_start, "2026-02-02T00:00:00+00:00")
                .await
                .unwrap();
        assert_eq!(
            persisted_usage.known_cost_usd,
            settled.daily.settled_spend_usd
        );

        let restarted = AdmissionController::default();
        let restarted_entry = restarted.entry("key");
        restarted
            .ensure_initialized_at(
                &pool,
                "key",
                &restarted_entry,
                after_midnight,
                after_instant,
            )
            .await
            .unwrap();
        let restored = restarted_entry
            .ledger
            .lock()
            .budget_snapshot(after_midnight, after_instant);
        assert_eq!(
            restored.daily.settled_spend_usd,
            settled.daily.settled_spend_usd
        );
        assert_eq!(
            restored.monthly.settled_spend_usd,
            settled.monthly.settled_spend_usd
        );
        assert!(restarted_entry
            .ledger
            .lock()
            .reserve(
                1,
                &key,
                AdmissionEstimate {
                    tokens: 1,
                    cost: Some(0.3),
                },
                after_midnight,
                after_instant,
            )
            .is_err());

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn budget_snapshot_includes_active_reservations_without_exposing_them_individually() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.daily_budget = Some(1.0);
        key.monthly_budget = Some(2.0);
        let reservation = controller
            .reserve_initialized(
                entry.clone(),
                &key,
                AdmissionEstimate {
                    tokens: 1,
                    cost: Some(0.8),
                },
            )
            .unwrap();

        let snapshot = entry
            .ledger
            .lock()
            .budget_snapshot(Utc::now(), Instant::now());
        assert_eq!(snapshot.daily.settled_spend_usd, 0.0);
        assert_eq!(snapshot.daily.active_reserved_usd, 0.8);
        assert!(!snapshot.daily.has_unknown_active_cost);
        assert_eq!(snapshot.monthly.active_reserved_usd, 0.8);
        assert!(!snapshot.monthly.has_unknown_active_cost);
        assert!(controller
            .reserve_initialized(
                entry.clone(),
                &key,
                AdmissionEstimate {
                    tokens: 1,
                    cost: Some(0.3),
                },
            )
            .is_err());

        let unknown = controller
            .reserve_initialized(
                entry.clone(),
                &key,
                AdmissionEstimate {
                    tokens: 1,
                    cost: None,
                },
            )
            .unwrap();
        let snapshot = entry
            .ledger
            .lock()
            .budget_snapshot(Utc::now(), Instant::now());
        assert!(snapshot.daily.has_unknown_active_cost);
        assert!(snapshot.monthly.has_unknown_active_cost);
        assert_eq!(snapshot.daily.active_reserved_usd, 0.8);
        drop(unknown);
        drop(reservation);
    }

    #[test]
    fn concurrent_budget_burst_reserves_spend_atomically() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.daily_budget = Some(1.0);
        let reservations = burst(
            controller,
            entry,
            key,
            AdmissionEstimate {
                tokens: 1,
                cost: Some(0.4),
            },
            16,
        );
        assert_eq!(reservations.len(), 2);
    }

    #[test]
    fn reconciliation_replaces_estimate_with_complete_usage() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.tpm_limit = Some(100);
        let reservation = controller
            .reserve_initialized(
                entry.clone(),
                &key,
                AdmissionEstimate {
                    tokens: 80,
                    cost: Some(0.8),
                },
            )
            .unwrap();
        reservation.reconcile(
            &TokenUsage {
                input: Some(10),
                output: Some(10),
                ..Default::default()
            },
            Some(0.2),
        );
        let second = controller.reserve_initialized(
            entry,
            &key,
            AdmissionEstimate {
                tokens: 70,
                cost: Some(0.1),
            },
        );
        assert!(second.is_ok());
    }

    #[test]
    fn incomplete_usage_keeps_the_live_tpm_estimate() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.tpm_limit = Some(100);
        let reservation = controller
            .reserve_initialized(
                entry.clone(),
                &key,
                AdmissionEstimate {
                    tokens: 80,
                    cost: Some(0.8),
                },
            )
            .unwrap();
        reservation.reconcile_incomplete_at(Utc::now());
        let second = controller.reserve_initialized(
            entry,
            &key,
            AdmissionEstimate {
                tokens: 20,
                cost: Some(0.1),
            },
        );
        assert!(second.is_ok());
    }

    #[test]
    fn settled_unknown_cost_keeps_conservative_spend_and_unknown_status() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.daily_budget = Some(1.0);
        key.monthly_budget = Some(1.0);
        let reservation = controller
            .reserve_initialized(
                entry.clone(),
                &key,
                AdmissionEstimate {
                    tokens: 80,
                    cost: Some(0.8),
                },
            )
            .unwrap();
        reservation.reconcile(
            &TokenUsage {
                input: Some(10),
                output: None,
                ..Default::default()
            },
            None,
        );

        let snapshot = entry
            .ledger
            .lock()
            .budget_snapshot(Utc::now(), Instant::now());
        assert_eq!(snapshot.daily.settled_spend_usd, 0.8);
        assert_eq!(snapshot.monthly.settled_spend_usd, 0.8);
        assert!(snapshot.daily.has_unknown_settled_cost);
        assert!(snapshot.monthly.has_unknown_settled_cost);
        assert!(controller
            .reserve_initialized(
                entry,
                &key,
                AdmissionEstimate {
                    tokens: 1,
                    cost: Some(0.3),
                },
            )
            .is_err());
    }

    #[test]
    fn settled_unknown_cost_flags_roll_with_utc_budget_periods() {
        let jan_30 = NaiveDate::from_ymd_opt(2026, 1, 30)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let mut ledger = KeyLedger::default();
        ledger.roll_periods(jan_30);
        ledger.daily_spend = 0.8;
        ledger.monthly_spend = 0.8;
        ledger.daily_has_unknown_settled_cost = true;
        ledger.monthly_has_unknown_settled_cost = true;

        ledger.roll_periods(jan_30 + chrono::Duration::days(1));
        assert_eq!(ledger.daily_spend, 0.0);
        assert!(!ledger.daily_has_unknown_settled_cost);
        assert_eq!(ledger.monthly_spend, 0.8);
        assert!(ledger.monthly_has_unknown_settled_cost);

        let feb_1 = NaiveDate::from_ymd_opt(2026, 2, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        ledger.roll_periods(feb_1);
        assert_eq!(ledger.monthly_spend, 0.0);
        assert!(!ledger.monthly_has_unknown_settled_cost);
    }

    #[tokio::test]
    async fn persisted_unknown_tokens_fail_closed_for_tpm() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-admission-unknown-tokens-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::connect(&format!("sqlite://{}", root.join("kinetix.db").display()))
            .await
            .unwrap();
        db::migrate(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO usage_logs
             (id, request_id, ts, key_id, client_format, requested_model, status, status_code)
             VALUES (?, ?, ?, ?, 'openai', 'model', 'upstream_error', 502)",
        )
        .bind("failed-request")
        .bind("failed-request")
        .bind(db::now_iso())
        .bind("key")
        .execute(&pool)
        .await
        .unwrap();

        let controller = AdmissionController::default();
        let entry = controller.entry("key");
        controller
            .ensure_initialized(&pool, "key", &entry)
            .await
            .unwrap();
        let mut key = key();
        key.tpm_limit = Some(100);
        let next = controller.reserve_initialized(
            entry,
            &key,
            AdmissionEstimate {
                tokens: 1,
                cost: None,
            },
        );
        assert!(next.is_err());

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persisted_unknown_cost_retains_budget_reservation_after_restart() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-admission-unknown-cost-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::connect(&format!("sqlite://{}", root.join("kinetix.db").display()))
            .await
            .unwrap();
        db::migrate(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO usage_logs
             (id, request_id, ts, key_id, client_format, requested_model, status, status_code,
              cost_usd, cost_known, admission_cost_usd)
             VALUES (?, ?, ?, ?, 'openai', 'model', 'upstream_error', 502, NULL, 0, ?)",
        )
        .bind("failed-request")
        .bind("failed-request")
        .bind(db::now_iso())
        .bind("key")
        .bind(0.8f64)
        .execute(&pool)
        .await
        .unwrap();

        let daily_since = crate::pool::window_start("daily", None);
        assert_eq!(
            db::key_spend_since(&pool, "key", &daily_since)
                .await
                .unwrap(),
            0.0
        );
        assert_eq!(
            db::key_admission_budget_spend_since(&pool, "key", &daily_since)
                .await
                .unwrap(),
            0.8
        );

        let controller = AdmissionController::default();
        let entry = controller.entry("key");
        controller
            .ensure_initialized(&pool, "key", &entry)
            .await
            .unwrap();
        let mut key = key();
        key.daily_budget = Some(1.0);
        let next = controller.reserve_initialized(
            entry,
            &key,
            AdmissionEstimate {
                tokens: 1,
                cost: Some(0.4),
            },
        );
        assert!(next.is_err());

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_global_key_and_route_bursts_respect_capacity() {
        let global = concurrency_burst(AdmissionController::new(4), None, None, 32);
        assert_eq!(global.len(), 4);
        drop(global);

        let key = concurrency_burst(
            AdmissionController::new(32),
            Some(("key", Some(3))),
            None,
            32,
        );
        assert_eq!(key.len(), 3);
        drop(key);

        let route = concurrency_burst(
            AdmissionController::new(32),
            None,
            Some(("route", Some(2))),
            32,
        );
        assert_eq!(route.len(), 2);
    }

    #[test]
    fn concurrency_reservations_release_capacity_and_count_rejections() {
        let controller = AdmissionController::new(1);
        let reservation = controller.reserve_concurrency(None, None).unwrap();
        assert!(controller.reserve_concurrency(None, None).is_err());
        assert_eq!(
            controller.metrics_snapshot().rejected[RejectionReason::GlobalConcurrency as usize],
            1
        );
        drop(reservation);
        let next = controller.reserve_concurrency(None, None).unwrap();
        assert_eq!(controller.metrics_snapshot().inflight_inferences, 1);
        drop(next);
        assert_eq!(controller.metrics_snapshot().inflight_inferences, 0);

        let key_controller = AdmissionController::new(2);
        let key_reservation = key_controller
            .reserve_concurrency(Some(("key", Some(1))), None)
            .unwrap();
        assert!(key_controller
            .reserve_concurrency(Some(("key", Some(1))), None)
            .is_err());
        assert_eq!(
            key_controller.metrics_snapshot().rejected[RejectionReason::KeyConcurrency as usize],
            1
        );
        drop(key_reservation);

        let route_controller = AdmissionController::new(2);
        assert!(route_controller.route_capacity_available("route", Some(1)));
        let route_reservation = route_controller
            .reserve_concurrency(None, Some(("route", Some(1))))
            .unwrap();
        assert!(!route_controller.route_capacity_available("route", Some(1)));
        assert!(route_controller.route_capacity_available("unlimited", None));
        assert!(route_controller
            .reserve_concurrency(None, Some(("route", Some(1))))
            .is_err());
        assert_eq!(
            route_controller.metrics_snapshot().rejected
                [RejectionReason::RouteConcurrency as usize],
            1
        );
        drop(route_reservation);
        assert!(route_controller.route_capacity_available("route", Some(1)));
    }

    #[test]
    fn reconciliation_metrics_track_complete_incomplete_and_dropped_reservations() {
        let (controller, entry) = initialized_controller();
        let estimate = AdmissionEstimate {
            tokens: 80,
            cost: Some(0.8),
        };
        let complete = controller
            .reserve_initialized(entry.clone(), &key(), estimate)
            .unwrap();
        assert_eq!(controller.metrics_snapshot().active_reservations, 1);
        assert_eq!(controller.metrics_snapshot().active_reserved_tokens, 80);
        complete.reconcile(
            &TokenUsage {
                input: Some(10),
                output: Some(20),
                ..Default::default()
            },
            Some(0.3),
        );

        let incomplete = controller
            .reserve_initialized(entry.clone(), &key(), estimate)
            .unwrap();
        incomplete.reconcile(
            &TokenUsage {
                input: Some(10),
                output: None,
                ..Default::default()
            },
            None,
        );
        let dropped = controller
            .reserve_initialized(entry, &key(), estimate)
            .unwrap();
        drop(dropped);

        let metrics = controller.metrics_snapshot();
        assert_eq!(metrics.active_reservations, 0);
        assert_eq!(metrics.active_reserved_tokens, 0);
        assert_eq!(metrics.reservations_total, 3);
        assert_eq!(metrics.reserved_tokens_total, 240);
        assert_eq!(metrics.reconciled_complete_total, 1);
        assert_eq!(metrics.reconciled_incomplete_total, 1);
        assert_eq!(metrics.dropped_total, 1);
        assert_eq!(metrics.reconciled_tokens_total, 30);
        assert_eq!(metrics.reduced_tokens_total, 50);
        assert_eq!(metrics.increased_tokens_total, 0);
    }
}
