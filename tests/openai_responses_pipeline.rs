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
use tokio::sync::{Mutex, Notify};

const CLIENT_KEY: &str = "sk-kinetix-responses-pipeline-test";
const RPM_DISCONNECT_KEY: &str = "sk-kinetix-disconnect-rpm-test";
const TPM_DISCONNECT_KEY: &str = "sk-kinetix-disconnect-tpm-test";
const BUDGET_DISCONNECT_KEY: &str = "sk-kinetix-disconnect-budget-test";
const UPSTREAM_MODEL: &str = "upstream-responses-model";

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: Method,
    path: String,
    body: Value,
    authorization: Option<String>,
}

#[derive(Clone, Default)]
struct MockUpstream {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    stale_auth_started: Arc<Notify>,
    release_stale_auth: Arc<Notify>,
}

async fn upstream(
    State(mock): State<MockUpstream>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
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
    let upstream_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    mock.requests.lock().await.push(CapturedRequest {
        method,
        path: uri.path().to_string(),
        body,
        authorization: headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    });
    let attempt_number = mock
        .requests
        .lock()
        .await
        .iter()
        .filter(|request| {
            request
                .body
                .to_string()
                .contains(&format!("case:{test_case}"))
        })
        .count();

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
        "stale_auth" => {
            if headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                == Some("Bearer previous")
            {
                mock.stale_auth_started.notify_one();
                mock.release_stale_auth.notified().await;
                Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"error":{"message":"invalid credential"}}"#))
                    .unwrap()
            } else {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "id": "resp_rotated_credential",
                            "status": "completed",
                            "output": [{
                                "type": "message",
                                "content": [{"type": "output_text", "text": "rotated"}]
                            }],
                            "usage": {"input_tokens": 1, "output_tokens": 1}
                        })
                        .to_string(),
                    ))
                    .unwrap()
            }
        }
        "probe_race" => {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "id": "resp_probe_race",
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "content": [{"type": "output_text", "text": "probe recovered"}]
                        }],
                        "usage": {"input_tokens": 1, "output_tokens": 1}
                    })
                    .to_string(),
                ))
                .unwrap()
        }
        "half_open_account_stream_timeout" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(async_stream::stream! {
                yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(
                    b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"visible probe event\"}\n\n"
                ));
                tokio::time::sleep(Duration::from_millis(2_500)).await;
            }))
            .unwrap(),
        "timeout" => {
            tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap()
        }
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
        "hidden_reasoning_fallback" if upstream_model == "upstream-anthropic-model" => {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(concat!(
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"private reasoning\"}}\n\n",
                    "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"temporarily unavailable\"}}\n\n"
                )))
                .unwrap()
        }
        "unknown_precommit_fallback" if upstream_model == "upstream-anthropic-model" => {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from("data: {\"type\":\"message_start\""))
                .unwrap()
        }
        "unknown_precommit_fallback" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fallback answer\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n"
            )))
            .unwrap(),
        "terminal_http_503" | "local_skip_http_503" => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"error":{"message":"temporarily unavailable"}}"#))
            .unwrap(),
        "terminal_http_429" | "local_skip_http_429" => Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limit exceeded"}}"#,
            ))
            .unwrap(),
        "terminal_http_quota_429" => Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"type":"error","error":{"type":"billing_error","message":"quota exhausted"}}"#,
            ))
            .unwrap(),
        "terminal_http_400" => Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"invalid request"}}"#,
            ))
            .unwrap(),
        "terminal_http_403" => Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"type":"error","error":{"type":"permission_error","message":"model forbidden"}}"#,
            ))
            .unwrap(),
        "terminal_http_404" => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"type":"error","error":{"type":"not_found_error","message":"model not found"}}"#,
            ))
            .unwrap(),
        "price_lookup_cancel" if upstream_model == "upstream-anthropic-model" => {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(concat!(
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":13}}}\n\n",
                    "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7},\"delta\":{}}\n\n",
                    "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"temporarily unavailable\"}}\n\n"
                )))
                .unwrap()
        }
        "known_partial_then_cancel"
            if upstream_model == "upstream-anthropic-model" && attempt_number == 1 => {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(concat!(
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":13}}}\n\n",
                    "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7},\"delta\":{}}\n\n",
                    "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"temporarily unavailable\"}}\n\n"
                )))
                .unwrap()
        }
        "known_partial_then_cancel" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(async_stream::stream! {
                std::future::pending::<()>().await;
                yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(
                    b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"late\"}\n\n"
                ));
            }))
            .unwrap(),
        "stream_precommit_usage_eof" if upstream_model == "upstream-anthropic-model" => {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":13}}}\n\n"
                ))
                .unwrap()
        }
        "hidden_reasoning_fallback" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fallback answer\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n"
            )))
            .unwrap(),
        "partial_usage_timeout" | "partial_usage_truncated"
            if upstream_model == "upstream-anthropic-model" =>
        {
            let events = concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":13}}}\n\n",
                "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7},\"delta\":{}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial answer\"}}\n\n"
            );
            let body = if test_case == "partial_usage_timeout" {
                Body::from_stream(async_stream::stream! {
                    yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(events.as_bytes()));
                    tokio::time::sleep(Duration::from_secs(5)).await;
                })
            } else {
                Body::from(events)
            };
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(body)
                .unwrap()
        }
        "partial_usage_timeout" | "partial_usage_truncated" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fallback answer\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n"
            )))
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

async fn call_responses_with_id(
    state: &AppState,
    model: &str,
    test_case: &str,
) -> (String, StatusCode, String) {
    call_responses_with_id_stream(state, model, test_case, false).await
}

async fn call_responses_with_id_stream(
    state: &AppState,
    model: &str,
    test_case: &str,
    stream: bool,
) -> (String, StatusCode, String) {
    let response = call_raw_responses(
        state,
        json!({"model": model, "input": format!("case:{test_case}"), "stream": stream}).to_string(),
    )
    .await;
    let request_id = response
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        request_id,
        status,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

async fn usage_rows_for_request(
    state: &AppState,
    request_id: &str,
) -> Vec<(
    String,
    i64,
    Option<i64>,
    Option<i64>,
    Option<f64>,
    i64,
    String,
)> {
    sqlx::query_as(
        "SELECT status, status_code, input_tokens, output_tokens, cost_usd, cost_known, commit_state \
         FROM usage_logs WHERE request_id = ? ORDER BY ts ASC",
    )
    .bind(request_id)
    .fetch_all(&state.pool)
    .await
    .unwrap()
}

async fn usage_attempt_rows_for_request(
    state: &AppState,
    request_id: &str,
) -> Vec<(
    i64,
    String,
    Option<i64>,
    Option<i64>,
    Option<f64>,
    i64,
    String,
)> {
    sqlx::query_as(
        "SELECT attempt_number, status, input_tokens, output_tokens, cost_usd, cost_known, commit_state \
         FROM usage_attempts WHERE request_id = ? ORDER BY attempt_number",
    )
    .bind(request_id)
    .fetch_all(&state.pool)
    .await
    .unwrap()
}

async fn cancellation_trace_count(state: &AppState, route_name: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM route_traces WHERE route_name = ? AND stream_outcome = 'client_cancelled'",
    )
    .bind(route_name)
    .fetch_one(&state.pool)
    .await
    .unwrap()
}

async fn wait_for_usage_rows(
    state: &AppState,
    request_id: &str,
    expected: usize,
) -> Vec<(
    String,
    i64,
    Option<i64>,
    Option<i64>,
    Option<f64>,
    i64,
    String,
)> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = usage_rows_for_request(state, request_id).await;
            if rows.len() >= expected {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request usage row was not persisted")
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

// The merged end-to-end future exceeds libtest's default thread stack.
#[test]
fn responses_passthrough_policy_refusal_and_incomplete_aggregation_work_end_to_end() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(responses_passthrough_policy_refusal_and_incomplete_aggregation_inner())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn responses_passthrough_policy_refusal_and_incomplete_aggregation_inner() {
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
            prices: json!({"input_per_1m": 3.0, "output_per_1m": 4.0}),
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

    let hidden_reasoning_route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "responses-hidden-reasoning-fallback-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "reject",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(2),
            max_concurrent_requests: None,
        },
    )
    .await
    .unwrap();
    for (model_id, priority) in [(&translated_model_id, 1), (&model_id, 2)] {
        db::insert_route_target(
            &pool,
            &hidden_reasoning_route_id,
            None,
            model_id,
            priority,
            1,
            "{}",
            "{}",
        )
        .await
        .unwrap();
    }
    for (route_name, with_fallback) in [
        ("responses-partial-usage-fallback", true),
        ("responses-partial-usage-terminal", false),
    ] {
        let partial_usage_route_id = db::insert_route(
            &pool,
            &db::NewRoute {
                name: route_name,
                description: "",
                strategy: "priority",
                fallback_triggers: json!({}),
                portability_policy: "reject",
                sticky_routing: false,
                cache_affinity: false,
                max_attempts: Some(if with_fallback { 2 } else { 1 }),
                max_concurrent_requests: None,
            },
        )
        .await
        .unwrap();
        let targets = if with_fallback {
            vec![(&translated_model_id, 1), (&model_id, 2)]
        } else {
            vec![(&translated_model_id, 1)]
        };
        for (target_model_id, priority) in targets {
            db::insert_route_target(
                &pool,
                &partial_usage_route_id,
                None,
                target_model_id,
                priority,
                1,
                "{}",
                "{}",
            )
            .await
            .unwrap();
        }
    }

    let terminal_provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "mock-terminal-accounting",
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
    let terminal_model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &terminal_provider_id,
            upstream_id: UPSTREAM_MODEL,
            display_name: "Terminal accounting model",
            enabled: true,
            context_window: None,
            max_output_tokens: Some(1024),
            capabilities: json!({"text": true}),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({"configured_transport": "openai-responses"}),
        },
    )
    .await
    .unwrap();
    for test_case in [
        "terminal_http_503",
        "terminal_http_429",
        "terminal_http_quota_429",
        "terminal_http_400",
        "terminal_http_403",
        "terminal_http_404",
    ] {
        let account_name = format!("responses-{test_case}-account");
        let account_id = db::insert_account(
            &pool,
            &terminal_provider_id,
            &account_name,
            &crypto.encrypt(&format!("{test_case}-key")).unwrap(),
            &format!("{test_case}-key"),
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        let route_name = format!("responses-{test_case}-route");
        let terminal_route_id = db::insert_route(
            &pool,
            &db::NewRoute {
                name: &route_name,
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
            &terminal_route_id,
            Some(&account_id),
            &terminal_model_id,
            1,
            1,
            "{}",
            "{}",
        )
        .await
        .unwrap();
    }

    let skipped_provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "mock-provider-circuit-skipped",
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
    let skipped_account_id = db::insert_account(
        &pool,
        &skipped_provider_id,
        "locally-skipped-account",
        &crypto.encrypt("locally-skipped-key").unwrap(),
        "locally-skipped-key",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();
    let skipped_model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &skipped_provider_id,
            upstream_id: UPSTREAM_MODEL,
            display_name: "Circuit-skipped model",
            enabled: true,
            context_window: None,
            max_output_tokens: Some(1024),
            capabilities: json!({"text": true}),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({"configured_transport": "openai-responses"}),
        },
    )
    .await
    .unwrap();
    for test_case in ["local_skip_http_429", "local_skip_http_503"] {
        let primary_account_id = db::insert_account(
            &pool,
            &terminal_provider_id,
            &format!("{test_case}-primary-account"),
            &crypto.encrypt(&format!("{test_case}-key")).unwrap(),
            &format!("{test_case}-key"),
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        let route_name = format!("responses-{test_case}-route");
        let route_id = db::insert_route(
            &pool,
            &db::NewRoute {
                name: &route_name,
                description: "",
                strategy: "priority",
                fallback_triggers: json!({}),
                portability_policy: "reject",
                sticky_routing: false,
                cache_affinity: false,
                max_attempts: Some(2),
                max_concurrent_requests: None,
            },
        )
        .await
        .unwrap();
        db::insert_route_target(
            &pool,
            &route_id,
            Some(&primary_account_id),
            &terminal_model_id,
            1,
            1,
            "{}",
            "{}",
        )
        .await
        .unwrap();
        db::insert_route_target(
            &pool,
            &route_id,
            Some(&skipped_account_id),
            &skipped_model_id,
            2,
            1,
            "{}",
            "{}",
        )
        .await
        .unwrap();
    }

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

    for index in 0..2 {
        let attempt = state
            .provider_circuits
            .begin_attempt(
                &skipped_provider_id,
                &format!("seed-account-{index}"),
                &format!("seed-target-{index}"),
            )
            .unwrap();
        attempt.finish_failure(kinetix::types::FailureKind::ServerError, Some(503));
    }
    assert_eq!(
        state.provider_circuits.snapshot(&skipped_provider_id).state,
        kinetix::provider_circuit::ProviderCircuitState::Open
    );
    let skipped_circuit_rejects_before = state
        .provider_circuits
        .snapshot(&skipped_provider_id)
        .rejects;
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
    assert_eq!(status, StatusCode::BAD_REQUEST, "{rejected_top_k}");

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
    let (stream_outcome, terminal_failure_kind, commit_state, fallback_allowed) =
        sqlx::query_as::<_, (Option<String>, Option<String>, String, Option<i64>)>(
            "SELECT stream_outcome, terminal_failure_kind, commit_state, fallback_allowed \
             FROM route_traces WHERE requested_model = ? ORDER BY ts DESC LIMIT 1",
        )
        .bind("responses-translated-route")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(stream_outcome.as_deref(), Some("upstream_clean_eof"));
    assert_eq!(terminal_failure_kind.as_deref(), Some("malformed_upstream"));
    assert_eq!(commit_state, "committed");
    assert_eq!(fallback_allowed, Some(0));
    assert!(failed_event["response"]["parallel_tool_calls"]
        .as_bool()
        .unwrap());
    assert_eq!(
        failed_event["response"]["tool_choice"],
        json!({"type": "function", "name": "weather"})
    );
    assert_eq!(failed_event["response"]["tools"][0]["name"], "weather");

    let (status, hidden_reasoning_fallback) = call_responses(
        &state,
        "responses-hidden-reasoning-fallback-route",
        "hidden_reasoning_fallback",
        true,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hidden_reasoning_fallback}");
    assert!(
        hidden_reasoning_fallback.contains("fallback answer"),
        "retryable failure after hidden reasoning should use the fallback target: {hidden_reasoning_fallback}"
    );
    assert!(!hidden_reasoning_fallback.contains("private reasoning"));

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

    let (status, timeout_body) = call_responses(
        &state,
        "mock-openai-responses/upstream-responses-model",
        "timeout",
        false,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{timeout_body}");

    let requests = mock.requests.lock().await;
    assert_eq!(requests.len(), 14, "captured requests: {requests:?}");
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
        Some(&json!("case:hidden_reasoning_fallback"))
    );
    assert_eq!(requests[11].body["model"], UPSTREAM_MODEL);
    assert_eq!(requests[11].body["input"], "case:hidden_reasoning_fallback");
    assert_eq!(requests[12].method, Method::POST);
    assert_eq!(requests[12].path, "/v1/messages");
    assert_eq!(
        requests[12].body.pointer("/messages/0/content/0/text"),
        Some(&json!("case:translated_full"))
    );
    assert_eq!(requests[13].body["model"], UPSTREAM_MODEL);
    assert_eq!(requests[13].body["input"], "case:timeout");
    drop(requests);

    for (test_case, client_status, request_status, request_status_code) in [
        (
            "terminal_http_503",
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            502,
        ),
        (
            "terminal_http_429",
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            429,
        ),
        (
            "terminal_http_quota_429",
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exhausted",
            429,
        ),
        (
            "terminal_http_400",
            StatusCode::BAD_REQUEST,
            "client_error",
            400,
        ),
        (
            "terminal_http_403",
            StatusCode::FORBIDDEN,
            "client_error",
            403,
        ),
        (
            "terminal_http_404",
            StatusCode::NOT_FOUND,
            "client_error",
            404,
        ),
    ] {
        let route = format!("responses-{test_case}-route");
        let (request_id, status, body) = call_responses_with_id(&state, &route, test_case).await;
        assert_eq!(status, client_status, "{test_case}: {body}");
        let rows = wait_for_usage_rows(&state, &request_id, 1).await;
        assert_eq!(rows.len(), 1, "{test_case} needs one request row");
        assert_eq!(
            (rows[0].0.as_str(), rows[0].1),
            (request_status, request_status_code),
            "{test_case} request usage must reflect classified client failure"
        );
        let attempts = usage_attempt_rows_for_request(&state, &request_id).await;
        assert_eq!(attempts.len(), 1, "{test_case} needs one attempt row");
        assert_eq!(attempts[0].0, 1);
        assert_eq!(attempts[0].1, "stream_error");
        assert_eq!(attempts[0].6, "pre_commit");
    }

    // Run after terminal-provider cases to avoid sharing their circuit history.
    // The outbound transport retries HTTP 503 once before route fallback.
    for (test_case, expected_response, expected_status, expected_code) in [
        (
            "local_skip_http_429",
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            429,
        ),
        (
            "local_skip_http_503",
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            502,
        ),
    ] {
        let route_name = format!("responses-{test_case}-route");
        let (request_id, response_status, body) =
            call_responses_with_id(&state, &route_name, test_case).await;
        assert_eq!(response_status, expected_response, "{test_case}: {body}");
        let rows = wait_for_usage_rows(&state, &request_id, 1).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].0.as_str(), rows[0].1),
            (expected_status, expected_code),
            "{test_case} must account for the dispatched upstream failure, not the local skip"
        );
        assert_eq!(
            usage_attempt_rows_for_request(&state, &request_id)
                .await
                .len(),
            1,
            "the locally skipped target must not create an attempt row"
        );
        let trace_steps: String =
            sqlx::query_scalar("SELECT steps FROM route_traces WHERE request_id = ?")
                .bind(&request_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert!(
            trace_steps.contains("provider_circuit_open"),
            "{test_case} should record the local circuit rejection: {trace_steps}"
        );
    }
    assert_eq!(
        state
            .provider_circuits
            .snapshot(&skipped_provider_id)
            .rejects,
        skipped_circuit_rejects_before + 2,
        "the fallback candidate should be rejected locally by the open provider circuit"
    );

    let before_unknown_attempt = state.admission.metrics_snapshot();
    let (request_id, status, unknown_fallback) = call_responses_with_id_stream(
        &state,
        "responses-hidden-reasoning-fallback-route",
        "unknown_precommit_fallback",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unknown_fallback}");
    assert!(
        unknown_fallback.contains("fallback answer"),
        "{unknown_fallback}"
    );
    let rows = wait_for_usage_rows(&state, &request_id, 1).await;
    assert_eq!(rows.len(), 1);
    let attempts = usage_attempt_rows_for_request(&state, &request_id).await;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].0, 1);
    assert_eq!(attempts[0].1, "stream_error");
    assert_eq!(
        (attempts[0].2, attempts[0].3, attempts[0].4),
        (None, None, None)
    );
    assert_eq!(attempts[1].1, "success");
    let after_unknown_attempt = state.admission.metrics_snapshot();
    assert_eq!(
        after_unknown_attempt.reconciled_incomplete_total,
        before_unknown_attempt.reconciled_incomplete_total + 1,
        "unknown usage from a dispatched precommit attempt makes aggregate reconciliation incomplete"
    );

    for (test_case, route, expected_status, expected_rows, expected_tokens, fallback) in [
        (
            "partial_usage_timeout",
            "responses-partial-usage-fallback",
            StatusCode::OK,
            1,
            23,
            true,
        ),
        (
            "partial_usage_truncated",
            "responses-partial-usage-fallback",
            StatusCode::OK,
            1,
            23,
            true,
        ),
        (
            "partial_usage_timeout",
            "responses-partial-usage-terminal",
            StatusCode::GATEWAY_TIMEOUT,
            1,
            20,
            false,
        ),
        (
            "partial_usage_truncated",
            "responses-partial-usage-terminal",
            StatusCode::BAD_GATEWAY,
            1,
            20,
            false,
        ),
    ] {
        let before = state.admission.metrics_snapshot();
        let (request_id, status, body) = call_responses_with_id(&state, route, test_case).await;
        assert_eq!(status, expected_status, "{test_case} {route}: {body}");
        if fallback {
            assert!(body.contains("fallback answer"), "{test_case}: {body}");
            assert!(!body.contains("partial answer"), "{test_case}: {body}");
        }
        let rows = wait_for_usage_rows(&state, &request_id, expected_rows).await;
        assert_eq!(rows.len(), 1, "one request-level usage row: {rows:?}");
        let request = &rows[0];
        let attempts = usage_attempt_rows_for_request(&state, &request_id).await;
        assert_eq!(attempts.len(), if fallback { 2 } else { 1 });
        assert_eq!(
            attempts.iter().map(|attempt| attempt.0).collect::<Vec<_>>(),
            if fallback { vec![1, 2] } else { vec![1] },
            "provider attempts need stable, unique numbering"
        );
        let failed = attempts
            .iter()
            .find(|attempt| attempt.1 == "stream_error")
            .unwrap();
        assert_eq!(
            failed.2,
            Some(13),
            "partial input usage must be attributed to the failed attempt"
        );
        assert_eq!(failed.3, Some(7));
        assert_eq!(failed.5, 1, "reported partial usage must be priced");
        assert!((failed.4.unwrap() - 0.000067).abs() < 1e-12);
        assert_eq!(failed.6, "pre_commit");
        if fallback {
            assert_eq!(request.0, "success");
            assert_eq!((request.2, request.3), (Some(14), Some(9)));
            let succeeded = attempts
                .iter()
                .find(|attempt| attempt.1 == "success")
                .unwrap();
            assert_eq!((succeeded.2, succeeded.3), (Some(1), Some(2)));
            let attempts_cost: f64 = attempts.iter().filter_map(|attempt| attempt.4).sum();
            assert_eq!(request.5, 1);
            assert!((request.4.unwrap() - attempts_cost).abs() < 1e-12);
        } else {
            assert_eq!(request.0, "stream_error");
            assert_eq!(
                request.1,
                if test_case.ends_with("timeout") {
                    504
                } else {
                    502
                }
            );
            assert_eq!((request.2, request.3), (Some(13), Some(7)));
            assert!((request.4.unwrap() - 0.000067).abs() < 1e-12);
            assert_eq!(request.6, "pre_commit");
        }
        let after = state.admission.metrics_snapshot();
        assert_eq!(
            after.reconciled_tokens_total,
            before.reconciled_tokens_total + expected_tokens,
            "admission must include all provider attempts for {test_case}"
        );
    }

    let (request_id, status, body) = call_responses_with_id_stream(
        &state,
        "responses-partial-usage-terminal",
        "stream_precommit_usage_eof",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let rows = wait_for_usage_rows(&state, &request_id, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].2, rows[0].3), (Some(13), None));
    assert_eq!(rows[0].6, "pre_commit");
    let attempts = usage_attempt_rows_for_request(&state, &request_id).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!((attempts[0].2, attempts[0].3), (Some(13), None));
    assert_eq!(attempts[0].0, 1);

    // Use a fresh provider circuit and route so earlier test traffic cannot
    // affect this concurrency regression.
    let probe_provider_id = db::insert_provider(
        &state.pool,
        &db::NewProvider {
            name: "mock-openai-probe-race",
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
    let probe_account_id = db::insert_account(
        &state.pool,
        &probe_provider_id,
        "probe-race-account",
        &state.crypto.encrypt("probe-race-key").unwrap(),
        "probe-race-key",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();
    let probe_model_id = db::insert_model(
        &state.pool,
        &db::NewModel {
            provider_id: &probe_provider_id,
            upstream_id: UPSTREAM_MODEL,
            display_name: "Probe race model",
            enabled: true,
            context_window: None,
            max_output_tokens: Some(1024),
            capabilities: json!({"text": true}),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({"configured_transport": "openai-responses"}),
        },
    )
    .await
    .unwrap();
    let probe_route_id = db::insert_route(
        &state.pool,
        &db::NewRoute {
            name: "probe-race-route",
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
        &state.pool,
        &probe_route_id,
        None,
        &probe_model_id,
        1,
        1,
        "{}",
        "{}",
    )
    .await
    .unwrap();
    db::record_account_failure(&state.pool, &probe_account_id, 1, -1)
        .await
        .unwrap();
    state.registry.reload(&state.pool).await.unwrap();

    let (first, second) = tokio::join!(
        call_responses(&state, "probe-race-route", "probe_race", false, json!({})),
        call_responses(&state, "probe-race-route", "probe_race", false, json!({})),
    );
    assert_eq!(
        usize::from(first.0.is_success()) + usize::from(second.0.is_success()),
        1,
        "exactly one concurrent request should win the half-open probe: {first:?}, {second:?}"
    );
    let probe_dispatches = mock
        .requests
        .lock()
        .await
        .iter()
        .filter(|request| request.body["input"] == "case:probe_race")
        .count();
    assert_eq!(probe_dispatches, 1, "only one upstream probe may dispatch");
    let recovered_probe_account = db::get_account(&state.pool, &probe_account_id)
        .await
        .unwrap()
        .unwrap();
    assert!(recovered_probe_account.circuit_open_until.is_none());
    assert_eq!(recovered_probe_account.consecutive_failures, 0);

    // An in-flight request that used a revoked manual key must not disable a
    // replacement credential after the operator rotates it.
    let stale_account_id = db::insert_account(
        &state.pool,
        &provider_id,
        "stale-auth-account",
        &state.crypto.encrypt("previous").unwrap(),
        "previous",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();
    let stale_route_id = db::insert_route(
        &state.pool,
        &db::NewRoute {
            name: "stale-auth-route",
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
        &state.pool,
        &stale_route_id,
        Some(&stale_account_id),
        &model_id,
        1,
        1,
        "{}",
        "{}",
    )
    .await
    .unwrap();
    state.registry.reload(&state.pool).await.unwrap();
    let attempt_account = db::get_account(&state.pool, &stale_account_id)
        .await
        .unwrap()
        .unwrap();
    let stale_request_state = state.clone();
    let stale_request = tokio::spawn(async move {
        call_responses(
            &stale_request_state,
            "stale-auth-route",
            "stale_auth",
            false,
            json!({}),
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        mock.stale_auth_started.notified(),
    )
    .await
    .expect("old-key request should reach the held upstream response");
    let stale_request_capture = mock
        .requests
        .lock()
        .await
        .iter()
        .find(|request| request.body["input"] == "case:stale_auth")
        .cloned()
        .expect("stale-auth request should be captured");
    assert_eq!(
        stale_request_capture.authorization.as_deref(),
        Some("Bearer previous")
    );

    let rotated_body: kinetix::admin::AccountBody = serde_json::from_value(json!({
        "provider_id": provider_id,
        "label": "stale-auth-account",
        "api_key": "replacement",
        "priority": 1,
        "weight": 1,
        "soft_quota_usd": null,
        "quota_type": "none",
        "status": null
    }))
    .unwrap();
    let _ = kinetix::admin::update_account(
        State(state.clone()),
        kinetix::auth::AdminAuth {
            actor: "test-admin".into(),
            token: "test-admin-token".into(),
        },
        axum::extract::Path(stale_account_id.clone()),
        Json(rotated_body),
    )
    .await
    .expect("manual credential rotation should succeed");
    let rotated_account = db::get_account(&state.pool, &stale_account_id)
        .await
        .unwrap()
        .unwrap();
    assert!(rotated_account.account_state_version > attempt_account.account_state_version);
    assert_eq!(rotated_account.status, "healthy");
    assert_eq!(rotated_account.key_mask, crypto::mask_secret("replacement"));

    mock.release_stale_auth.notify_one();
    let _ = stale_request.await.unwrap();
    let after_stale_failure = db::get_account(&state.pool, &stale_account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after_stale_failure.status, "healthy",
        "a 401 from the replaced key must not disable the new credential"
    );
    assert_eq!(
        after_stale_failure.key_mask,
        crypto::mask_secret("replacement")
    );
    assert_eq!(
        state
            .crypto
            .decrypt(&after_stale_failure.secret_enc)
            .unwrap(),
        "replacement"
    );
    let (status, body) =
        call_responses(&state, "stale-auth-route", "stale_auth", false, json!({})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "rotated credential should remain eligible: {body}"
    );
    let rotated_request = mock
        .requests
        .lock()
        .await
        .iter()
        .rfind(|request| request.body["input"] == "case:stale_auth")
        .cloned()
        .expect("replacement credential request should be captured");
    assert_eq!(
        rotated_request.authorization.as_deref(),
        Some("Bearer replacement")
    );
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
        let prior_cancellation_traces = cancellation_trace_count(&state, "responses-route").await;
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
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if cancellation_trace_count(&state, "responses-route").await
                    > prior_cancellation_traces
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("precommit client cancellation Route Trace was not persisted");
        let cancelled_trace: (String, String, String, Option<i64>, Option<String>, String) =
            sqlx::query_as(
                "SELECT route_name, commit_state, outcome, fallback_allowed, stream_outcome, steps \
                 FROM route_traces WHERE route_name = 'responses-route' \
                 AND stream_outcome = 'client_cancelled' ORDER BY ts DESC LIMIT 1",
            )
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(cancelled_trace.0, "responses-route");
        assert_eq!(cancelled_trace.1, "not_committed");
        assert_eq!(cancelled_trace.2, "cancelled");
        assert_eq!(cancelled_trace.3, Some(0));
        assert_eq!(cancelled_trace.4.as_deref(), Some("client_cancelled"));
        let trace_steps: serde_json::Value = serde_json::from_str(&cancelled_trace.5).unwrap();
        assert!(trace_steps.as_array().is_some_and(|steps| {
            steps
                .iter()
                .any(|step| step["stage"] == "stream_termination")
        }));

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

    let before = state.admission.metrics_snapshot();
    let prior_known_partial: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM usage_attempts WHERE route_name = ? AND input_tokens = 13 AND output_tokens = 7",
    )
    .bind("responses-hidden-reasoning-fallback-route")
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let cancellation_input = "case:known_partial_then_cancel";
    let abandoned_client = client.clone();
    let abandoned = tokio::spawn(async move {
        abandoned_client
            .post(format!("http://{gateway_addr}/v1/responses"))
            .header("authorization", format!("Bearer {CLIENT_KEY}"))
            .json(&json!({
                "model": "responses-hidden-reasoning-fallback-route",
                "input": cancellation_input,
                "stream": true
            }))
            .send()
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let requests = mock.requests.lock().await;
            if requests
                .iter()
                .filter(|request| request.body.to_string().contains(cancellation_input))
                .count()
                >= 2
            {
                break;
            }
            drop(requests);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let requests = mock.requests.try_lock().unwrap();
        panic!(
            "fallback attempt was not dispatched; captured: {:?}",
            requests
                .iter()
                .map(|request| (&request.path, &request.body))
                .collect::<Vec<_>>()
        )
    });
    abandoned.abort();
    let _ = abandoned.await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let known_partial: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM usage_attempts WHERE route_name = ? AND input_tokens = 13 AND output_tokens = 7",
            )
            .bind("responses-hidden-reasoning-fallback-route")
            .fetch_one(&state.pool)
            .await
            .unwrap();
            if known_partial > prior_known_partial {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cancelled request accounting bundle was not persisted");
    let cancelled_request_id: String = sqlx::query_scalar(
        "SELECT request_id FROM usage_attempts WHERE route_name = ? AND input_tokens = 13 AND output_tokens = 7 ORDER BY ts DESC LIMIT 1",
    )
    .bind("responses-hidden-reasoning-fallback-route")
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let cancelled_request_rows = usage_rows_for_request(&state, &cancelled_request_id).await;
    assert_eq!(cancelled_request_rows.len(), 1);
    assert_eq!(cancelled_request_rows[0].0, "client_disconnect");
    assert_eq!(cancelled_request_rows[0].1, 499);
    assert_eq!(
        (cancelled_request_rows[0].2, cancelled_request_rows[0].3),
        (None, None),
        "unknown usage from the cancelled active attempt prevents an exact request total"
    );
    let cancelled_attempts = usage_attempt_rows_for_request(&state, &cancelled_request_id).await;
    assert_eq!(cancelled_attempts.len(), 2);
    assert_eq!(cancelled_attempts[0].0, 1);
    assert_eq!(cancelled_attempts[0].1, "stream_error");
    assert_eq!(
        (cancelled_attempts[0].2, cancelled_attempts[0].3),
        (Some(13), Some(7))
    );
    assert_eq!(cancelled_attempts[1].0, 2);
    assert_eq!(cancelled_attempts[1].1, "client_disconnect");
    assert_eq!(
        (cancelled_attempts[1].2, cancelled_attempts[1].3),
        (None, None)
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while state.admission.metrics_snapshot().inflight_inferences != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fallback cancellation did not release concurrency admission promptly");
    let after = state.admission.metrics_snapshot();
    assert_eq!(
        after.reconciled_incomplete_total,
        before.reconciled_incomplete_total + 1,
        "unknown usage from the active fallback attempt must keep reconciliation incomplete"
    );
    assert_eq!(
        after.reconciled_tokens_total, before.reconciled_tokens_total,
        "known usage from an earlier attempt cannot stand in for the cancelled attempt"
    );

    db::update_model_prices(
        &state.pool,
        &translated_model_id,
        &kinetix::types::Prices {
            input_per_1m: Some(101.0),
            output_per_1m: Some(102.0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let price_cancel_account_id = db::insert_account(
        &state.pool,
        &provider_id,
        "price-cancel-account",
        &state.crypto.encrypt("price-cancel-key").unwrap(),
        "price-cancel-key",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();
    let price_cancel_route_id = db::insert_route(
        &state.pool,
        &db::NewRoute {
            name: "price-lookup-cancel-route",
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
        &state.pool,
        &price_cancel_route_id,
        Some(&price_cancel_account_id),
        &translated_model_id,
        1,
        1,
        "{}",
        "{}",
    )
    .await
    .unwrap();
    state.registry.reload(&state.pool).await.unwrap();

    let mut price_lock = state.pool.begin().await.unwrap();
    sqlx::query("UPDATE accounts SET last_error = 'hold price lookup writer' WHERE id = ?")
        .bind(&price_cancel_account_id)
        .execute(&mut *price_lock)
        .await
        .unwrap();
    let price_cancel_client = client.clone();
    let price_cancel_request = tokio::spawn(async move {
        price_cancel_client
            .post(format!("http://{gateway_addr}/v1/responses"))
            .header("authorization", format!("Bearer {CLIENT_KEY}"))
            .json(&json!({
                "model": "price-lookup-cancel-route",
                "input": "case:price_lookup_cancel",
                "stream": true
            }))
            .send()
            .await
    });
    let price_attempt_dispatched = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if mock.requests.lock().await.iter().any(|request| {
                request
                    .body
                    .to_string()
                    .contains("case:price_lookup_cancel")
            }) {
                break true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        price_attempt_dispatched,
        "price lookup cancellation attempt was not dispatched; finished={}, captured={:?}",
        price_cancel_request.is_finished(),
        mock.requests
            .lock()
            .await
            .iter()
            .map(|request| &request.body)
            .collect::<Vec<_>>()
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !price_cancel_request.is_finished(),
        "the held price-version write should keep precommit finalization pending"
    );
    price_cancel_request.abort();
    let _ = price_cancel_request.await;
    price_lock.commit().await.unwrap();

    let price_cancel_request_id = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(request_id) = sqlx::query_scalar::<_, String>(
                "SELECT request_id FROM usage_logs WHERE route_name = ? AND status = 'client_disconnect' ORDER BY ts DESC LIMIT 1",
            )
            .bind("price-lookup-cancel-route")
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            {
                break request_id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cancelled price lookup request accounting was not persisted");
    let price_cancel_rows = usage_rows_for_request(&state, &price_cancel_request_id).await;
    assert_eq!(price_cancel_rows.len(), 1);
    assert_eq!(price_cancel_rows[0].0, "client_disconnect");
    assert_eq!(price_cancel_rows[0].1, 499);
    assert_eq!(
        (price_cancel_rows[0].2, price_cancel_rows[0].3),
        (Some(13), Some(7)),
        "observed partial usage must be staged before awaiting price-version persistence"
    );
    let price_cancel_attempts =
        usage_attempt_rows_for_request(&state, &price_cancel_request_id).await;
    assert_eq!(price_cancel_attempts.len(), 1);
    assert_eq!(price_cancel_attempts[0].1, "stream_error");
    assert_eq!(
        (price_cancel_attempts[0].2, price_cancel_attempts[0].3),
        (Some(13), Some(7))
    );

    let terminal_probe_account_id = db::insert_account(
        &state.pool,
        &provider_id,
        "terminal-probe-account",
        &state.crypto.encrypt("terminal-probe-key").unwrap(),
        "terminal-probe-key",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();
    let terminal_probe_route_id = db::insert_route(
        &state.pool,
        &db::NewRoute {
            name: "terminal-probe-route",
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
        &state.pool,
        &terminal_probe_route_id,
        Some(&terminal_probe_account_id),
        &model_id,
        1,
        1,
        "{}",
        "{}",
    )
    .await
    .unwrap();
    db::record_account_failure(&state.pool, &terminal_probe_account_id, 1, -1)
        .await
        .unwrap();
    state.registry.reload(&state.pool).await.unwrap();

    let (status, body) = call_responses(
        &state,
        "terminal-probe-route",
        "half_open_account_stream_timeout",
        true,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("visible probe event"), "{body}");
    let terminal_probe_account = db::get_account(&state.pool, &terminal_probe_account_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        terminal_probe_account.circuit_open_until.is_some(),
        "a half-open account must not recover before its stream completes: {terminal_probe_account:?}"
    );
    assert_eq!(terminal_probe_account.consecutive_failures, 1);
    assert!(terminal_probe_account.last_probe_at.is_some());
    assert_eq!(
        kinetix::pool::effective_status(&terminal_probe_account),
        kinetix::pool::AccountStatus::CircuitOpen
    );

    gateway_server.abort();
    server.abort();
    let _ = std::fs::remove_dir_all(&root);
}
