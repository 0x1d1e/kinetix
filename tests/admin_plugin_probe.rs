use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use kinetix::{
    admin::{self, TestBody},
    app::AppState,
    auth::AdminAuth,
    config::Config,
    crypto::Crypto,
    db,
    logqueue::UsageLogQueue,
    paths::Paths,
    registry::Registry,
    types::{AuthScheme, WireFormat},
};
use serde_json::json;

async fn chat_completions(
    State(attempts): State<Arc<AtomicUsize>>,
) -> (StatusCode, Json<serde_json::Value>) {
    attempts.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        Json(json!({
            "id": "chatcmpl-probe",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "ok"
                },
                "finish_reason": "stop"
            }]
        })),
    )
}

#[tokio::test]
async fn account_probe_uses_registered_plugin_adapter() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/chat/completions", post(chat_completions))
        .with_state(attempts.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-admin-plugin-probe-{}",
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

    let crypto = Arc::new(Crypto::new(&[9_u8; 32]));
    let base_url = format!("http://{addr}");
    let plugin_ref = "plugin:dev.kinetix.test/chat";

    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "plugin-probe",
            base_url: &base_url,
            wire_format: WireFormat::Plugin,
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
            wire_plugin: plugin_ref,
            credential_plugin: "",
            model_source_plugin: "",
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .unwrap();

    let encrypted = crypto.encrypt("probe-key").unwrap();
    let account_id = db::insert_account(
        &pool,
        &provider_id,
        "plugin-account",
        &encrypted,
        "probe-key",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();

    db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &provider_id,
            upstream_id: "probe-model",
            display_name: "Probe Model",
            enabled: true,
            context_window: None,
            max_output_tokens: Some(64),
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

    let registry = Arc::new(Registry::new());
    let config = Arc::new(Config {
        bind: "127.0.0.1:0".into(),
        public_base_url: "http://127.0.0.1".into(),
        database_url,
        master_key: [9_u8; 32],
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

    state.registry.reload(&state.pool).await.unwrap();
    let plugin_adapter = state.adapters.for_format(WireFormat::Openai);
    state.register_plugin_adapter(plugin_ref, plugin_adapter);

    let response = match admin::test_account(
        State(state),
        AdminAuth {
            actor: "test".into(),
            token: "test-admin".into(),
        },
        Path(account_id.clone()),
        Json(TestBody {
            model: Some("probe-model".into()),
            account_id: None,
        }),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => panic!("plugin-backed account probe should succeed"),
    };

    assert_eq!(response.0.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(response.0.get("status").and_then(|v| v.as_u64()), Some(200));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    let account = db::get_account(&pool, &account_id).await.unwrap().unwrap();
    assert!(account.last_probe_at.is_some());

    server.abort();
    let _ = std::fs::remove_dir_all(root);
}
