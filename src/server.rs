//! Server startup: database, migrations, registry, HTTP clients, background
//! tasks, and the axum listener. Kept separate from `main.rs` so the CLI can
//! reuse it for the `serve` subcommand.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::connect_info::Connected;
use axum::serve::{IncomingStream, Listener};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::app::AppState;
use crate::client_disconnect::ClientDisconnect;
use crate::config::Config;
use crate::crypto::Crypto;
use crate::logqueue::UsageLogQueue;
use crate::opaque_state::OpaqueStateStore;
use crate::plugins::{HostPolicy, PluginManager};
use crate::registry::Registry;
use crate::{alerts, bootstrap, db, export, router};
use tokio_util::sync::CancellationToken;

/// Listener that makes the client socket's lifetime available to request handlers.
pub struct DisconnectAwareListener {
    inner: TcpListener,
    abort: CancellationToken,
}

impl DisconnectAwareListener {
    pub fn new(inner: TcpListener) -> Self {
        Self::with_abort(inner, CancellationToken::new())
    }

    /// Accepted connections observe `abort` as a client disconnect, so the
    /// graceful-shutdown deadline cancels uncommitted upstream work.
    pub fn with_abort(inner: TcpListener, abort: CancellationToken) -> Self {
        Self { inner, abort }
    }
}

/// Connection metadata added to each request by Axum.
#[derive(Clone, Debug)]
pub struct ClientConnectionInfo {
    pub peer_addr: SocketAddr,
    pub disconnect: ClientDisconnect,
}

/// IO wrapper used to stop the socket monitor when the server drops a connection.
#[doc(hidden)]
pub struct DisconnectAwareIo {
    stream: TcpStream,
    disconnect: ClientDisconnect,
}

impl Drop for DisconnectAwareIo {
    fn drop(&mut self) {
        self.disconnect.connection_closed();
    }
}

impl AsyncRead for DisconnectAwareIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for DisconnectAwareIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write_vectored(cx, bufs)
    }
}

impl Listener for DisconnectAwareListener {
    type Io = DisconnectAwareIo;
    type Addr = ClientConnectionInfo;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, peer_addr) = Listener::accept(&mut self.inner).await;
            let std_stream = match stream.into_std() {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::warn!(%error, "could not convert client stream for disconnect monitoring");
                    continue;
                }
            };
            let monitor_stream = match std_stream.try_clone() {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::warn!(%error, "could not clone client stream for disconnect monitoring");
                    continue;
                }
            };
            let stream = match TcpStream::from_std(std_stream) {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::warn!(%error, "could not register client stream with Tokio");
                    continue;
                }
            };
            let monitor = match TcpStream::from_std(monitor_stream) {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::warn!(%error, "could not register client monitor with Tokio");
                    continue;
                }
            };
            let disconnect = ClientDisconnect::new(monitor, self.abort.clone());
            return (
                DisconnectAwareIo {
                    stream,
                    disconnect: disconnect.clone(),
                },
                ClientConnectionInfo {
                    peer_addr,
                    disconnect,
                },
            );
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(ClientConnectionInfo {
            peer_addr: self.inner.local_addr()?,
            disconnect: ClientDisconnect::unmonitored(),
        })
    }
}

impl Connected<IncomingStream<'_, DisconnectAwareListener>> for ClientConnectionInfo {
    fn connect_info(stream: IncomingStream<'_, DisconnectAwareListener>) -> Self {
        stream.remote_addr().clone()
    }
}

pub fn init_tracing(json: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("kinetix=info,tower_http=warn,sqlx=warn"));
    let registry = tracing_subscriber::registry().with(filter);
    if json {
        registry
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().compact())
            .init();
    }
}

/// Build the shared control-plane state used by serving and offline route tools.
/// Plugin activation remains a separate server-startup step because it can
/// reconcile persisted plugin state.
pub async fn build_app_state(
    config: Arc<Config>,
    pool: db::Pool,
    registry: Arc<Registry>,
    crypto: Arc<Crypto>,
) -> Result<AppState> {
    let log_queue = UsageLogQueue::new(pool.clone(), 4096);
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(10))
        .http2_adaptive_window(true)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("kinetix/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;
    let state = AppState::new(
        config.clone(),
        pool.clone(),
        registry,
        crypto.clone(),
        http,
        log_queue,
        config.ip_rate_limit_per_min,
    );
    match PluginManager::new(
        pool,
        crypto,
        HostPolicy {
            allow_private_network: config.allow_private_upstreams,
            ..HostPolicy::default()
        },
        config.paths.plugin_packages_dir(),
    ) {
        Ok(manager) => Ok(state.with_plugins(Arc::new(manager))),
        Err(error) => {
            tracing::warn!(error = %error, "plugin host unavailable; plugins disabled");
            Ok(state)
        }
    }
}

/// Run the full server with the given configuration.
pub async fn run(config: Arc<Config>) -> Result<()> {
    init_tracing(config.log_json);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting Kinetix");

    // A freshly generated admin password is only known once, at config build
    // time; surface it now that logging is initialized.
    if let Some(pw) = &config.generated_admin_password {
        tracing::warn!(admin_password = %pw, "generated admin password (shown once)");
        eprintln!("Generated admin password (shown once — store it now):\n  {pw}");
    }

    // Open and migrate with a pre-migration backup when existing data will change.
    let pool = db::open_and_migrate(&config.database_url, &config.data_dir).await?;

    let crypto = Arc::new(Crypto::new(&config.master_key));

    // Optional bootstrap seed (first run only).
    if let Some(path) = &config.bootstrap_file {
        if path.exists() {
            let boot = crate::config::load_bootstrap(path)?;
            match bootstrap::seed_if_empty(&pool, &crypto, config.as_ref().into(), &boot).await {
                Ok(generated) => {
                    for (name, key) in generated {
                        tracing::warn!(key_name = %name, virtual_key = %key, "generated bootstrap virtual key (shown once)");
                    }
                }
                Err(e) => tracing::error!(error = %e, "bootstrap seeding failed"),
            }
        } else {
            tracing::warn!(path = %path.display(), "bootstrap file does not exist; skipping");
        }
    }

    // Registry + usage log queue. A reload failure at startup must not take
    // down serving (NFR-2.6/2.7).
    let registry = Arc::new(Registry::new());
    if let Err(e) = registry.reload(&pool).await {
        tracing::error!(
            error = %e,
            "initial registry reload failed; starting with an empty snapshot and retrying in the background"
        );
    }
    warn_on_unroutable_routes(&registry);
    let state = build_app_state(config.clone(), pool.clone(), registry, crypto).await?;

    // Disable persisted plugins that fail the current manifest or permission
    // contract before re-registering previously enabled capabilities. This also
    // ensures invalid legacy rows cannot activate credential strategies or adapters.
    if let Some(lifecycle) = crate::plugin_lifecycle::PluginLifecycle::new(&state) {
        if let Err(e) = lifecycle.activate_persisted().await {
            tracing::warn!(
                error = %e,
                "could not activate persisted plugins at startup; plugin capabilities unavailable"
            );
        }
    }

    spawn_background_tasks(state.clone());

    let app = router::build(state.clone());
    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;

    tracing::info!(addr = %config.bind, "Kinetix is listening");
    serve_with_shutdown(
        listener,
        app,
        Duration::from_secs(config.shutdown_grace_secs),
        shutdown_signal(),
        ShutdownResources::from_state(&state),
    )
    .await?;

    tracing::info!("Kinetix server stopped");
    Ok(())
}

/// Bound on each post-drain flush step so a wedged writer cannot hold the
/// process past shutdown indefinitely.
const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Process-owned resources the shutdown sequence drains, flushes, and closes.
pub(crate) struct ShutdownResources {
    pub lifecycle: crate::process::ProcessLifecycle,
    pub opaque_state: Arc<OpaqueStateStore>,
    pub log_queue: Option<UsageLogQueue>,
    pub target_telemetry: Option<crate::target_telemetry::TargetTelemetry>,
    pub pool: Option<db::Pool>,
}

impl ShutdownResources {
    pub(crate) fn from_state(state: &AppState) -> Self {
        Self {
            lifecycle: state.lifecycle.clone(),
            opaque_state: state.opaque_state.clone(),
            log_queue: Some(state.log_queue.clone()),
            target_telemetry: Some(state.target_telemetry.clone()),
            pool: Some(state.pool.clone()),
        }
    }

    /// Flush accounting and durability queues, then close the database. Each
    /// step is bounded by [`SHUTDOWN_FLUSH_TIMEOUT`].
    async fn flush_and_close(self) {
        async fn bounded(step: &str, work: impl std::future::Future<Output = ()>) {
            if tokio::time::timeout(SHUTDOWN_FLUSH_TIMEOUT, work)
                .await
                .is_err()
            {
                tracing::warn!(step, "shutdown flush step timed out");
            }
        }
        if let Some(queue) = &self.log_queue {
            bounded("usage_log", queue.flush()).await;
        }
        if let Some(telemetry) = &self.target_telemetry {
            bounded("target_telemetry", telemetry.flush()).await;
        }
        // A tool call already returned to a client may still have a queued
        // opaque-state durability write. Flush it so a restart does not lose a
        // signature the client was told was accepted.
        bounded("opaque_state", self.opaque_state.flush()).await;
        if let Some(pool) = &self.pool {
            bounded("database", pool.close()).await;
        }
    }
}

/// Serve until `shutdown` resolves, then run the graceful shutdown sequence
/// (`docs/guarantees.md`):
///
/// 1. stop accepting connections and stop scheduling background work;
/// 2. drain in-flight requests, committed streams, background iterations, and
///    tracked flights until `grace` elapses;
/// 3. at the deadline fire the abort token: uncommitted requests are cancelled
///    (cancelling their upstream calls and recording cancellation accounting),
///    committed streams end with an explicit error, and remaining tracked
///    flights are dropped, each given [`crate::process::ABORT_UNWIND`];
/// 4. flush usage accounting, target telemetry, and opaque state;
/// 5. close the database.
async fn serve_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    grace: Duration,
    shutdown: F,
    resources: ShutdownResources,
) -> Result<()>
where
    F: std::future::Future<Output = ()>,
{
    let lifecycle = resources.lifecycle.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let listener = DisconnectAwareListener::with_abort(listener, lifecycle.abort_token());
    let mut server_task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<ClientConnectionInfo>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    // Keep serving normally until either the server exits unexpectedly or an
    // OS shutdown signal arrives.
    let server_exit = tokio::select! {
        result = &mut server_task => Some(result),
        _ = shutdown => None,
    };
    if let Some(result) = server_exit {
        lifecycle.begin_shutdown();
        lifecycle.drain(tokio::time::Instant::now() + grace).await;
        resources.flush_and_close().await;
        result
            .context("server task failed")?
            .context("server error")?;
        return Ok(());
    }

    tracing::info!(
        grace_secs = grace.as_secs(),
        "shutdown signal received; draining in-flight requests"
    );
    lifecycle.begin_shutdown();
    let _ = shutdown_tx.send(());
    let deadline = tokio::time::Instant::now() + grace;

    let server_drain = async {
        match tokio::time::timeout_at(deadline, &mut server_task).await {
            Ok(result) => {
                tracing::info!("all in-flight requests drained");
                Some(result)
            }
            Err(_) => {
                tracing::warn!(
                    grace_secs = grace.as_secs(),
                    "graceful shutdown deadline exceeded; cancelling in-flight requests"
                );
                lifecycle.abort();
                match tokio::time::timeout(crate::process::ABORT_UNWIND, &mut server_task).await {
                    Ok(result) => Some(result),
                    Err(_) => {
                        // A handler that ignores cancellation: stop polling the
                        // server. Returning from run() then tears down the
                        // runtime and any connection task still running.
                        server_task.abort();
                        let _ = (&mut server_task).await;
                        tracing::warn!("in-flight requests did not unwind; forcing shutdown");
                        None
                    }
                }
            }
        }
    };
    let (server_result, background) = tokio::join!(server_drain, lifecycle.drain(deadline));
    if background == crate::process::DrainOutcome::Aborted {
        tracing::warn!("background work aborted at the shutdown deadline");
    }

    resources.flush_and_close().await;

    if let Some(result) = server_result {
        result
            .context("server task failed")?
            .context("server error")?;
    }
    Ok(())
}

/// Longest the credential refresh scheduler sleeps with nothing scheduled. A
/// new or moved schedule wakes it early through
/// [`crate::credential_refresh::RefreshCoordinator::schedule_changed`].
const CREDENTIAL_REFRESH_IDLE_WAIT: Duration = Duration::from_secs(3600);
/// Minimum spacing between credential refresh passes.
const CREDENTIAL_REFRESH_MIN_WAIT: Duration = Duration::from_millis(250);

/// Spawn every process-owned background loop through
/// [`crate::process::ProcessLifecycle`] so graceful shutdown stops scheduling
/// and drains them. Integration-specific work is demand-driven or next-due
/// scheduled; an idle integration costs no network calls, probes, or WASM
/// instantiations (`docs/guarantees.md`).
pub fn spawn_background_tasks(state: AppState) {
    let lifecycle = state.lifecycle.clone();

    // Proactive credential refresh. Resolve plugin-backed accounts once at
    // startup to rehydrate lease deadlines, then sleep until the earliest
    // coordinator deadline (or until a schedule changes) instead of polling.
    {
        let st = state.clone();
        lifecycle.spawn_background(async move {
            st.seed_credential_refreshes().await;
            loop {
                let now = chrono::Utc::now();
                let wait = st
                    .credential_refresh
                    .next_due(now)
                    .map(|at| (at - now).to_std().unwrap_or(Duration::ZERO))
                    .unwrap_or(CREDENTIAL_REFRESH_IDLE_WAIT)
                    .clamp(CREDENTIAL_REFRESH_MIN_WAIT, CREDENTIAL_REFRESH_IDLE_WAIT);
                tokio::select! {
                    biased;
                    _ = st.lifecycle.stopping() => break,
                    _ = st.credential_refresh.schedule_changed() => continue,
                    _ = tokio::time::sleep(wait) => {}
                }
                st.refresh_due_credentials().await;
            }
        });
    }

    // Optional model reconciliation / pricing synchronization. The scheduler
    // only wakes once per minute; per-provider due times and deterministic
    // jitter are persisted in settings by the lifecycle runner. Both intervals
    // default to 0 (disabled), in which case a tick reads one settings row.
    {
        let st = state.clone();
        lifecycle.spawn_background(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            while st.lifecycle.next_tick(&mut tick).await {
                crate::admin::run_scheduled_model_lifecycle(&st).await;
            }
        });
    }

    // Change-driven registry reload (#203). Every registry-affecting write
    // bumps `registry_revision`; the loop compares one integer per second and
    // rebuilds the snapshot only when it moved, keeping health-state changes
    // visible within 1s (NFR-2.8) without a periodic full rebuild.
    {
        let st = state.clone();
        lifecycle.spawn_background(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(1000));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut ticks: u64 = 0;
            while st.lifecycle.next_tick(&mut tick).await {
                if let Err(e) = st.registry.reload_if_changed(&st.pool).await {
                    tracing::warn!(error = %e, "registry reload failed; continuing on last snapshot");
                }
                if ticks.is_multiple_of(60) {
                    st.sticky_sweep(Duration::from_secs(30 * 60));
                }
                ticks = ticks.wrapping_add(1);
            }
        });
    }

    // Scheduled consistent backup with retention (NFR-2.4).
    {
        let st = state.clone();
        lifecycle.spawn_background(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(6 * 3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            tick.tick().await; // skip the immediate first tick
            while st.lifecycle.next_tick(&mut tick).await {
                match db::scheduled_backup(
                    &st.pool,
                    &st.config.database_url,
                    &st.config.data_dir,
                    14,
                )
                .await
                {
                    Ok(Some(_)) => {
                        *st.last_backup_at.lock() = Some(db::now_iso());
                        st.last_backup_failed
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!(error = %e, "scheduled backup failed");
                        st.last_backup_failed
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        });
    }

    // Cached routing facts (§6.4). Refresh them off the request path on the
    // manifest-requested cadence, only for plugins a request read facts from
    // within the demand window. The request path only reads the last
    // host-stamped snapshot, so no plugin/network wall time enters routing.
    if let Some(manager) = state.plugin_manager().cloned() {
        let st = state.clone();
        lifecycle.spawn_background(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut next_due: std::collections::HashMap<String, tokio::time::Instant> =
                std::collections::HashMap::new();
            while st.lifecycle.next_tick(&mut tick).await {
                refresh_demanded_routing_facts(&st, &manager, &mut next_due).await;
            }
        });
    }

    // Per-plugin health probes (§6.5). Run on a background schedule owned by
    // core, never lazily on the routing path, so a cold account never pays a
    // probe's wall time inside a client request (NFR-1.1/1.2). Only Providers
    // a request used within the demand window are probed.
    if let Some(manager) = state.plugin_manager().cloned() {
        let st = state.clone();
        lifecycle.spawn_background(async move {
            if !st
                .lifecycle
                .sleep(st.provider_work.scheduler_jitter(Duration::from_secs(15)))
                .await
            {
                return;
            }
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            while st.lifecycle.next_tick(&mut tick).await {
                run_plugin_health_probes(&st, &manager).await;
            }
        });
    }

    // Webhook alerting (FR-6.6/FR-12.17).
    {
        let st = state.clone();
        let alerts = std::sync::Arc::new(alerts::AlertState::new());
        lifecycle.spawn_background(async move {
            alerts::run(st, alerts).await;
        });
    }

    // Purge expired body logs (FR-6.5 retention) and old route traces.
    {
        let st = state.clone();
        lifecycle.spawn_background(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            while st.lifecycle.next_tick(&mut tick).await {
                if let Ok(n) = db::purge_expired_body_logs(&st.pool).await {
                    if n > 0 {
                        tracing::info!(purged = n, "purged expired body logs");
                    }
                }
                if let Ok(n) = db::purge_old_route_traces(&st.pool, 30).await {
                    if n > 0 {
                        tracing::info!(purged = n, "purged old route traces");
                    }
                }
            }
        });
    }

    // Per-day usage/log export to disk (JSONL logs + CSV summaries) with
    // retention pruning. Runs hourly; failures never touch the data plane.
    {
        let st = state.clone();
        lifecycle.spawn_background(async move {
            let dir = st.config.paths.exports_dir();
            let retention = st.config.export_retention_days as i64;
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            while st.lifecycle.next_tick(&mut tick).await {
                match export::run_export(&st.pool, &dir, 40, retention).await {
                    Ok(n) if n > 0 => tracing::info!(files = n, "exported closed usage days"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "usage export failed"),
                }
            }
        });
    }
}

/// One pass of the cached routing-fact scheduler. Plugins nobody demanded
/// within [`crate::integrations::DEMAND_WINDOW`] are skipped without listing
/// plugins or instantiating anything. A newly demanded plugin without a
/// schedule is refreshed immediately so its first facts land promptly.
async fn refresh_demanded_routing_facts(
    st: &AppState,
    manager: &Arc<PluginManager>,
    next_due: &mut std::collections::HashMap<String, tokio::time::Instant>,
) {
    use crate::integrations::{plugin_key, IntegrationWork, DEMAND_WINDOW};

    let demanded: std::collections::HashSet<String> = st
        .integrations
        .demanded(DEMAND_WINDOW)
        .into_iter()
        .filter_map(|key| key.strip_prefix("plugin:").map(str::to_owned))
        .collect();
    if demanded.is_empty() {
        next_due.clear();
        return;
    }
    let rows = match manager.list().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::debug!(error = %error, "listing plugins for cached routing fact refresh failed");
            return;
        }
    };

    let now = tokio::time::Instant::now();
    let mut active = std::collections::HashSet::new();
    let mut due = Vec::new();

    for row in rows {
        if !demanded.contains(&row.id) || !row.status().is_enabled() {
            continue;
        }
        let Some(manifest) = row.manifest() else {
            continue;
        };
        if manifest.routing_facts_mode != "cached" || manifest.provides.routing_facts.is_empty() {
            continue;
        }

        let integration = plugin_key(&row.id);
        st.integrations.record(&integration, IntegrationWork::Poll);
        active.insert(row.id.clone());
        let cadence = Duration::from_millis(manifest.routing_facts_refresh_ms);
        if next_due
            .get(&row.id)
            .is_some_and(|deadline| *deadline > now)
        {
            continue;
        }
        let Some(identity) = st.provider_work.auxiliary_identity(&integration) else {
            continue;
        };

        next_due.insert(
            row.id.clone(),
            now + cadence
                + st.provider_work
                    .scheduler_jitter(cadence.min(Duration::from_secs(30))),
        );
        st.integrations.record(&integration, IntegrationWork::Wake);
        due.push((row.id, identity));
    }

    next_due.retain(|plugin_id, _| active.contains(plugin_id));

    let mut jobs = tokio::task::JoinSet::new();
    for (plugin_id, identity) in due {
        let manager = manager.clone();
        let state = st.clone();
        jobs.spawn(async move {
            let refresh_plugin_id = plugin_id.clone();
            let result = state
                .provider_work
                .run(
                    identity,
                    crate::provider_work::ProviderWorkClass::RoutingFactsRefresh,
                    Some("cached_snapshot".into()),
                    move || async move {
                        manager
                            .refresh_cached_routing_facts(&refresh_plugin_id)
                            .await
                    },
                    |error| {
                        crate::provider_work::plugin_backoff_evidence_for_scope(
                            error,
                            crate::provider_work::RateLimitScope::Provider,
                        )
                    },
                )
                .await;
            (plugin_id, result)
        });
    }

    while let Some(joined) = jobs.join_next().await {
        match joined {
            Ok((plugin_id, Ok(count))) => {
                tracing::debug!(
                    plugin = %plugin_id,
                    facts = *count,
                    "refreshed cached plugin routing facts"
                );
            }
            Ok((plugin_id, Err(error))) => match error.as_ref() {
                crate::provider_work::ProviderWorkError::BackedOff(wait) => {
                    tracing::debug!(plugin = %plugin_id, retry_after_secs = wait.as_secs(), "cached routing-fact refresh backed off");
                }
                crate::provider_work::ProviderWorkError::Operation(error) => {
                    tracing::debug!(plugin = %plugin_id, error = %error.message(), "cached plugin routing fact refresh failed");
                }
                crate::provider_work::ProviderWorkError::Aborted => {
                    tracing::debug!(plugin = %plugin_id, "cached routing-fact refresh task aborted");
                }
            },
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    "cached plugin routing fact refresh task failed"
                );
            }
        }
    }
}

/// Probe every account of a plugin-bound provider off the request path (§6.5)
/// and fold the observation into account health so routing avoids an account a
/// plugin knows is cooling down or out of quota. The core still owns the policy
/// decision (cooldown windows, circuit breakers); the plugin only supplies
/// evidence.
fn plugin_quota_reset_at(
    reset_at: Option<chrono::DateTime<chrono::Utc>>,
    retry_after_secs: Option<u64>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    reset_at.or_else(|| {
        let seconds = i64::try_from(retry_after_secs?).ok()?;
        now.checked_add_signed(chrono::Duration::try_seconds(seconds)?)
    })
}

async fn apply_plugin_health_status(
    pool: &db::Pool,
    account_id: &str,
    observed_state_version: i64,
    current_status: &str,
    state: &str,
    reset_at: Option<&str>,
    retry_after: Option<u64>,
) {
    match state {
        "healthy" => {
            if current_status != "healthy" {
                let _ = db::set_account_status_if_version(
                    pool,
                    account_id,
                    observed_state_version,
                    "healthy",
                    "probe_healthy",
                    None,
                    None,
                    None,
                )
                .await;
            }
        }
        "degraded" => {
            // Advisory only: surface the reset hint in last_error without
            // taking the account out of rotation.
            let note = reset_at.unwrap_or("plugin reports degraded");
            let _ = db::set_account_status_if_version(
                pool,
                account_id,
                observed_state_version,
                "healthy",
                "probe_degraded",
                None,
                None,
                Some(note),
            )
            .await;
        }
        "unavailable" => {
            // Core owns the cooldown window; the plugin only says the
            // account is not usable right now.
            let until = reset_at.map(str::to_owned).or_else(|| {
                retry_after.map(|seconds| {
                    (chrono::Utc::now() + chrono::Duration::seconds(seconds as i64)).to_rfc3339()
                })
            });
            let _ = db::set_account_status_if_version(
                pool,
                account_id,
                observed_state_version,
                "cooldown",
                "probe_unavailable",
                until.as_deref(),
                None,
                Some("plugin health probe: unavailable"),
            )
            .await;
        }
        // "unknown" and anything else: leave the account as-is.
        _ => {}
    }
}

pub(crate) async fn run_plugin_health_probes(state: &AppState, manager: &Arc<PluginManager>) {
    use crate::integrations::{provider_key, DEMAND_WINDOW};

    if !state
        .integrations
        .demanded(DEMAND_WINDOW)
        .iter()
        .any(|key| key.starts_with("provider:"))
    {
        return;
    }
    let providers = match db::list_providers(&state.pool).await {
        Ok(p) => p
            .into_iter()
            .filter(|provider| {
                state
                    .integrations
                    .demanded_within(&provider_key(&provider.id), DEMAND_WINDOW)
            })
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    let task_errors = crate::provider_work::run_bounded_provider_jobs(providers, {
        let state = state.clone();
        let manager = manager.clone();
        move |provider| {
            let state = state.clone();
            let manager = manager.clone();
            async move {
                run_plugin_health_probes_for_provider(&state, &manager, provider).await;
            }
        }
    })
    .await;
    for error in task_errors {
        tracing::warn!(%error, "plugin health probe provider task failed");
    }
}

async fn run_plugin_health_probes_for_provider(
    state: &AppState,
    manager: &Arc<PluginManager>,
    provider: db::ProviderRow,
) {
    state.integrations.record(
        &crate::integrations::provider_key(&provider.id),
        crate::integrations::IntegrationWork::Poll,
    );
    if !state
        .registry
        .snapshot()
        .providers
        .contains_key(&provider.id)
    {
        return;
    }
    let Some(pref) = provider.credential_plugin_ref() else {
        return;
    };
    // The plugin must be enabled and actually provide a health probe.
    if manager
        .resolve_binding(
            &format!("plugin:{}/{}", pref.plugin_id, pref.capability),
            crate::plugins::Capability::HealthProbe,
        )
        .await
        .is_none()
    {
        return;
    }
    let accounts = match db::accounts_for_provider(&state.pool, &provider.id).await {
        Ok(accounts) => accounts,
        Err(_) => return,
    };
    for account in accounts {
        if account.status == "disabled"
            || !state
                .registry
                .contains_provider_account(&provider.id, &account.id)
        {
            continue;
        }
        let Some(identity) = state.provider_work_identity(&provider.id, Some(&account.id)) else {
            continue;
        };
        state.integrations.record(
            &crate::integrations::provider_key(&provider.id),
            crate::integrations::IntegrationWork::Probe,
        );
        let probe_plugin = pref.plugin_id.clone();
        let probe_provider = provider.id.clone();
        let probe_account = account.id.clone();
        let probe_manager = manager.clone();
        let probe = match state
            .provider_work
            .run(
                identity,
                crate::provider_work::ProviderWorkClass::HealthProbe,
                Some(format!("account:{}", account.id)),
                move || async move {
                    probe_manager
                        .health_probe_with_snapshots(&probe_plugin, &probe_provider, &probe_account)
                        .await
                },
                |error| {
                    crate::provider_work::plugin_backoff_evidence_for_scope(
                        error,
                        crate::provider_work::RateLimitScope::Account,
                    )
                },
            )
            .await
        {
            Ok(observation) => observation,
            Err(error) => {
                match error.as_ref() {
                    crate::provider_work::ProviderWorkError::BackedOff(wait) => {
                        tracing::debug!(plugin = %pref.plugin_id, account = %account.id, retry_after_secs = wait.as_secs(), "plugin health probe backed off");
                    }
                    crate::provider_work::ProviderWorkError::Operation(error) => {
                        tracing::debug!(plugin = %pref.plugin_id, account = %account.id, error = %error.message(), "plugin health probe failed");
                    }
                    crate::provider_work::ProviderWorkError::Aborted => {
                        tracing::debug!(plugin = %pref.plugin_id, account = %account.id, "plugin health probe task aborted");
                    }
                }
                continue;
            }
        };
        let obs = &probe.observation;
        let quota_observation = match probe.quota_snapshots.as_ref() {
            Some(snapshots) => state.quota.observe_plugin_snapshots(
                &provider.id,
                &account.id,
                snapshots
                    .iter()
                    .map(|snapshot| crate::quota::PluginQuotaSnapshot {
                        scope: match &snapshot.scope {
                            crate::plugins::runtime::health_v2_wit::types::QuotaScopeV1::Account => {
                                crate::quota::PluginQuotaScope::Account
                            }
                            crate::plugins::runtime::health_v2_wit::types::QuotaScopeV1::Model(model) => {
                                crate::quota::PluginQuotaScope::Model(model.clone())
                            }
                            crate::plugins::runtime::health_v2_wit::types::QuotaScopeV1::Unknown => {
                                crate::quota::PluginQuotaScope::Unknown
                            }
                        },
                        group: snapshot.group.clone(),
                        bucket_id: snapshot.bucket_id.clone(),
                        remaining_fraction: snapshot.remaining_fraction,
                        remaining: snapshot.remaining,
                        limit: snapshot.limit,
                        unit: snapshot.unit.clone(),
                        window: snapshot.window.clone(),
                        reset_at: snapshot.reset_at.clone(),
                    })
                    .collect(),
            ),
            None => state.quota.observe_plugin(
                &provider.id,
                &account.id,
                obs.quota_state.as_deref(),
                obs.reset_at.as_deref(),
            ),
        };
        if let Some(observation) = quota_observation.filter(|observation| observation.exhausted) {
            let now = chrono::Utc::now();
            let reset_at = plugin_quota_reset_at(observation.reset_at, obs.retry_after, now)
                .unwrap_or_else(|| now + chrono::Duration::seconds(3600))
                .to_rfc3339();
            if matches!(
                db::set_account_status_if_version(
                    &state.pool,
                    &account.id,
                    account.account_state_version,
                    "exhausted",
                    "account_quota_exhausted",
                    None,
                    Some(&reset_at),
                    Some("plugin health probe: quota exhausted"),
                )
                .await,
                Ok(true)
            ) {
                let _ = state.registry.reload(&state.pool).await;
            }
            continue;
        }
        apply_plugin_health_status(
            &state.pool,
            &account.id,
            account.account_state_version,
            &account.status,
            &obs.state,
            obs.reset_at.as_deref(),
            obs.retry_after,
        )
        .await;
    }
}

/// Startup diagnostic: warn about enabled Routes with no eligible target so an
/// operator notices a misconfiguration at boot (NFR-2.7 / Monitoring).
fn warn_on_unroutable_routes(registry: &Registry) {
    let names = registry.routes_with_no_targets();
    if !names.is_empty() {
        tracing::warn!(
            routes = ?names,
            "enabled route(s) have no eligible target at startup; requests to them will fail until configured"
        );
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    #[test]
    fn plugin_quota_retry_hint_supplies_missing_reset_safely() {
        let now = chrono::Utc::now();
        assert_eq!(
            plugin_quota_reset_at(None, Some(90), now),
            Some(now + chrono::Duration::seconds(90))
        );
        let explicit = now + chrono::Duration::seconds(300);
        assert_eq!(
            plugin_quota_reset_at(Some(explicit), Some(90), now),
            Some(explicit)
        );
        assert_eq!(plugin_quota_reset_at(None, Some(u64::MAX), now), None);
    }

    #[tokio::test]
    async fn plugin_health_status_preserves_runtime_state_and_rejects_stale_updates() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-server-health-probe-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let url = format!("sqlite://{}?mode=rwc", root.join("k.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let provider_id = crate::db::insert_provider(
            &pool,
            &crate::db::NewProvider {
                name: "health-probe-test",
                base_url: "https://example.test",
                wire_format: crate::types::WireFormat::Plugin,
                auth_scheme: crate::types::AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: serde_json::json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: serde_json::json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: true,
                wire_plugin: "",
                credential_plugin: "",
                model_source_plugin: "",
                credential_mode: "manual",
                source_plugin_id: None,
                source_integration_id: None,
            },
        )
        .await
        .unwrap();

        let quota = crate::quota::QuotaRegistry::default();
        assert!(quota
            .observe_plugin_snapshots(
                &provider_id,
                "account",
                vec![
                    crate::quota::PluginQuotaSnapshot {
                        scope: crate::quota::PluginQuotaScope::Unknown,
                        group: Some("Gemini Models".into()),
                        bucket_id: Some("gemini-5h".into()),
                        remaining_fraction: Some(0.6),
                        remaining: None,
                        limit: None,
                        unit: Some("requests".into()),
                        window: Some("5h".into()),
                        reset_at: None,
                    },
                    crate::quota::PluginQuotaSnapshot {
                        scope: crate::quota::PluginQuotaScope::Model("gemini-2.5-pro".into()),
                        group: Some("Gemini Models".into()),
                        bucket_id: Some("gemini-pro".into()),
                        remaining_fraction: Some(0.4),
                        remaining: None,
                        limit: None,
                        unit: Some("requests".into()),
                        window: Some("weekly".into()),
                        reset_at: None,
                    },
                ],
            )
            .is_none());
        assert!(quota.adaptive_snapshot(&provider_id, "account").is_none());
        for status in ["cooldown", "exhausted"] {
            let account_id = crate::db::insert_account(
                &pool,
                &provider_id,
                status,
                "encrypted",
                "masked",
                1,
                1,
                None,
                "none",
            )
            .await
            .unwrap();
            let reset_at = "2026-04-01T00:00:00Z";
            crate::db::set_account_status(
                &pool,
                &account_id,
                status,
                if status == "cooldown" {
                    "rate_limited"
                } else {
                    "account_quota_exhausted"
                },
                (status == "cooldown").then_some(reset_at),
                (status == "exhausted").then_some(reset_at),
                Some("runtime-owned failure"),
            )
            .await
            .unwrap();
            let before = crate::db::get_account(&pool, &account_id)
                .await
                .unwrap()
                .unwrap();

            // A quota-only Antigravity observation without account-wide fields
            // reports unknown; core must not treat it as recovery.
            apply_plugin_health_status(
                &pool,
                &account_id,
                before.account_state_version,
                &before.status,
                "unknown",
                None,
                None,
            )
            .await;

            let after = crate::db::get_account(&pool, &account_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(after.status, status);
            assert_eq!(after.cooldown_until, before.cooldown_until);
            assert_eq!(after.quota_reset_at, before.quota_reset_at);
            assert_eq!(after.last_error, before.last_error);
        }

        let account_id = crate::db::insert_account(
            &pool,
            &provider_id,
            "cooldown",
            "encrypted",
            "masked",
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        let observed = crate::db::get_account(&pool, &account_id)
            .await
            .unwrap()
            .unwrap();
        let reset_at = "2026-04-01T00:00:00Z";
        assert!(crate::db::set_account_status_if_version(
            &pool,
            &account_id,
            observed.account_state_version,
            "exhausted",
            "runtime_rate_limited",
            None,
            Some(reset_at),
            Some("newer runtime failure"),
        )
        .await
        .unwrap());
        let newer = crate::db::get_account(&pool, &account_id)
            .await
            .unwrap()
            .unwrap();

        // A probe started before the runtime failure must not undo any of the
        // four account-state transitions, including quota exhaustion.
        for state in ["healthy", "degraded", "unavailable"] {
            apply_plugin_health_status(
                &pool,
                &account_id,
                observed.account_state_version,
                &observed.status,
                state,
                Some("2026-04-01T01:00:00Z"),
                Some(120),
            )
            .await;
        }
        assert!(!crate::db::set_account_status_if_version(
            &pool,
            &account_id,
            observed.account_state_version,
            "exhausted",
            "account_quota_exhausted",
            None,
            Some("2026-04-01T02:00:00Z"),
            Some("plugin health probe: quota exhausted"),
        )
        .await
        .unwrap());

        let after = crate::db::get_account(&pool, &account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.status, newer.status);
        assert_eq!(after.status_reason, newer.status_reason);
        assert_eq!(after.account_state_version, newer.account_state_version);
        assert_eq!(after.cooldown_until, newer.cooldown_until);
        assert_eq!(after.quota_reset_at, newer.quota_reset_at);
        assert_eq!(after.last_error, newer.last_error);

        let _ = std::fs::remove_dir_all(root);
    }

    /// A real temp-file-backed opaque-state store, so the shutdown-flush
    /// regression can prove SQLite durability rather than only RAM.
    async fn test_store() -> (Arc<OpaqueStateStore>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "kinetix-server-opaque-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let url = format!("sqlite://{}?mode=rwc", root.join("k.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let crypto = Arc::new(Crypto::new(&[7_u8; 32]));
        (Arc::new(OpaqueStateStore::new(pool, crypto)), root)
    }

    impl ShutdownResources {
        fn for_opaque_state(opaque_state: Arc<OpaqueStateStore>) -> Self {
            Self {
                lifecycle: crate::process::ProcessLifecycle::new(),
                opaque_state,
                log_queue: None,
                target_telemetry: None,
                pool: None,
            }
        }
    }

    /// The grace deadline fires the abort token, which disconnect-aware
    /// handlers observe as a client disconnect: they cancel their upstream
    /// work instead of holding the process for the unwind window.
    #[tokio::test]
    async fn shutdown_deadline_cancels_disconnect_aware_requests() {
        use axum::extract::ConnectInfo;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_started_tx, request_started_rx) = oneshot::channel::<()>();
        let request_started_tx = Arc::new(Mutex::new(Some(request_started_tx)));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let app = Router::new().route(
            "/upstream",
            get({
                let request_started_tx = request_started_tx.clone();
                let cancelled = cancelled.clone();
                move |ConnectInfo(info): ConnectInfo<ClientConnectionInfo>| {
                    let request_started_tx = request_started_tx.clone();
                    let cancelled = cancelled.clone();
                    async move {
                        if let Some(tx) = request_started_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        info.disconnect.cancelled().await;
                        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                        "cancelled"
                    }
                }
            }),
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let grace = Duration::from_millis(50);
        let (store, root) = test_store().await;
        let server_task = tokio::spawn(serve_with_shutdown(
            listener,
            app,
            grace,
            async move {
                let _ = shutdown_rx.await;
            },
            ShutdownResources::for_opaque_state(store),
        ));
        let request_task = tokio::spawn(async move {
            reqwest::Client::new()
                .get(format!("http://{addr}/upstream"))
                .send()
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), request_started_rx)
            .await
            .expect("request never reached the handler")
            .expect("request-start signal sender dropped");

        let started = tokio::time::Instant::now();
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(500), server_task)
            .await
            .expect("abort should unwind a disconnect-aware request well before ABORT_UNWIND")
            .expect("server task panicked")
            .expect("server returned an error");
        assert!(started.elapsed() >= grace);
        assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));

        request_task.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Shutdown stops background scheduling, lets the running iteration finish
    /// (no half-written state), refuses new work, flushes, and closes the pool.
    #[tokio::test]
    async fn shutdown_drains_background_work_then_closes_database() {
        let (store, root) = test_store().await;
        let pool = crate::db::connect(&format!(
            "sqlite://{}?mode=rwc",
            root.join("bg.db").display()
        ))
        .await
        .unwrap();
        let mut resources = ShutdownResources::for_opaque_state(store);
        resources.pool = Some(pool.clone());
        let lifecycle = resources.lifecycle.clone();

        let iterations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let in_iteration = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let lifecycle_loop = lifecycle.clone();
            let iterations = iterations.clone();
            let in_iteration = in_iteration.clone();
            assert!(lifecycle.spawn_background(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(10));
                while lifecycle_loop.next_tick(&mut tick).await {
                    in_iteration.store(true, std::sync::atomic::Ordering::SeqCst);
                    iterations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    in_iteration.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }));
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_task = tokio::spawn(serve_with_shutdown(
            listener,
            Router::new(),
            Duration::from_secs(1),
            async move {
                let _ = shutdown_rx.await;
            },
            resources,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(900), server_task)
            .await
            .expect("server did not return after shutdown")
            .expect("server task panicked")
            .expect("server returned an error");

        assert!(!in_iteration.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(lifecycle.tracked_len(), 0);
        let after = iterations.load(std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(iterations.load(std::sync::atomic::Ordering::SeqCst), after);
        assert!(!lifecycle.spawn_background(async {}));
        assert!(!lifecycle.spawn_flight(async {}));
        assert!(pool.is_closed());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn shutdown_deadline_bounds_stuck_in_flight_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (request_started_tx, request_started_rx) = oneshot::channel::<()>();
        let request_started_tx = Arc::new(Mutex::new(Some(request_started_tx)));

        let app = Router::new().route(
            "/stuck",
            get({
                let request_started_tx = request_started_tx.clone();
                move || {
                    let request_started_tx = request_started_tx.clone();
                    async move {
                        if let Some(tx) = request_started_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        std::future::pending::<()>().await;
                        "unreachable"
                    }
                }
            }),
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let grace = Duration::from_millis(50);
        let (store, root) = test_store().await;
        let server_task = tokio::spawn(serve_with_shutdown(
            listener,
            app,
            grace,
            async move {
                let _ = shutdown_rx.await;
            },
            ShutdownResources::for_opaque_state(store),
        ));

        let request_task = tokio::spawn(async move {
            reqwest::Client::new()
                .get(format!("http://{addr}/stuck"))
                .send()
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), request_started_rx)
            .await
            .expect("request never reached the handler")
            .expect("request-start signal sender dropped");

        let started = tokio::time::Instant::now();
        shutdown_tx.send(()).unwrap();

        // A handler that ignores cancellation is given ABORT_UNWIND after the
        // grace deadline, then dropped.
        let bound = grace + crate::process::ABORT_UNWIND + Duration::from_millis(500);
        tokio::time::timeout(bound, server_task)
            .await
            .expect("server exceeded the shutdown deadline tolerance")
            .expect("server task panicked")
            .expect("server returned an error");

        assert!(
            started.elapsed() < bound,
            "shutdown should return shortly after the grace deadline plus abort unwind"
        );

        request_task.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn shutdown_returns_cleanly_when_nothing_is_in_flight() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Router::new();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (store, root) = test_store().await;

        let server_task = tokio::spawn(serve_with_shutdown(
            listener,
            app,
            Duration::from_secs(1),
            async move {
                let _ = shutdown_rx.await;
            },
            ShutdownResources::for_opaque_state(store),
        ));

        shutdown_tx.send(()).unwrap();

        tokio::time::timeout(Duration::from_millis(500), server_task)
            .await
            .expect("idle server did not drain promptly")
            .expect("server task panicked")
            .expect("server returned an error");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: a signature captured just before shutdown is still only a
    /// queued durability job (RAM is synchronous, SQLite is async). The
    /// shutdown path must flush it, otherwise a restart loses a continuation
    /// the client was already told was accepted.
    #[tokio::test]
    async fn graceful_shutdown_flushes_queued_opaque_state() {
        use crate::opaque_state::{
            OpaqueClientScope, OpaqueLookupResult, OpaqueStateKind, OpaqueStateTarget,
        };

        let (store, root) = test_store().await;
        let scope = OpaqueClientScope::for_key("shutdown-key");
        let target = OpaqueStateTarget {
            kind: OpaqueStateKind::GeminiThoughtSignature,
            provider_id: "p".into(),
            family: "gemini".into(),
            producer: "native:gemini:v1".into(),
            model_id: "gemini-3".into(),
        };
        store.capture_tool_signature(&scope, &target, None, "call_1", "bash", "SIG");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Router::new();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_task = tokio::spawn(serve_with_shutdown(
            listener,
            app,
            Duration::from_secs(1),
            async move {
                let _ = shutdown_rx.await;
            },
            ShutdownResources::for_opaque_state(store.clone()),
        ));
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(500), server_task)
            .await
            .expect("server did not return after shutdown")
            .expect("server task panicked")
            .expect("server returned an error");

        // Only SQLite can answer now: the shutdown flush must have made the
        // queued write durable.
        store.clear_memory_cache_for_test();
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG".into()),
            "shutdown must flush queued opaque-state durability writes"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
