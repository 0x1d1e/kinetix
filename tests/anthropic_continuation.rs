//! End-to-end coverage for Anthropic thinking continuation across Route fallback.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
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
    types::{AuthScheme, WireFormat},
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
struct MockRequest {
    body: Value,
    authorization: Option<String>,
}

#[derive(Clone, Default)]
struct MockUpstream {
    requests: Arc<Mutex<Vec<MockRequest>>>,
}

async fn upstream(
    State(mock): State<MockUpstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut requests = mock.requests.lock().await;
    let is_first_attempt = requests.is_empty();
    requests.push(MockRequest {
        body: body.clone(),
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    });
    drop(requests);
    if is_first_attempt && body["metadata"]["test_allow_initial_success"] != true {
        // A 500 exercises Route fallback without the outbound transport's
        // separate retry for transient 502/503/504 gateway errors.
        return (StatusCode::INTERNAL_SERVER_ERROR, "try fallback").into_response();
    }

    if body["stream"] == true {
        let events = concat!(
            "event: message_start\r\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\r\n\r\n",
            "event: content_block_start\r\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\r\n\r\n",
            "event: content_block_delta\r\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"foo\"}}\r\n\r\n",
            "event: content_block_delta\r\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"bar\"}}\r\n\r\n",
            "event: content_block_delta\r\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\r\n\r\n",
            "event: content_block_stop\r\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\r\n\r\n",
            "event: content_block_start\r\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque-redacted-state\"}}\r\n\r\n",
            "event: content_block_stop\r\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\r\n\r\n",
            "event: content_block_start\r\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_mock\",\"name\":\"lookup\",\"input\":{}}}\r\n\r\n",
            "event: content_block_delta\r\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}\r\n\r\n",
            "event: content_block_stop\r\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\r\n\r\n",
            "event: message_delta\r\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\r\n\r\n",
            "event: message_stop\r\n",
            "data: {\"type\":\"message_stop\"}\r\n\r\n",
        );
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(events))
            .unwrap();
    }

    Json(json!({
        "id": "msg_mock",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-5",
        "content": [{"type": "text", "text": "continued"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .into_response()
}

struct Harness {
    state: AppState,
    mock: MockUpstream,
    server: tokio::task::JoinHandle<()>,
    root: std::path::PathBuf,
    initial_origin_key: String,
    history_origin_key: String,
}

async fn setup(
    portability: &str,
    fallback_model: &str,
    share_continuation_family: bool,
) -> Harness {
    let mock = MockUpstream::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_mock = mock.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(post(upstream))
                .with_state(server_mock),
        )
        .await
        .unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-anthropic-continuation-{}",
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
    let crypto = Arc::new(Crypto::new(&[11_u8; 32]));
    let base_url = format!("http://{addr}");

    let mut model_ids = Vec::new();
    let mut account_ids = Vec::new();
    for provider_name in ["anthropic-a", "anthropic-b", "anthropic-s"] {
        let provider_id = db::insert_provider(
            &pool,
            &db::NewProvider {
                name: provider_name,
                base_url: &base_url,
                wire_format: WireFormat::Anthropic,
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
        account_ids.push(
            db::insert_account(
                &pool,
                &provider_id,
                provider_name,
                &crypto
                    .encrypt(match provider_name {
                        "anthropic-a" => "key-a",
                        "anthropic-b" => "key-b",
                        _ => "key-s",
                    })
                    .unwrap(),
                match provider_name {
                    "anthropic-a" => "key-a",
                    "anthropic-b" => "key-b",
                    _ => "key-s",
                },
                1,
                1,
                None,
                "none",
            )
            .await
            .unwrap(),
        );
        let upstream_id = match provider_name {
            "anthropic-a" => "claude-sonnet-5",
            "anthropic-b" => fallback_model,
            _ => "claude-history-5",
        };
        let capabilities = if share_continuation_family && provider_name != "anthropic-s" {
            json!({"continuation_families": ["anthropic_thinking_signature:v1"]})
        } else {
            json!({})
        };
        model_ids.push(
            db::insert_model(
                &pool,
                &db::NewModel {
                    provider_id: &provider_id,
                    upstream_id,
                    display_name: "Claude Sonnet 5",
                    enabled: true,
                    context_window: None,
                    max_output_tokens: None,
                    capabilities,
                    prices: json!({}),
                    parameters: json!({}),
                    thinking_map: json!({}),
                    extra_request: json!({}),
                    discovery: json!({}),
                },
            )
            .await
            .unwrap(),
        );
    }

    let route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "anthropic-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({"on5xx": true}),
            portability_policy: portability,
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(2),
        },
    )
    .await
    .unwrap();
    for (model_id, priority) in model_ids.iter().zip([1, 2, 3]) {
        db::insert_route_target(&pool, &route_id, None, model_id, priority, 1, "{}", "{}")
            .await
            .unwrap();
    }

    let registry = Arc::new(Registry::new());
    registry.reload(&pool).await.unwrap();
    let usage_queue = UsageLogQueue::new(pool.clone(), 16);
    let state = AppState::new(
        Arc::new(Config {
            bind: "127.0.0.1:0".into(),
            public_base_url: "http://127.0.0.1".into(),
            database_url,
            master_key: [11_u8; 32],
            admin_token: "test-admin".into(),
            cf_access_aud: None,
            cf_access_team_domain: None,
            log_json: false,
            bootstrap_file: None,
            allow_private_upstreams: true,
            allow_insecure_tls: true,
            data_dir: paths.data_dir.clone(),
            shutdown_grace_secs: 1,
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
        pool,
        registry,
        crypto,
        reqwest::Client::new(),
        usage_queue,
        0,
    );

    Harness {
        state,
        mock,
        server,
        root,
        initial_origin_key: format!("{route_id}|{}|{}", account_ids[0], model_ids[0]),
        history_origin_key: format!("{route_id}|{}|{}", account_ids[2], model_ids[2]),
    }
}

async fn cleanup(harness: Harness) {
    harness.server.abort();
    let _ = std::fs::remove_dir_all(&harness.root);
}

async fn dispatch(harness: &Harness, body: Value) -> Response {
    dispatch_with_session(harness, body, None).await
}

async fn dispatch_with_session(
    harness: &Harness,
    body: Value,
    session: Option<String>,
) -> Response {
    let raw = serde_json::to_string(&body).unwrap();
    let mut request = kinetix::frontends::anthropic::decode_request(body).unwrap();
    request.raw_body = Some(raw);
    pipeline::run(
        &harness.state,
        FrontendFormat::Anthropic,
        None,
        request,
        "req_anthropic_continuation".into(),
        true,
        session,
        vec![],
    )
    .await
    .unwrap()
}

async fn run(harness: &Harness, body: Value) -> (u16, Option<String>) {
    let response = dispatch(harness, body).await;
    let status = response.status().as_u16();
    let warning = response
        .headers()
        .get("x-kinetix-warning")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, warning)
}

fn first_turn_request() -> Value {
    json!({
        "model": "anthropic-route",
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "first turn"}]
    })
}

fn continuation_request() -> Value {
    json!({
        "model": "anthropic-route",
        "max_tokens": 32,
        "messages": [
            {"role": "user", "content": "continue"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hidden reasoning", "signature": "signed-state"},
                {"type": "redacted_thinking", "data": "opaque-redacted-state"},
                {"type": "text", "text": "answer"}
            ]},
            {"role": "user", "content": "next"}
        ]
    })
}

#[tokio::test]
async fn compatible_anthropic_fallback_preserves_thinking_and_redacted_state() {
    let harness = setup("strip_with_warning", "claude-sonnet-5", true).await;
    harness
        .state
        .sticky_remember("session-a", harness.initial_origin_key.clone());
    let response =
        dispatch_with_session(&harness, continuation_request(), Some("session-a".into())).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-kinetix-warning"),
        None,
        "compatible continuation should not warn"
    );
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();

    let requests = harness.mock.requests.lock().await;
    assert_eq!(
        requests.len(),
        2,
        "the first provider should fail before fallback"
    );
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer key-a"));
    let fallback = &requests[1];
    assert_eq!(fallback.authorization.as_deref(), Some("Bearer key-b"));
    assert_eq!(fallback.body["model"], "claude-sonnet-5");
    assert!(fallback.body["messages"][1]["content"]
        .as_array()
        .unwrap()
        .contains(&json!({
            "type": "thinking",
            "thinking": "hidden reasoning",
            "signature": "signed-state"
        })));
    assert!(fallback.body["messages"][1]["content"]
        .as_array()
        .unwrap()
        .contains(&json!({
            "type": "redacted_thinking",
            "data": "opaque-redacted-state"
        })));
    drop(requests);
    cleanup(harness).await;
}

#[tokio::test]
async fn missing_session_provenance_does_not_guess_first_route_target() {
    let harness = setup("strip_with_warning", "claude-opus-5", false).await;

    let mut first_request = first_turn_request();
    first_request["stream"] = json!(true);
    let first_response = dispatch(&harness, first_request).await;
    assert_eq!(first_response.status(), StatusCode::OK);
    let first_bytes = axum::body::to_bytes(first_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let first_body = String::from_utf8(first_bytes.to_vec()).unwrap();
    assert!(first_body.contains(r#""thinking":"foo""#));
    assert!(first_body.contains(r#""signature":"sig""#));
    assert!(first_body.contains("opaque-redacted-state"));

    let second_request = json!({
        "model": "anthropic-route",
        "max_tokens": 32,
        "messages": [
            {"role": "user", "content": "first turn"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "foobar", "signature": "sig"},
                {"type": "redacted_thinking", "data": "opaque-redacted-state"},
                {"type": "tool_use", "id": "toolu_mock", "name": "lookup", "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": "second turn"}
        ]
    });
    let second_response = dispatch(&harness, second_request).await;
    assert_eq!(second_response.status(), StatusCode::OK);
    let warning = second_response
        .headers()
        .get("x-kinetix-warning")
        .and_then(|value| value.to_str().ok())
        .expect("unknown continuation origin must be reported");
    assert!(warning.contains("omitted"));
    let _ = axum::body::to_bytes(second_response.into_body(), 1024 * 1024)
        .await
        .unwrap();

    let requests = harness.mock.requests.lock().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer key-a"));
    assert_eq!(requests[1].authorization.as_deref(), Some("Bearer key-b"));
    assert_eq!(requests[2].authorization.as_deref(), Some("Bearer key-a"));
    let second_turn_content = requests[2].body["messages"][1]["content"]
        .as_array()
        .unwrap();
    assert!(
        !second_turn_content.iter().any(|part| {
            matches!(
                part["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            )
        }),
        "B's continuation state must not be replayed to recovered A without provenance"
    );
    drop(requests);
    cleanup(harness).await;
}

#[tokio::test]
async fn compatible_raw_passthrough_strips_unsigned_thinking() {
    let harness = setup("strip_with_warning", "claude-sonnet-5", true).await;
    harness
        .state
        .sticky_remember("session-a", harness.initial_origin_key.clone());
    let mut bootstrap = first_turn_request();
    bootstrap["metadata"] = json!({"test_allow_initial_success": true});
    let first = dispatch_with_session(&harness, bootstrap, Some("session-a".into())).await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(first.into_body(), 1024 * 1024)
        .await
        .unwrap();

    let mut request = continuation_request();
    request["messages"][1]["content"][0]
        .as_object_mut()
        .unwrap()
        .remove("signature");
    let response = dispatch_with_session(&harness, request, Some("session-a".into())).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("x-kinetix-warning"));
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();

    let requests = harness.mock.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer key-a"));
    assert_eq!(requests[1].authorization.as_deref(), Some("Bearer key-a"));
    let content = requests[1].body["messages"][1]["content"]
        .as_array()
        .unwrap();
    assert!(
        !content.iter().any(|part| part["type"] == "thinking"),
        "unsigned thinking must not leak through raw-body passthrough"
    );
    drop(requests);
    cleanup(harness).await;
}

#[tokio::test]
async fn failed_candidate_does_not_replace_session_continuation_origin() {
    let harness = setup("strip_with_warning", "claude-sonnet-5", true).await;
    harness
        .state
        .sticky_remember("session-s", harness.history_origin_key.clone());
    let response =
        dispatch_with_session(&harness, continuation_request(), Some("session-s".into())).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("x-kinetix-warning"));
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();

    let requests = harness.mock.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer key-a"));
    assert_eq!(requests[1].authorization.as_deref(), Some("Bearer key-b"));
    let fallback_content = requests[1].body["messages"][1]["content"]
        .as_array()
        .unwrap();
    assert!(
        !fallback_content.iter().any(|part| {
            matches!(
                part["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            )
        }),
        "B must use session origin S, not failed candidate A, for portability"
    );
    drop(requests);
    cleanup(harness).await;
}

#[tokio::test]
async fn native_anthropic_sse_preserves_payload_and_normalizes_crlf_framing() {
    let harness = setup("strip_with_warning", "claude-sonnet-5", true).await;
    let mut request = continuation_request();
    request["stream"] = json!(true);
    let response = dispatch(&harness, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains(
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"foo\"}}\n\n"
    ));
    assert!(body.contains(
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"bar\"}}\n\n"
    ));
    assert!(body.contains(
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n"
    ));
    assert!(body.contains("opaque-redacted-state"));
    assert!(body.contains("toolu_mock"));
    assert!(!body.contains('\r'), "SSE framing is normalized to LF");
    cleanup(harness).await;
}

#[tokio::test]
async fn non_streaming_anthropic_aggregation_reassembles_continuation_blocks() {
    let harness = setup("strip_with_warning", "claude-sonnet-5", true).await;
    let response = dispatch(&harness, continuation_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["content"],
        json!([
            {"type": "thinking", "thinking": "foobar", "signature": "sig"},
            {"type": "redacted_thinking", "data": "opaque-redacted-state"},
            {"type": "tool_use", "id": "toolu_mock", "name": "lookup", "input": {"city": "Paris"}}
        ])
    );
    assert_eq!(body["stop_reason"], "tool_use");
    cleanup(harness).await;
}

#[tokio::test]
async fn incompatible_anthropic_fallback_obeys_strip_policy() {
    let harness = setup("strip_with_warning", "claude-opus-5", false).await;
    let (status, warning) = run(&harness, continuation_request()).await;
    assert_eq!(status, 200);
    assert!(warning.is_some(), "stripping must be visible to the client");

    let requests = harness.mock.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let fallback = &requests[1];
    assert_eq!(fallback.authorization.as_deref(), Some("Bearer key-b"));
    let content = fallback.body["messages"][1]["content"].as_array().unwrap();
    assert!(!content.iter().any(|part| {
        matches!(
            part["type"].as_str(),
            Some("thinking" | "redacted_thinking")
        )
    }));
    drop(requests);
    cleanup(harness).await;
}

#[tokio::test]
async fn same_model_name_without_shared_family_is_not_compatible_across_providers() {
    let harness = setup("strip_with_warning", "claude-sonnet-5", false).await;
    let (status, warning) = run(&harness, continuation_request()).await;
    assert_eq!(status, 200);
    assert!(
        warning.is_some(),
        "model-name equality must not imply compatibility"
    );

    let requests = harness.mock.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].authorization.as_deref(), Some("Bearer key-b"));
    let content = requests[1].body["messages"][1]["content"]
        .as_array()
        .unwrap();
    assert!(!content.iter().any(|part| {
        matches!(
            part["type"].as_str(),
            Some("thinking" | "redacted_thinking")
        )
    }));
    drop(requests);
    cleanup(harness).await;
}

#[tokio::test]
async fn incompatible_anthropic_fallback_obeys_reject_policy() {
    let harness = setup("reject", "claude-opus-5", false).await;
    let raw = serde_json::to_string(&continuation_request()).unwrap();
    let mut request =
        kinetix::frontends::anthropic::decode_request(continuation_request()).unwrap();
    request.raw_body = Some(raw);
    let result = pipeline::run(
        &harness.state,
        FrontendFormat::Anthropic,
        None,
        request,
        "req_anthropic_reject".into(),
        true,
        None,
        vec![],
    )
    .await;
    assert!(
        result.is_err(),
        "reject policy must stop before fallback dispatch"
    );
    assert_eq!(
        harness.mock.requests.lock().await.len(),
        0,
        "reject policy must fail closed before dispatch when historical provenance is unknown"
    );
    cleanup(harness).await;
}
