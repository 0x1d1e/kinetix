//! End-to-end coverage for validated OpenAI Responses requests dispatched to a
//! native Responses target through the API frontend and streaming pipeline.

use std::{sync::Arc, time::Duration};

use futures::StreamExt;

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header::AUTHORIZATION, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
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
use tokio::sync::Mutex;

const CLIENT_KEY: &str = "sk-kinetix-responses-pipeline-test";
const RPM_DISCONNECT_KEY: &str = "sk-kinetix-disconnect-rpm-test";
const TPM_DISCONNECT_KEY: &str = "sk-kinetix-disconnect-tpm-test";
const BUDGET_DISCONNECT_KEY: &str = "sk-kinetix-disconnect-budget-test";
const UPSTREAM_MODEL: &str = "upstream-responses-model";

#[derive(Clone)]
struct CapturedRequest {
    method: Method,
    path: String,
    body: Value,
}

#[derive(Clone, Default)]
struct MockUpstream {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

async fn upstream(
    State(mock): State<MockUpstream>,
    method: Method,
    uri: Uri,
    Json(body): Json<Value>,
) -> Response {
    let input = body.get("input");
    let test_case = input
        .and_then(Value::as_str)
        .and_then(|input| input.strip_prefix("case:"))
        .or_else(|| {
            input
                .and_then(|input| input.pointer("/0/content/0/text"))
                .and_then(Value::as_str)
                .and_then(|input| input.strip_prefix("case:"))
        })
        .or_else(|| {
            body.pointer("/messages/0/content/0/text")
                .and_then(Value::as_str)
                .and_then(|input| input.strip_prefix("case:"))
        })
        .or_else(|| {
            body.pointer("/messages/0/content")
                .and_then(Value::as_str)
                .and_then(|input| input.strip_prefix("case:"))
        })
        .unwrap_or_default()
        .to_string();
    mock.requests.lock().await.push(CapturedRequest {
        method,
        path: uri.path().to_string(),
        body,
    });

    match test_case.as_str() {
        test_case if test_case == "lease_lifecycle" || test_case.starts_with("constraint_probe") => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"second\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n"
            )))
            .unwrap(),
        delayed_case if delayed_case.starts_with("delayed_first_event") => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(async_stream::stream! {
                tokio::time::sleep(Duration::from_secs(5)).await;
                yield Ok::<_, std::io::Error>(bytes::Bytes::from(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"late\"}\n\n"
                ));
                yield Ok(bytes::Bytes::from(
                    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
                ));
            }))
            .unwrap(),
        "stream_refusal" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"type\":\"response.refusal.delta\",\"delta\":\"I cannot help with that.\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":6}}}\n\n"
            )))
            .unwrap(),
        "translated_midstream_failure" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial translated answer\"}}\n\n",
            ))
            .unwrap(),
        "translated_full" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(concat!(
                "{\"id\":\"msg_translated\",\"type\":\"message\",\"role\":\"assistant\",",
                "\"model\":\"upstream-anthropic-model\",\"content\":[{\"type\":\"text\",\"text\":\"translated answer\"}],",
                "\"stop_reason\":\"end_turn\",\"stop_sequence\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}"
            )))
            .unwrap(),
        "incomplete" | "incomplete_native" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial answer\"}\n\n",
                "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":11,\"output_tokens\":12}}}\n\n"
            )))
            .unwrap(),
        "temp_clamp"
        | "drop_presence_penalty"
        | "route_override"
        | "completion_override"
        | "output_override" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            ))
            .unwrap(),
        "full_refusal" => {
            let response = json!({
                "id": "resp_full_refusal",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "content": [{"type": "refusal", "refusal": "I cannot help with that."}]
                }],
                "usage": {
                    "input_tokens": 7,
                    "output_tokens": 8,
                    "input_tokens_details": {
                        "cached_tokens": 2,
                        "cache_write_tokens": 3
                    },
                    "output_tokens_details": {"reasoning_tokens": 1}
                }
            });
            let refusal_delta = json!({
                "type": "response.refusal.delta",
                "delta": "I cannot help with that."
            });
            let completion = json!({ "type": "response.completed", "response": response });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(format!(
                    "data: {refusal_delta}\n\ndata: {completion}\n\n"
                )))
                .unwrap()
        }
        _ => (StatusCode::BAD_REQUEST, "unexpected test case").into_response(),
    }
}

async fn call_responses(
    state: &AppState,
    model: &str,
    test_case: &str,
    stream: bool,
    fields: Value,
) -> (StatusCode, String) {
    let mut body = json!({
        "model": model,
        "input": format!("case:{test_case}"),
        "stream": stream,
        "stream_options": {"include_obfuscation": false},
        "text": {"format": {"type": "text"}}
    });
    if let Some(fields) = fields.as_object() {
        for (key, value) in fields {
            body[key] = value.clone();
        }
    }
    let raw_body = body.to_string();
    let response = call_raw_responses(state, raw_body).await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn call_raw_responses(state: &AppState, raw_body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap(),
    );
    api::responses(State(state.clone()), None, headers, raw_body).await
}

fn test_key(
    id: &str,
    api_key: &str,
    rpm_limit: Option<i64>,
    tpm_limit: Option<i64>,
    daily_budget: Option<f64>,
) -> db::VirtualKeyRow {
    db::VirtualKeyRow {
        id: id.into(),
        key_hash: crypto::hash_virtual_key(api_key),
        name: id.into(),
        owner: "test".into(),
        tag: String::new(),
        allowed_models: json!(["*"]).to_string(),
        allowed_providers: json!([]).to_string(),
        rpm_limit,
        tpm_limit,
        max_concurrent_requests: None,
        daily_budget,
        monthly_budget: None,
        expires_at: None,
        status: "active".into(),
        allowed_ips: json!([]).to_string(),
        body_logging: 0,
        created_at: db::now_iso(),
        revoked_at: None,
    }
}

async fn call_chat(state: &AppState) -> (StatusCode, String) {
    let raw_body = json!({
        "model": "responses-route",
        "messages": [{"role": "user", "content": "case:incomplete"}],
        "stream": true
    })
    .to_string();
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap(),
    );
    let response = api::chat_completions(State(state.clone()), None, headers, raw_body).await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn responses_passthrough_policy_refusal_and_incomplete_aggregation_work_end_to_end() {
    let mock = MockUpstream::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_capture = mock.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(any(upstream))
                .with_state(server_capture),
        )
        .await
        .unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-responses-pipeline-{}",
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
    let crypto = Arc::new(Crypto::new(&[17_u8; 32]));

    let base_url = format!("http://{addr}/v1");
    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "mock-openai-responses",
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
            upstream_id: UPSTREAM_MODEL,
            display_name: "Responses model",
            enabled: true,
            context_window: None,
            max_output_tokens: Some(1024),
            capabilities: json!({"text": true}),
            prices: json!({"input_per_1m": 1.0, "output_per_1m": 1.0}),
            parameters: json!({
                "temperature": {"supported": true, "min": 0.0, "max": 1.0, "default": 0.25, "policy": "clamp"},
                "top_p": {"supported": true, "min": 0.0, "max": 1.0, "default": 0.8, "policy": "clamp"},
                "top_k": {"supported": false, "policy": "reject"},
                "presence_penalty": {"supported": false, "policy": "drop"},
                "frequency_penalty": {"supported": false, "policy": "reject"}
            }),
            thinking_map: json!({"levels": {"high": {"reasoning.effort": "high"}}}),
            extra_request: json!({}),
            discovery: json!({"configured_transport": "openai-responses"}),
        },
    )
    .await
    .unwrap();
    let route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "responses-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "reject",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(1),
            max_concurrent_requests: Some(1),
        },
    )
    .await
    .unwrap();
    db::insert_route_target(&pool, &route_id, None, &model_id, 1, 1, "{}", "{}")
        .await
        .unwrap();
    for (route_name, param_overrides) in [
        ("responses-override-route", r#"{"max_tokens":512}"#),
        (
            "responses-completion-override-route",
            r#"{"max_completion_tokens":512}"#,
        ),
        (
            "responses-output-override-route",
            r#"{"max_output_tokens":512}"#,
        ),
    ] {
        let override_route_id = db::insert_route(
            &pool,
            &db::NewRoute {
                name: route_name,
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
        db::insert_route_target(
            &pool,
            &override_route_id,
            None,
            &model_id,
            1,
            1,
            "{}",
            param_overrides,
        )
        .await
        .unwrap();
    }
    let translated_model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &provider_id,
            upstream_id: "upstream-anthropic-model",
            display_name: "Translated Anthropic model",
            enabled: true,
            context_window: None,
            max_output_tokens: Some(1024),
            capabilities: json!({"text": true}),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({"configured_transport": "anthropic"}),
        },
    )
    .await
    .unwrap();
    let translated_route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "responses-translated-route",
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
    db::insert_route_target(
        &pool,
        &translated_route_id,
        None,
        &translated_model_id,
        1,
        1,
        "{}",
        "{}",
    )
    .await
    .unwrap();

    db::insert_virtual_key(
        &pool,
        &db::VirtualKeyRow {
            id: "responses-test-key".into(),
            key_hash: crypto::hash_virtual_key(CLIENT_KEY),
            name: "Responses pipeline test".into(),
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
    for (id, api_key, rpm_limit, tpm_limit, daily_budget) in [
        (
            "responses-rpm-disconnect-key",
            RPM_DISCONNECT_KEY,
            Some(1),
            None,
            None,
        ),
        (
            "responses-tpm-disconnect-key",
            TPM_DISCONNECT_KEY,
            None,
            Some(15),
            None,
        ),
        (
            "responses-budget-disconnect-key",
            BUDGET_DISCONNECT_KEY,
            None,
            None,
            Some(0.000015),
        ),
    ] {
        db::insert_virtual_key(
            &pool,
            &test_key(id, api_key, rpm_limit, tpm_limit, daily_budget),
        )
        .await
        .unwrap();
    }

    let registry = Arc::new(Registry::new());
    registry.reload(&pool).await.unwrap();
    let state = AppState::new(
        Arc::new(Config {
            bind: "127.0.0.1:0".into(),
            public_base_url: "http://127.0.0.1".into(),
            database_url,
            master_key: [17_u8; 32],
            admin_token: "test-admin-token".into(),
            cf_access_aud: None,
            cf_access_team_domain: None,
            log_json: false,
            bootstrap_file: None,
            allow_private_upstreams: true,
            allow_insecure_tls: true,
            data_dir: paths.data_dir.clone(),
            shutdown_grace_secs: 1,
            max_inflight_inferences: 1,
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
    );

    let (status, streamed_refusal) = call_responses(
        &state,
        "responses-route",
        "stream_refusal",
        true,
        json!({"max_tokens": 2048, "reasoning_effort": "high"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{streamed_refusal}");
    assert!(streamed_refusal.contains("I cannot help with that."));
    assert!(streamed_refusal.contains("response.completed"));
    assert!(!streamed_refusal.contains("response.failed"));

    let (status, full_refusal) =
        call_responses(&state, "responses-route", "full_refusal", false, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{full_refusal}");
    let full_refusal: Value = serde_json::from_str(&full_refusal).unwrap();
    assert_eq!(full_refusal["status"], "completed");
    assert_eq!(
        full_refusal.pointer("/output/0/content/0/type"),
        Some(&json!("refusal"))
    );
    assert_eq!(
        full_refusal.pointer("/output/0/content/0/refusal"),
        Some(&json!("I cannot help with that."))
    );
    assert_eq!(
        full_refusal.pointer("/usage/input_tokens_details/cached_tokens"),
        Some(&json!(2))
    );
    assert_eq!(
        full_refusal.pointer("/usage/input_tokens_details/cache_write_tokens"),
        Some(&json!(3))
    );
    assert_eq!(
        full_refusal.pointer("/usage/output_tokens_details/reasoning_tokens"),
        Some(&json!(1))
    );
    assert!(full_refusal.pointer("/usage/input_token_details").is_none());
    assert!(full_refusal
        .pointer("/usage/output_token_details")
        .is_none());

    let (status, incomplete_responses) = call_responses(
        &state,
        "responses-route",
        "incomplete_native",
        false,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{incomplete_responses}");
    let incomplete_responses: Value = serde_json::from_str(&incomplete_responses).unwrap();
    assert_eq!(incomplete_responses["status"], "incomplete");
    assert_eq!(
        incomplete_responses.pointer("/incomplete_details/reason"),
        Some(&json!("max_output_tokens"))
    );
    assert_eq!(
        incomplete_responses.pointer("/usage/input_tokens"),
        Some(&json!(11))
    );
    assert_eq!(
        incomplete_responses.pointer("/usage/output_tokens"),
        Some(&json!(12))
    );

    let (status, incomplete) = call_chat(&state).await;
    assert_eq!(status, StatusCode::OK, "{incomplete}");
    assert!(incomplete.contains("partial answer"));
    assert!(
        incomplete.contains("\"finish_reason\":\"length\""),
        "{incomplete}"
    );
    assert!(incomplete.contains("[DONE]"));
    assert!(!incomplete.contains("response.failed"));

    let (status, clamped_temperature) = call_responses(
        &state,
        "responses-route",
        "temp_clamp",
        true,
        json!({"temperature": 2.0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{clamped_temperature}");

    let (status, dropped_presence_penalty) = call_responses(
        &state,
        "responses-route",
        "drop_presence_penalty",
        true,
        json!({"presence_penalty": 0.25}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{dropped_presence_penalty}");

    let (status, rejected_top_k) = call_responses(
        &state,
        "responses-route",
        "rejected_top_k",
        true,
        json!({"top_k": 23}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected_top_k}");

    let (status, route_override) = call_responses(
        &state,
        "responses-override-route",
        "route_override",
        true,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{route_override}");

    let (status, completion_override) = call_responses(
        &state,
        "responses-completion-override-route",
        "completion_override",
        true,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completion_override}");

    let (status, output_override) = call_responses(
        &state,
        "responses-output-override-route",
        "output_override",
        true,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{output_override}");

    let (status, rejected_parameter) = call_responses(
        &state,
        "responses-route",
        "rejected_presence_penalty",
        true,
        json!({"frequency_penalty": 0.5}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{rejected_parameter}");

    let (status, translated_failure) = call_responses(
        &state,
        "responses-translated-route",
        "translated_midstream_failure",
        true,
        json!({
            "tool_choice": {"type": "function", "name": "weather"},
            "tools": [{
                "type": "function",
                "name": "weather",
                "parameters": {"type": "object"}
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{translated_failure}");
    assert!(translated_failure.contains("partial translated answer"));
    let failed_event = translated_failure
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .find(|event| event["type"] == "response.failed")
        .expect("translated mid-stream failure event");
    assert_eq!(failed_event["type"], "response.failed");
    assert_eq!(failed_event["response"]["status"], "failed");
    assert_eq!(failed_event["response"]["error"]["code"], "server_error");
    assert!(failed_event["response"]["parallel_tool_calls"]
        .as_bool()
        .unwrap());
    assert_eq!(
        failed_event["response"]["tool_choice"],
        json!({"type": "function", "name": "weather"})
    );
    assert_eq!(failed_event["response"]["tools"][0]["name"], "weather");

    let (status, translated_full) = call_responses(
        &state,
        "responses-translated-route",
        "translated_full",
        false,
        json!({
            "tool_choice": {"type": "function", "name": "weather"},
            "tools": [{
                "type": "function",
                "name": "weather",
                "parameters": {"type": "object"}
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{translated_full}");
    let translated_full: Value = serde_json::from_str(&translated_full).unwrap();
    assert_eq!(translated_full["status"], "completed");
    assert!(translated_full["parallel_tool_calls"].as_bool().unwrap());
    assert_eq!(
        translated_full["tool_choice"],
        json!({"type": "function", "name": "weather"})
    );
    assert_eq!(translated_full["tools"][0]["name"], "weather");

    let requests = mock.requests.lock().await;
    assert_eq!(requests.len(), 11);
    for (request, test_case) in requests
        .iter()
        .take(2)
        .zip(["stream_refusal", "full_refusal"])
    {
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(request.body["model"], UPSTREAM_MODEL);
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["store"], false);
        assert_eq!(request.body["input"], format!("case:{test_case}"));
        assert_eq!(
            request.body.pointer("/stream_options/include_obfuscation"),
            Some(&json!(false))
        );
        assert_eq!(
            request.body.pointer("/text/format/type"),
            Some(&json!("text"))
        );
    }
    let normalized_request = &requests[0].body;
    assert_eq!(normalized_request["max_output_tokens"], 1024);
    assert!(normalized_request.get("max_tokens").is_none());
    assert_eq!(normalized_request["temperature"], 0.25);
    assert_eq!(normalized_request["top_p"], 0.8);
    assert_eq!(
        normalized_request.pointer("/reasoning/effort"),
        Some(&json!("high"))
    );
    assert!(normalized_request.get("reasoning_effort").is_none());

    assert_eq!(requests[2].body["input"], "case:incomplete_native");
    assert_eq!(
        requests[3].body.pointer("/input/0/content/0/text"),
        Some(&json!("case:incomplete"))
    );
    assert_eq!(requests[4].body["temperature"], 1.0);
    assert!(requests[5].body.get("presence_penalty").is_none());
    for request in &requests[6..9] {
        assert_eq!(request.body["max_output_tokens"], 512);
        assert!(request.body.get("max_tokens").is_none());
        assert!(request.body.get("max_completion_tokens").is_none());
    }
    assert_eq!(requests[7].body["input"], "case:completion_override");
    assert_eq!(requests[8].body["input"], "case:output_override");
    assert_eq!(requests[9].method, Method::POST);
    assert_eq!(requests[9].path, "/v1/messages");
    assert_eq!(
        requests[9].body.pointer("/messages/0/content/0/text"),
        Some(&json!("case:translated_midstream_failure"))
    );
    assert!(requests[9].body.get("input").is_none());
    assert_eq!(requests[10].method, Method::POST);
    assert_eq!(requests[10].path, "/v1/messages");
    assert_eq!(
        requests[10].body.pointer("/messages/0/content/0/text"),
        Some(&json!("case:translated_full"))
    );
    drop(requests);

    let first = call_raw_responses(
        &state,
        json!({
            "model": "responses-route",
            "input": "case:lease_lifecycle",
            "stream": true,
            "stream_options": {"include_obfuscation": false},
            "text": {"format": {"type": "text"}}
        })
        .to_string(),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let request_id = first
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let mut first_body = first.into_body().into_data_stream();
    assert!(!first_body.next().await.unwrap().unwrap().is_empty());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state
                .live
                .snapshot()
                .iter()
                .any(|request| request.request_id == request_id && request.finished)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("upstream stream driver should finish");
    assert_eq!(state.admission.metrics_snapshot().inflight_inferences, 1);

    let second = call_raw_responses(
        &state,
        json!({
            "model": "responses-route",
            "input": "case:lease_lifecycle",
            "stream": true
        })
        .to_string(),
    )
    .await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(second);

    drop(first_body);
    let third = call_raw_responses(
        &state,
        json!({
            "model": "responses-route",
            "input": "case:lease_lifecycle",
            "stream": true
        })
        .to_string(),
    )
    .await;
    assert_eq!(third.status(), StatusCode::OK);
    let _ = to_bytes(third.into_body(), 1024 * 1024).await.unwrap();

    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = gateway_listener.local_addr().unwrap();
    let gateway = kinetix::router::build(state.clone());
    let gateway_server = tokio::spawn(async move {
        axum::serve(
            kinetix::server::DisconnectAwareListener::new(gateway_listener),
            gateway.into_make_service_with_connect_info::<kinetix::server::ClientConnectionInfo>(),
        )
        .await
        .unwrap();
    });

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    for (test_case, api_key, probe_input, rejection) in [
        (
            "delayed_first_event_rpm",
            RPM_DISCONNECT_KEY,
            "case:constraint_probe_rpm",
            "rate limit exceeded: 1 requests per minute",
        ),
        (
            "delayed_first_event_tpm",
            TPM_DISCONNECT_KEY,
            "case:constraint_probe_tpm",
            "token rate limit exceeded",
        ),
        (
            "delayed_first_event_budget",
            BUDGET_DISCONNECT_KEY,
            "case:constraint_probe_budget",
            "daily budget would be exceeded",
        ),
    ] {
        let before = state.admission.metrics_snapshot();
        assert_eq!(before.inflight_inferences, 0);
        let input = format!("case:{test_case}");
        let abandoned_client = client.clone();
        let abandoned_input = input.clone();
        let abandoned = tokio::spawn(async move {
            abandoned_client
                .post(format!("http://{gateway_addr}/v1/responses"))
                .header("authorization", format!("Bearer {api_key}"))
                .json(&json!({
                    "model": "responses-route",
                    "input": abandoned_input,
                    "max_output_tokens": 1,
                    "stream": true
                }))
                .send()
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock
                    .requests
                    .lock()
                    .await
                    .iter()
                    .any(|request| request.body["input"] == input)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("upstream did not receive {test_case}"));
        assert_eq!(state.admission.metrics_snapshot().inflight_inferences, 1);

        abandoned.abort();
        let _ = abandoned.await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.admission.metrics_snapshot().inflight_inferences != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("client disconnect did not release concurrency admission promptly");
        let after = state.admission.metrics_snapshot();
        assert_eq!(after.active_reservations, before.active_reservations);
        assert_eq!(after.dropped_total, before.dropped_total);
        assert_eq!(
            after.reconciled_incomplete_total,
            before.reconciled_incomplete_total + 1,
            "{test_case} admission should reconcile conservatively"
        );

        let constrained = client
            .post(format!("http://{gateway_addr}/v1/responses"))
            .header("authorization", format!("Bearer {api_key}"))
            .json(&json!({
                "model": "responses-route",
                "input": probe_input,
                "max_output_tokens": 1,
                "stream": true
            }))
            .send()
            .await
            .unwrap();
        let status = constrained.status();
        let body = constrained.text().await.unwrap();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{test_case}: {body}");
        assert!(body.contains(rejection), "{test_case}: {body}");
        assert_eq!(state.admission.metrics_snapshot().inflight_inferences, 0);
    }

    gateway_server.abort();
    server.abort();
    let _ = std::fs::remove_dir_all(&root);
}
