use std::sync::Arc;

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
use tokio::sync::Mutex;

#[derive(Clone, Default)]
struct MockUpstream {
    attempts: Arc<Mutex<Vec<String>>>,
}

async fn chat_completions(State(state): State<MockUpstream>, headers: HeaderMap) -> Response {
    let auth = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    state.attempts.lock().await.push(auth.clone());

    match auth.as_str() {
        "Bearer key-1" => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "message": "credit insufficient balance: balance=731 required=2612",
                "type": "invalid_request_error",
                "param": null,
                "code": "invalid_request_error"
            })),
        )
            .into_response(),
        "Bearer key-2" => (
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
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "message": "unexpected credential",
                    "code": "invalid_api_key"
                }
            })),
        )
            .into_response(),
    }
}

#[tokio::test]
async fn insufficient_credit_falls_back_to_second_account() {
    let upstream = MockUpstream::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/chat/completions", post(chat_completions))
        .with_state(upstream.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-byok-credit-fallback-{}",
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
            name: "mock-openai",
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
            credential_plugin: "",
            model_source_plugin: "",
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .unwrap();

    let key_1 = crypto.encrypt("key-1").unwrap();
    let account_1 = db::insert_account(
        &pool,
        &provider_id,
        "account-1",
        &key_1,
        "key-1",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();

    let key_2 = crypto.encrypt("key-2").unwrap();
    let account_2 = db::insert_account(
        &pool,
        &provider_id,
        "account-2",
        &key_2,
        "key-2",
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
            name: "byok-route",
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
            max_attempts: Some(2),
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

    let request = InternalRequest {
        requested_model: "byok-route".into(),
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
                "model": "byok-route",
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        ),
    };

    let response = pipeline::run(
        &state,
        FrontendFormat::OpenAi,
        None,
        request,
        "req_byok_credit_fallback".into(),
        true,
        None,
        vec![],
    )
    .await
    .expect("second account should succeed");
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();

    let first = db::get_account(&pool, &account_1).await.unwrap().unwrap();
    let second = db::get_account(&pool, &account_2).await.unwrap().unwrap();

    assert_eq!(first.status, "exhausted");
    assert!(first.quota_reset_at.is_some());
    assert_eq!(second.status, "healthy");
    assert_eq!(
        upstream.attempts.lock().await.as_slice(),
        ["Bearer key-1", "Bearer key-2"]
    );

    server.abort();
    let _ = std::fs::remove_dir_all(root);
}
