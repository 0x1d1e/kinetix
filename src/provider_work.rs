//! Provider-scoped budgets for background and control-plane work.
//!
//! Inference does not consult this coordinator. Callers use cached observations
//! on the request path and route scheduled/provider control-plane work here.

use std::any::{Any, TypeId};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use futures::future::{BoxFuture, FutureExt, Shared};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::credentials::CredentialRotationError;
use crate::outbound::OutboundError;
use crate::plugins::runtime::PluginFault;
use crate::types::{FailureCategory, FailureKind, UpstreamFailure};

const MAX_CONCURRENT_PER_PROVIDER: usize = 2;
const MAX_OPERATIONS_PER_WINDOW: usize = 20;
const RATE_WINDOW: Duration = Duration::from_secs(10);
const MIN_PROVIDER_SPACING: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
const MAX_SINGLEFLIGHTS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderWorkClass {
    CredentialRefresh,
    HealthProbe,
    QuotaProbe,
    CapabilityProbe,
    ModelDiscovery,
    RoutingFactsRefresh,
    PricingRefresh,
}

impl ProviderWorkClass {
    fn min_spacing(self) -> Duration {
        match self {
            Self::CredentialRefresh => Duration::from_millis(100),
            Self::RoutingFactsRefresh => Duration::from_millis(250),
            Self::HealthProbe
            | Self::QuotaProbe
            | Self::CapabilityProbe
            | Self::ModelDiscovery
            | Self::PricingRefresh => Duration::from_millis(500),
        }
    }
}

#[derive(Default)]
struct WorkMetrics {
    scheduled: AtomicU64,
    executed: AtomicU64,
    coalesced: AtomicU64,
    rate_limited: AtomicU64,
    provider_throttled: AtomicU64,
    backed_off: AtomicU64,
    failed: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderWorkMetricsSnapshot {
    pub scheduled: u64,
    pub executed: u64,
    pub coalesced: u64,
    pub rate_limited: u64,
    pub provider_throttled: u64,
    pub backed_off: u64,
    pub failed: u64,
}

impl WorkMetrics {
    fn snapshot(&self) -> ProviderWorkMetricsSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        ProviderWorkMetricsSnapshot {
            scheduled: load(&self.scheduled),
            executed: load(&self.executed),
            coalesced: load(&self.coalesced),
            rate_limited: load(&self.rate_limited),
            provider_throttled: load(&self.provider_throttled),
            backed_off: load(&self.backed_off),
            failed: load(&self.failed),
        }
    }
}

#[derive(Default)]
struct RateState {
    starts: VecDeque<Instant>,
    last_start: Option<Instant>,
    last_by_class: HashMap<ProviderWorkClass, Instant>,
    consecutive_failures: u32,
    backed_off_until: Option<Instant>,
}

struct ProviderGate {
    slots: Arc<Semaphore>,
    rate: tokio::sync::Mutex<RateState>,
}

impl Default for ProviderGate {
    fn default() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_PER_PROVIDER)),
            rate: tokio::sync::Mutex::new(RateState::default()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FlightKey {
    provider_id: String,
    class: ProviderWorkClass,
    key: String,
    result_types: TypeId,
}

type WorkResult<T, E> = Result<Arc<T>, Arc<ProviderWorkError<E>>>;
type SharedWork<T, E> = Shared<BoxFuture<'static, WorkResult<T, E>>>;

struct Flight<T, E> {
    result: tokio::sync::Mutex<SharedWork<T, E>>,
}

#[derive(Debug)]
pub enum ProviderWorkError<E> {
    /// An upstream failure installed provider backoff. The work was not run.
    BackedOff(Duration),
    /// The work ran and returned its operation-specific error.
    Operation(E),
    /// The coordinator task ended unexpectedly.
    Aborted,
}

/// Shared provider-level rate, concurrency, and backoff budgets.
///
/// Provider and in-flight maps contain configured providers and active
/// single-flights only. Metrics are aggregate counters with no high-cardinality
/// provider/account/model labels.
#[derive(Clone, Default)]
pub struct ProviderWorkCoordinator {
    providers: Arc<DashMap<String, Arc<ProviderGate>>>,
    flights: Arc<DashMap<FlightKey, Arc<dyn Any + Send + Sync>>>,
    metrics: Arc<WorkMetrics>,
}

impl ProviderWorkCoordinator {
    fn gate(&self, provider_id: &str) -> Arc<ProviderGate> {
        self.providers
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(ProviderGate::default()))
            .clone()
    }

    pub fn metrics_snapshot(&self) -> ProviderWorkMetricsSnapshot {
        self.metrics.snapshot()
    }

    fn remove_flight(&self, key: &FlightKey, expected: &Arc<dyn Any + Send + Sync>) {
        self.flights
            .remove_if(key, |_, current| Arc::ptr_eq(current, expected));
    }

    /// Add restart-safe random delay to a scheduled job's next interval.
    pub fn scheduler_jitter(&self, max: Duration) -> Duration {
        use rand::Rng;
        let max_ms = max.as_millis().min(u64::MAX as u128) as u64;
        if max_ms == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(rand::thread_rng().gen_range(0..=max_ms))
    }

    /// Run provider work, optionally coalescing concurrent equivalent work.
    /// `classify_failure` returns normalized upstream evidence that should
    /// extend the provider-wide bounded backoff. Request/configuration failures
    /// should return `None`.
    pub async fn run<T, E, F, Fut, C>(
        &self,
        provider_id: impl Into<String>,
        class: ProviderWorkClass,
        singleflight_key: Option<String>,
        work: F,
        classify_failure: C,
    ) -> WorkResult<T, E>
    where
        T: Send + Sync + 'static,
        E: Send + Sync + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
        C: Fn(&E) -> Option<(FailureKind, Option<u64>)> + Send + Sync + 'static,
    {
        self.metrics.scheduled.fetch_add(1, Ordering::Relaxed);
        let provider_id = provider_id.into();
        let Some(key) = singleflight_key else {
            return self
                .execute(provider_id, class, work, classify_failure)
                .await;
        };

        // Bound the coordination table even if an operator submits many
        // distinct model/account keys concurrently. Calls beyond the cap still
        // use provider concurrency and rate budgets, but are not coalesced.
        if self.flights.len() >= MAX_SINGLEFLIGHTS {
            return self
                .execute(provider_id, class, work, classify_failure)
                .await;
        }

        let flight_key = FlightKey {
            provider_id: provider_id.clone(),
            class,
            key,
            result_types: TypeId::of::<(T, E)>(),
        };
        let (flight_any, created) = match self.flights.entry(flight_key.clone()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => (entry.get().clone(), false),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let (tx, mut rx) = tokio::sync::watch::channel(None::<WorkResult<T, E>>);
                let result = async move {
                    loop {
                        if let Some(result) = rx.borrow().clone() {
                            return result;
                        }
                        if rx.changed().await.is_err() {
                            return Err(Arc::new(ProviderWorkError::Aborted));
                        }
                    }
                }
                .boxed()
                .shared();
                let flight = Arc::new(Flight {
                    result: tokio::sync::Mutex::new(result),
                });
                let erased: Arc<dyn Any + Send + Sync> = flight.clone();
                entry.insert(erased.clone());

                let coordinator = self.clone();
                let cleanup_key = flight_key.clone();
                let cleanup_flight = erased.clone();
                tokio::spawn(async move {
                    let output = coordinator
                        .execute(provider_id, class, work, classify_failure)
                        .await;
                    let _ = tx.send(Some(output));
                    coordinator.remove_flight(&cleanup_key, &cleanup_flight);
                });
                (erased, true)
            }
        };

        let Some(flight) = flight_any.clone().downcast::<Flight<T, E>>().ok() else {
            // TypeId is part of the key, so this indicates an internal
            // invariant violation. Fail closed without invoking duplicate work.
            return Err(Arc::new(ProviderWorkError::Aborted));
        };
        if !created {
            self.metrics.coalesced.fetch_add(1, Ordering::Relaxed);
        }
        let result_future = flight.result.lock().await.clone();
        let result = result_future.await;

        self.remove_flight(&flight_key, &flight_any);
        result
    }

    async fn execute<T, E, F, Fut, C>(
        &self,
        provider_id: String,
        class: ProviderWorkClass,
        work: F,
        classify_failure: C,
    ) -> WorkResult<T, E>
    where
        T: Send + Sync + 'static,
        E: Send + Sync + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
        C: Fn(&E) -> Option<(FailureKind, Option<u64>)> + Send + Sync + 'static,
    {
        let permit = match self.acquire_inner(&provider_id, class, false).await {
            Ok(permit) => permit,
            Err(wait) => return Err(Arc::new(ProviderWorkError::BackedOff(wait))),
        };
        match work().await {
            Ok(value) => {
                permit.finish_success().await;
                Ok(Arc::new(value))
            }
            Err(error) => {
                permit.finish_failure(classify_failure(&error)).await;
                Err(Arc::new(ProviderWorkError::Operation(error)))
            }
        }
    }

    /// Acquire a budget for code that needs to inspect the upstream response
    /// before it can classify the outcome (for example, an HTTP diagnostic).
    pub async fn acquire(
        &self,
        provider_id: &str,
        class: ProviderWorkClass,
    ) -> Result<ProviderWorkPermit, Duration> {
        self.metrics.scheduled.fetch_add(1, Ordering::Relaxed);
        self.acquire_inner(provider_id, class, false).await
    }

    async fn acquire_inner(
        &self,
        provider_id: &str,
        class: ProviderWorkClass,
        count_scheduled: bool,
    ) -> Result<ProviderWorkPermit, Duration> {
        if count_scheduled {
            self.metrics.scheduled.fetch_add(1, Ordering::Relaxed);
        }
        let gate = self.gate(provider_id);
        loop {
            let slot = gate
                .slots
                .clone()
                .acquire_owned()
                .await
                .expect("provider work semaphore is never closed");
            let mut rate = gate.rate.lock().await;
            let now = Instant::now();
            if let Some(until) = rate.backed_off_until {
                if until > now {
                    let wait = until.duration_since(now);
                    self.metrics.backed_off.fetch_add(1, Ordering::Relaxed);
                    return Err(wait);
                }
                rate.backed_off_until = None;
                rate.consecutive_failures = 0;
            }

            while rate
                .starts
                .front()
                .is_some_and(|started| now.duration_since(*started) >= RATE_WINDOW)
            {
                rate.starts.pop_front();
            }
            let mut wait = rate
                .last_start
                .map(|last| last + MIN_PROVIDER_SPACING)
                .filter(|due| *due > now)
                .map(|due| due.duration_since(now))
                .unwrap_or(Duration::ZERO);
            if rate.starts.len() >= MAX_OPERATIONS_PER_WINDOW {
                if let Some(oldest) = rate.starts.front() {
                    wait = wait.max(
                        (*oldest + RATE_WINDOW)
                            .checked_duration_since(now)
                            .unwrap_or(Duration::ZERO),
                    );
                }
            }
            if let Some(last) = rate.last_by_class.get(&class) {
                let due = *last + class.min_spacing();
                if due > now {
                    wait = wait.max(due.duration_since(now));
                }
            }
            if !wait.is_zero() {
                self.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
                drop(rate);
                drop(slot);
                tokio::time::sleep(wait).await;
                continue;
            }

            rate.starts.push_back(now);
            rate.last_start = Some(now);
            rate.last_by_class.insert(class, now);
            self.metrics.executed.fetch_add(1, Ordering::Relaxed);
            drop(rate);
            return Ok(ProviderWorkPermit {
                gate,
                metrics: self.metrics.clone(),
                _slot: slot,
                completed: false,
            });
        }
    }
}

pub struct ProviderWorkPermit {
    gate: Arc<ProviderGate>,
    metrics: Arc<WorkMetrics>,
    _slot: OwnedSemaphorePermit,
    completed: bool,
}

impl ProviderWorkPermit {
    pub async fn finish_success(mut self) {
        self.completed = true;
        let mut rate = self.gate.rate.lock().await;
        if rate
            .backed_off_until
            .is_none_or(|until| until <= Instant::now())
        {
            rate.consecutive_failures = 0;
            rate.backed_off_until = None;
        }
    }

    pub async fn finish_failure(mut self, failure: Option<(FailureKind, Option<u64>)>) {
        self.completed = true;
        self.metrics.failed.fetch_add(1, Ordering::Relaxed);
        if let Some((kind, retry_after)) = failure {
            if provider_backoff_evidence(kind) {
                let mut rate = self.gate.rate.lock().await;
                rate.consecutive_failures = rate.consecutive_failures.saturating_add(1);
                let backoff = backoff_delay(rate.consecutive_failures, retry_after);
                let until = Instant::now() + backoff;
                rate.backed_off_until =
                    Some(rate.backed_off_until.map_or(until, |old| old.max(until)));
                self.metrics
                    .provider_throttled
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for ProviderWorkPermit {
    fn drop(&mut self) {
        if !self.completed {
            self.metrics.failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn provider_backoff_evidence(kind: FailureKind) -> bool {
    matches!(
        kind.policy().category,
        FailureCategory::RateLimited
            | FailureCategory::TransientUpstream
            | FailureCategory::Timeout
    )
}

fn backoff_delay(failures: u32, retry_after_secs: Option<u64>) -> Duration {
    let seconds = retry_after_secs
        .unwrap_or_else(|| 5_u64.saturating_mul(1_u64 << failures.saturating_sub(1).min(6)));
    Duration::from_secs(seconds.max(1).min(MAX_BACKOFF.as_secs()))
}

/// Normalized adapter failures that are evidence of provider-wide throttling or
/// transient unavailability. Request-local, account-quota, and target-local
/// failures deliberately do not extend provider backoff.
pub fn upstream_backoff_evidence(failure: &UpstreamFailure) -> Option<(FailureKind, Option<u64>)> {
    provider_backoff_evidence(failure.kind).then_some((failure.kind, failure.retry_after_secs))
}

pub fn outbound_error_backoff_evidence(
    error: &OutboundError,
) -> Option<(FailureKind, Option<u64>)> {
    if let Some(failure) = &error.adapter_failure {
        return upstream_backoff_evidence(failure);
    }
    error
        .timeout
        .then_some((FailureKind::Timeout, None))
        .filter(|(kind, _)| provider_backoff_evidence(*kind))
}

pub fn credential_backoff_evidence(
    error: &CredentialRotationError,
) -> Option<(FailureKind, Option<u64>)> {
    if !error.retryable {
        return None;
    }
    let kind = FailureKind::parse(&error.code)?;
    provider_backoff_evidence(kind).then_some((kind, error.retry_after_secs))
}

pub fn plugin_backoff_evidence(error: &PluginFault) -> Option<(FailureKind, Option<u64>)> {
    let PluginFault::PluginError {
        code, retry_after, ..
    } = error
    else {
        return None;
    };
    let kind = FailureKind::parse(code)?;
    provider_backoff_evidence(kind).then_some((kind, *retry_after))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn provider_concurrency_is_bounded_and_providers_are_independent() {
        let coordinator = ProviderWorkCoordinator::default();
        let first = coordinator
            .acquire("provider-a", ProviderWorkClass::HealthProbe)
            .await
            .unwrap();
        let second = coordinator
            .acquire("provider-a", ProviderWorkClass::HealthProbe)
            .await
            .unwrap();
        let waiting_coordinator = coordinator.clone();
        let waiting = tokio::spawn(async move {
            waiting_coordinator
                .acquire("provider-a", ProviderWorkClass::HealthProbe)
                .await
                .unwrap()
        });
        let independent = coordinator
            .acquire("provider-b", ProviderWorkClass::HealthProbe)
            .await
            .unwrap();
        assert!(!waiting.is_finished());
        drop(first);
        let third = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        drop((second, third, independent));
    }

    #[tokio::test]
    async fn provider_probe_spacing_is_enforced() {
        let coordinator = ProviderWorkCoordinator::default();
        coordinator
            .acquire("provider-a", ProviderWorkClass::HealthProbe)
            .await
            .unwrap()
            .finish_success()
            .await;
        let started_waiting = Instant::now();
        coordinator
            .acquire("provider-a", ProviderWorkClass::HealthProbe)
            .await
            .unwrap()
            .finish_success()
            .await;
        assert!(Instant::now().duration_since(started_waiting) >= Duration::from_millis(500));
        assert!(coordinator.metrics_snapshot().rate_limited > 0);
    }

    #[test]
    fn outbound_timeout_is_provider_backoff_evidence() {
        let timeout = OutboundError {
            message: "timed out".into(),
            timeout: true,
            adapter_failure: None,
        };
        assert_eq!(
            outbound_error_backoff_evidence(&timeout),
            Some((FailureKind::Timeout, None))
        );
        let denied = OutboundError {
            message: "request denied".into(),
            timeout: false,
            adapter_failure: None,
        };
        assert_eq!(outbound_error_backoff_evidence(&denied), None);
    }

    #[tokio::test]
    async fn rate_limit_starts_bounded_provider_backoff_but_bad_request_does_not() {
        let coordinator = ProviderWorkCoordinator::default();
        let permit = coordinator
            .acquire("provider-a", ProviderWorkClass::ModelDiscovery)
            .await
            .unwrap();
        permit
            .finish_failure(Some((FailureKind::RateLimit, Some(60))))
            .await;
        assert!(coordinator
            .acquire("provider-a", ProviderWorkClass::HealthProbe)
            .await
            .is_err());
        assert_eq!(coordinator.metrics_snapshot().provider_throttled, 1);

        let other = coordinator
            .acquire("provider-b", ProviderWorkClass::ModelDiscovery)
            .await
            .unwrap();
        other
            .finish_failure(Some((FailureKind::BadRequest, None)))
            .await;
        assert!(coordinator
            .acquire("provider-b", ProviderWorkClass::HealthProbe)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn equivalent_concurrent_work_shares_one_result() {
        let coordinator = ProviderWorkCoordinator::default();
        let executions = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let run = |coordinator: ProviderWorkCoordinator| {
            let executions = executions.clone();
            let started = started.clone();
            async move {
                coordinator
                    .run(
                        "provider-a",
                        ProviderWorkClass::ModelDiscovery,
                        Some("integration".into()),
                        move || async move {
                            executions.fetch_add(1, Ordering::Relaxed);
                            started.notify_one();
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Ok::<_, String>(42_u32)
                        },
                        |_error| None,
                    )
                    .await
            }
        };
        let first = tokio::spawn(run(coordinator.clone()));
        started.notified().await;
        let second = tokio::spawn(run(coordinator.clone()));
        let a = first.await.unwrap().unwrap();
        let b = second.await.unwrap().unwrap();
        assert_eq!(*a, 42);
        assert_eq!(*b, 42);
        assert_eq!(executions.load(Ordering::Relaxed), 1);
        assert_eq!(coordinator.metrics_snapshot().coalesced, 1);
    }

    #[test]
    fn only_provider_level_failure_categories_back_off() {
        for kind in [
            FailureKind::BadRequest,
            FailureKind::QuotaExhausted,
            FailureKind::TargetError,
        ] {
            assert!(!provider_backoff_evidence(kind));
        }
        for kind in [
            FailureKind::RateLimit,
            FailureKind::ServerError,
            FailureKind::ConnectionError,
            FailureKind::Timeout,
        ] {
            assert!(provider_backoff_evidence(kind));
        }
        assert_eq!(backoff_delay(1, None), Duration::from_secs(5));
        assert_eq!(backoff_delay(2, None), Duration::from_secs(10));
        assert_eq!(backoff_delay(99, Some(u64::MAX)), MAX_BACKOFF);
    }

    #[test]
    fn scheduler_jitter_is_bounded() {
        let coordinator = ProviderWorkCoordinator::default();
        let max = Duration::from_millis(50);
        for _ in 0..100 {
            assert!(coordinator.scheduler_jitter(max) <= max);
        }
        assert_eq!(coordinator.scheduler_jitter(Duration::ZERO), Duration::ZERO);
    }
}
