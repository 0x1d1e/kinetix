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
    monthly_key: Option<(i32, u32)>,
    monthly_spend: f64,
}

struct MinuteUse {
    at: Instant,
    tokens: u64,
}

struct ActiveReservation {
    at: Instant,
    tokens: u64,
    cost: Option<f64>,
    day: NaiveDate,
    month: (i32, u32),
}

#[derive(Debug, Clone, Copy)]
pub struct AdmissionEstimate {
    pub tokens: u64,
    pub cost: Option<f64>,
}

pub struct AdmissionReservation {
    entry: Arc<KeyAdmission>,
    // Instrument the existing reservation lifetime without changing ledger policy.
    metrics: Arc<AdmissionMetrics>,
    id: u64,
    estimate_tokens: u64,
    settled: bool,
}

impl AdmissionReservation {
    pub fn reconcile(mut self, usage: &TokenUsage, actual_cost: Option<f64>) {
        let actual_tokens = match (usage.input, usage.output) {
            (Some(input), Some(output)) => Some(input.saturating_add(output)),
            _ => None,
        };
        let actual_cost = actual_tokens.and(actual_cost);
        self.entry.ledger.lock().reconcile(
            self.id,
            actual_tokens,
            actual_cost,
            Utc::now(),
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

    async fn ensure_initialized(&self, pool: &Pool, key_id: &str, entry: &Arc<KeyAdmission>) {
        if entry.initialized.load(Ordering::Acquire) {
            return;
        }

        let _guard = entry.init_lock.lock().await;
        if entry.initialized.load(Ordering::Acquire) {
            return;
        }

        let wall_now = Utc::now();
        let instant_now = Instant::now();
        let minute_since = (wall_now - chrono::Duration::seconds(60)).to_rfc3339();
        let daily_since = crate::pool::window_start("daily", None);
        let monthly_since = crate::pool::window_start("monthly", None);

        let (minute, daily, monthly) = tokio::join!(
            db::key_usage_entries_since(pool, key_id, &minute_since),
            db::key_spend_since(pool, key_id, &daily_since),
            db::key_spend_since(pool, key_id, &monthly_since),
        );

        let mut ledger = entry.ledger.lock();
        ledger.roll_periods(wall_now);
        match minute {
            Ok(rows) => {
                for (ts, tokens) in rows {
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
                        tokens: tokens.max(0) as u64,
                    });
                }
            }
            Err(error) => tracing::warn!(%error, key_id, "could not seed admission minute window"),
        }
        match daily {
            Ok(spend) => ledger.daily_spend = spend.max(0.0),
            Err(error) => tracing::warn!(%error, key_id, "could not seed daily admission spend"),
        }
        match monthly {
            Ok(spend) => ledger.monthly_spend = spend.max(0.0),
            Err(error) => tracing::warn!(%error, key_id, "could not seed monthly admission spend"),
        }
        ledger.prune(instant_now);
        entry.initialized.store(true, Ordering::Release);
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
        self.ensure_initialized(pool, &key.id, &entry).await;
        self.reserve_initialized(entry, key, estimate)
    }

    pub async fn check_current(&self, pool: &Pool, key: &VirtualKeyRow) -> Result<(), ProxyError> {
        let entry = self.entry(&key.id);
        self.ensure_initialized(pool, &key.id, &entry).await;
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
        }
        let month = (now.year(), now.month());
        if self.monthly_key != Some(month) {
            self.monthly_key = Some(month);
            self.monthly_spend = 0.0;
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
        let day = now_wall.date_naive();
        let month = (now_wall.year(), now_wall.month());

        let mut requests = self.minute.len() as u64;
        let mut tokens: u64 = self.minute.iter().map(|entry| entry.tokens).sum();
        let mut daily = self.daily_spend;
        let mut monthly = self.monthly_spend;

        for active in self.active.values() {
            if now.saturating_duration_since(active.at) < MINUTE_WINDOW {
                requests = requests.saturating_add(1);
                tokens = tokens.saturating_add(active.tokens);
            }
            if let Some(cost) = active.cost {
                if active.day == day {
                    daily += cost;
                }
                if active.month == month {
                    monthly += cost;
                }
            }
        }
        (requests, tokens, daily, monthly)
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
                day: now_wall.date_naive(),
                month: (now_wall.year(), now_wall.month()),
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
                tokens,
            });
        }

        let cost = actual_cost.or(active.cost);
        if let Some(cost) = cost {
            if self.daily_day == Some(active.day) {
                self.daily_spend += cost;
            }
            if self.monthly_key == Some(active.month) {
                self.monthly_spend += cost;
            }
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
    fn incomplete_usage_keeps_conservative_reservation() {
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
                output: None,
                ..Default::default()
            },
            None,
        );
        let second = controller.reserve_initialized(
            entry,
            &key,
            AdmissionEstimate {
                tokens: 30,
                cost: Some(0.1),
            },
        );
        assert!(second.is_err());
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
        let route_reservation = route_controller
            .reserve_concurrency(None, Some(("route", Some(1))))
            .unwrap();
        assert!(route_controller
            .reserve_concurrency(None, Some(("route", Some(1))))
            .is_err());
        assert_eq!(
            route_controller.metrics_snapshot().rejected
                [RejectionReason::RouteConcurrency as usize],
            1
        );
        drop(route_reservation);
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
