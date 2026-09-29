//! Server startup: database, migrations, registry, HTTP clients, background
//! tasks, and the axum listener. Kept separate from `main.rs` so the CLI can
//! reuse it for the `serve` subcommand.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::app::AppState;
use crate::config::Config;
use crate::crypto::Crypto;
use crate::logqueue::UsageLogQueue;
use crate::opaque_state::OpaqueStateStore;
use crate::plugins::{HostPolicy, PluginManager};
use crate::registry::Registry;
use crate::{alerts, bootstrap, db, export, router};

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

    // Database + migrations (with a pre-migration backup, NFR-2.4).
    let pool = db::connect(&config.database_url).await?;
    db::backup_before_migration(&config.database_url, &config.data_dir);
    db::migrate(&pool).await?;

    let crypto = Arc::new(Crypto::new(&config.master_key));

    // Optional bootstrap seed (first run only).
    if let Some(path) = &config.bootstrap_file {
        if path.exists() {
            let boot = crate::config::load_bootstrap(path)?;
            match bootstrap::seed_if_empty(&pool, &crypto, &boot).await {
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
    let log_queue = UsageLogQueue::new(pool.clone(), 4096);
    warn_on_unroutable_routes(&registry);

    // HTTP client for upstreams: pooled, HTTP/2, bounded connect timeout, and
    // zero redirects by default (NFR-3.10).
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
        registry.clone(),
        crypto,
        http,
        log_queue,
        config.ip_rate_limit_per_min,
    );
    // Build the plugin host even when no plugins are installed so the registry
    // participates in the runtime
    // snapshot from the start. If the host cannot be constructed the server
    // still starts; plugin-backed capabilities simply stay unavailable.
    let state = match PluginManager::new(
        pool.clone(),
        state.crypto.clone(),
        HostPolicy {
            allow_private_network: config.allow_private_upstreams,
            ..HostPolicy::default()
        },
        config.paths.plugin_packages_dir(),
    ) {
        Ok(manager) => state.with_plugins(Arc::new(manager)),
        Err(e) => {
            tracing::warn!(error = %e, "plugin host unavailable; plugins disabled");
            state
        }
    };

    // Re-register capabilities for plugins that were already enabled in a
    // previous run, so their credential strategies and adapters are available
    // without a re-enable. (Install/enable-time registration covers the rest.)
    if let Some(manager) = state.plugin_manager().cloned() {
        match manager.list().await {
            Ok(rows) => {
                for row in rows.iter().filter(|r| r.status().is_enabled()) {
                    crate::admin::register_enabled_plugin_capabilities(&state, &row.id).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not enumerate plugins at startup"),
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
        state.opaque_state.clone(),
    )
    .await?;

    tracing::info!("Kinetix server stopped");
    Ok(())
}

async fn serve_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    grace: Duration,
    shutdown: F,
    opaque_state: Arc<OpaqueStateStore>,
) -> Result<()>
where
    F: std::future::Future<Output = ()>,
{
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let mut server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });

    // Keep serving normally until either the server exits unexpectedly or an
    // OS shutdown signal arrives.
    tokio::select! {
        result = &mut server_task => {
            result.context("server task failed")?.context("server error")?;
            opaque_state.flush().await;
            return Ok(());
        }
        _ = shutdown => {}
    }

    tracing::info!(
        grace_secs = grace.as_secs(),
        "shutdown signal received; draining in-flight requests"
    );
    let _ = shutdown_tx.send(());

    match tokio::time::timeout(grace, &mut server_task).await {
        Ok(result) => {
            result
                .context("server task failed")?
                .context("server error")?;
            tracing::info!("all in-flight requests drained");
        }
        Err(_) => {
            // run() is the top-level server future. Stop polling the Axum
            // server now; returning from run() then tears down the process
            // runtime and any connection tasks still draining.
            server_task.abort();
            let _ = server_task.await;
            tracing::warn!(
                grace_secs = grace.as_secs(),
                "graceful shutdown deadline exceeded; forcing shutdown"
            );
        }
    }

    // A tool call already returned to a client may still have a queued
    // opaque-state durability write. Flush it before `run()` returns so a
    // restart does not lose a signature the client was told was accepted.
    opaque_state.flush().await;

    Ok(())
}

pub fn spawn_background_tasks(state: AppState) {
    // Proactive credential refresh. Resolve plugin-backed accounts once at
    // startup to rehydrate lease deadlines, then operate only on coordinator
    // entries whose refresh_after/derived expiry lead becomes due.
    {
        let st = state.clone();
        tokio::spawn(async move {
            st.seed_credential_refreshes().await;

            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            tick.tick().await; // seed already performed the immediate pass
            loop {
                tick.tick().await;
                st.refresh_due_credentials().await;
            }
        });
    }

    // Optional model reconciliation / pricing synchronization. The scheduler
    // only wakes once per minute; per-provider due times and deterministic
    // jitter are persisted in settings by the lifecycle runner.
    {
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                crate::admin::run_scheduled_model_lifecycle(&st).await;
            }
        });
    }

    // Frequent registry reload (NFR-2.8: health-state changes visible within 1s;
    // NFR-2.10: reload only swaps an immutable snapshot).
    let st = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(1000));
        loop {
            tick.tick().await;
            if let Err(e) = st.registry.reload(&st.pool).await {
                tracing::warn!(error = %e, "registry reload failed; continuing on last snapshot");
            }
            st.sticky_sweep(Duration::from_secs(30 * 60));
        }
    });

    // Scheduled consistent backup with retention (NFR-2.4).
    {
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(6 * 3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            tick.tick().await; // skip the immediate first tick
            loop {
                tick.tick().await;
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
    // manifest-requested cadence. The request path only reads the last
    // host-stamped snapshot, so no plugin/network wall time enters routing.
    if let Some(manager) = state.plugin_manager().cloned() {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut next_due: std::collections::HashMap<String, tokio::time::Instant> =
                std::collections::HashMap::new();

            loop {
                tick.tick().await;
                let rows = match manager.list().await {
                    Ok(rows) => rows,
                    Err(error) => {
                        tracing::debug!(error = %error, "listing plugins for cached routing fact refresh failed");
                        continue;
                    }
                };

                let now = tokio::time::Instant::now();
                let mut active = std::collections::HashSet::new();
                let mut due = Vec::new();

                for row in rows {
                    if !row.status().is_enabled() {
                        continue;
                    }
                    let Some(manifest) = row.manifest() else {
                        continue;
                    };
                    if manifest.routing_facts_mode != "cached"
                        || manifest.provides.routing_facts.is_empty()
                    {
                        continue;
                    }

                    active.insert(row.id.clone());
                    let is_due = next_due
                        .get(&row.id)
                        .map(|deadline| *deadline <= now)
                        .unwrap_or(true);
                    if !is_due {
                        continue;
                    }

                    next_due.insert(
                        row.id.clone(),
                        now + Duration::from_millis(manifest.routing_facts_refresh_ms),
                    );
                    due.push(row.id);
                }

                next_due.retain(|plugin_id, _| active.contains(plugin_id));

                let mut jobs = tokio::task::JoinSet::new();
                for plugin_id in due {
                    let manager = manager.clone();
                    jobs.spawn(async move {
                        let result = manager.refresh_cached_routing_facts(&plugin_id).await;
                        (plugin_id, result)
                    });
                }

                while let Some(joined) = jobs.join_next().await {
                    match joined {
                        Ok((plugin_id, Ok(count))) => {
                            tracing::debug!(
                                plugin = %plugin_id,
                                facts = count,
                                "refreshed cached plugin routing facts"
                            );
                        }
                        Ok((plugin_id, Err(error))) => {
                            tracing::debug!(
                                plugin = %plugin_id,
                                error = %error.message(),
                                "cached plugin routing fact refresh failed"
                            );
                        }
                        Err(error) => {
                            tracing::debug!(
                                error = %error,
                                "cached plugin routing fact refresh task failed"
                            );
                        }
                    }
                }
            }
        });
    }

    // Per-plugin health probes (§6.5). Run on a background schedule owned by
    // core, never lazily on the routing path, so a cold account never pays a
    // probe's wall time inside a client request (NFR-1.1/1.2).
    if let Some(manager) = state.plugin_manager().cloned() {
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                run_plugin_health_probes(&st, &manager).await;
            }
        });
    }

    // Webhook alerting (FR-6.6/FR-12.17).
    {
        let st = state.clone();
        let alerts = std::sync::Arc::new(alerts::AlertState::new());
        tokio::spawn(async move {
            alerts::run(st, alerts).await;
        });
    }

    // Purge expired body logs (FR-6.5 retention) and old route traces.
    let st = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
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

    // Per-day usage/log export to disk (JSONL logs + CSV summaries) with
    // retention pruning. Runs hourly; failures never touch the data plane.
    let st = state.clone();
    tokio::spawn(async move {
        let dir = st.config.paths.exports_dir();
        let retention = st.config.export_retention_days as i64;
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            match export::run_export(&st.pool, &dir, 40, retention).await {
                Ok(n) if n > 0 => tracing::info!(files = n, "exported closed usage days"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "usage export failed"),
            }
        }
    });
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

async fn run_plugin_health_probes(state: &AppState, manager: &Arc<PluginManager>) {
    let providers = match db::list_providers(&state.pool).await {
        Ok(p) => p,
        Err(_) => return,
    };
    for provider in providers {
        let Some(pref) = provider.credential_plugin_ref() else {
            continue;
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
            continue;
        }
        let accounts = match db::accounts_for_provider(&state.pool, &provider.id).await {
            Ok(a) => a,
            Err(_) => continue,
        };
        for account in accounts {
            if account.status == "disabled" {
                continue;
            }
            let obs = match manager
                .health_probe(&pref.plugin_id, &provider.id, &account.id)
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    tracing::debug!(plugin = %pref.plugin_id, account = %account.id,
                        error = %e.message(), "plugin health probe failed");
                    continue;
                }
            };
            let quota_observation = state.quota.observe_plugin(
                &provider.id,
                &account.id,
                obs.quota_state.as_deref(),
                obs.reset_at.as_deref(),
            );
            if let Some(observation) = quota_observation.filter(|observation| observation.exhausted)
            {
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
            match obs.state.as_str() {
                "healthy" => {
                    if account.status != "healthy" {
                        let _ = db::set_account_status_if_version(
                            &state.pool,
                            &account.id,
                            account.account_state_version,
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
                    let note = obs
                        .reset_at
                        .clone()
                        .unwrap_or_else(|| "plugin reports degraded".into());
                    let _ = db::set_account_status_if_version(
                        &state.pool,
                        &account.id,
                        account.account_state_version,
                        "healthy",
                        "probe_degraded",
                        None,
                        None,
                        Some(&note),
                    )
                    .await;
                }
                "unavailable" => {
                    // Core owns the cooldown window; the plugin only says the
                    // account is not usable right now.
                    let until = obs.reset_at.clone().or_else(|| {
                        obs.retry_after.map(|s| {
                            (chrono::Utc::now() + chrono::Duration::seconds(s as i64)).to_rfc3339()
                        })
                    });
                    let _ = db::set_account_status_if_version(
                        &state.pool,
                        &account.id,
                        account.account_state_version,
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
            store,
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

        tokio::time::timeout(Duration::from_millis(500), server_task)
            .await
            .expect("server exceeded the shutdown deadline tolerance")
            .expect("server task panicked")
            .expect("server returned an error");

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "shutdown should return shortly after the grace deadline"
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
            store,
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
            store.clone(),
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
