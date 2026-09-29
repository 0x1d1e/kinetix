//! Public API surface (virtual-key auth): OpenAI Chat Completions, Anthropic
//! Messages, model listing, and health.

use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::app::AppState;
use crate::auth::{authenticate_virtual_key, extract_virtual_key};
use crate::client_disconnect::ClientDisconnect;
use crate::frontends::{self, FrontendFormat};
use crate::limits;
use crate::pipeline;
use crate::types::ProxyError;

/// Generate a short, human-friendly request id.
fn new_request_id() -> String {
    format!("req_{}", uuid::Uuid::new_v4().simple())
}

/// Session identity for prompt-cache affinity (FR-7.3, FR-7.5).
///
/// We only use an **explicit** session header; Kinetix never guesses a
/// conversation identity when evidence is insufficient. An absent header means
/// "no session", and sticky routing is simply not applied.
fn extract_session(headers: &HeaderMap) -> Option<String> {
    // `x-session-affinity` is included because Pi's OpenAI-compatible client
    // can send it alongside `x-session-id` (observed Pi wire behavior, FR-9.3);
    // it is a stable per-conversation identifier, not a guessed one.
    for name in [
        "x-kinetix-session",
        "x-claude-code-session-id",
        "x-session-id",
        // Pi's `sessionAffinityFormat: "openai"` uses the underscore spelling.
        "session_id",
        "x-conversation-id",
        "x-session-affinity",
    ] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() && v.len() <= 200 {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn extract_protocol_headers(format: FrontendFormat, headers: &HeaderMap) -> Vec<(String, String)> {
    if format != FrontendFormat::Anthropic {
        return Vec::new();
    }
    ["anthropic-version", "anthropic-beta"]
        .into_iter()
        .filter_map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| (name.to_string(), value.to_string()))
        })
        .collect()
}

struct PreCommitRequestGuard {
    state: AppState,
    request_id: String,
    started: std::time::Instant,
    disconnect: Option<ClientDisconnect>,
    finished: bool,
}

impl PreCommitRequestGuard {
    fn new(
        state: AppState,
        request_id: String,
        started: std::time::Instant,
        disconnect: Option<ClientDisconnect>,
    ) -> Self {
        Self {
            state,
            request_id,
            started,
            disconnect,
            finished: false,
        }
    }

    fn finish(&mut self) {
        self.finished = true;
    }

    fn record_cancellation(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.state
            .cancellations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let signal_latency_ms = self
            .disconnect
            .as_ref()
            .map(ClientDisconnect::cancellation_latency_ms)
            .unwrap_or(0);
        self.state
            .cancellation_latency_ms_total
            .fetch_add(signal_latency_ms, std::sync::atomic::Ordering::Relaxed);
        let latency_ms = self.started.elapsed().as_millis() as u64;
        self.state
            .live
            .finish(&self.request_id, "cancelled", latency_ms, None, None);
        self.state.flight.record(
            &self.request_id,
            latency_ms,
            "cancellation_issued",
            "client disconnected before response commit",
        );
    }
}

impl Drop for PreCommitRequestGuard {
    fn drop(&mut self) {
        self.record_cancellation();
    }
}

/// Shared entry for both inbound frontends.
async fn handle(
    state: AppState,
    format: FrontendFormat,
    headers: HeaderMap,
    body: Value,
    raw_body: String,
    client_disconnect: Option<ClientDisconnect>,
) -> Response {
    let request_id = new_request_id();

    // 0. Per-IP abuse limit, before key auth so an unauthenticated flood is
    //    rejected cheaply (NFR-3.6). Fails open when no client IP is known.
    if let Err(retry) = state.ip_limiter.check(limits::client_ip(&headers)) {
        return error_response(
            format,
            &request_id,
            ProxyError::rate_limited("too many requests from this client", Some(retry)),
        );
    }

    // 1. Authenticate the virtual key.
    let presented = match extract_virtual_key(&headers) {
        Some(k) => k,
        None => {
            return error_response(format, &request_id, ProxyError::unauthorized(
                "missing API key. Provide it via 'Authorization: Bearer sk-kinetix-...' or 'x-api-key'.",
            ));
        }
    };
    let key = match authenticate_virtual_key(&state, &presented).await {
        Ok(k) => k,
        Err(e) => return error_response(format, &request_id, e),
    };

    // 2. Decode the request into the internal model.
    let mut req = match frontends::decode(format, body) {
        Ok(r) => r,
        Err(e) => return error_response(format, &request_id, e),
    };
    // Keep the raw body for same-format passthrough (FR-2.7, FR-2.10).
    req.raw_body = Some(raw_body);

    // 3. Enforce per-key IP allowlist (FR-3.4) and limits/budgets.
    if let Err(e) = limits::enforce_ip(&key, limits::client_ip(&headers)) {
        return error_response(format, &request_id, e);
    }
    if let Err(e) = limits::validate(&key, &req.requested_model) {
        return error_response(format, &request_id, e);
    }

    // 4. Run the pipeline.
    let session = extract_session(&headers);
    let protocol_headers = extract_protocol_headers(format, &headers);
    let request_started = std::time::Instant::now();
    if let Some(disconnect) = client_disconnect.as_ref() {
        disconnect.start_monitor();
    }
    let mut request_guard = PreCommitRequestGuard::new(
        state.clone(),
        request_id.clone(),
        request_started,
        client_disconnect.clone(),
    );
    let pipeline_run = pipeline::run(
        &state,
        format,
        Some(key),
        req,
        request_id.clone(),
        true,
        session,
        protocol_headers,
    );
    let pipeline_result = if let Some(disconnect) = client_disconnect {
        tokio::select! {
            biased;
            _ = disconnect.cancelled() => {
                request_guard.record_cancellation();
                Err(ProxyError::internal("client disconnected"))
            }
            result = pipeline_run => result,
        }
    } else {
        pipeline_run.await
    };
    request_guard.finish();
    match pipeline_result {
        Ok(resp) => resp,
        Err(e) => error_response(format, &request_id, e),
    }
}

pub async fn chat_completions(
    State(state): State<AppState>,
    client_disconnect: Option<Extension<ClientDisconnect>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let json: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                FrontendFormat::OpenAi,
                &new_request_id(),
                ProxyError::bad_request(format!("invalid JSON body: {e}")),
            )
        }
    };
    handle(
        state,
        FrontendFormat::OpenAi,
        headers,
        json,
        body,
        client_disconnect.map(|Extension(disconnect)| disconnect),
    )
    .await
}

pub async fn responses(
    State(state): State<AppState>,
    client_disconnect: Option<Extension<ClientDisconnect>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let json: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                FrontendFormat::OpenAiResponses,
                &new_request_id(),
                ProxyError::bad_request(format!("invalid JSON body: {e}")),
            )
        }
    };
    handle(
        state,
        FrontendFormat::OpenAiResponses,
        headers,
        json,
        body,
        client_disconnect.map(|Extension(disconnect)| disconnect),
    )
    .await
}

pub async fn messages(
    State(state): State<AppState>,
    client_disconnect: Option<Extension<ClientDisconnect>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let json: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                FrontendFormat::Anthropic,
                &new_request_id(),
                ProxyError::bad_request(format!("invalid JSON body: {e}")),
            )
        }
    };
    handle(
        state,
        FrontendFormat::Anthropic,
        headers,
        json,
        body,
        client_disconnect.map(|Extension(disconnect)| disconnect),
    )
    .await
}

pub async fn count_message_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let request_id = new_request_id();

    if let Err(retry) = state.ip_limiter.check(limits::client_ip(&headers)) {
        return error_response(
            FrontendFormat::Anthropic,
            &request_id,
            ProxyError::rate_limited("too many requests from this client", Some(retry)),
        );
    }

    let presented = match extract_virtual_key(&headers) {
        Some(key) => key,
        None => {
            return error_response(
                FrontendFormat::Anthropic,
                &request_id,
                ProxyError::unauthorized(
                    "missing API key. Provide it via 'Authorization: Bearer sk-kinetix-...' or 'x-api-key'.",
                ),
            )
        }
    };
    let key = match authenticate_virtual_key(&state, &presented).await {
        Ok(key) => key,
        Err(error) => return error_response(FrontendFormat::Anthropic, &request_id, error),
    };

    let json: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(error) => {
            return error_response(
                FrontendFormat::Anthropic,
                &request_id,
                ProxyError::bad_request(format!("invalid JSON body: {error}")),
            )
        }
    };
    let mut req = match frontends::decode(FrontendFormat::Anthropic, json) {
        Ok(req) => req,
        Err(error) => return error_response(FrontendFormat::Anthropic, &request_id, error),
    };
    req.raw_body = Some(body);

    if let Err(error) = limits::enforce_ip(&key, limits::client_ip(&headers)) {
        return error_response(FrontendFormat::Anthropic, &request_id, error);
    }
    if let Err(error) =
        limits::check_current(&state.admission, &state.pool, &key, &req.requested_model).await
    {
        return error_response(FrontendFormat::Anthropic, &request_id, error);
    }

    let protocol_headers = extract_protocol_headers(FrontendFormat::Anthropic, &headers);
    match pipeline::count_tokens(&state, &key, &req, &request_id, &protocol_headers).await {
        Ok(result) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-request-id", &request_id)
            .header(
                "x-kinetix-token-count",
                if result.exact { "exact" } else { "estimated" },
            )
            .body(Body::from(
                serde_json::json!({ "input_tokens": result.input_tokens }).to_string(),
            ))
            .unwrap_or_else(|_| Response::new(Body::from("internal error"))),
        Err(error) => error_response(FrontendFormat::Anthropic, &request_id, error),
    }
}

/// `GET /v1/models`. The response shape is chosen by the client's auth style so
/// both OpenAI and Anthropic clients can discover models (FR-1.2, FR-10.10).
pub async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(retry) = state.ip_limiter.check(limits::client_ip(&headers)) {
        return error_response(
            FrontendFormat::OpenAi,
            &new_request_id(),
            ProxyError::rate_limited("too many requests from this client", Some(retry)),
        );
    }
    let format = if headers.contains_key("x-api-key")
        || headers
            .get("anthropic-version")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    {
        FrontendFormat::Anthropic
    } else {
        FrontendFormat::OpenAi
    };

    // Authentication is required so the list can be scoped to the key.
    let presented = match extract_virtual_key(&headers) {
        Some(k) => k,
        None => {
            return error_response(
                format,
                &new_request_id(),
                ProxyError::unauthorized("missing API key"),
            )
        }
    };
    let key = match authenticate_virtual_key(&state, &presented).await {
        Ok(k) => k,
        Err(e) => return error_response(format, &new_request_id(), e),
    };
    if let Err(e) = limits::enforce_ip(&key, limits::client_ip(&headers)) {
        return error_response(format, &new_request_id(), e);
    }

    let body = frontends::models::models_body(
        format,
        &state.registry,
        &key.allowed_models(),
        &key.allowed_providers(),
    );
    Json(body).into_response()
}

pub async fn healthz(State(state): State<AppState>) -> Response {
    // Lightweight process + database check (Monitoring section).
    let db_ok = sqlx::query("SELECT 1").fetch_one(&state.pool).await.is_ok();
    // The data plane serves inference from an immutable in-memory snapshot, so
    // it remains serviceable even when the control-plane database is briefly
    // unavailable (NFR-2.7). `/healthz` therefore reports the DATA PLANE as the
    // external probe signal and must not drop out of rotation while inference
    // still works; control-plane degradation is surfaced in the body and by the
    // `kinetix_control_plane_degraded` metric instead of a 503.
    let status = StatusCode::OK;
    let body = serde_json::json!({
        "status": "ok",
        "uptime_secs": state.uptime_secs(),
        "database": if db_ok { "ok" } else { "unavailable" },
        // Distinguish data-plane serviceability from degraded control-plane
        // state (Monitoring section). The data plane serves from an in-memory
        // snapshot, so it stays up even if the database is briefly unavailable.
        "data_plane": "serving",
        "control_plane": if db_ok { "ok" } else { "degraded" },
    });
    (status, Json(body)).into_response()
}

/// Build a format-correct error response with the standard Kinetix headers.
pub fn error_response(format: FrontendFormat, request_id: &str, err: ProxyError) -> Response {
    let status =
        StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = err
        .body_override
        .clone()
        .unwrap_or_else(|| frontends::models::error_body(format, &err));

    let mut builder = Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("x-request-id", request_id);
    let has_retry_after = err
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("retry-after"));
    if let Some(retry) = err.retry_after_secs {
        if !has_retry_after {
            builder = builder.header("retry-after", retry.to_string());
        }
    }
    for (k, v) in &err.headers {
        builder = builder.header(k, v);
    }
    builder
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from("internal error")))
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn claude_code_session_header_drives_affinity() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "session-claude-code-1".parse().unwrap(),
        );
        assert_eq!(
            extract_session(&headers).as_deref(),
            Some("session-claude-code-1")
        );
    }

    #[test]
    fn only_safe_anthropic_protocol_headers_are_forwarded() {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers.insert(
            "anthropic-beta",
            "prompt-caching-2024-07-31".parse().unwrap(),
        );
        headers.insert(
            "x-claude-code-session-id",
            "session-secret-ish-context".parse().unwrap(),
        );
        headers.insert("authorization", "Bearer client-secret".parse().unwrap());

        let forwarded = extract_protocol_headers(FrontendFormat::Anthropic, &headers);
        assert_eq!(
            forwarded,
            vec![
                ("anthropic-version".into(), "2023-06-01".into()),
                ("anthropic-beta".into(), "prompt-caching-2024-07-31".into()),
            ]
        );
        assert!(extract_protocol_headers(FrontendFormat::OpenAi, &headers).is_empty());
    }
}
