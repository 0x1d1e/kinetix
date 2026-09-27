//! Shared application state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Context;
use dashmap::DashMap;

use crate::adapters::AdapterRegistry;
use crate::config::Config;
use crate::credentials::{CredentialRotationError, CredentialStrategy, StaticKeyStrategy};
use crate::crypto::Crypto;
use crate::db::Pool;
use crate::logqueue::UsageLogQueue;
use crate::opaque_state::OpaqueStateStore;
use crate::registry::Registry;
use crate::trace::FlightRecorder;

#[derive(Clone)]
pub struct AppState {
    /// Live in-flight request view (FR-8.3), bounded and metadata-only.
    pub live: crate::live::LiveRequests,
    pub config: Arc<Config>,
    pub pool: Pool,
    pub registry: Arc<Registry>,
    pub crypto: Arc<Crypto>,
    pub credentials: Arc<StaticKeyStrategy>,
    /// Per-plugin credential strategies, keyed by plugin id (§6.0). When a
    /// provider binds `credential_plugin`, the pipeline resolves through here
    /// before falling back to the static strategy.
    pub plugin_credentials:
        Arc<dashmap::DashMap<String, Arc<dyn crate::credentials::CredentialStrategy>>>,
    /// Host-owned proactive refresh scheduler and account-scoped rotation
    /// singleflight shared by scheduled refresh and reactive auth recovery.
    pub credential_refresh: crate::credential_refresh::RefreshCoordinator,
    pub adapters: AdapterRegistry,
    pub http: reqwest::Client,
    /// Pinned provider clients keyed by validated host/address set. This keeps
    /// connection pooling without reopening a DNS rebinding window.
    pub outbound_clients: Arc<dashmap::DashMap<String, reqwest::Client>>,
    pub log_queue: UsageLogQueue,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Diagnostic flight recorder (FR-13).
    pub flight: Arc<FlightRecorder>,
    /// Host-owned opaque provider continuation-state store (Gemini
    /// `thoughtSignature` persistence/replay for translated frontends).
    pub opaque_state: Arc<OpaqueStateStore>,
    /// Session id -> last successful route target. Used for optional
    /// sticky/cache affinity and FR-2.11 opaque-state provenance. TTL-bounded.
    sticky: Arc<DashMap<String, StickyEntry>>,
    rr_counters: Arc<DashMap<String, Arc<AtomicU64>>>,
    /// Before-commit / after-commit failure counters (FR-4.9).
    pub failures_pre_commit: Arc<AtomicU64>,
    pub failures_post_commit: Arc<AtomicU64>,
    pub cancellations: Arc<AtomicU64>,
    pub cancellation_latency_ms_total: Arc<AtomicU64>,
    /// Requests rejected because the provider was at its RPM/TPM or the key hit
    /// a budget (dashboard counters).
    pub total_requests: Arc<AtomicU64>,
    /// Route targets skipped during eligibility filtering (FR-12.11, NFR-4.2).
    pub route_skips: Arc<AtomicU64>,
    /// Requests that used at least one fallback hop (FR-12.6, NFR-4.2).
    pub route_fallbacks: Arc<AtomicU64>,
    /// Last successful scheduled backup timestamp (RFC3339), for backup-failure
    /// alerting (Monitoring). None until a backup has run.
    pub last_backup_at: Arc<parking_lot::Mutex<Option<String>>>,
    /// Whether the last scheduled backup attempt failed.
    pub last_backup_failed: Arc<std::sync::atomic::AtomicBool>,
    /// Atomic per-key RPM/TPM/budget admission state.
    pub admission: crate::admission::AdmissionController,
    /// Target-local adaptive upstream concurrency. This is deliberately
    /// independent from virtual-key admission limits.
    pub upstream_traffic: crate::upstream_traffic::UpstreamTraffic,
    /// Fresh provider/account quota evidence used only by adaptive routing.
    pub quota: crate::quota::QuotaRegistry,
    /// Provider-wide correlated transient-failure breaker.
    pub provider_circuits: crate::provider_circuit::ProviderCircuits,
    /// Durable per-target telemetry; request-path recording is non-blocking.
    pub target_telemetry: crate::target_telemetry::TargetTelemetry,
    /// Per-IP abuse limiter (NFR-3.6), applied before virtual-key auth.
    pub ip_limiter: crate::ratelimit::IpLimiter,
    /// In-memory admin sessions (dropped on restart; TTL-bounded).
    pub sessions: Arc<crate::auth::Sessions>,
    /// One-time browser sessions for plugin-provided account authorization.
    pub plugin_auth_sessions: Arc<crate::auth::PluginAuthSessions>,
    /// Plugin host.
    pub plugins: Option<Arc<crate::plugins::PluginManager>>,
    /// Bounded fire-and-forget queue for plugin hook side effects (§6.6). Hooks
    /// run off the request path; if the queue is full a hook is dropped rather
    /// than delaying a client request.
    hook_tx: tokio::sync::mpsc::Sender<HookJob>,
}

enum RefreshLookup<T, E> {
    Found(T),
    Missing,
    Transient(E),
}

fn classify_refresh_lookup<T, E>(result: Result<Option<T>, E>) -> RefreshLookup<T, E> {
    match result {
        Ok(Some(value)) => RefreshLookup::Found(value),
        Ok(None) => RefreshLookup::Missing,
        Err(error) => RefreshLookup::Transient(error),
    }
}

#[derive(Clone)]
pub struct StickyEntry {
    /// The selected route target key (route id + account id + model id).
    pub target_key: String,
    pub at: std::time::Instant,
}

/// A deferred, side-effect-only plugin hook invocation (§6.6).
type HookJob =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

const HOOK_QUEUE_CAPACITY: usize = 1024;
const MAX_CONCURRENT_HOOK_JOBS: usize = 32;

fn spawn_hook_worker(mut hook_rx: tokio::sync::mpsc::Receiver<HookJob>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HOOK_JOBS));
    tokio::spawn(async move {
        while let Some(job) = hook_rx.recv().await {
            let permit = slots
                .clone()
                .acquire_owned()
                .await
                .expect("hook dispatcher semaphore is never closed");
            tokio::spawn(async move {
                let _permit = permit;
                job().await;
            });
        }
    });
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        pool: Pool,
        registry: Arc<Registry>,
        crypto: Arc<Crypto>,
        http: reqwest::Client,
        log_queue: UsageLogQueue,
        config_ip_limit: u64,
    ) -> Self {
        let pool_clone = pool.clone();
        let crypto_clone = crypto.clone();
        let credentials = Arc::new(StaticKeyStrategy::new(crypto.clone()));
        let sessions = Arc::new(crate::auth::Sessions::new(config.session_ttl_minutes));
        let plugin_auth_sessions = Arc::new(crate::auth::PluginAuthSessions::new());
        let target_telemetry = crate::target_telemetry::TargetTelemetry::new(pool.clone());
        let (hook_tx, hook_rx) = tokio::sync::mpsc::channel::<HookJob>(HOOK_QUEUE_CAPACITY);
        spawn_hook_worker(hook_rx);
        AppState {
            config,
            pool,
            registry,
            crypto,
            credentials,
            plugin_credentials: Arc::new(DashMap::new()),
            credential_refresh: crate::credential_refresh::RefreshCoordinator::default(),
            adapters: AdapterRegistry::new(),
            http,
            outbound_clients: Arc::new(DashMap::new()),
            log_queue,
            started_at: chrono::Utc::now(),
            flight: Arc::new(FlightRecorder::new(512, 128)),
            opaque_state: Arc::new(OpaqueStateStore::new(pool_clone, crypto_clone)),
            live: crate::live::LiveRequests::new(512),
            sticky: Arc::new(DashMap::new()),
            rr_counters: Arc::new(DashMap::new()),
            failures_pre_commit: Arc::new(AtomicU64::new(0)),
            failures_post_commit: Arc::new(AtomicU64::new(0)),
            cancellations: Arc::new(AtomicU64::new(0)),
            cancellation_latency_ms_total: Arc::new(AtomicU64::new(0)),
            total_requests: Arc::new(AtomicU64::new(0)),
            route_skips: Arc::new(AtomicU64::new(0)),
            route_fallbacks: Arc::new(AtomicU64::new(0)),
            last_backup_at: Arc::new(parking_lot::Mutex::new(None)),
            last_backup_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            admission: crate::admission::AdmissionController::default(),
            upstream_traffic: crate::upstream_traffic::UpstreamTraffic::default(),
            quota: crate::quota::QuotaRegistry::default(),
            provider_circuits: crate::provider_circuit::ProviderCircuits::default(),
            target_telemetry,
            ip_limiter: crate::ratelimit::IpLimiter::new(config_ip_limit),
            sessions,
            plugin_auth_sessions,
            plugins: None,
            hook_tx,
        }
    }

    /// Attach the plugin host (called during server startup).
    pub fn with_plugins(mut self, plugins: Arc<crate::plugins::PluginManager>) -> Self {
        self.plugins = Some(plugins);
        self
    }

    /// Enqueue a plugin hook side effect (§6.6). Never blocks: if the queue is
    /// saturated the hook is dropped, because a hook must not affect the client
    /// request.
    pub fn spawn_hook<F, Fut>(&self, make: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let job: HookJob = Box::new(move || Box::pin(make()));
        if self.hook_tx.try_send(job).is_err() {
            tracing::debug!("plugin hook queue saturated; dropping hook");
        }
    }

    /// The plugin manager, when the host is enabled.
    pub fn plugin_manager(&self) -> Option<&Arc<crate::plugins::PluginManager>> {
        self.plugins.as_ref()
    }

    /// Register the built-in capability implementations a plugin host provides
    /// (§6.0): credential strategies and wire-format adapters, keyed by plugin
    /// id / namespaced reference. Called once at startup, before the state is
    /// shared, so every capability resolution fails closed until a plugin is
    /// both installed and enabled.
    pub fn register_plugin_capabilities(&self) {
        // Credential strategies and adapters are registered lazily by the
        // enable path (`register_plugin_credential_strategy` /
        // `register_plugin_adapter`); this hook exists so callers have one
        // obvious place to do so and so the intent is documented.
    }

    /// Register a plugin-backed credential strategy for a plugin id (§6.1).
    pub fn register_plugin_credential_strategy(
        &self,
        plugin_id: impl Into<String>,
        strategy: Arc<dyn crate::credentials::CredentialStrategy>,
    ) {
        self.plugin_credentials.insert(plugin_id.into(), strategy);
    }

    /// Register a plugin-backed outbound adapter under its namespaced
    /// reference `plugin:<id>/<cap>` (§6.0).
    pub fn register_plugin_adapter(
        &self,
        reference: impl Into<String>,
        adapter: Arc<dyn crate::adapters::Adapter>,
    ) {
        self.adapters.register_plugin(reference, adapter);
    }

    /// Drop every capability a plugin registered (credential strategy and
    /// adapters). Called when a plugin is disabled, removed, rolled back, or
    /// has a permission revoked, so a disabled plugin cannot keep resolving
    /// credentials or serving adapters.
    pub fn unregister_plugin_capabilities(&self, plugin_id: &str) {
        self.plugin_credentials.remove(plugin_id);
        self.adapters.unregister_plugin(plugin_id);
    }

    /// Resolve the credential for an account, honouring a provider's plugin
    /// credential binding (§6.0). A bound-but-unavailable plugin fails closed.
    pub async fn credential_for(
        &self,
        provider: &crate::db::ProviderRow,
        account: &crate::db::AccountRow,
    ) -> std::result::Result<crate::credentials::ResolvedCredential, CredentialRotationError> {
        if let Some(r) = provider.credential_plugin_ref() {
            let Some(strategy) = self.plugin_credentials.get(&r.plugin_id) else {
                return Err(CredentialRotationError::new(
                    "plugin_internal",
                    format!(
                        "provider '{}' is bound to unavailable plugin credential strategy '{}'",
                        provider.name,
                        r.to_string_ref()
                    ),
                    true,
                    None,
                ));
            };
            let strategy = Arc::clone(strategy.value());
            return self
                .credential_refresh
                .resolve(&provider.id, strategy, account)
                .await;
        }
        self.credentials.resolve(account).await
    }

    /// Disable an account when credential resolution confirms its credential
    /// is terminally invalid. Returns whether it was terminal, and propagates
    /// persistence or registry-reload failures instead of claiming success.
    pub(crate) async fn disable_invalid_credential(
        &self,
        account: &crate::db::AccountRow,
        error: &CredentialRotationError,
        context: &'static str,
    ) -> anyhow::Result<bool> {
        if !error.invalid_credential() {
            return Ok(false);
        }

        crate::db::set_account_status(
            &self.pool,
            &account.id,
            "disabled",
            None,
            None,
            Some(&format!("{context}: {}", error.message)),
        )
        .await
        .with_context(|| format!("failed to persist disabling invalid account {}", account.id))?;
        self.credential_refresh
            .forget(&account.provider_id, &account.id);
        self.registry.reload(&self.pool).await.with_context(|| {
            format!(
                "account {} was disabled but registry reload failed",
                account.id
            )
        })?;
        tracing::warn!(
            account = %account.id,
            code = %error.code,
            context,
            "credential confirmed invalid; account disabled"
        );
        Ok(true)
    }

    /// Force renewal for a plugin-backed credential after an upstream auth
    /// failure. Proactive and reactive rotation share the coordinator's
    /// account-scoped singleflight gate so rotating refresh tokens cannot race.
    pub async fn rotate_credential_after_auth_error(
        &self,
        provider: &crate::db::ProviderRow,
        account: &crate::db::AccountRow,
        failed_secret: &str,
    ) -> std::result::Result<bool, CredentialRotationError> {
        let Some(r) = provider.credential_plugin_ref() else {
            return Ok(false);
        };
        let strategy = self
            .plugin_credentials
            .get(&r.plugin_id)
            .map(|entry| Arc::clone(entry.value()))
            .ok_or_else(|| {
                CredentialRotationError::new(
                    "plugin_internal",
                    format!(
                        "provider '{}' is bound to unavailable plugin credential strategy '{}'",
                        provider.name,
                        r.to_string_ref()
                    ),
                    true,
                    None,
                )
            })?;

        self.credential_refresh
            .rotate_after_auth_error(&provider.id, strategy, account, failed_secret)
            .await
    }

    /// Resolve every currently configured plugin-backed account once at
    /// startup. This rehydrates proactive lease deadlines after a process
    /// restart without keeping a periodic full-table scanner alive.
    pub async fn seed_credential_refreshes(&self) {
        let providers = match crate::db::list_providers(&self.pool).await {
            Ok(providers) => providers,
            Err(error) => {
                tracing::warn!(%error, "could not seed credential refresh schedules");
                return;
            }
        };

        for provider in providers {
            if provider.credential_plugin_ref().is_none() || provider.enabled == 0 {
                continue;
            }
            let accounts = match crate::db::accounts_for_provider(&self.pool, &provider.id).await {
                Ok(accounts) => accounts,
                Err(error) => {
                    tracing::debug!(provider = %provider.id, %error, "could not enumerate accounts for credential refresh seed");
                    continue;
                }
            };
            for account in accounts {
                if account.status == "disabled" {
                    continue;
                }
                if let Err(error) = self.credential_for(&provider, &account).await {
                    match self
                        .disable_invalid_credential(&account, &error, "startup credential seed")
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => tracing::debug!(
                            provider = %provider.id,
                            account = %account.id,
                            %error,
                            "credential refresh seed resolve failed"
                        ),
                        Err(disable_error) => tracing::error!(
                            provider = %provider.id,
                            account = %account.id,
                            credential_error = %error,
                            %disable_error,
                            "failed to disable account after startup credential resolution confirmed invalid"
                        ),
                    }
                }
            }
        }
    }

    /// Refresh all leases claimed due by the coordinator. A bounded JoinSet
    /// keeps idle credential maintenance off the request path without bursting
    /// provider token endpoints.
    pub async fn refresh_due_credentials(&self) {
        const MAX_CONCURRENT_REFRESHES: usize = 4;

        let due = self.credential_refresh.claim_due(chrono::Utc::now());
        if due.is_empty() {
            return;
        }

        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REFRESHES));
        let mut jobs = tokio::task::JoinSet::new();
        for key in due {
            let state = self.clone();
            let semaphore = semaphore.clone();
            jobs.spawn(async move {
                let permit = semaphore.acquire_owned().await;
                if permit.is_err() {
                    return;
                }
                let _permit = permit.expect("checked above");
                state.refresh_due_credential(key).await;
            });
        }
        while jobs.join_next().await.is_some() {}
    }

    async fn refresh_due_credential(&self, key: crate::credential_refresh::CredentialKey) {
        let account = match classify_refresh_lookup(
            crate::db::get_account(&self.pool, &key.account_id).await,
        ) {
            RefreshLookup::Found(account) => account,
            RefreshLookup::Missing => {
                self.credential_refresh
                    .forget(&key.provider_id, &key.account_id);
                return;
            }
            RefreshLookup::Transient(error) => {
                tracing::debug!(
                    provider = %key.provider_id,
                    account = %key.account_id,
                    %error,
                    "credential refresh account lookup failed; keeping schedule"
                );
                return;
            }
        };
        if account.status == "disabled" {
            self.credential_refresh
                .forget(&key.provider_id, &key.account_id);
            return;
        }

        let provider = match classify_refresh_lookup(
            crate::db::get_provider(&self.pool, &key.provider_id).await,
        ) {
            RefreshLookup::Found(provider) => provider,
            RefreshLookup::Missing => {
                self.credential_refresh
                    .forget(&key.provider_id, &key.account_id);
                return;
            }
            RefreshLookup::Transient(error) => {
                tracing::debug!(
                    provider = %key.provider_id,
                    account = %key.account_id,
                    %error,
                    "credential refresh provider lookup failed; keeping schedule"
                );
                return;
            }
        };
        let Some(reference) = provider.credential_plugin_ref() else {
            self.credential_refresh
                .forget(&key.provider_id, &key.account_id);
            return;
        };
        let Some(strategy) = self
            .plugin_credentials
            .get(&reference.plugin_id)
            .map(|entry| Arc::clone(entry.value()))
        else {
            // Plugin may be temporarily disabled/reloading. Keep the schedule;
            // the claimed grace window prevents a hot retry loop.
            return;
        };

        match self
            .credential_refresh
            .rotate_scheduled(&provider.id, strategy, &account)
            .await
        {
            Ok(true) => tracing::debug!(
                provider = %provider.id,
                account = %account.id,
                "proactively refreshed credential"
            ),
            Ok(false) => {}
            Err(error) => match self
                .disable_invalid_credential(&account, &error, "proactive credential refresh")
                .await
            {
                Ok(true) => {}
                Ok(false) => tracing::warn!(
                    provider = %provider.id,
                    account = %account.id,
                    code = %error.code,
                    retryable = error.retryable,
                    "proactive credential refresh failed; keeping current credential until expiry"
                ),
                Err(disable_error) => tracing::error!(
                    provider = %provider.id,
                    account = %account.id,
                    credential_error = %error,
                    %disable_error,
                    "failed to disable account after proactive credential refresh confirmed invalid"
                ),
            },
        }
    }

    /// Count a route target skipped during eligibility filtering.
    pub fn record_skip(&self) {
        self.route_skips.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a request that required at least one fallback hop.
    pub fn record_fallback(&self) {
        self.route_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    /// Round-robin counter for a route.
    pub fn rr_counter(&self, route_id: &str) -> Arc<AtomicU64> {
        self.rr_counters
            .entry(route_id.to_string())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone()
    }

    /// Remember the last successful target for affinity and state provenance.
    pub fn sticky_remember(&self, session: &str, target_key: String) {
        self.sticky.insert(
            session.to_string(),
            StickyEntry {
                target_key,
                at: std::time::Instant::now(),
            },
        );
    }

    /// Look up a session's previously-selected target, if still fresh.
    pub fn sticky_lookup(&self, session: &str, ttl: std::time::Duration) -> Option<String> {
        self.sticky.get(session).and_then(|e| {
            if e.at.elapsed() <= ttl {
                Some(e.target_key.clone())
            } else {
                None
            }
        })
    }

    /// Drop sticky entries older than `ttl` (bounded memory).
    pub fn sticky_sweep(&self, ttl: std::time::Duration) {
        self.sticky.retain(|_, e| e.at.elapsed() <= ttl);
    }

    pub fn uptime_secs(&self) -> i64 {
        (chrono::Utc::now() - self.started_at).num_seconds()
    }

    pub fn bump_counter(&self, _k: &str) {
        let _ = Ordering::Relaxed;
    }
}

#[cfg(test)]
mod hook_dispatch_tests {
    use super::*;

    #[test]
    fn refresh_lookup_distinguishes_missing_from_transient_failure() {
        assert!(matches!(
            classify_refresh_lookup::<i32, &str>(Ok(None)),
            RefreshLookup::Missing
        ));
        assert!(matches!(
            classify_refresh_lookup::<i32, &str>(Err("sqlite busy")),
            RefreshLookup::Transient("sqlite busy")
        ));
    }

    #[tokio::test]
    async fn blocked_hook_job_does_not_block_next_job() {
        let (tx, rx) = tokio::sync::mpsc::channel::<HookJob>(HOOK_QUEUE_CAPACITY);
        spawn_hook_worker(rx);

        let gate = Arc::new(tokio::sync::Notify::new());
        let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
        let first_gate = gate.clone();
        let first: HookJob = Box::new(move || {
            Box::pin(async move {
                let _ = first_started_tx.send(());
                first_gate.notified().await;
            })
        });
        tx.send(first).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(100), first_started_rx)
            .await
            .expect("first hook should start")
            .unwrap();

        let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
        let second: HookJob = Box::new(move || {
            Box::pin(async move {
                let _ = second_started_tx.send(());
            })
        });
        tx.send(second).await.unwrap();

        tokio::time::timeout(std::time::Duration::from_millis(100), second_started_rx)
            .await
            .expect("second hook should start while first hook is blocked")
            .unwrap();

        gate.notify_waiters();
    }
}
