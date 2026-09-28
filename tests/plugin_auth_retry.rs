use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    extract::State,
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use kinetix::{
    app::AppState,
    config::Config,
    credentials::{
        CredentialHealth, CredentialRotationError, CredentialStrategy, ResolvedCredential,
    },
    crypto::Crypto,
    db,
    frontends::FrontendFormat,
    logqueue::UsageLogQueue,
    paths::Paths,
    pipeline,
    registry::Registry,
    types::{AuthScheme, InternalRequest, Message, Part, Role, WireFormat},
};
use serde_json::json;
use tokio::sync::{Barrier, Mutex};

#[derive(Clone, Default)]
struct MockUpstream {
    attempts: Arc<Mutex<Vec<String>>>,
    stale_barrier: Option<Arc<Barrier>>,
}

async fn chat_completions(State(state): State<MockUpstream>, headers: HeaderMap) -> Response {
    let auth = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    state.attempts.lock().await.push(auth.clone());

    match auth.as_str() {
        "Bearer stale-token" => {
            if let Some(barrier) = &state.stale_barrier {
                barrier.wait().await;
            }
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": {
                        "message": "invalid authentication credentials",
                        "code": "invalid_api_key"
                    }
                })),
            )
                .into_response()
        }
        "Bearer fresh-token" | "Bearer fallback-token" => (
            StatusCode::OK,
            Json(json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "ok"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 1
                }
            })),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Clone, Copy)]
enum RotationMode {
    Success { delay_ms: u64 },
    RetryableFailure,
    TerminalExpired,
}

struct TestCredential {
    secret: Mutex<String>,
    rotations: AtomicUsize,
    mode: RotationMode,
}

impl TestCredential {
    fn new(mode: RotationMode) -> Self {
        Self {
            secret: Mutex::new("stale-token".into()),
            rotations: AtomicUsize::new(0),
            mode,
        }
    }
}

#[async_trait]
impl CredentialStrategy for TestCredential {
    fn name(&self) -> &'static str {
        "test_rotating_credential"
    }

    async fn resolve(
        &self,
        account: &db::AccountRow,
    ) -> std::result::Result<ResolvedCredential, CredentialRotationError> {
        let secret = if account.label == "fallback-account" {
            "fallback-token".to_string()
        } else {
            self.secret.lock().await.clone()
        };
        Ok(ResolvedCredential {
            secret,
            expires_at: None,
            refresh_after: None,
            rotated: self.rotations.load(Ordering::Relaxed) > 0,
        })
    }

    async fn health(&self, _account: &db::AccountRow) -> CredentialHealth {
        CredentialHealth::Healthy
    }

    async fn rotate(
        &self,
        _account: &db::AccountRow,
    ) -> std::result::Result<(), CredentialRotationError> {
        self.rotations.fetch_add(1, Ordering::Relaxed);
        match self.mode {
            RotationMode::Success { delay_ms } => {
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                *self.secret.lock().await = "fresh-token".into();
                Ok(())
            }
            RotationMode::RetryableFailure => Err(CredentialRotationError::new(
                "upstream_unavailable",
                "refresh endpoint unavailable",
                true,
                Some(5),
            )),
            RotationMode::TerminalExpired => Err(CredentialRotationError::new(
                "credential_expired",
                "refresh token revoked",
                false,
                None,
            )),
        }
    }
}

struct ResolveRefreshingCredential {
    secret: Mutex<String>,
    refreshes: AtomicUsize,
    rotations: AtomicUsize,
}

impl ResolveRefreshingCredential {
    fn new() -> Self {
        Self {
            secret: Mutex::new("stale-token".into()),
            refreshes: AtomicUsize::new(0),
            rotations: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl CredentialStrategy for ResolveRefreshingCredential {
    fn name(&self) -> &'static str {
        "test_resolve_refreshing_credential"
    }

    async fn resolve(
        &self,
        account: &db::AccountRow,
    ) -> std::result::Result<ResolvedCredential, CredentialRotationError> {
        if account.label == "fallback-account" {
            return Ok(ResolvedCredential {
                secret: "fallback-token".into(),
                expires_at: None,
                refresh_after: None,
                rotated: false,
            });
        }

        let stale = self.secret.lock().await.as_str() == "stale-token";
        if stale {
            self.refreshes.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            *self.secret.lock().await = "fresh-token".into();
        }

        let now = chrono::Utc::now();
        Ok(ResolvedCredential {
            secret: self.secret.lock().await.clone(),
            expires_at: Some((now.to_owned() + chrono::Duration::hours(2)).to_rfc3339()),
            refresh_after: Some((now + chrono::Duration::hours(1)).to_rfc3339()),
            rotated: stale,
        })
    }

    async fn rotate(
        &self,
        _account: &db::AccountRow,
    ) -> std::result::Result<(), CredentialRotationError> {
        self.rotations.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

struct TerminalResolveCredential {
    rotations: AtomicUsize,
}

impl TerminalResolveCredential {
    fn new() -> Self {
        Self {
            rotations: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl CredentialStrategy for TerminalResolveCredential {
    fn name(&self) -> &'static str {
        "test_terminal_resolve_credential"
    }

    async fn resolve(
        &self,
        account: &db::AccountRow,
    ) -> std::result::Result<ResolvedCredential, CredentialRotationError> {
        if account.label == "fallback-account" {
            return Ok(ResolvedCredential {
                secret: "fallback-token".into(),
                expires_at: None,
                refresh_after: None,
                rotated: false,
            });
        }

        Err(CredentialRotationError::new(
            "credential_expired",
            "refresh token revoked during resolve",
            false,
            None,
        ))
    }

    async fn rotate(
        &self,
        _account: &db::AccountRow,
    ) -> std::result::Result<(), CredentialRotationError> {
        self.rotations.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

struct Harness {
    state: AppState,
    pool: db::Pool,
    provider_id: String,
    account_id: String,
    fallback_account_id: String,
    request: InternalRequest,
    upstream: MockUpstream,
    server: tokio::task::JoinHandle<()>,
    root: PathBuf,
}

async fn setup(
    strategy: Arc<dyn CredentialStrategy>,
    stale_barrier: Option<Arc<Barrier>>,
    max_attempts: i64,
) -> Harness {
    let upstream = MockUpstream {
        attempts: Arc::new(Mutex::new(Vec::new())),
        stale_barrier,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/chat/completions", post(chat_completions))
        .with_state(upstream.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-plugin-auth-retry-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let paths = Paths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        state_dir: root.join("state"),
    };
    paths.ensure_dirs().unwrap();
    let database_url = paths.database_url();
    let pool = db::connect(&database_url).await.unwrap();
    db::migrate(&pool).await.unwrap();

    let crypto = Arc::new(Crypto::new(&[7_u8; 32]));
    let base_url = format!("http://{addr}");
    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "mock-oauth",
            base_url: &base_url,
            wire_format: WireFormat::Openai,
            auth_scheme: AuthScheme::Bearer,
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: json!({}),
            timeout_ms: 2_000,
            capability_mode: "permissive",
            models_path: None,
            rate_limit_rules: json!({}),
            follow_redirects: false,
            credential_hosts: "",
            allow_insecure_tls: true,
            wire_plugin: "",
            credential_plugin: "plugin:test.oauth/oauth",
            model_source_plugin: "",
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .unwrap();

    let account_id = db::insert_account(
        &pool,
        &provider_id,
        "oauth-account",
        &crypto.encrypt("ignored").unwrap(),
        "oauth",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();

    let fallback_account_id = db::insert_account(
        &pool,
        &provider_id,
        "fallback-account",
        &crypto.encrypt("ignored-fallback").unwrap(),
        "oauth-fallback",
        2,
        1,
        None,
        "none",
    )
    .await
    .unwrap();

    let model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &provider_id,
            upstream_id: "test-model",
            display_name: "Test Model",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: json!({}),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({}),
        },
    )
    .await
    .unwrap();

    let route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "oauth-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({
                "on429": true,
                "onQuota": true,
                "on5xx": true,
                "onTimeout": true
            }),
            portability_policy: "strip_with_warning",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(max_attempts),
            max_concurrent_requests: None,
        },
    )
    .await
    .unwrap();
    db::insert_route_target(&pool, &route_id, None, &model_id, 1, 1, "{}", "{}")
        .await
        .unwrap();

    let registry = Arc::new(Registry::new());
    registry.reload(&pool).await.unwrap();

    let config = Arc::new(Config {
        bind: "127.0.0.1:0".into(),
        public_base_url: "http://127.0.0.1".into(),
        database_url,
        master_key: [7_u8; 32],
        admin_token: "test-admin".into(),
        cf_access_aud: None,
        cf_access_team_domain: None,
        log_json: false,
        bootstrap_file: None,
        allow_private_upstreams: true,
        allow_insecure_tls: true,
        data_dir: paths.data_dir.clone(),
        shutdown_grace_secs: 1,
        max_inflight_inferences: kinetix::config::DEFAULT_MAX_INFLIGHT_INFERENCES,
        alert_webhook_url: None,
        alert_fallback_rate: 1.0,
        alert_error_rate: 1.0,
        alert_min_requests: 1,
        alert_interval_secs: 60,
        alert_p95_latency_ms: 1_000,
        ip_rate_limit_per_min: 0,
        session_ttl_minutes: 60,
        export_retention_days: 1,
        paths,
        generated_admin_password: None,
    });

    let state = AppState::new(
        config,
        pool.clone(),
        registry,
        crypto,
        reqwest::Client::new(),
        UsageLogQueue::new(pool.clone(), 16),
        0,
    );
    state.register_plugin_credential_strategy("test.oauth", strategy);

    let request = InternalRequest {
        requested_model: "oauth-route".into(),
        system: vec![],
        messages: vec![Message {
            role: Role::User,
            parts: vec![Part::Text("hi".into())],
        }],
        tools: vec![],
        tool_choice: None,
        tool_choice_name: None,
        params: Default::default(),
        stream: false,
        include_usage: false,
        thinking: None,
        extra: Default::default(),
        raw_body: Some(
            json!({
                "model": "oauth-route",
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        ),
    };

    Harness {
        state,
        pool,
        provider_id,
        account_id,
        fallback_account_id,
        request,
        upstream,
        server,
        root,
    }
}

async fn consume(response: Response) {
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
}

async fn cleanup(harness: Harness) {
    harness.server.abort();
    let _ = std::fs::remove_dir_all(harness.root);
}

#[tokio::test]
async fn plugin_auth_error_rotates_and_retries_same_account_before_disable() {
    let strategy = Arc::new(TestCredential::new(RotationMode::Success { delay_ms: 0 }));
    let harness = setup(strategy.clone(), None, 1).await;

    let response = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_retry".into(),
        true,
        None,
        vec![],
    )
    .await
    .expect("rotated credential should succeed on the same account");
    consume(response).await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "healthy");
    assert_eq!(strategy.rotations.load(Ordering::Relaxed), 1);
    assert_eq!(
        harness.upstream.attempts.lock().await.as_slice(),
        ["Bearer stale-token", "Bearer fresh-token"]
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn retryable_rotation_failure_cools_down_and_falls_back_without_disabling() {
    let strategy = Arc::new(TestCredential::new(RotationMode::RetryableFailure));
    let harness = setup(strategy.clone(), None, 2).await;

    let response = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_retryable_rotation_failure".into(),
        true,
        None,
        vec![],
    )
    .await
    .expect("temporary refresh failure should fall back to the sibling account");
    consume(response).await;

    let second = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_retryable_rotation_failure_immediate".into(),
        true,
        None,
        vec![],
    )
    .await
    .expect("immediate next request should skip the cooled account");
    consume(second).await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "cooldown");
    assert_ne!(account.status, "disabled");
    let cooldown_until = account
        .cooldown_until
        .as_deref()
        .and_then(db::parse_dt)
        .expect("retryable rotation failure should set a cooldown");
    let remaining = (cooldown_until - chrono::Utc::now()).num_seconds();
    assert!(
        (1..=5).contains(&remaining),
        "plugin retry-after should drive the cooldown, got {remaining}s"
    );

    let fallback = db::get_account(&harness.pool, &harness.fallback_account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fallback.status, "healthy");
    assert_eq!(
        strategy.rotations.load(Ordering::Relaxed),
        1,
        "cooled account must not refresh again on the immediate next request"
    );
    assert_eq!(
        harness.upstream.attempts.lock().await.as_slice(),
        [
            "Bearer stale-token",
            "Bearer fallback-token",
            "Bearer fallback-token",
        ]
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn terminal_expired_rotation_disables_account_then_falls_back() {
    let strategy = Arc::new(TestCredential::new(RotationMode::TerminalExpired));
    let harness = setup(strategy.clone(), None, 2).await;

    let response = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_terminal_rotation_failure".into(),
        true,
        None,
        vec![],
    )
    .await
    .expect("terminal credential failure should fall back to the sibling account");
    consume(response).await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "disabled");

    let fallback = db::get_account(&harness.pool, &harness.fallback_account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fallback.status, "healthy");
    assert_eq!(strategy.rotations.load(Ordering::Relaxed), 1);
    assert_eq!(
        harness.upstream.attempts.lock().await.as_slice(),
        ["Bearer stale-token", "Bearer fallback-token"]
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn concurrent_auth_failures_singleflight_forced_rotation_per_account() {
    let strategy = Arc::new(TestCredential::new(RotationMode::Success { delay_ms: 50 }));
    let barrier = Arc::new(Barrier::new(2));
    let harness = setup(strategy.clone(), Some(barrier), 1).await;

    let first = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_retry_concurrent_1".into(),
        true,
        None,
        vec![],
    );
    let second = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_retry_concurrent_2".into(),
        true,
        None,
        vec![],
    );

    let (first, second) = tokio::join!(first, second);
    consume(first.expect("first request should recover")).await;
    consume(second.expect("second request should share the rotated credential")).await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "healthy");
    assert_eq!(
        strategy.rotations.load(Ordering::Relaxed),
        1,
        "concurrent 401s must perform one forced rotation"
    );

    let attempts = harness.upstream.attempts.lock().await;
    assert_eq!(
        attempts
            .iter()
            .filter(|value| value.as_str() == "Bearer stale-token")
            .count(),
        2
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|value| value.as_str() == "Bearer fresh-token")
            .count(),
        2
    );
    drop(attempts);

    cleanup(harness).await;
}

#[tokio::test]
async fn request_resolve_and_scheduled_refresh_share_one_refresh_singleflight() {
    let strategy = Arc::new(ResolveRefreshingCredential::new());
    let harness = setup(strategy.clone(), None, 1).await;
    let provider = db::get_provider(&harness.pool, &harness.provider_id)
        .await
        .unwrap()
        .unwrap();
    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();

    harness.state.credential_refresh.observe(
        &provider.id,
        &account.id,
        &ResolvedCredential {
            secret: "stale-token".into(),
            expires_at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
            refresh_after: Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339()),
            rotated: false,
        },
    );

    let request_resolve = harness.state.credential_for(&provider, &account);
    let scheduled_strategy: Arc<dyn CredentialStrategy> = strategy.clone();
    let scheduled = harness.state.credential_refresh.rotate_scheduled(
        &provider.id,
        scheduled_strategy,
        &account,
    );
    let (request_resolve, scheduled) = tokio::join!(request_resolve, scheduled);

    assert_eq!(request_resolve.unwrap().secret, "fresh-token");
    assert!(!scheduled.unwrap());
    assert_eq!(
        strategy.refreshes.load(Ordering::Relaxed),
        1,
        "request-path resolve and scheduled refresh must share one refresh exchange"
    );
    assert_eq!(
        strategy.rotations.load(Ordering::Relaxed),
        0,
        "scheduled path should observe the fresh lease instead of rotating again"
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn terminal_credential_expired_from_scheduled_resolve_disables_account() {
    let strategy = Arc::new(TerminalResolveCredential::new());
    let harness = setup(strategy.clone(), None, 1).await;

    harness.state.credential_refresh.observe(
        &harness.provider_id,
        &harness.account_id,
        &ResolvedCredential {
            secret: "stale-token".into(),
            expires_at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
            refresh_after: Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339()),
            rotated: false,
        },
    );

    harness.state.refresh_due_credentials().await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "disabled");
    assert_eq!(
        strategy.rotations.load(Ordering::Relaxed),
        0,
        "terminal resolve evidence must propagate without another rotate attempt"
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn startup_seed_disables_account_when_resolve_confirms_credential_expired() {
    let strategy = Arc::new(TerminalResolveCredential::new());
    let harness = setup(strategy, None, 1).await;

    harness.state.seed_credential_refreshes().await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "disabled");
    let fallback = db::get_account(&harness.pool, &harness.fallback_account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fallback.status, "healthy");

    cleanup(harness).await;
}

#[tokio::test]
async fn terminal_resolve_credential_expired_disables_account_and_uses_fallback() {
    let strategy = Arc::new(TerminalResolveCredential::new());
    let harness = setup(strategy.clone(), None, 2).await;

    let response = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_terminal_resolve_failure".into(),
        true,
        None,
        vec![],
    )
    .await
    .expect("request should fall back after resolve confirms the credential is revoked");
    consume(response).await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "disabled");
    let fallback = db::get_account(&harness.pool, &harness.fallback_account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fallback.status, "healthy");
    assert_eq!(
        harness.upstream.attempts.lock().await.as_slice(),
        ["Bearer fallback-token"],
        "a revoked credential must be skipped before any upstream request"
    );
    assert_eq!(strategy.rotations.load(Ordering::Relaxed), 0);

    cleanup(harness).await;
}

#[tokio::test]
async fn token_count_terminal_resolve_disables_account() {
    let strategy = Arc::new(TerminalResolveCredential::new());
    let harness = setup(strategy, None, 1).await;
    sqlx::query("UPDATE providers SET wire_format = ? WHERE id = ?")
        .bind(WireFormat::Anthropic.as_str())
        .bind(&harness.provider_id)
        .execute(&harness.pool)
        .await
        .unwrap();
    harness.state.registry.reload(&harness.pool).await.unwrap();
    let key = db::VirtualKeyRow {
        id: "token-count-test-key".into(),
        key_hash: String::new(),
        name: "token-count-test".into(),
        owner: "test".into(),
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
        created_at: db::now_iso(),
        revoked_at: None,
    };

    let result = pipeline::count_tokens(
        &harness.state,
        &key,
        &harness.request,
        "req_plugin_auth_token_count_terminal_resolve",
        &[],
    )
    .await;

    assert!(result.is_err());
    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "disabled");
    let fallback = db::get_account(&harness.pool, &harness.fallback_account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fallback.status, "healthy");

    cleanup(harness).await;
}

#[tokio::test]
async fn disable_write_failure_keeps_existing_refresh_schedule() {
    let strategy = Arc::new(TerminalResolveCredential::new());
    let harness = setup(strategy, None, 1).await;
    sqlx::query("UPDATE providers SET wire_format = ? WHERE id = ?")
        .bind(WireFormat::Anthropic.as_str())
        .bind(&harness.provider_id)
        .execute(&harness.pool)
        .await
        .unwrap();
    harness.state.registry.reload(&harness.pool).await.unwrap();
    let now = chrono::Utc::now();
    harness.state.credential_refresh.observe(
        &harness.provider_id,
        &harness.account_id,
        &ResolvedCredential {
            secret: "previously-valid-token".into(),
            expires_at: Some((now + chrono::Duration::hours(1)).to_rfc3339()),
            refresh_after: Some((now - chrono::Duration::seconds(1)).to_rfc3339()),
            rotated: false,
        },
    );
    sqlx::query(
        "CREATE TRIGGER reject_account_status_update BEFORE UPDATE OF status ON accounts \
         BEGIN SELECT RAISE(ABORT, 'injected status persistence failure'); END",
    )
    .execute(&harness.pool)
    .await
    .unwrap();
    let key = db::VirtualKeyRow {
        id: "token-count-disable-failure-key".into(),
        key_hash: String::new(),
        name: "token-count-test".into(),
        owner: "test".into(),
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
        created_at: db::now_iso(),
        revoked_at: None,
    };

    let result = pipeline::count_tokens(
        &harness.state,
        &key,
        &harness.request,
        "req_plugin_auth_token_count_disable_failure",
        &[],
    )
    .await;

    assert!(result.is_err());
    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "healthy");

    // Exercise the scheduled path too: its terminal resolve must retain the
    // schedule until the failed disabled-status write can be retried.
    harness.state.refresh_due_credentials().await;
    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "healthy");

    let retry_due = chrono::Utc::now() + chrono::Duration::seconds(61);
    let retry_claims = harness.state.credential_refresh.claim_due(retry_due);
    assert!(
        retry_claims
            .iter()
            .any(|key| key.account_id == harness.account_id),
        "a failed disable write must leave the previously scheduled account claimable"
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn confirmed_missing_account_forgets_refresh_schedule() {
    let strategy = Arc::new(TestCredential::new(RotationMode::Success { delay_ms: 0 }));
    let harness = setup(strategy, None, 1).await;
    let missing_account = "acc_missing";

    harness.state.credential_refresh.observe(
        &harness.provider_id,
        missing_account,
        &ResolvedCredential {
            secret: "stale-token".into(),
            expires_at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
            refresh_after: Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339()),
            rotated: false,
        },
    );

    harness.state.refresh_due_credentials().await;

    let future = chrono::Utc::now() + chrono::Duration::minutes(2);
    assert!(
        harness
            .state
            .credential_refresh
            .claim_due(future)
            .is_empty(),
        "confirmed missing account must permanently forget its refresh schedule"
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn confirmed_missing_provider_forgets_refresh_schedule() {
    let strategy = Arc::new(TestCredential::new(RotationMode::Success { delay_ms: 0 }));
    let harness = setup(strategy, None, 1).await;
    let missing_provider = "prov_missing";

    harness.state.credential_refresh.observe(
        missing_provider,
        &harness.account_id,
        &ResolvedCredential {
            secret: "stale-token".into(),
            expires_at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
            refresh_after: Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339()),
            rotated: false,
        },
    );

    harness.state.refresh_due_credentials().await;

    let future = chrono::Utc::now() + chrono::Duration::minutes(2);
    assert!(
        harness
            .state
            .credential_refresh
            .claim_due(future)
            .is_empty(),
        "confirmed missing provider must permanently forget its refresh schedule"
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn transient_account_lookup_failure_keeps_refresh_schedule() {
    let strategy = Arc::new(TestCredential::new(RotationMode::Success { delay_ms: 0 }));
    let harness = setup(strategy, None, 1).await;

    harness.state.credential_refresh.observe(
        &harness.provider_id,
        &harness.account_id,
        &ResolvedCredential {
            secret: "stale-token".into(),
            expires_at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
            refresh_after: Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339()),
            rotated: false,
        },
    );

    harness.pool.close().await;
    harness.state.refresh_due_credentials().await;

    let future = chrono::Utc::now() + chrono::Duration::minutes(2);
    let due = harness.state.credential_refresh.claim_due(future);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].provider_id.as_str(), harness.provider_id.as_str());
    assert_eq!(due[0].account_id.as_str(), harness.account_id.as_str());

    cleanup(harness).await;
}
