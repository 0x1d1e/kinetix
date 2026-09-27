//! Durable, bounded per-target latency and reliability telemetry.
//!
//! Request handling only performs a non-blocking channel send. A background
//! worker folds events into minute buckets and persists fixed histograms so
//! p50/p95/p99 can be reconstructed without storing unbounded raw samples.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use sqlx::Row;

use crate::db::Pool;
use crate::upstream_traffic::TargetKey;

const QUEUE_CAPACITY: usize = 2048;
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const RETENTION_SECS: i64 = 25 * 60 * 60;
const HISTOGRAM_BUCKETS: usize = 10;
const TTFT_BOUNDS_MS: [u64; HISTOGRAM_BUCKETS] = [
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000,
];
const DURATION_BOUNDS_MS: [u64; HISTOGRAM_BUCKETS] = [
    250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 180_000, 600_000,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryOutcome {
    Success,
    RateLimit,
    QuotaExhausted,
    ServerError,
    ConnectionError,
    Timeout,
    AuthError,
    TargetError,
    BadRequest,
    Cancelled,
    AdaptiveSaturation,
    ProviderCircuitReject,
}

#[derive(Debug, Clone)]
pub struct TelemetryEvent {
    pub target: TargetKey,
    pub attempted: bool,
    pub outcome: TelemetryOutcome,
    pub ttft_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub fallback_attempt: bool,
    pub caused_fallback: bool,
    pub provider_circuit_open: bool,
    pub provider_circuit_recovery: bool,
    pub half_open_probe: bool,
}

impl TelemetryEvent {
    pub fn attempt(
        target: TargetKey,
        outcome: TelemetryOutcome,
        ttft_ms: Option<u64>,
        duration_ms: u64,
        fallback_attempt: bool,
        caused_fallback: bool,
    ) -> Self {
        Self {
            target,
            attempted: true,
            outcome,
            ttft_ms,
            duration_ms: Some(duration_ms),
            fallback_attempt,
            caused_fallback,
            provider_circuit_open: false,
            provider_circuit_recovery: false,
            half_open_probe: false,
        }
    }

    pub fn synthetic(target: TargetKey, outcome: TelemetryOutcome) -> Self {
        Self {
            target,
            attempted: false,
            outcome,
            ttft_ms: None,
            duration_ms: None,
            fallback_attempt: false,
            caused_fallback: false,
            provider_circuit_open: false,
            provider_circuit_recovery: false,
            half_open_probe: false,
        }
    }
}

#[derive(Clone)]
pub struct TargetTelemetry {
    tx: tokio::sync::mpsc::Sender<TelemetryEvent>,
    dropped_queue: Arc<AtomicU64>,
    dropped_persistence: Arc<AtomicU64>,
}

impl TargetTelemetry {
    pub fn new(pool: Pool) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_CAPACITY);
        let dropped_queue = Arc::new(AtomicU64::new(0));
        let dropped_persistence = Arc::new(AtomicU64::new(0));
        spawn_worker(pool, rx, dropped_persistence.clone());
        Self {
            tx,
            dropped_queue,
            dropped_persistence,
        }
    }

    pub fn record(&self, event: TelemetryEvent) {
        if self.tx.try_send(event).is_err() {
            self.dropped_queue.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn dropped_queue(&self) -> u64 {
        self.dropped_queue.load(Ordering::Relaxed)
    }

    pub fn dropped_persistence(&self) -> u64 {
        self.dropped_persistence.load(Ordering::Relaxed)
    }

    pub async fn summaries(
        &self,
        pool: &Pool,
        window_secs: i64,
    ) -> anyhow::Result<Vec<TelemetrySummary>> {
        let cutoff = chrono::Utc::now().timestamp() - window_secs.max(1);
        let rows = sqlx::query(
            "SELECT * FROM target_health_buckets WHERE bucket_start >= ? ORDER BY scope, provider_id, account_id, model_id",
        )
        .bind(cutoff)
        .fetch_all(pool)
        .await?;

        let mut combined: HashMap<SummaryKey, BucketAggregate> = HashMap::new();
        for row in rows {
            let key = SummaryKey {
                scope: row.try_get::<String, _>("scope")?,
                provider_id: row.try_get::<String, _>("provider_id")?,
                account_id: empty_to_none(row.try_get::<String, _>("account_id")?),
                model_id: empty_to_none(row.try_get::<String, _>("model_id")?),
            };
            let aggregate = combined.entry(key).or_default();
            aggregate.attempts += row.try_get::<i64, _>("attempts")?.max(0) as u64;
            aggregate.successes += row.try_get::<i64, _>("successes")?.max(0) as u64;
            aggregate.fallback_failures +=
                row.try_get::<i64, _>("fallback_failures")?.max(0) as u64;
            aggregate.rate_limits += row.try_get::<i64, _>("rate_limits")?.max(0) as u64;
            aggregate.quota_exhausted += row.try_get::<i64, _>("quota_exhausted")?.max(0) as u64;
            aggregate.auth_errors += row.try_get::<i64, _>("auth_errors")?.max(0) as u64;
            aggregate.target_errors += row.try_get::<i64, _>("target_errors")?.max(0) as u64;
            aggregate.bad_requests += row.try_get::<i64, _>("bad_requests")?.max(0) as u64;
            aggregate.server_5xx += row.try_get::<i64, _>("server_5xx")?.max(0) as u64;
            aggregate.connection_errors +=
                row.try_get::<i64, _>("connection_errors")?.max(0) as u64;
            aggregate.timeouts += row.try_get::<i64, _>("timeouts")?.max(0) as u64;
            aggregate.cancellations += row.try_get::<i64, _>("cancellations")?.max(0) as u64;
            aggregate.fallbacks += row.try_get::<i64, _>("fallbacks")?.max(0) as u64;
            aggregate.adaptive_saturation +=
                row.try_get::<i64, _>("adaptive_saturation")?.max(0) as u64;
            aggregate.provider_circuit_rejects +=
                row.try_get::<i64, _>("provider_circuit_rejects")?.max(0) as u64;
            aggregate.provider_circuit_opens +=
                row.try_get::<i64, _>("provider_circuit_opens")?.max(0) as u64;
            aggregate.provider_circuit_recoveries +=
                row.try_get::<i64, _>("provider_circuit_recoveries")?.max(0) as u64;
            aggregate.half_open_probes += row.try_get::<i64, _>("half_open_probes")?.max(0) as u64;
            for index in 0..HISTOGRAM_BUCKETS {
                let ttft_column = format!("ttft_b{index}");
                let duration_column = format!("duration_b{index}");
                aggregate.ttft_hist[index] +=
                    row.try_get::<i64, _>(ttft_column.as_str())?.max(0) as u64;
                aggregate.duration_hist[index] +=
                    row.try_get::<i64, _>(duration_column.as_str())?.max(0) as u64;
            }
        }

        let mut out: Vec<_> = combined
            .into_iter()
            .map(|(key, aggregate)| TelemetrySummary::from_aggregate(key, aggregate, window_secs))
            .collect();
        out.sort_by(|a, b| {
            a.scope
                .cmp(&b.scope)
                .then_with(|| a.provider_id.cmp(&b.provider_id))
                .then_with(|| a.account_id.cmp(&b.account_id))
                .then_with(|| a.model_id.cmp(&b.model_id))
        });
        Ok(out)
    }
}

#[derive(Debug, Clone, Eq)]
struct BucketKey {
    bucket_start: i64,
    scope: &'static str,
    provider_id: String,
    account_id: String,
    model_id: String,
}

impl PartialEq for BucketKey {
    fn eq(&self, other: &Self) -> bool {
        self.bucket_start == other.bucket_start
            && self.scope == other.scope
            && self.provider_id == other.provider_id
            && self.account_id == other.account_id
            && self.model_id == other.model_id
    }
}

impl Hash for BucketKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bucket_start.hash(state);
        self.scope.hash(state);
        self.provider_id.hash(state);
        self.account_id.hash(state);
        self.model_id.hash(state);
    }
}

#[derive(Debug, Clone, Default)]
struct BucketAggregate {
    attempts: u64,
    successes: u64,
    fallback_failures: u64,
    rate_limits: u64,
    quota_exhausted: u64,
    auth_errors: u64,
    target_errors: u64,
    bad_requests: u64,
    server_5xx: u64,
    connection_errors: u64,
    timeouts: u64,
    cancellations: u64,
    fallbacks: u64,
    adaptive_saturation: u64,
    provider_circuit_rejects: u64,
    provider_circuit_opens: u64,
    provider_circuit_recoveries: u64,
    half_open_probes: u64,
    ttft_hist: [u64; HISTOGRAM_BUCKETS],
    duration_hist: [u64; HISTOGRAM_BUCKETS],
}

impl BucketAggregate {
    fn add(&mut self, event: &TelemetryEvent) {
        self.attempts += event.attempted as u64;
        self.successes += (event.outcome == TelemetryOutcome::Success) as u64;
        self.fallbacks += event.fallback_attempt as u64;
        self.fallback_failures += event.caused_fallback as u64;
        self.rate_limits += (event.outcome == TelemetryOutcome::RateLimit) as u64;
        self.quota_exhausted += (event.outcome == TelemetryOutcome::QuotaExhausted) as u64;
        self.auth_errors += (event.outcome == TelemetryOutcome::AuthError) as u64;
        self.target_errors += (event.outcome == TelemetryOutcome::TargetError) as u64;
        self.bad_requests += (event.outcome == TelemetryOutcome::BadRequest) as u64;
        self.server_5xx += (event.outcome == TelemetryOutcome::ServerError) as u64;
        self.connection_errors += (event.outcome == TelemetryOutcome::ConnectionError) as u64;
        self.timeouts += (event.outcome == TelemetryOutcome::Timeout) as u64;
        self.cancellations += (event.outcome == TelemetryOutcome::Cancelled) as u64;
        self.adaptive_saturation += (event.outcome == TelemetryOutcome::AdaptiveSaturation) as u64;
        self.provider_circuit_rejects +=
            (event.outcome == TelemetryOutcome::ProviderCircuitReject) as u64;
        self.provider_circuit_opens += event.provider_circuit_open as u64;
        self.provider_circuit_recoveries += event.provider_circuit_recovery as u64;
        self.half_open_probes += event.half_open_probe as u64;
        if let Some(value) = event.ttft_ms {
            self.ttft_hist[hist_bucket(value, &TTFT_BOUNDS_MS)] += 1;
        }
        if let Some(value) = event.duration_ms {
            self.duration_hist[hist_bucket(value, &DURATION_BOUNDS_MS)] += 1;
        }
    }
}

fn add_event(pending: &mut HashMap<BucketKey, BucketAggregate>, event: TelemetryEvent) {
    let now = chrono::Utc::now().timestamp();
    let bucket_start = now - now.rem_euclid(60);
    let dimensions = [
        ("provider", "", ""),
        ("account", event.target.account_id.as_str(), ""),
        (
            "model",
            event.target.account_id.as_str(),
            event.target.model_id.as_str(),
        ),
    ];
    for (scope, account_id, model_id) in dimensions {
        let key = BucketKey {
            bucket_start,
            scope,
            provider_id: event.target.provider_id.clone(),
            account_id: account_id.to_string(),
            model_id: model_id.to_string(),
        };
        pending.entry(key).or_default().add(&event);
    }
}

fn spawn_worker(
    pool: Pool,
    mut rx: tokio::sync::mpsc::Receiver<TelemetryEvent>,
    dropped_persistence: Arc<AtomicU64>,
) {
    tokio::spawn(async move {
        let mut pending = HashMap::new();
        let mut pending_events = 0u64;
        let mut tick = tokio::time::interval(FLUSH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;

        loop {
            tokio::select! {
                event = rx.recv() => {
                    let Some(event) = event else {
                        if !pending.is_empty() {
                            if flush(&pool, &mut pending).await.is_err() {
                                dropped_persistence.fetch_add(pending_events, Ordering::Relaxed);
                            }
                        }
                        break;
                    };
                    add_event(&mut pending, event);
                    pending_events = pending_events.saturating_add(1);
                    if pending.len() >= 512 {
                        if let Err(error) = flush(&pool, &mut pending).await {
                            dropped_persistence.fetch_add(pending_events, Ordering::Relaxed);
                            tracing::warn!(%error, "target telemetry flush failed");
                        }
                        pending_events = 0;
                    }
                }
                _ = tick.tick() => {
                    if pending.is_empty() {
                        continue;
                    }
                    if let Err(error) = flush(&pool, &mut pending).await {
                        dropped_persistence.fetch_add(pending_events, Ordering::Relaxed);
                        tracing::warn!(%error, "target telemetry flush failed");
                    }
                    pending_events = 0;
                }
            }
        }
    });
}

async fn flush(
    pool: &Pool,
    pending: &mut HashMap<BucketKey, BucketAggregate>,
) -> anyhow::Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let drained = std::mem::take(pending);
    let mut tx = pool.begin().await?;

    for (key, value) in drained {
        let mut query = sqlx::query(
            "INSERT INTO target_health_buckets (
                bucket_start, scope, provider_id, account_id, model_id,
                attempts, successes, fallback_failures, rate_limits, quota_exhausted,
                auth_errors, target_errors, bad_requests,
                server_5xx, connection_errors, timeouts, cancellations, fallbacks,
                adaptive_saturation, provider_circuit_rejects, provider_circuit_opens,
                provider_circuit_recoveries, half_open_probes,
                ttft_b0, ttft_b1, ttft_b2, ttft_b3, ttft_b4, ttft_b5, ttft_b6, ttft_b7, ttft_b8, ttft_b9,
                duration_b0, duration_b1, duration_b2, duration_b3, duration_b4,
                duration_b5, duration_b6, duration_b7, duration_b8, duration_b9
             ) VALUES (
                ?, ?, ?, ?, ?,
                ?, ?, ?, ?, ?, ?, ?, ?,
                ?, ?, ?, ?, ?,
                ?, ?, ?, ?, ?,
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
             )
             ON CONFLICT(bucket_start, scope, provider_id, account_id, model_id) DO UPDATE SET
                attempts = attempts + excluded.attempts,
                successes = successes + excluded.successes,
                fallback_failures = fallback_failures + excluded.fallback_failures,
                rate_limits = rate_limits + excluded.rate_limits,
                quota_exhausted = quota_exhausted + excluded.quota_exhausted,
                auth_errors = auth_errors + excluded.auth_errors,
                target_errors = target_errors + excluded.target_errors,
                bad_requests = bad_requests + excluded.bad_requests,
                server_5xx = server_5xx + excluded.server_5xx,
                connection_errors = connection_errors + excluded.connection_errors,
                timeouts = timeouts + excluded.timeouts,
                cancellations = cancellations + excluded.cancellations,
                fallbacks = fallbacks + excluded.fallbacks,
                adaptive_saturation = adaptive_saturation + excluded.adaptive_saturation,
                provider_circuit_rejects = provider_circuit_rejects + excluded.provider_circuit_rejects,
                provider_circuit_opens = provider_circuit_opens + excluded.provider_circuit_opens,
                provider_circuit_recoveries = provider_circuit_recoveries + excluded.provider_circuit_recoveries,
                half_open_probes = half_open_probes + excluded.half_open_probes,
                ttft_b0 = ttft_b0 + excluded.ttft_b0,
                ttft_b1 = ttft_b1 + excluded.ttft_b1,
                ttft_b2 = ttft_b2 + excluded.ttft_b2,
                ttft_b3 = ttft_b3 + excluded.ttft_b3,
                ttft_b4 = ttft_b4 + excluded.ttft_b4,
                ttft_b5 = ttft_b5 + excluded.ttft_b5,
                ttft_b6 = ttft_b6 + excluded.ttft_b6,
                ttft_b7 = ttft_b7 + excluded.ttft_b7,
                ttft_b8 = ttft_b8 + excluded.ttft_b8,
                ttft_b9 = ttft_b9 + excluded.ttft_b9,
                duration_b0 = duration_b0 + excluded.duration_b0,
                duration_b1 = duration_b1 + excluded.duration_b1,
                duration_b2 = duration_b2 + excluded.duration_b2,
                duration_b3 = duration_b3 + excluded.duration_b3,
                duration_b4 = duration_b4 + excluded.duration_b4,
                duration_b5 = duration_b5 + excluded.duration_b5,
                duration_b6 = duration_b6 + excluded.duration_b6,
                duration_b7 = duration_b7 + excluded.duration_b7,
                duration_b8 = duration_b8 + excluded.duration_b8,
                duration_b9 = duration_b9 + excluded.duration_b9",
        )
        .bind(key.bucket_start)
        .bind(key.scope)
        .bind(key.provider_id)
        .bind(key.account_id)
        .bind(key.model_id)
        .bind(value.attempts as i64)
        .bind(value.successes as i64)
        .bind(value.fallback_failures as i64)
        .bind(value.rate_limits as i64)
        .bind(value.quota_exhausted as i64)
        .bind(value.auth_errors as i64)
        .bind(value.target_errors as i64)
        .bind(value.bad_requests as i64)
        .bind(value.server_5xx as i64)
        .bind(value.connection_errors as i64)
        .bind(value.timeouts as i64)
        .bind(value.cancellations as i64)
        .bind(value.fallbacks as i64)
        .bind(value.adaptive_saturation as i64)
        .bind(value.provider_circuit_rejects as i64)
        .bind(value.provider_circuit_opens as i64)
        .bind(value.provider_circuit_recoveries as i64)
        .bind(value.half_open_probes as i64);

        for count in value.ttft_hist {
            query = query.bind(count as i64);
        }
        for count in value.duration_hist {
            query = query.bind(count as i64);
        }
        query.execute(&mut *tx).await?;
    }

    let cutoff = chrono::Utc::now().timestamp() - RETENTION_SECS;
    sqlx::query("DELETE FROM target_health_buckets WHERE bucket_start < ?")
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

fn hist_bucket(value: u64, bounds: &[u64; HISTOGRAM_BUCKETS]) -> usize {
    bounds
        .iter()
        .position(|bound| value <= *bound)
        .unwrap_or(bounds.len() - 1)
}

fn quantile(
    hist: &[u64; HISTOGRAM_BUCKETS],
    bounds: &[u64; HISTOGRAM_BUCKETS],
    q: f64,
) -> Option<u64> {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return None;
    }
    let target = ((total as f64) * q).ceil().max(1.0) as u64;
    let mut seen = 0u64;
    for (index, count) in hist.iter().enumerate() {
        seen = seen.saturating_add(*count);
        if seen >= target {
            return Some(bounds[index]);
        }
    }
    bounds.last().copied()
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct SummaryKey {
    scope: String,
    provider_id: String,
    account_id: Option<String>,
    model_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelemetrySummary {
    pub scope: String,
    pub provider_id: String,
    pub account_id: Option<String>,
    pub model_id: Option<String>,
    pub window_secs: i64,
    pub attempts: u64,
    pub successes: u64,
    pub success_rate: Option<f64>,
    pub fallback_failures: u64,
    pub rate_limits: u64,
    pub quota_exhausted: u64,
    pub auth_errors: u64,
    pub target_errors: u64,
    pub bad_requests: u64,
    pub server_5xx: u64,
    pub connection_errors: u64,
    pub timeouts: u64,
    pub cancellations: u64,
    pub fallbacks: u64,
    pub adaptive_saturation: u64,
    pub provider_circuit_rejects: u64,
    pub provider_circuit_opens: u64,
    pub provider_circuit_recoveries: u64,
    pub half_open_probes: u64,
    pub ttft_p50_ms: Option<u64>,
    pub ttft_p95_ms: Option<u64>,
    pub ttft_p99_ms: Option<u64>,
    pub duration_p50_ms: Option<u64>,
    pub duration_p95_ms: Option<u64>,
    pub duration_p99_ms: Option<u64>,
}

impl TelemetrySummary {
    fn from_aggregate(key: SummaryKey, value: BucketAggregate, window_secs: i64) -> Self {
        Self {
            scope: key.scope,
            provider_id: key.provider_id,
            account_id: key.account_id,
            model_id: key.model_id,
            window_secs,
            attempts: value.attempts,
            successes: value.successes,
            success_rate: {
                let reliability_attempts = value.attempts.saturating_sub(value.cancellations);
                (reliability_attempts > 0)
                    .then_some(value.successes as f64 / reliability_attempts as f64)
            },
            fallback_failures: value.fallback_failures,
            rate_limits: value.rate_limits,
            quota_exhausted: value.quota_exhausted,
            auth_errors: value.auth_errors,
            target_errors: value.target_errors,
            bad_requests: value.bad_requests,
            server_5xx: value.server_5xx,
            connection_errors: value.connection_errors,
            timeouts: value.timeouts,
            cancellations: value.cancellations,
            fallbacks: value.fallbacks,
            adaptive_saturation: value.adaptive_saturation,
            provider_circuit_rejects: value.provider_circuit_rejects,
            provider_circuit_opens: value.provider_circuit_opens,
            provider_circuit_recoveries: value.provider_circuit_recoveries,
            half_open_probes: value.half_open_probes,
            ttft_p50_ms: quantile(&value.ttft_hist, &TTFT_BOUNDS_MS, 0.50),
            ttft_p95_ms: quantile(&value.ttft_hist, &TTFT_BOUNDS_MS, 0.95),
            ttft_p99_ms: quantile(&value.ttft_hist, &TTFT_BOUNDS_MS, 0.99),
            duration_p50_ms: quantile(&value.duration_hist, &DURATION_BOUNDS_MS, 0.50),
            duration_p95_ms: quantile(&value.duration_hist, &DURATION_BOUNDS_MS, 0.95),
            duration_p99_ms: quantile(&value.duration_hist, &DURATION_BOUNDS_MS, 0.99),
        }
    }
}

fn empty_to_none(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failure_taxonomy_is_persisted_and_returned_by_summaries() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-telemetry-taxonomy-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let database_url = format!("sqlite://{}?mode=rwc", root.join("health.db").display());
        let pool = crate::db::connect(&database_url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();

        let mut pending = HashMap::new();
        for outcome in [
            TelemetryOutcome::AuthError,
            TelemetryOutcome::TargetError,
            TelemetryOutcome::BadRequest,
        ] {
            add_event(
                &mut pending,
                TelemetryEvent::attempt(
                    TargetKey::new("p", "a", "m"),
                    outcome,
                    None,
                    10,
                    false,
                    false,
                ),
            );
        }
        flush(&pool, &mut pending).await.unwrap();

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let telemetry = TargetTelemetry {
            tx,
            dropped_queue: Arc::new(AtomicU64::new(0)),
            dropped_persistence: Arc::new(AtomicU64::new(0)),
        };
        let summaries = telemetry.summaries(&pool, 3600).await.unwrap();
        let provider = summaries
            .iter()
            .find(|row| row.scope == "provider")
            .unwrap();
        assert_eq!(provider.auth_errors, 1);
        assert_eq!(provider.target_errors, 1);
        assert_eq!(provider.bad_requests, 1);
        let json = serde_json::to_value(provider).unwrap();
        assert_eq!(json["auth_errors"], 1);
        assert_eq!(json["target_errors"], 1);
        assert_eq!(json["bad_requests"], 1);

        pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn histogram_quantiles_are_bounded_and_monotonic() {
        let mut hist = [0u64; HISTOGRAM_BUCKETS];
        for value in [10, 60, 200, 900, 2_000, 8_000, 50_000] {
            hist[hist_bucket(value, &TTFT_BOUNDS_MS)] += 1;
        }
        let p50 = quantile(&hist, &TTFT_BOUNDS_MS, 0.50).unwrap();
        let p95 = quantile(&hist, &TTFT_BOUNDS_MS, 0.95).unwrap();
        let p99 = quantile(&hist, &TTFT_BOUNDS_MS, 0.99).unwrap();
        assert!(p50 <= p95 && p95 <= p99);
        assert!(p99 <= 60_000);
    }

    #[test]
    fn duration_histogram_keeps_long_llm_streams_distinct() {
        let mut hist = [0u64; HISTOGRAM_BUCKETS];
        for value in [20_000, 70_000, 240_000, 500_000] {
            hist[hist_bucket(value, &DURATION_BOUNDS_MS)] += 1;
        }
        assert_eq!(quantile(&hist, &DURATION_BOUNDS_MS, 0.99), Some(600_000));
    }

    #[test]
    fn cancellation_is_not_a_reliability_failure() {
        let mut aggregate = BucketAggregate::default();
        let event = TelemetryEvent::attempt(
            TargetKey::new("p", "a", "m"),
            TelemetryOutcome::Cancelled,
            Some(100),
            500,
            false,
            false,
        );
        aggregate.add(&event);
        assert_eq!(aggregate.attempts, 1);
        assert_eq!(aggregate.cancellations, 1);
        assert_eq!(aggregate.fallback_failures, 0);
        assert_eq!(aggregate.successes, 0);
    }

    #[test]
    fn cancellations_do_not_reduce_reliability_success_rate() {
        let key = SummaryKey {
            scope: "model".into(),
            provider_id: "p".into(),
            account_id: Some("a".into()),
            model_id: Some("m".into()),
        };
        let value = BucketAggregate {
            attempts: 2,
            successes: 1,
            cancellations: 1,
            ..Default::default()
        };

        let summary = TelemetrySummary::from_aggregate(key, value, 300);
        assert_eq!(summary.success_rate, Some(1.0));
    }

    #[test]
    fn target_event_updates_all_three_dimensions() {
        let mut pending = HashMap::new();
        add_event(
            &mut pending,
            TelemetryEvent::attempt(
                TargetKey::new("p", "a", "m"),
                TelemetryOutcome::Success,
                Some(100),
                400,
                false,
                false,
            ),
        );
        assert_eq!(pending.len(), 3);
        assert!(pending.keys().any(|key| key.scope == "provider"));
        assert!(pending.keys().any(|key| key.scope == "account"));
        assert!(pending.keys().any(|key| key.scope == "model"));
    }
}
