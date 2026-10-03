//! Every stream driver records the same commit point: the passthrough,
//! translated, and aggregated paths each emit one `commit` flight event, and
//! measured streams record `upstream_first_frame` before it.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{header::AUTHORIZATION, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    routing::any,
    Json, Router,
};
use kinetix::{
    api,
    app::AppState,
    config::Config,
    crypto::{self, Crypto},
    db,
    logqueue::UsageLogQueue,
    paths::Paths,
    registry::Registry,
    types::{AuthScheme, WireFormat},
};
use serde_json::{json, Value};

const CLIENT_KEY: &str = "sk-kinetix-commit-point-test";
const ROUTE: &str = "commit-point-route";

async fn upstream(Json(body): Json<Value>) -> Response {
    if body["stream"] == json!(true) {
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                "data: [DONE]\n\n"
            )))
            .unwrap();
    }
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "id": "c1",
                "object": "chat.completion",
                "model": "m",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })
            .to_string(),
        ))
        .unwrap()
}

async fn state(base_url: &str, root: &std::path::Path) -> AppState {
    let paths = Paths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        state_dir: root.join("state"),
    };
    paths.ensure_dirs().unwrap();
    let database_url = paths.database_url();
    let pool = db::connect(&database_url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let crypto = Arc::new(Crypto::new(&[23_u8; 32]));
    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "mock-openai",
            base_url,
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
    db::insert_account(
        &pool,
        &provider_id,
        "mock-account",
        &crypto.encrypt("mock-upstream-key").unwrap(),
        "mock-upstream-key",
        1,
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
            upstream_id: "m",
            display_name: "m",
            enabled: true,
            context_window: None,
            max_output_tokens: Some(1024),
            capabilities: json!({"text": true}),
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
            name: ROUTE,
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "reject",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(1),
            max_concurrent_requests: None,
        },
    )
    .await
    .unwrap();
    db::insert_route_target(&pool, &route_id, None, &model_id, 1, 1, "{}", "{}")
        .await
        .unwrap();
    db::insert_virtual_key(
        &pool,
        &db::VirtualKeyRow {
            id: "vk_commit_point".into(),
            key_hash: crypto::hash_virtual_key(CLIENT_KEY),
            name: "commit point".into(),
            owner: "test".into(),
            tag: String::new(),
            allowed_models: json!(["*"]).to_string(),
            allowed_providers: json!([]).to_string(),
            rpm_limit: None,
            tpm_limit: None,
            max_concurrent_requests: None,
            daily_budget: None,
            monthly_budget: None,
            expires_at: None,
            status: "active".into(),
            allowed_ips: json!([]).to_string(),
            body_logging: 0,
            created_at: db::now_iso(),
            revoked_at: None,
        },
    )
    .await
    .unwrap();
    let registry = Arc::new(Registry::new());
    registry.reload(&pool).await.unwrap();
    AppState::new(
        Arc::new(Config {
            bind: "127.0.0.1:0".into(),
            public_base_url: "http://127.0.0.1".into(),
            database_url,
            master_key: [23_u8; 32],
            admin_token: "test-admin-token".into(),
            cf_access_aud: None,
            cf_access_team_domain: None,
            log_json: false,
            bootstrap_file: None,
            allow_private_upstreams: true,
            allow_insecure_tls: true,
            data_dir: paths.data_dir.clone(),
            shutdown_grace_secs: 1,
            max_inflight_inferences: 4,
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
        }),
        pool.clone(),
        registry,
        crypto,
        reqwest::Client::new(),
        UsageLogQueue::new(pool, 16),
        0,
    )
}

/// Send one request and return the flight events recorded after it completes.
async fn flight_events(state: &AppState, anthropic: bool, stream: bool) -> Vec<String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap(),
    );
    let messages = json!([{"role": "user", "content": "hi"}]);
    let response = if anthropic {
        let body =
            json!({"model": ROUTE, "max_tokens": 16, "stream": stream, "messages": messages});
        api::messages(
            axum::extract::State(state.clone()),
            None,
            headers,
            body.to_string(),
        )
        .await
    } else {
        let body = json!({"model": ROUTE, "stream": stream, "messages": messages});
        api::chat_completions(
            axum::extract::State(state.clone()),
            None,
            headers,
            body.to_string(),
        )
        .await
    };
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("hi"));
    let finished = async {
        loop {
            let events: Vec<String> = state
                .flight
                .events(&request_id)
                .into_iter()
                .map(|event| event.event)
                .collect();
            if events.iter().any(|event| event == "usage_finalized") {
                return events;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), finished)
        .await
        .expect("request did not finalize")
}

fn position(events: &[String], name: &str) -> usize {
    let matches: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| *event == name)
        .collect();
    assert_eq!(matches.len(), 1, "expected one {name}: {events:?}");
    matches[0].0
}

#[tokio::test]
async fn every_stream_driver_records_one_commit_point() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(any(upstream)))
            .await
            .unwrap();
    });
    let root = std::env::temp_dir().join(format!(
        "kinetix-commit-point-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let state = state(&format!("http://{addr}/v1"), &root).await;

    for (driver, anthropic) in [("passthrough", false), ("translated", true)] {
        let events = flight_events(&state, anthropic, true).await;
        assert!(
            position(&events, "upstream_first_frame") < position(&events, "commit"),
            "{driver}: {events:?}"
        );
    }
    let events = flight_events(&state, false, false).await;
    assert!(
        position(&events, "upstream_validated") < position(&events, "commit"),
        "aggregate: {events:?}"
    );
    assert!(
        position(&events, "commit") < position(&events, "usage_finalized"),
        "aggregate: {events:?}"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&root);
}
