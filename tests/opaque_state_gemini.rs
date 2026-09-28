//! Integration coverage for Gemini opaque `thoughtSignature` replay through a
//! translated frontend (OpenAI Chat Completions). The mock upstream fails
//! closed exactly like the real Gemini API: once a signed function call is
//! continued, every historical function-call part must carry its signature.
//! Anything less returns the provider's real "Function call is missing a
//! thought_signature" 400. That makes every passing assertion below evidence
//! that Kinetix really captured, persisted, and replayed the signature rather
//! than accidentally satisfying the mock.
//!
//! It is also model-strict: a signature captured on one Gemini model must
//! never be replayed onto another, and a cross-model continuation must use the
//! documented placeholder instead of a real-but-foreign signature.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::StatusCode,
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
    opaque_state::{OpaqueClientScope, OpaqueStateKind, OpaqueStateTarget},
    paths::Paths,
    pipeline,
    registry::Registry,
    types::{AuthScheme, InternalRequest, Message, Part, Role, WireFormat},
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const SIGNATURE: &str = "MOCK_GEMINI_SIGNATURE";
/// The signature the `gemini-3-mock-pro` model produces. Used to prove that a
/// Flash signature is never replayed onto Pro (and vice-versa).
const SIGNATURE_PRO: &str = "MOCK_GEMINI_SIGNATURE_PRO";
/// The signature the `gemini-2.5-pro` model produces. Gemini 2.5 still emits a
/// signature, but treats it as optional when the history is replayed.
const SIGNATURE_25: &str = "MOCK_GEMINI_SIGNATURE_25";
/// Google's documented placeholder for a historical function call whose real
/// signature this model cannot carry (a trace transferred from another model).
/// The mechanism is a Gemini 3 `generateContent` feature.
const GEMINI_PLACEHOLDER: &str = "skip_thought_signature_validator";
const TOOL_CALL_ID: &str = "call_mock_1";
const TOOL_NAME: &str = "get_weather";

/// The exact signature a given upstream model is allowed to receive.
fn expected_signature(model: &str) -> &'static str {
    if model.starts_with("gemini-2.5") {
        SIGNATURE_25
    } else if model.ends_with("pro") {
        SIGNATURE_PRO
    } else {
        SIGNATURE
    }
}

/// Whether this model enforces a thought signature on a replayed
/// `functionCall`. Only Gemini 3 does; Gemini 2.5 and older accept an unsigned
/// historical call, and never document the Gemini 3 validator-bypass sentinel.
/// Later majors are not assumed to inherit the Gemini 3 contract.
fn signature_required(model: &str) -> bool {
    let Some((_, rest)) = model.rsplit_once("gemini-") else {
        return false;
    };
    let major: String = rest.chars().take_while(char::is_ascii_digit).collect();
    major.parse::<u32>().is_ok_and(|version| version == 3)
}

/// How the mock classified a continuation request. `Invalid` is exactly what
/// real Gemini rejects with HTTP 400.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Continuation {
    Unrelated,
    /// Every historical function call carried this model's exact signature.
    Signed,
    /// Every call carried the documented cross-model placeholder.
    Placeholder,
    /// The call arrived unsigned on a model that does not require a signature
    /// (Gemini 2.5 and older), which is accepted.
    Unsigned,
    /// A call was unsigned on a model that requires it, carried a signature
    /// that does not belong to this model, or carried a placeholder this model
    /// does not document.
    Invalid,
}

#[derive(Clone, Default)]
struct MockUpstream {
    requests: Arc<Mutex<Vec<Value>>>,
    /// Continuations that arrived with the expected signature on every
    /// function-call part.
    signed_continuations: Arc<AtomicUsize>,
    /// Continuations that arrived with the documented placeholder instead of a
    /// real signature (cross-model transfer).
    placeholder_continuations: Arc<AtomicUsize>,
    /// Continuations that arrived unsigned, with a foreign model's signature,
    /// or with no function-call part at all.
    unsigned_continuations: Arc<AtomicUsize>,
}

impl MockUpstream {
    async fn record(&self, model: &str, body: &Value) -> Continuation {
        self.requests.lock().await.push(body.clone());
        if !contains_key(body, "functionResponse") {
            return Continuation::Unrelated;
        }
        let expected = expected_signature(model);
        let required = signature_required(model);
        let calls = function_call_parts(body);
        let mut placeholder = false;
        let mut unsigned = false;
        let mut foreign = calls.is_empty();
        for part in &calls {
            match part.get("thoughtSignature").and_then(Value::as_str) {
                Some(sig) if sig == expected => {}
                Some(GEMINI_PLACEHOLDER) => placeholder = true,
                Some(_) => {
                    foreign = true;
                    break;
                }
                None => unsigned = true,
            }
        }
        if foreign {
            self.unsigned_continuations.fetch_add(1, Ordering::SeqCst);
            return Continuation::Invalid;
        }
        if placeholder {
            // Counted either way, so a regression that wrongly injects the
            // Gemini 3 sentinel into an older model is visible in the tally.
            self.placeholder_continuations
                .fetch_add(1, Ordering::SeqCst);
            return if required {
                Continuation::Placeholder
            } else {
                Continuation::Invalid
            };
        }
        if unsigned {
            self.unsigned_continuations.fetch_add(1, Ordering::SeqCst);
            return if required {
                Continuation::Invalid
            } else {
                Continuation::Unsigned
            };
        }
        self.signed_continuations.fetch_add(1, Ordering::SeqCst);
        Continuation::Signed
    }
}

fn contains_key(value: &Value, name: &str) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(k, v)| k == name || contains_key(v, name)),
        Value::Array(items) => items.iter().any(|item| contains_key(item, name)),
        _ => false,
    }
}

fn function_call_parts(value: &Value) -> Vec<Value> {
    // Return the enclosing part so the sibling `thoughtSignature` is visible.
    let mut found = Vec::new();
    fn walk(node: &Value, found: &mut Vec<Value>) {
        match node {
            Value::Object(map) => {
                for (key, child) in map {
                    if key == "functionCall" && child.is_object() {
                        found.push(Value::Object(map.clone()));
                    }
                    walk(child, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, found)),
            _ => {}
        }
    }
    walk(value, &mut found);
    found
}

/// The upstream model id from a Gemini `.../models/<id>:method` path. The
/// Gemini wire format carries the model in the URL, not the body.
fn path_model(path: &str) -> Option<String> {
    let rest = path.split("/models/").nth(1)?;
    let id = rest.split([':', '?']).next()?;
    (!id.is_empty()).then(|| id.to_string())
}

/// One mock server serves both a Gemini-wire and an OpenAI-wire provider so a
/// Route can mix them and exercise portability across formats/providers.
async fn upstream(
    State(mock): State<MockUpstream>,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> Response {
    let is_gemini = contains_key(&body, "contents");
    // Gemini puts the model in the URL path, so resolve it from there first;
    // falling back to the body keeps the OpenAI-wire path working.
    let model = path_model(uri.path())
        .or_else(|| {
            body.get("model")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let has_response = contains_key(&body, "functionResponse");
    let continuation = mock.record(&model, &body).await;
    if is_gemini && continuation == Continuation::Invalid {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "Function call is missing a thought_signature in functionCall parts. \
                                This is required for tools to work correctly.",
                    "status": "INVALID_ARGUMENT"
                }
            })),
        )
            .into_response();
    }

    let sse = if is_gemini {
        gemini_sse(&model, has_response)
    } else {
        openai_sse()
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

fn gemini_sse(model: &str, has_response: bool) -> String {
    let signature = expected_signature(model);
    let frames = if has_response {
        vec![
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "done"}]}}]}),
            json!({
                "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}
            }),
        ]
    } else {
        vec![
            json!({"candidates": [{"content": {"role": "model", "parts": [{
                "functionCall": {"id": TOOL_CALL_ID, "name": TOOL_NAME, "args": {"city": "Paris"}},
                "thoughtSignature": signature
            }]}}]}),
            json!({
                "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}
            }),
        ]
    };

    let mut sse = String::new();
    for frame in frames {
        sse.push_str("data: ");
        sse.push_str(&frame.to_string());
        sse.push_str("\r\n\r\n");
    }
    sse
}

/// Minimal, valid OpenAI streaming completion. Only reached on the
/// `strip_with_warning` path, where a non-portable continuation is dropped and
/// dispatch must still succeed.
fn openai_sse() -> String {
    let frames = [
        json!({"id":"chatcmpl-mock","object":"chat.completion.chunk","created":1,"model":"openai-mock","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":null}]}),
        json!({"id":"chatcmpl-mock","object":"chat.completion.chunk","created":1,"model":"openai-mock","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ];
    let mut sse = String::new();
    for frame in frames {
        sse.push_str("data: ");
        sse.push_str(&frame.to_string());
        sse.push_str("\n\n");
    }
    sse.push_str("data: [DONE]\n\n");
    sse
}

struct Harness {
    state: AppState,
    pool: db::Pool,
    crypto: Arc<Crypto>,
    registry: Arc<Registry>,
    database_url: String,
    mock: MockUpstream,
    server: tokio::task::JoinHandle<()>,
    root: std::path::PathBuf,
}

async fn build_state(harness: &Harness) -> AppState {
    let config = Arc::new(Config {
        bind: "127.0.0.1:0".into(),
        public_base_url: "http://127.0.0.1".into(),
        database_url: harness.database_url.clone(),
        master_key: [11_u8; 32],
        admin_token: "test-admin".into(),
        cf_access_aud: None,
        cf_access_team_domain: None,
        log_json: false,
        bootstrap_file: None,
        allow_private_upstreams: true,
        allow_insecure_tls: true,
        data_dir: harness.root.join("data"),
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
        paths: Paths {
            config_dir: harness.root.join("config"),
            data_dir: harness.root.join("data"),
            state_dir: harness.root.join("state"),
        },
        generated_admin_password: None,
    });
    AppState::new(
        config,
        harness.pool.clone(),
        harness.registry.clone(),
        harness.crypto.clone(),
        reqwest::Client::new(),
        UsageLogQueue::new(harness.pool.clone(), 16),
        0,
    )
}

async fn setup() -> Harness {
    let mock = MockUpstream::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(post(upstream))
        .with_state(mock.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-opaque-gemini-{}",
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
    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "mock-gemini",
            base_url: &base_url,
            wire_format: WireFormat::Gemini,
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
        "mock-gemini-account",
        &crypto.encrypt("mock-key").unwrap(),
        "mock-key",
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
            upstream_id: "gemini-3-mock",
            display_name: "Gemini Mock",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: json!(["text", "tool_calling"]),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({}),
        },
    )
    .await
    .unwrap();

    // A second Gemini model on the same provider. State captured on one model
    // must never be replayed onto the other; a cross-model continuation must
    // use the documented placeholder instead.
    let pro_model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &provider_id,
            upstream_id: "gemini-3-mock-pro",
            display_name: "Gemini Mock Pro",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: json!(["text", "tool_calling"]),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({}),
        },
    )
    .await
    .unwrap();

    // A Gemini 2.5 model. It predates the Gemini 3 signature-validation
    // semantics: a replayed function call may be unsigned, and the Gemini 3
    // validator-bypass sentinel is not a documented substitute here.
    let legacy_model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &provider_id,
            upstream_id: "gemini-2.5-pro",
            display_name: "Gemini 2.5 Pro",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: json!(["text", "tool_calling"]),
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
            name: "opaque-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "strip_with_warning",
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

    let pro_route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "pro-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "strip_with_warning",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(1),
            max_concurrent_requests: None,
        },
    )
    .await
    .unwrap();
    db::insert_route_target(&pool, &pro_route_id, None, &pro_model_id, 1, 1, "{}", "{}")
        .await
        .unwrap();

    let legacy_route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "legacy-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "strip_with_warning",
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
        &legacy_route_id,
        None,
        &legacy_model_id,
        1,
        1,
        "{}",
        "{}",
    )
    .await
    .unwrap();

    // A second, OpenAI-wire provider behind the same mock. Its model is the
    // target of the portability-bypass regressions below: an OpenAI client
    // whose earlier Gemini turn stored a signature, now routed here.
    let openai_provider_id = db::insert_provider(
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
    db::insert_account(
        &pool,
        &openai_provider_id,
        "mock-openai-account",
        &crypto.encrypt("mock-key").unwrap(),
        "mock-key",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();
    let openai_model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &openai_provider_id,
            upstream_id: "openai-mock",
            display_name: "OpenAI Mock",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: json!(["text", "tool_calling"]),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({}),
        },
    )
    .await
    .unwrap();

    let reject_route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "reject-route",
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
        &reject_route_id,
        None,
        &openai_model_id,
        1,
        1,
        "{}",
        "{}",
    )
    .await
    .unwrap();

    let strip_route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "strip-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "strip_with_warning",
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
        &strip_route_id,
        None,
        &openai_model_id,
        1,
        1,
        "{}",
        "{}",
    )
    .await
    .unwrap();

    let registry = Arc::new(Registry::new());
    registry.reload(&pool).await.unwrap();

    Harness {
        state: AppState::new(
            Arc::new(Config {
                bind: "127.0.0.1:0".into(),
                public_base_url: "http://127.0.0.1".into(),
                database_url: database_url.clone(),
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
            }),
            pool.clone(),
            registry.clone(),
            crypto.clone(),
            reqwest::Client::new(),
            UsageLogQueue::new(pool.clone(), 16),
            0,
        ),
        pool,
        crypto,
        registry,
        database_url,
        mock,
        server,
        root,
    }
}

async fn cleanup(harness: Harness) {
    harness.server.abort();
    let _ = std::fs::remove_dir_all(&harness.root);
}

fn virtual_key(id: &str) -> db::VirtualKeyRow {
    db::VirtualKeyRow {
        id: id.into(),
        key_hash: format!("hash-{id}"),
        name: id.into(),
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
    }
}

fn first_turn(stream: bool) -> InternalRequest {
    first_turn_to("opaque-route", stream)
}

fn first_turn_to(model: &str, stream: bool) -> InternalRequest {
    InternalRequest {
        requested_model: model.into(),
        system: vec![],
        messages: vec![Message {
            role: Role::User,
            parts: vec![Part::Text("what is the weather in Paris?".into())],
        }],
        tools: vec![],
        tool_choice: None,
        tool_choice_name: None,
        params: Default::default(),
        stream,
        include_usage: false,
        thinking: None,
        extra: serde_json::Map::new(),
        raw_body: None,
    }
}

fn second_turn(stream: bool, tool_call_id: &str, tool_name: &str) -> InternalRequest {
    second_turn_to("opaque-route", stream, tool_call_id, tool_name)
}

fn second_turn_to(
    model: &str,
    stream: bool,
    tool_call_id: &str,
    tool_name: &str,
) -> InternalRequest {
    InternalRequest {
        requested_model: model.into(),
        system: vec![],
        messages: vec![
            Message {
                role: Role::User,
                parts: vec![Part::Text("what is the weather in Paris?".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall {
                    id: Some(tool_call_id.into()),
                    name: tool_name.into(),
                    arguments: json!({"city": "Paris"}).to_string(),
                    // The client protocol cannot carry the Gemini signature, so
                    // it always arrives absent; only Kinetix can restore it.
                    signature: None,
                }],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    tool_call_id: tool_call_id.into(),
                    name: Some(tool_name.into()),
                    content: "18C".into(),
                    is_error: false,
                }],
            },
        ],
        tools: vec![],
        tool_choice: None,
        tool_choice_name: None,
        params: Default::default(),
        stream,
        include_usage: false,
        thinking: None,
        extra: serde_json::Map::new(),
        raw_body: None,
    }
}

/// Run one request and fully drain the response body, so any capture that
/// happens while the response is streamed has completed before the caller
/// inspects the mock's counters.
async fn run(
    state: &AppState,
    key: Option<&db::VirtualKeyRow>,
    req: InternalRequest,
    request_id: &str,
) -> Result<u16, String> {
    run_session(state, key, req, request_id, None).await
}

/// Like [`run`], but forwards an explicit client session so session-bound
/// opaque-state identity can be exercised end to end.
async fn run_session(
    state: &AppState,
    key: Option<&db::VirtualKeyRow>,
    req: InternalRequest,
    request_id: &str,
    session: Option<&str>,
) -> Result<u16, String> {
    let response = pipeline::run(
        state,
        FrontendFormat::OpenAi,
        key.cloned(),
        req,
        request_id.into(),
        true,
        session.map(str::to_string),
        vec![],
    )
    .await
    .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;
    Ok(status)
}

/// Like [`run`], but also returns the `x-kinetix-warning` header (used to
/// prove a `strip_with_warning` portability decision actually happened).
async fn run_with_warning(
    state: &AppState,
    key: Option<&db::VirtualKeyRow>,
    req: InternalRequest,
    request_id: &str,
) -> Result<(u16, Option<String>), String> {
    let response = pipeline::run(
        state,
        FrontendFormat::OpenAi,
        key.cloned(),
        req,
        request_id.into(),
        true,
        None,
        vec![],
    )
    .await
    .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let warning = response
        .headers()
        .get("x-kinetix-warning")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;
    Ok((status, warning))
}

#[tokio::test]
async fn translated_tool_signature_is_replayed_after_restart() {
    let harness = setup().await;
    let key = virtual_key("key-restart");

    // Turn 1 captures the signature.
    run(
        &harness.state,
        Some(&key),
        first_turn(false),
        "req_opaque_restart_1",
    )
    .await
    .expect("turn 1 should dispatch and capture the signature");
    assert_eq!(harness.mock.requests.lock().await.len(), 1);
    // Ensure the asynchronous durability write has landed before a fresh
    // process (empty RAM cache) reads it back.
    harness.state.opaque_state.flush().await;

    // A fresh AppState over the same database has an empty RAM cache: this is
    // the process-restart case where only SQLite survives.
    let restarted = build_state(&harness).await;
    let status = run(
        &restarted,
        Some(&key),
        second_turn(false, TOOL_CALL_ID, TOOL_NAME),
        "req_opaque_restart_2",
    )
    .await
    .expect("turn 2 should replay the persisted signature");
    assert_eq!(status, 200);

    assert_eq!(
        harness.mock.signed_continuations.load(Ordering::SeqCst),
        1,
        "the continuation must have reached Gemini with its signature restored"
    );
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        0
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn translated_tool_signature_is_replayed_on_streaming_path() {
    let harness = setup().await;
    let key = virtual_key("key-stream");

    run(
        &harness.state,
        Some(&key),
        first_turn(true),
        "req_opaque_stream_1",
    )
    .await
    .expect("streaming turn 1 should capture the signature");
    run(
        &harness.state,
        Some(&key),
        second_turn(true, TOOL_CALL_ID, TOOL_NAME),
        "req_opaque_stream_2",
    )
    .await
    .expect("streaming turn 2 should replay the signature");

    assert_eq!(harness.mock.signed_continuations.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        0
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn cross_model_flash_to_pro_continuation_uses_documented_placeholder() {
    let harness = setup().await;
    let key = virtual_key("key-flash-pro");

    // Turn 1 runs on the flash model and stores its signature.
    run(
        &harness.state,
        Some(&key),
        first_turn_to("opaque-route", false),
        "req_xmodel_fp_1",
    )
    .await
    .expect("flash turn 1 should capture the signature");
    harness.state.opaque_state.flush().await;

    // Turn 2 is routed to the *pro* model. The stored flash signature is not
    // portable, so Kinetix must translate the historical call with Gemini's
    // documented placeholder instead of stripping it (which the model rejects).
    let (status, warning) = run_with_warning(
        &harness.state,
        Some(&key),
        second_turn_to("pro-route", false, TOOL_CALL_ID, TOOL_NAME),
        "req_xmodel_fp_2",
    )
    .await
    .expect("a cross-model continuation must dispatch with the placeholder");
    assert_eq!(status, 200);
    assert!(
        warning.as_deref().is_some_and(|value| !value.is_empty()),
        "the substituted placeholder must be reported, got {warning:?}"
    );
    assert_eq!(
        harness
            .mock
            .placeholder_continuations
            .load(Ordering::SeqCst),
        1,
        "the pro model must have received the documented placeholder"
    );
    assert_eq!(
        harness.mock.signed_continuations.load(Ordering::SeqCst),
        0,
        "flash's real signature must never be replayed onto pro"
    );
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        0
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn cross_model_pro_to_flash_continuation_uses_documented_placeholder() {
    let harness = setup().await;
    let key = virtual_key("key-pro-flash");

    run(
        &harness.state,
        Some(&key),
        first_turn_to("pro-route", false),
        "req_xmodel_pf_1",
    )
    .await
    .expect("pro turn 1 should capture the signature");
    harness.state.opaque_state.flush().await;

    let (status, warning) = run_with_warning(
        &harness.state,
        Some(&key),
        second_turn_to("opaque-route", false, TOOL_CALL_ID, TOOL_NAME),
        "req_xmodel_pf_2",
    )
    .await
    .expect("a cross-model continuation must dispatch with the placeholder");
    assert_eq!(status, 200);
    assert!(
        warning.as_deref().is_some_and(|value| !value.is_empty()),
        "the substituted placeholder must be reported, got {warning:?}"
    );
    assert_eq!(
        harness
            .mock
            .placeholder_continuations
            .load(Ordering::SeqCst),
        1
    );
    assert_eq!(harness.mock.signed_continuations.load(Ordering::SeqCst), 0);
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        0
    );

    cleanup(harness).await;
}

/// A Gemini 3 -> 2.5 continuation is *not* a translatable cross-model trace:
/// Gemini 2.5 predates the documented signature validation, accepts an
/// unsigned historical call, and never documented the Gemini 3
/// validator-bypass sentinel. Kinetix must strip the incompatible signature
/// without injecting that sentinel.
#[tokio::test]
async fn cross_model_gemini_3_to_2_5_does_not_inject_the_placeholder() {
    let harness = setup().await;
    let key = virtual_key("key-3-to-25");

    run(
        &harness.state,
        Some(&key),
        first_turn_to("opaque-route", false),
        "req_xmodel_25_1",
    )
    .await
    .expect("gemini 3 turn 1 should capture the signature");
    harness.state.opaque_state.flush().await;

    let (status, warning) = run_with_warning(
        &harness.state,
        Some(&key),
        second_turn_to("legacy-route", false, TOOL_CALL_ID, TOOL_NAME),
        "req_xmodel_25_2",
    )
    .await
    .expect("a Gemini 3 -> 2.5 continuation must dispatch after stripping");
    assert_eq!(status, 200);
    assert!(
        warning.as_deref().is_some_and(|value| !value.is_empty()),
        "the portability decision must surface a warning header, got {warning:?}"
    );
    assert_eq!(
        harness
            .mock
            .placeholder_continuations
            .load(Ordering::SeqCst),
        0,
        "the Gemini 3 validator-bypass sentinel must never be injected into Gemini 2.5"
    );
    assert_eq!(
        harness.mock.signed_continuations.load(Ordering::SeqCst),
        0,
        "the Gemini 3 signature must never be replayed onto Gemini 2.5"
    );
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        1,
        "the stripped historical call must reach Gemini 2.5 unsigned"
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn direct_cross_model_switch_uses_documented_placeholder() {
    let harness = setup().await;
    let key = virtual_key("key-direct-xmodel");

    // Turn 1 runs on the flash model through its Route.
    run(
        &harness.state,
        Some(&key),
        first_turn_to("opaque-route", false),
        "req_direct_fp_1",
    )
    .await
    .expect("flash turn 1 should capture the signature");
    harness.state.opaque_state.flush().await;

    // Turn 2 switches to the *pro* model directly (no Route, so no portability
    // policy). A deliberate same-family model change is a documented, valid
    // translation, so the adapter's placeholder must be applied rather than
    // failing the request with the direct-target portability error.
    let (status, warning) = run_with_warning(
        &harness.state,
        Some(&key),
        second_turn_to("gemini-3-mock-pro", false, TOOL_CALL_ID, TOOL_NAME),
        "req_direct_fp_2",
    )
    .await
    .expect("a direct cross-model switch must dispatch with the placeholder");
    assert_eq!(status, 200);
    assert!(
        warning.as_deref().is_some_and(|value| !value.is_empty()),
        "the substituted placeholder must be reported, got {warning:?}"
    );
    assert_eq!(
        harness
            .mock
            .placeholder_continuations
            .load(Ordering::SeqCst),
        1,
        "the direct pro target must have received the documented placeholder"
    );
    assert_eq!(
        harness.mock.signed_continuations.load(Ordering::SeqCst),
        0,
        "flash's real signature must never be replayed onto pro"
    );
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        0
    );

    cleanup(harness).await;
}

/// A direct cross-model switch must still validate tool-call identity before
/// classifying the stored state as transferable: reusing the id with a
/// different tool name is malformed history, not a translatable continuation.
#[tokio::test]
async fn direct_cross_model_switch_with_a_different_tool_name_is_rejected() {
    let harness = setup().await;
    let key = virtual_key("key-direct-name");

    run(
        &harness.state,
        Some(&key),
        first_turn_to("opaque-route", false),
        "req_direct_name_1",
    )
    .await
    .expect("flash turn 1 should capture the signature");
    harness.state.opaque_state.flush().await;
    let requests_before = harness.mock.requests.lock().await.len();

    let error = run(
        &harness.state,
        Some(&key),
        second_turn_to("gemini-3-mock-pro", false, TOOL_CALL_ID, "read_file"),
        "req_direct_name_2",
    )
    .await
    .expect_err("a reused id with a different tool name must be rejected");
    assert!(
        error.contains("different tool name"),
        "unexpected rejection message: {error}"
    );
    assert_eq!(
        harness.mock.requests.lock().await.len(),
        requests_before,
        "the mismatch must be rejected before any upstream dispatch"
    );
    assert_eq!(
        harness
            .mock
            .placeholder_continuations
            .load(Ordering::SeqCst),
        0,
        "a name mismatch must never be translated with a placeholder"
    );

    cleanup(harness).await;
}

/// The same reuse check applies to an explicit session: a row captured under a
/// different session must not be translated with the cross-model placeholder.
#[tokio::test]
async fn direct_cross_model_switch_with_a_different_session_is_not_translated() {
    let harness = setup().await;
    let key = virtual_key("key-direct-session");

    run_session(
        &harness.state,
        Some(&key),
        first_turn_to("opaque-route", false),
        "req_direct_session_1",
        Some("session-a"),
    )
    .await
    .expect("flash turn 1 should capture the signature");
    harness.state.opaque_state.flush().await;

    // Continuing on pro with a different explicit session must not restore the
    // signature. With no stored state to translate, the historical call is
    // sent unsigned and the provider rejects it: the placeholder must not be
    // invented for a stranger's session.
    let error = run_session(
        &harness.state,
        Some(&key),
        second_turn_to("gemini-3-mock-pro", false, TOOL_CALL_ID, TOOL_NAME),
        "req_direct_session_2",
        Some("session-b"),
    )
    .await
    .expect_err("a cross-session continuation must not succeed");
    assert!(
        error.contains("thought_signature"),
        "the unsigned call must reach the provider, got: {error}"
    );
    assert_eq!(
        harness
            .mock
            .placeholder_continuations
            .load(Ordering::SeqCst),
        0,
        "a session mismatch must never be translated with a placeholder"
    );
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        1,
        "the mismatched continuation must have reached Gemini unsigned"
    );

    cleanup(harness).await;
}

/// Two models can share a tool-call id under *different* tool names (a
/// realistic case: the client id space is not scoped per model). The bug this
/// guards: once SQLite holds a row for each model under the same id, a lookup
/// for the exact target model must validate identity against *that model's
/// own row*, not get shadowed by the other model's row sharing the id. Before
/// the fix, filtering rows by tool name/session across *every* row for the id
/// (not just the exact-model one) could drop the exact-model row from the
/// surviving set and fall through to `Incompatible`, letting a reused id on
/// the *exact* model it was captured on receive the cross-model placeholder
/// instead of the correct hard rejection. This only surfaces once RAM no
/// longer holds the answer (restart or eviction), because the RAM cache key
/// is already exact-model and never sees the other row — so the regression
/// captures directly through the store and clears RAM to force the SQLite
/// fallback, then drives the mismatch through the full pipeline.
#[tokio::test]
async fn exact_model_identity_survives_restart_when_another_models_row_shares_the_id() {
    let harness = setup().await;
    let key = virtual_key("key-exact-identity");

    let models = db::list_models(&harness.pool).await.unwrap();
    let flash_model = models
        .iter()
        .find(|m| m.upstream_id == "gemini-3-mock")
        .expect("flash model must exist")
        .clone();
    let pro_model = models
        .iter()
        .find(|m| m.upstream_id == "gemini-3-mock-pro")
        .expect("pro model must exist")
        .clone();
    let flash_target = OpaqueStateTarget {
        kind: OpaqueStateKind::GeminiThoughtSignature,
        provider_id: flash_model.provider_id.clone(),
        family: "gemini".into(),
        producer: "native:gemini:v1".into(),
        model_id: flash_model.upstream_id.clone(),
    };
    let pro_target = OpaqueStateTarget {
        model_id: pro_model.upstream_id.clone(),
        ..flash_target.clone()
    };
    let scope = OpaqueClientScope::for_key(&key.id);

    // Flash captures call_mock_1/get_weather; pro captures the *same*
    // tool-call id under a *different* tool name (a different historical
    // call that merely happens to reuse the id).
    harness.state.opaque_state.capture_tool_signature(
        &scope,
        &flash_target,
        None,
        TOOL_CALL_ID,
        TOOL_NAME,
        SIGNATURE,
    );
    harness.state.opaque_state.capture_tool_signature(
        &scope,
        &pro_target,
        None,
        TOOL_CALL_ID,
        "read_file",
        SIGNATURE_PRO,
    );
    harness.state.opaque_state.flush().await;

    // Simulate a process restart: a fresh `AppState` over the same database
    // has an empty RAM cache, so this lookup can only be answered from the
    // multi-row SQLite scan — the exact bug surface (a hot-RAM lookup is
    // already keyed per exact model and would never see the other row).
    let restarted = build_state(&harness).await;
    assert_eq!(restarted.opaque_state.count_rows().await, 2);

    // Continue on flash (the model that owns the get_weather row) but reuse
    // the id as pipeline history would if the client resent the pro turn's
    // tool name. This must be rejected as a tool-name mismatch against
    // flash's own row — never answered as `Incompatible` (which the pipeline
    // would translate into a placeholder).
    let requests_at_upstream_before = harness.mock.requests.lock().await.len();
    let error = run(
        &restarted,
        Some(&key),
        second_turn_to("opaque-route", false, TOOL_CALL_ID, "read_file"),
        "req_exact_identity_flash_2",
    )
    .await
    .expect_err("a reused id under a foreign tool name must be rejected");
    assert!(
        error.contains("different tool name"),
        "unexpected rejection message: {error}"
    );
    assert_eq!(
        harness.mock.requests.lock().await.len(),
        requests_at_upstream_before,
        "the mismatch must be rejected before any upstream dispatch"
    );
    assert_eq!(
        harness
            .mock
            .placeholder_continuations
            .load(Ordering::SeqCst),
        0,
        "a tool-name mismatch on the exact model must never be translated with a placeholder"
    );

    // Flash's own row, addressed correctly, still restores from SQLite after
    // the restart.
    let status = run(
        &restarted,
        Some(&key),
        second_turn_to("opaque-route", false, TOOL_CALL_ID, TOOL_NAME),
        "req_exact_identity_flash_3",
    )
    .await
    .expect("the exact-model row with the matching tool name must still restore");
    assert_eq!(status, 200);
    assert_eq!(harness.mock.signed_continuations.load(Ordering::SeqCst), 1);

    cleanup(harness).await;
}

#[tokio::test]
async fn cross_scope_replay_is_never_served_to_another_key() {
    let harness = setup().await;
    let owner = virtual_key("key-owner");
    let stranger = virtual_key("key-stranger");

    run(
        &harness.state,
        Some(&owner),
        first_turn(false),
        "req_opaque_scope_1",
    )
    .await
    .expect("owner turn 1 should capture the signature");
    harness.state.opaque_state.flush().await;

    // The stranger reuses the same client-visible tool-call id. It must not be
    // handed the owner's signature.
    let response = run(
        &harness.state,
        Some(&stranger),
        second_turn(false, TOOL_CALL_ID, TOOL_NAME),
        "req_opaque_scope_2",
    )
    .await;
    assert!(
        response.is_err(),
        "a cross-scope continuation must not silently succeed"
    );
    assert_eq!(harness.mock.signed_continuations.load(Ordering::SeqCst), 0);
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        1,
        "the stranger's continuation must have reached Gemini unsigned"
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn reusing_a_tool_call_id_with_a_different_name_is_rejected_before_dispatch() {
    let harness = setup().await;
    let key = virtual_key("key-name-mismatch");

    run(
        &harness.state,
        Some(&key),
        first_turn(false),
        "req_opaque_name_1",
    )
    .await
    .expect("turn 1 should capture the signature");
    assert_eq!(harness.mock.requests.lock().await.len(), 1);

    // Same id, different tool name: the stored signature belongs to
    // `get_weather`, so replaying it onto `read_file` would be wrong.
    let error = run(
        &harness.state,
        Some(&key),
        second_turn(false, TOOL_CALL_ID, "read_file"),
        "req_opaque_name_2",
    )
    .await
    .expect_err("a reused id with a different tool name must be rejected");
    assert!(
        error.contains("different tool name"),
        "unexpected rejection message: {error}"
    );
    assert_eq!(
        harness.mock.requests.lock().await.len(),
        1,
        "the mismatch must be rejected before any upstream dispatch"
    );

    cleanup(harness).await;
}

/// Capture a Gemini signature for `key` and flush it to SQLite. The
/// regressions below then look the state up for an OpenAI target, whose
/// adapter reports no opaque-state capability and therefore reads SQLite
/// directly (never the Gemini-keyed RAM entry).
async fn capture_gemini_signature(harness: &Harness, key: &db::VirtualKeyRow, request_id: &str) {
    run(&harness.state, Some(key), first_turn(false), request_id)
        .await
        .expect("gemini turn 1 should capture the signature");
    harness.state.opaque_state.flush().await;
}

/// A stored Gemini signature reached a *reject* Route whose target is an
/// OpenAI-wire model. Neither `cross_provider` nor `cross_format` is set (the
/// client is OpenAI, the target is OpenAI, and there is no session
/// provenance), but the stored state is still non-portable and the Route's
/// reject policy must apply before dispatch.
#[tokio::test]
async fn stored_nonportable_state_is_rejected_by_route_without_session_provenance() {
    let harness = setup().await;
    let key = virtual_key("key-reject");
    capture_gemini_signature(&harness, &key, "req_reject_1").await;
    let requests_before = harness.mock.requests.lock().await.len();

    let error = run(
        &harness.state,
        Some(&key),
        second_turn_to("reject-route", false, TOOL_CALL_ID, TOOL_NAME),
        "req_reject_2",
    )
    .await
    .expect_err("a reject route must refuse non-portable stored state");
    assert!(
        error.contains("forbids"),
        "unexpected rejection message: {error}"
    );
    assert_eq!(
        harness.mock.requests.lock().await.len(),
        requests_before,
        "rejection must happen before upstream dispatch"
    );

    cleanup(harness).await;
}

/// The same non-portable state on a *strip_with_warning* Route must be
/// dropped with a client-visible warning rather than silently ignored.
#[tokio::test]
async fn stored_nonportable_state_is_stripped_with_warning_without_session_provenance() {
    let harness = setup().await;
    let key = virtual_key("key-strip");
    capture_gemini_signature(&harness, &key, "req_strip_1").await;

    let (status, warning) = run_with_warning(
        &harness.state,
        Some(&key),
        second_turn_to("strip-route", false, TOOL_CALL_ID, TOOL_NAME),
        "req_strip_2",
    )
    .await
    .expect("strip_with_warning must dispatch after dropping the state");
    assert_eq!(status, 200);
    assert!(
        warning.as_deref().is_some_and(|value| !value.is_empty()),
        "the portability decision must surface a warning header, got {warning:?}"
    );

    cleanup(harness).await;
}

/// A direct target (no Route, so no policy) must refuse known non-portable
/// stored state instead of silently dropping it.
#[tokio::test]
async fn stored_nonportable_state_is_rejected_for_direct_target_without_session_provenance() {
    let harness = setup().await;
    let key = virtual_key("key-direct");
    capture_gemini_signature(&harness, &key, "req_direct_1").await;
    let requests_before = harness.mock.requests.lock().await.len();

    let error = run(
        &harness.state,
        Some(&key),
        second_turn_to("openai-mock", false, TOOL_CALL_ID, TOOL_NAME),
        "req_direct_2",
    )
    .await
    .expect_err("a direct target must refuse non-portable stored state");
    assert!(
        error.contains("non-portable"),
        "unexpected rejection message: {error}"
    );
    assert_eq!(
        harness.mock.requests.lock().await.len(),
        requests_before,
        "rejection must happen before upstream dispatch"
    );

    cleanup(harness).await;
}
