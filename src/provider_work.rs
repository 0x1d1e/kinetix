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
pub(crate) const MAX_CONCURRENT_SCHEDULED_PROVIDERS: usize = 4;

/// Run scheduled work for independent providers concurrently while keeping the
/// scheduler's process-wide task count bounded. Provider-local rate and
/// concurrency limits remain the coordinator's responsibility.
pub(crate) async fn run_bounded_provider_jobs<I, T, F, Fut>(
    jobs: I,
    work: F,
) -> Vec<tokio::task::JoinError>
where
    I: IntoIterator<Item = T>,
    T: Send + 'static,
    F: Fn(T) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut pending = jobs.into_iter();
    let mut tasks = tokio::task::JoinSet::new();
    let mut errors = Vec::new();

    loop {
        while tasks.len() < MAX_CONCURRENT_SCHEDULED_PROVIDERS {
            let Some(job) = pending.next() else {
                break;
            };
            let work = work.clone();
            tasks.spawn(async move { work(job).await });
        }
        match tasks.join_next().await {
            Some(Ok(())) => {}
            Some(Err(error)) => errors.push(error),
            None => break,
        }
    }

    errors
}

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

tokio::task_local! {
    static PROVIDER_WORK_SCOPE: ProviderWorkScope;
}

#[derive(Clone)]
struct ProviderWorkScope {
    coordinator: Arc<()>,
    provider_id: String,
    gate: Arc<ProviderGate>,
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

/// Scope assigned by the control-plane operation boundary when classifying
/// 429 failures. Account-scoped limits never throttle sibling-account work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitScope {
    Account,
    Provider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderBackoffEvidence {
    Transient { retry_after_secs: Option<u64> },
    ProviderRateLimited { retry_after_secs: Option<u64> },
}

impl ProviderBackoffEvidence {
    fn retry_after_secs(self) -> Option<u64> {
        match self {
            Self::Transient { retry_after_secs }
            | Self::ProviderRateLimited { retry_after_secs } => retry_after_secs,
        }
    }
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
/// Provider gates are evicted when providers are deleted. In-flight
/// single-flights are bounded independently. Metrics are aggregate counters
/// with no high-cardinality provider/account/model labels.
#[derive(Clone, Default)]
pub struct ProviderWorkCoordinator {
    providers: Arc<DashMap<String, Arc<ProviderGate>>>,
    flights: Arc<DashMap<FlightKey, Arc<dyn Any + Send + Sync>>>,
    flight_registration: Arc<parking_lot::Mutex<()>>,
    identity: Arc<()>,
    metrics: Arc<WorkMetrics>,
}

impl ProviderWorkCoordinator {
    fn gate(&self, provider_id: &str) -> Arc<ProviderGate> {
        if let Some(gate) = PROVIDER_WORK_SCOPE
            .try_with(|scope| {
                (scope.provider_id == provider_id
                    && Arc::ptr_eq(&scope.coordinator, &self.identity))
                .then(|| scope.gate.clone())
            })
            .ok()
            .flatten()
        {
            return gate;
        }
        self.providers
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(ProviderGate::default()))
            .clone()
    }

    /// Release provider-scoped budget state after provider deletion. Work that
    /// already captured the gate may finish against it, but cannot recreate the
    /// entry after eviction.
    pub fn forget_provider(&self, provider_id: &str) {
        self.providers.remove(provider_id);
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
        C: Fn(&E) -> Option<ProviderBackoffEvidence> + Send + Sync + 'static,
    {
        self.metrics.scheduled.fetch_add(1, Ordering::Relaxed);
        let provider_id = provider_id.into();
        let gate = self.gate(&provider_id);
        if let Some(key) = singleflight_key {
            let coordinator = self.clone();
            let flight_gate = gate.clone();
            self.singleflight(provider_id.clone(), class, key, gate, move || async move {
                coordinator
                    .execute(class, flight_gate, work, classify_failure)
                    .await
            })
            .await
        } else {
            let scope = ProviderWorkScope {
                coordinator: self.identity.clone(),
                provider_id,
                gate: gate.clone(),
            };
            PROVIDER_WORK_SCOPE
                .scope(scope, self.execute(class, gate, work, classify_failure))
                .await
        }
    }

    /// Coalesce provider-level orchestration without charging an operation
    /// permit. The operation itself must acquire a permit for each upstream
    /// request it performs.
    pub async fn coalesce<T, E, F, Fut>(
        &self,
        provider_id: impl Into<String>,
        class: ProviderWorkClass,
        key: String,
        work: F,
    ) -> WorkResult<T, E>
    where
        T: Send + Sync + 'static,
        E: Send + Sync + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.metrics.scheduled.fetch_add(1, Ordering::Relaxed);
        let provider_id = provider_id.into();
        let gate = self.gate(&provider_id);
        let work = move || async move {
            work()
                .await
                .map(Arc::new)
                .map_err(|error| Arc::new(ProviderWorkError::Operation(error)))
        };
        self.singleflight(provider_id, class, key, gate, work).await
    }

    async fn singleflight<T, E, F, Fut>(
        &self,
        provider_id: String,
        class: ProviderWorkClass,
        key: String,
        gate: Arc<ProviderGate>,
        work: F,
    ) -> WorkResult<T, E>
    where
        T: Send + Sync + 'static,
        E: Send + Sync + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = WorkResult<T, E>> + Send + 'static,
    {
        let flight_key = FlightKey {
            provider_id,
            class,
            key,
            result_types: TypeId::of::<(T, E)>(),
        };
        let mut work = Some(work);
        // Serialize only registration. Existing keys are joined before the cap
        // is considered, and concurrent unique insertions cannot exceed it.
        let registration = {
            let _registration = self.flight_registration.lock();
            if let Some(existing) = self.flights.get(&flight_key) {
                Some((existing.value().clone(), false))
            } else if self.flights.len() >= MAX_SINGLEFLIGHTS {
                None
            } else {
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
                self.flights.insert(flight_key.clone(), erased.clone());

                let coordinator = self.clone();
                let cleanup_key = flight_key.clone();
                let cleanup_flight = erased.clone();
                let work = work.take().expect("singleflight work is consumed once");
                let scope = ProviderWorkScope {
                    coordinator: self.identity.clone(),
                    provider_id: flight_key.provider_id.clone(),
                    gate: gate.clone(),
                };
                tokio::spawn(async move {
                    let output = PROVIDER_WORK_SCOPE.scope(scope, work()).await;
                    let _ = tx.send(Some(output));
                    coordinator.remove_flight(&cleanup_key, &cleanup_flight);
                });
                Some((erased, true))
            }
        };
        let Some((flight_any, created)) = registration else {
            let scope = ProviderWorkScope {
                coordinator: self.identity.clone(),
                provider_id: flight_key.provider_id.clone(),
                gate,
            };
            return PROVIDER_WORK_SCOPE
                .scope(
                    scope,
                    work.take().expect("fallback work is consumed once")(),
                )
                .await;
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
        class: ProviderWorkClass,
        gate: Arc<ProviderGate>,
        work: F,
        classify_failure: C,
    ) -> WorkResult<T, E>
    where
        T: Send + Sync + 'static,
        E: Send + Sync + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
        C: Fn(&E) -> Option<ProviderBackoffEvidence> + Send + Sync + 'static,
    {
        let permit = match self.acquire_with_gate(gate, class, false).await {
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
        let gate = self.gate(provider_id);
        self.acquire_with_gate(gate, class, count_scheduled).await
    }

    async fn acquire_with_gate(
        &self,
        gate: Arc<ProviderGate>,
        class: ProviderWorkClass,
        count_scheduled: bool,
    ) -> Result<ProviderWorkPermit, Duration> {
        if count_scheduled {
            self.metrics.scheduled.fetch_add(1, Ordering::Relaxed);
        }
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
        rate.consecutive_failures = 0;
        if rate
            .backed_off_until
            .is_some_and(|until| until <= Instant::now())
        {
            rate.backed_off_until = None;
        }
    }

    pub async fn finish_failure(mut self, evidence: Option<ProviderBackoffEvidence>) {
        self.completed = true;
        self.metrics.failed.fetch_add(1, Ordering::Relaxed);
        if let Some(evidence) = evidence {
            let mut rate = self.gate.rate.lock().await;
            rate.consecutive_failures = rate.consecutive_failures.saturating_add(1);
            let backoff = backoff_delay(rate.consecutive_failures, evidence.retry_after_secs());
            let until = Instant::now() + backoff;
            rate.backed_off_until = Some(rate.backed_off_until.map_or(until, |old| old.max(until)));
            self.metrics
                .provider_throttled
                .fetch_add(1, Ordering::Relaxed);
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

fn normalized_backoff_evidence(
    kind: FailureKind,
    retry_after_secs: Option<u64>,
    rate_limit_scope: RateLimitScope,
) -> Option<ProviderBackoffEvidence> {
    if kind == FailureKind::RateLimit {
        return (rate_limit_scope == RateLimitScope::Provider)
            .then_some(ProviderBackoffEvidence::ProviderRateLimited { retry_after_secs });
    }

    matches!(
        kind.policy().category,
        FailureCategory::TransientUpstream | FailureCategory::Timeout
    )
    .then_some(ProviderBackoffEvidence::Transient { retry_after_secs })
}

fn backoff_delay(failures: u32, retry_after_secs: Option<u64>) -> Duration {
    let seconds = retry_after_secs
        .unwrap_or_else(|| 5_u64.saturating_mul(1_u64 << failures.saturating_sub(1).min(6)));
    Duration::from_secs(seconds.max(1).min(MAX_BACKOFF.as_secs()))
}

/// Normalized adapter failures from a provider-scoped control-plane operation.
/// Callers must identify whether a 429 applies to the provider or only the
/// account used for that operation. Inference account-rate-limits never enter
/// this coordinator.
pub fn upstream_backoff_evidence(
    failure: &UpstreamFailure,
    rate_limit_scope: RateLimitScope,
) -> Option<ProviderBackoffEvidence> {
    normalized_backoff_evidence(failure.kind, failure.retry_after_secs, rate_limit_scope)
}

pub fn outbound_error_backoff_evidence(
    error: &OutboundError,
    rate_limit_scope: RateLimitScope,
) -> Option<ProviderBackoffEvidence> {
    if let Some(failure) = &error.adapter_failure {
        return upstream_backoff_evidence(failure, rate_limit_scope);
    }
    error.timeout.then_some(ProviderBackoffEvidence::Transient {
        retry_after_secs: None,
    })
}

/// Canonical mapping for plugin and credential error codes. Plugin vocabulary
/// has aliases that are not part of `FailureKind::parse`; rate-limit scope is
/// supplied by the operation boundary rather than inferred from the string.
fn plugin_code_backoff_evidence(
    code: &str,
    retryable: bool,
    retry_after_secs: Option<u64>,
    rate_limit_scope: RateLimitScope,
) -> Option<ProviderBackoffEvidence> {
    if !retryable {
        return None;
    }
    let kind = match code {
        "upstream_unavailable" => FailureKind::ServerError,
        "rate_limited" => FailureKind::RateLimit,
        code => FailureKind::parse(code)?,
    };
    normalized_backoff_evidence(kind, retry_after_secs, rate_limit_scope)
}

pub fn credential_backoff_evidence(
    error: &CredentialRotationError,
) -> Option<ProviderBackoffEvidence> {
    plugin_code_backoff_evidence(
        &error.code,
        error.retryable,
        error.retry_after_secs,
        RateLimitScope::Account,
    )
}

pub fn plugin_backoff_evidence_for_scope(
    error: &PluginFault,
    rate_limit_scope: RateLimitScope,
) -> Option<ProviderBackoffEvidence> {
    match error {
        PluginFault::PluginError {
            code,
            retryable,
            retry_after,
            ..
        } => plugin_code_backoff_evidence(code, *retryable, *retry_after, rate_limit_scope),
        PluginFault::Timeout => {
            plugin_code_backoff_evidence("timeout", true, None, rate_limit_scope)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn bounded_provider_jobs_start_siblings_while_one_provider_is_stalled() {
        use tokio::sync::{Notify, Semaphore};

        let provider_a_started = Arc::new(Notify::new());
        let provider_b_started = Arc::new(Notify::new());
        let release_provider_a = Arc::new(Semaphore::new(0));
        let a_started = provider_a_started.clone();
        let b_started = provider_b_started.clone();
        let release_a = release_provider_a.clone();
        let task = tokio::spawn(run_bounded_provider_jobs(
            vec!["provider-a", "provider-b"],
            move |provider| {
                let a_started = a_started.clone();
                let b_started = b_started.clone();
                let release_a = release_a.clone();
                async move {
                    if provider == "provider-a" {
                        a_started.notify_one();
                        release_a.acquire().await.unwrap().forget();
                    } else {
                        b_started.notify_one();
                    }
                }
            },
        ));

        tokio::time::timeout(Duration::from_secs(1), provider_a_started.notified())
            .await
            .expect("provider A job did not start");
        tokio::time::timeout(Duration::from_secs(1), provider_b_started.notified())
            .await
            .expect("stalled provider A blocked provider B");
        release_provider_a.add_permits(1);
        assert!(task.await.unwrap().is_empty());
    }

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
    async fn deleting_provider_releases_provider_work_gate() {
        let coordinator = ProviderWorkCoordinator::default();
        let permit = coordinator
            .acquire("deleted-provider", ProviderWorkClass::HealthProbe)
            .await
            .unwrap();
        drop(permit);
        assert_eq!(coordinator.providers.len(), 1);

        coordinator.forget_provider("deleted-provider");
        assert!(coordinator.providers.is_empty());
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
            outbound_error_backoff_evidence(&timeout, RateLimitScope::Provider),
            Some(ProviderBackoffEvidence::Transient {
                retry_after_secs: None,
            })
        );
        let denied = OutboundError {
            message: "request denied".into(),
            timeout: false,
            adapter_failure: None,
        };
        assert_eq!(
            outbound_error_backoff_evidence(&denied, RateLimitScope::Provider),
            None
        );
    }

    #[test]
    fn plugin_and_credential_codes_share_scoped_backoff_mapping() {
        let plugin_error = |code: &str, retryable: bool, retry_after| PluginFault::PluginError {
            code: code.into(),
            message: "upstream request failed".into(),
            retryable,
            retry_after,
        };

        assert_eq!(
            plugin_backoff_evidence_for_scope(
                &plugin_error("upstream_unavailable", true, None),
                RateLimitScope::Provider,
            ),
            Some(ProviderBackoffEvidence::Transient {
                retry_after_secs: None,
            })
        );
        assert_eq!(
            plugin_backoff_evidence_for_scope(
                &plugin_error("timeout", true, None),
                RateLimitScope::Provider,
            ),
            Some(ProviderBackoffEvidence::Transient {
                retry_after_secs: None,
            })
        );
        assert_eq!(
            plugin_backoff_evidence_for_scope(
                &plugin_error("rate_limited", true, Some(17)),
                RateLimitScope::Provider,
            ),
            Some(ProviderBackoffEvidence::ProviderRateLimited {
                retry_after_secs: Some(17),
            })
        );
        assert_eq!(
            plugin_backoff_evidence_for_scope(
                &plugin_error("rate_limited", true, Some(17)),
                RateLimitScope::Account,
            ),
            None
        );
        assert_eq!(
            plugin_backoff_evidence_for_scope(
                &plugin_error("upstream_unavailable", false, None),
                RateLimitScope::Provider,
            ),
            None
        );
        assert_eq!(
            plugin_backoff_evidence_for_scope(&PluginFault::Timeout, RateLimitScope::Provider),
            Some(ProviderBackoffEvidence::Transient {
                retry_after_secs: None,
            })
        );

        let credential_error =
            CredentialRotationError::new("upstream_unavailable", "refresh failed", true, Some(9));
        assert_eq!(
            credential_backoff_evidence(&credential_error),
            Some(ProviderBackoffEvidence::Transient {
                retry_after_secs: Some(9),
            })
        );
        let account_rate_limit =
            CredentialRotationError::new("rate_limited", "account rate limited", true, Some(17));
        assert_eq!(credential_backoff_evidence(&account_rate_limit), None);
    }

    #[tokio::test]
    async fn plugin_failure_aliases_install_scoped_provider_backoff() {
        let coordinator = ProviderWorkCoordinator::default();
        let error = PluginFault::PluginError {
            code: "upstream_unavailable".into(),
            message: "discovery temporarily unavailable".into(),
            retryable: true,
            retry_after: None,
        };
        let first = coordinator
            .run(
                "provider-a",
                ProviderWorkClass::RoutingFactsRefresh,
                None,
                move || async move { Err::<(), _>(error) },
                |error| plugin_backoff_evidence_for_scope(error, RateLimitScope::Provider),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            first.as_ref(),
            ProviderWorkError::Operation(PluginFault::PluginError { .. })
        ));

        let second = coordinator
            .run(
                "provider-a",
                ProviderWorkClass::RoutingFactsRefresh,
                None,
                || async { Ok::<_, PluginFault>(()) },
                |error| plugin_backoff_evidence_for_scope(error, RateLimitScope::Provider),
            )
            .await
            .unwrap_err();
        assert!(matches!(second.as_ref(), ProviderWorkError::BackedOff(_)));
        assert_eq!(coordinator.metrics_snapshot().provider_throttled, 1);

        let provider_rate_limit = plugin_backoff_evidence_for_scope(
            &PluginFault::PluginError {
                code: "rate_limited".into(),
                message: "provider is rate limited".into(),
                retryable: true,
                retry_after: Some(30),
            },
            RateLimitScope::Provider,
        );
        assert_eq!(
            provider_rate_limit,
            Some(ProviderBackoffEvidence::ProviderRateLimited {
                retry_after_secs: Some(30),
            })
        );
        assert_eq!(
            plugin_backoff_evidence_for_scope(
                &PluginFault::PluginError {
                    code: "rate_limited".into(),
                    message: "one account is rate limited".into(),
                    retryable: true,
                    retry_after: Some(30),
                },
                RateLimitScope::Account,
            ),
            None
        );
    }

    #[tokio::test]
    async fn account_rate_limit_does_not_back_off_other_account_work() {
        let coordinator = ProviderWorkCoordinator::default();
        let account_a = coordinator
            .acquire("provider-a", ProviderWorkClass::ModelDiscovery)
            .await
            .unwrap();
        let account_rate_limit =
            normalized_backoff_evidence(FailureKind::RateLimit, Some(60), RateLimitScope::Account);
        assert_eq!(account_rate_limit, None);
        account_a.finish_failure(account_rate_limit).await;

        let account_b = coordinator
            .acquire("provider-a", ProviderWorkClass::HealthProbe)
            .await
            .expect("account A's rate limit must not back off account B");
        account_b.finish_success().await;
        assert_eq!(coordinator.metrics_snapshot().provider_throttled, 0);

        let provider_rate_limit =
            normalized_backoff_evidence(FailureKind::RateLimit, Some(60), RateLimitScope::Provider);
        assert_eq!(
            provider_rate_limit,
            Some(ProviderBackoffEvidence::ProviderRateLimited {
                retry_after_secs: Some(60),
            })
        );
        let provider_error = coordinator
            .acquire("provider-b", ProviderWorkClass::ModelDiscovery)
            .await
            .unwrap();
        provider_error.finish_failure(provider_rate_limit).await;
        assert!(coordinator
            .acquire("provider-b", ProviderWorkClass::HealthProbe)
            .await
            .is_err());
        assert_eq!(coordinator.metrics_snapshot().provider_throttled, 1);
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

    #[tokio::test]
    async fn coalesced_lifecycle_budgets_each_upstream_operation() {
        let coordinator = ProviderWorkCoordinator::default();
        let operation_coordinator = coordinator.clone();
        coordinator
            .coalesce(
                "provider-a",
                ProviderWorkClass::ModelDiscovery,
                "reconciliation".into(),
                move || async move {
                    for _ in 0..2 {
                        let permit = operation_coordinator
                            .acquire("provider-a", ProviderWorkClass::ModelDiscovery)
                            .await
                            .map_err(|wait| format!("backed off for {wait:?}"))?;
                        permit.finish_success().await;
                    }
                    Ok::<_, String>(())
                },
            )
            .await
            .unwrap();

        let metrics = coordinator.metrics_snapshot();
        assert_eq!(metrics.scheduled, 3);
        assert_eq!(metrics.executed, 2);
    }

    #[test]
    fn backoff_evidence_requires_explicit_rate_limit_scope() {
        for kind in [
            FailureKind::BadRequest,
            FailureKind::RateLimit,
            FailureKind::QuotaExhausted,
            FailureKind::TargetError,
        ] {
            assert_eq!(
                normalized_backoff_evidence(kind, None, RateLimitScope::Account),
                None
            );
        }
        for kind in [
            FailureKind::ServerError,
            FailureKind::ConnectionError,
            FailureKind::Timeout,
        ] {
            assert!(normalized_backoff_evidence(kind, None, RateLimitScope::Account).is_some());
        }
        assert_eq!(backoff_delay(1, None), Duration::from_secs(5));
        assert_eq!(backoff_delay(2, None), Duration::from_secs(10));
        assert_eq!(backoff_delay(99, Some(u64::MAX)), MAX_BACKOFF);
    }

    #[tokio::test]
    async fn sequential_failures_increase_backoff_after_expiry() {
        let coordinator = ProviderWorkCoordinator::default();
        let first = coordinator
            .acquire("provider-a", ProviderWorkClass::CredentialRefresh)
            .await
            .unwrap();
        first
            .finish_failure(Some(ProviderBackoffEvidence::Transient {
                retry_after_secs: None,
            }))
            .await;

        let gate = coordinator.gate("provider-a");
        {
            let mut rate = gate.rate.lock().await;
            assert_eq!(rate.consecutive_failures, 1);
            rate.backed_off_until = Some(Instant::now() - Duration::from_millis(1));
        }

        let retry = coordinator
            .acquire("provider-a", ProviderWorkClass::CredentialRefresh)
            .await
            .expect("expired backoff should permit the next attempt");
        retry
            .finish_failure(Some(ProviderBackoffEvidence::Transient {
                retry_after_secs: None,
            }))
            .await;

        let rate = gate.rate.lock().await;
        assert_eq!(rate.consecutive_failures, 2);
        let remaining = rate
            .backed_off_until
            .expect("second failure should install another backoff")
            .duration_since(Instant::now());
        assert!(
            remaining > Duration::from_secs(9),
            "second sequential failure should back off for about 10s, got {remaining:?}"
        );
    }

    #[tokio::test]
    async fn deleting_provider_during_pending_singleflight_does_not_recreate_gate() {
        let coordinator = ProviderWorkCoordinator::default();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let task_coordinator = coordinator.clone();
        let task_started = started.clone();
        let task_release = release.clone();
        let task = tokio::spawn(async move {
            let nested_coordinator = task_coordinator.clone();
            task_coordinator
                .run(
                    "deleted-provider",
                    ProviderWorkClass::HealthProbe,
                    Some("pending-probe".into()),
                    move || async move {
                        task_started.notify_one();
                        task_release
                            .acquire()
                            .await
                            .expect("test semaphore remains open")
                            .forget();
                        let permit = nested_coordinator
                            .acquire("deleted-provider", ProviderWorkClass::CredentialRefresh)
                            .await
                            .map_err(|wait| format!("unexpected backoff: {wait:?}"))?;
                        permit.finish_success().await;
                        Ok::<_, String>(())
                    },
                    |_| None,
                )
                .await
        });

        started.notified().await;
        coordinator.forget_provider("deleted-provider");
        release.add_permits(1);
        task.await.unwrap().unwrap();

        assert!(!coordinator.providers.contains_key("deleted-provider"));
    }

    #[tokio::test]
    async fn full_singleflight_table_still_joins_existing_flights() {
        use futures::FutureExt;
        use tokio::sync::Semaphore;

        let coordinator = ProviderWorkCoordinator::default();
        let release = Arc::new(Semaphore::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::with_capacity(MAX_SINGLEFLIGHTS);

        for index in 0..MAX_SINGLEFLIGHTS {
            let release = release.clone();
            let started = started.clone();
            let coordinator = coordinator.clone();
            tasks.push(tokio::spawn(async move {
                coordinator
                    .coalesce(
                        "provider-a",
                        ProviderWorkClass::HealthProbe,
                        format!("flight-{index}"),
                        move || async move {
                            started.fetch_add(1, Ordering::Relaxed);
                            release.acquire().await.unwrap().forget();
                            Ok::<_, String>(())
                        },
                    )
                    .await
            }));
        }

        tokio::time::timeout(Duration::from_secs(10), async {
            while started.load(Ordering::Relaxed) < MAX_SINGLEFLIGHTS {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("all unique flights should start");
        assert_eq!(coordinator.flights.len(), MAX_SINGLEFLIGHTS);

        let duplicate_executions = Arc::new(AtomicUsize::new(0));
        let duplicate_count = duplicate_executions.clone();
        let mut duplicate = Box::pin(coordinator.coalesce(
            "provider-a",
            ProviderWorkClass::HealthProbe,
            "flight-0".into(),
            move || async move {
                duplicate_count.fetch_add(1, Ordering::Relaxed);
                Ok::<_, String>(())
            },
        ));
        assert!(duplicate.as_mut().now_or_never().is_none());
        assert_eq!(duplicate_executions.load(Ordering::Relaxed), 0);
        assert_eq!(coordinator.metrics_snapshot().coalesced, 1);

        release.add_permits(MAX_SINGLEFLIGHTS);
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        duplicate.await.unwrap();
        assert_eq!(duplicate_executions.load(Ordering::Relaxed), 0);
        assert!(coordinator.flights.is_empty());
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
