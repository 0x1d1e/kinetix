//! Public API surface (virtual-key auth): OpenAI Chat Completions, Anthropic
//! Messages, model listing, and health.

use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Datelike;
use serde_json::Value;

use crate::app::AppState;
use crate::auth::{authenticate_virtual_key, extract_virtual_key};
use crate::client_disconnect::ClientDisconnect;
use crate::db;
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
    cancellation_trace:
        Option<std::sync::Arc<parking_lot::Mutex<Option<crate::trace::RouteTrace>>>>,
    finished: bool,
    trace_persisted: bool,
}

impl PreCommitRequestGuard {
    fn new(
        state: AppState,
        request_id: String,
        started: std::time::Instant,
        disconnect: Option<ClientDisconnect>,
        cancellation_trace: Option<
            std::sync::Arc<parking_lot::Mutex<Option<crate::trace::RouteTrace>>>,
        >,
    ) -> Self {
        Self {
            state,
            request_id,
            started,
            disconnect,
            cancellation_trace,
            finished: false,
            trace_persisted: false,
        }
    }

    fn finish(&mut self) {
        self.finished = true;
        self.trace_persisted = true;
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
        if !self.finished {
            self.record_cancellation();
        }
        if !self.trace_persisted {
            if let Some(snapshot) = self.cancellation_trace.clone() {
                let state = self.state.clone();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        persist_client_cancelled_trace(&state, &snapshot).await;
                    });
                }
            }
        }
    }
}

async fn persist_client_cancelled_trace(
    state: &AppState,
    snapshot: &std::sync::Arc<parking_lot::Mutex<Option<crate::trace::RouteTrace>>>,
) {
    let Some(mut trace) = snapshot.lock().clone() else {
        return;
    };
    trace.stream_termination(
        crate::stream_outcome::StreamTermination::new(
            crate::stream_outcome::StreamOutcome::ClientCancelled,
            crate::stream_outcome::CommitState::PreCommit,
            None,
            None,
        ),
        Some(false),
    );
    trace.finish("cancelled");
    if let Err(error) = crate::db::insert_route_trace(&state.pool, &trace).await {
        tracing::warn!(request_id = %trace.request_id, %error, "failed to persist precommit cancellation trace");
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
    let cancellation_trace = client_disconnect.as_ref().map(|_| {
        std::sync::Arc::new(parking_lot::Mutex::new(Some(
            crate::trace::RouteTrace::new(request_id.clone(), req.requested_model.clone()),
        )))
    });
    let mut request_guard = PreCommitRequestGuard::new(
        state.clone(),
        request_id.clone(),
        request_started,
        client_disconnect.clone(),
        cancellation_trace.clone(),
    );
    let mut pipeline_run = Box::pin(pipeline::run_with_disconnect(
        &state,
        format,
        Some(key),
        req,
        request_id.clone(),
        true,
        session,
        protocol_headers,
        client_disconnect.clone(),
        cancellation_trace.clone(),
    ));
    let pipeline_result = if let Some(disconnect) = client_disconnect {
        tokio::select! {
            biased;
            _ = disconnect.cancelled() => {
                request_guard.record_cancellation();
                if let Some(snapshot) = cancellation_trace.as_ref() {
                    persist_client_cancelled_trace(&state, snapshot).await;
                }
                Err(ProxyError::internal("client disconnected"))
            }
            result = &mut pipeline_run => result,
        }
    } else {
        (&mut pipeline_run).await
    };
    drop(pipeline_run);
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

/// `GET /v1/usage` returns bounded, self-service usage for the authenticated key.
pub async fn client_usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let request_id = new_request_id();
    let format = if headers.contains_key("x-api-key")
        || headers
            .get("anthropic-version")
            .map(|value| !value.is_empty())
            .unwrap_or(false)
    {
        FrontendFormat::Anthropic
    } else {
        FrontendFormat::OpenAi
    };

    if let Err(retry) = state.ip_limiter.check(limits::client_ip(&headers)) {
        return error_response(
            format,
            &request_id,
            ProxyError::rate_limited("too many requests from this client", Some(retry)),
        );
    }
    let presented = match extract_virtual_key(&headers) {
        Some(key) => key,
        None => {
            return error_response(
                format,
                &request_id,
                ProxyError::unauthorized("missing API key"),
            )
        }
    };
    let key = match authenticate_virtual_key(&state, &presented).await {
        Ok(key) => key,
        Err(error) => return error_response(format, &request_id, error),
    };
    if let Err(error) = limits::validate_status(&key) {
        return error_response(format, &request_id, error);
    }
    if let Err(error) = limits::enforce_ip(&key, limits::client_ip(&headers)) {
        return error_response(format, &request_id, error);
    }

    let now = chrono::Utc::now();
    let daily_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_utc();
    let monthly_start = now
        .date_naive()
        .with_day(1)
        .expect("the first day of a month is valid")
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_utc();
    let daily_reset = daily_start + chrono::Duration::days(1);
    let next_month_date = if now.month() == 12 {
        chrono::NaiveDate::from_ymd_opt(now.year() + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(now.year(), now.month() + 1, 1)
    }
    .expect("the next month is a valid date");
    let monthly_reset = next_month_date
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_utc();
    let now_iso = now.to_rfc3339();
    let daily_from = daily_start.to_rfc3339();
    let monthly_from = monthly_start.to_rfc3339();
    let (daily, monthly) = match tokio::try_join!(
        db::client_usage_summary(&state.pool, &key.id, &daily_from, &now_iso),
        db::client_usage_summary(&state.pool, &key.id, &monthly_from, &now_iso),
    ) {
        Ok(summaries) => summaries,
        Err(error) => {
            tracing::error!(%error, "client usage query failed");
            return error_response(
                format,
                &request_id,
                ProxyError::unavailable("usage temporarily unavailable"),
            );
        }
    };

    let remaining = |limit: Option<f64>, summary: &db::ClientUsageSummary| {
        let limit = limit.filter(|value| *value > 0.0)?;
        if summary.unknown_cost_requests > 0 {
            return None;
        }
        Some((limit - summary.known_cost_usd).max(0.0))
    };
    Json(serde_json::json!({
        "periods": {
            "daily": { "from": daily_from, "to": now_iso, "timezone": "UTC" },
            "monthly": { "from": monthly_from, "to": now_iso, "timezone": "UTC" },
        },
        "usage": { "daily": daily, "monthly": monthly },
        "limits": {
            "rpm": key.rpm_limit.filter(|value| *value > 0),
            "tpm": key.tpm_limit.filter(|value| *value > 0),
            "concurrency": key.max_concurrent_requests.filter(|value| *value > 0),
            "daily_budget_usd": key.daily_budget.filter(|value| *value > 0.0),
            "monthly_budget_usd": key.monthly_budget.filter(|value| *value > 0.0),
        },
        "remaining": {
            "daily_budget_usd": remaining(key.daily_budget, &daily),
            "monthly_budget_usd": remaining(key.monthly_budget, &monthly),
        },
        "resets": {
            "daily": daily_reset.to_rfc3339(),
            "monthly": monthly_reset.to_rfc3339(),
        },
        "admission": { "in_flight": state.admission.key_inflight(&key.id) },
    }))
    .into_response()
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

#[cfg(test)]
mod client_usage_tests {
    use super::*;
    use crate::db::{self, UsageLogRow, VirtualKeyRow};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn test_state(tag: &str) -> (AppState, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "kinetix-client-usage-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let paths = crate::paths::Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            state_dir: root.join("state"),
        };
        paths.ensure_dirs().unwrap();
        let database_url = paths.database_url();
        let pool = db::connect(&database_url).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let config = Arc::new(crate::config::Config {
            bind: "127.0.0.1:0".into(),
            public_base_url: "http://127.0.0.1".into(),
            database_url,
            master_key: [42_u8; 32],
            admin_token: "test-admin".into(),
            cf_access_aud: None,
            cf_access_team_domain: None,
            log_json: false,
            bootstrap_file: None,
            allow_private_upstreams: true,
            allow_insecure_tls: true,
            data_dir: paths.data_dir.clone(),
            shutdown_grace_secs: 1,
            max_inflight_inferences: crate::config::DEFAULT_MAX_INFLIGHT_INFERENCES,
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
        let registry = Arc::new(crate::registry::Registry::new());
        registry.reload(&pool).await.unwrap();
        let state = AppState::new(
            config,
            pool.clone(),
            registry,
            Arc::new(crate::crypto::Crypto::new(&[42_u8; 32])),
            reqwest::Client::new(),
            crate::logqueue::UsageLogQueue::new(pool, 16),
            0,
        );
        (state, root)
    }

    fn test_key(id: &str, secret: &str, status: &str) -> VirtualKeyRow {
        VirtualKeyRow {
            id: id.into(),
            key_hash: crate::crypto::hash_virtual_key(secret),
            name: format!("{id} name"),
            owner: String::new(),
            tag: String::new(),
            allowed_models: "[\"*\"]".into(),
            allowed_providers: "[]".into(),
            rpm_limit: Some(20),
            tpm_limit: Some(1000),
            max_concurrent_requests: Some(2),
            daily_budget: Some(5.0),
            monthly_budget: Some(50.0),
            expires_at: None,
            status: status.into(),
            allowed_ips: "[]".into(),
            body_logging: 0,
            created_at: db::now_iso(),
            revoked_at: None,
        }
    }

    fn usage_row(
        key_id: &str,
        input: Option<i64>,
        output: Option<i64>,
        cost: Option<f64>,
    ) -> UsageLogRow {
        UsageLogRow {
            id: uuid::Uuid::new_v4().to_string(),
            request_id: format!("req-{}", uuid::Uuid::new_v4()),
            ts: chrono::Utc::now().to_rfc3339(),
            key_id: Some(key_id.into()),
            key_name: Some("private key name".into()),
            client_format: "openai".into(),
            requested_model: "private-model".into(),
            effective_model: Some("private-model".into()),
            route_id: Some("private-route-id".into()),
            route_name: Some("private-route-name".into()),
            fallback_hops: 0,
            fallback_path: "[]".into(),
            status: if cost.is_some() {
                "success"
            } else {
                "upstream_error"
            }
            .into(),
            status_code: if cost.is_some() { 200 } else { 502 },
            latency_ms: Some(10),
            ttft_ms: None,
            input_tokens: input,
            output_tokens: output,
            cached_tokens: None,
            cache_write_tokens: None,
            thinking_tokens: None,
            cost_usd: cost,
            cost_known: i64::from(cost.is_some()),
            price_version_id: None,
            cache_status: "bypass".into(),
            serving_account_id: Some("private-account-id".into()),
            serving_account: Some("private-account-label".into()),
            serving_provider: Some("private-provider-name".into()),
            upstream_request_id: Some("private-upstream-request-id".into()),
            flagged: 0,
            error_message: None,
            usage_confidence: if input.is_some() && output.is_some() {
                "provider_reported"
            } else {
                "unknown"
            }
            .into(),
            commit_state: "committed".into(),
            retry_count: 0,
            route_trace_id: None,
            opaque_route_id: Some("private-opaque-route-id".into()),
            admission_cost_usd: None,
        }
    }

    async fn request_usage(app: &axum::Router, path: &str, secret: Option<&str>) -> Response {
        let mut request = axum::http::Request::builder().method("GET").uri(path);
        if let Some(secret) = secret {
            request = request.header("authorization", format!("Bearer {secret}"));
        }
        app.clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn response_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn usage_is_key_scoped_and_does_not_expose_routing_topology() {
        let (state, root) = test_state("scope").await;
        let key = test_key("own-key-id", "own-client-key", "active");
        let other = test_key("other-key-id", "other-client-key", "active");
        db::insert_virtual_key(&state.pool, &key).await.unwrap();
        db::insert_virtual_key(&state.pool, &other).await.unwrap();
        db::insert_usage_log(
            &state.pool,
            &usage_row(&key.id, Some(3), Some(4), Some(0.5)),
        )
        .await
        .unwrap();
        db::insert_usage_log(&state.pool, &usage_row(&key.id, None, Some(2), None))
            .await
            .unwrap();
        db::insert_usage_log(
            &state.pool,
            &usage_row(&other.id, Some(100), Some(200), Some(9.0)),
        )
        .await
        .unwrap();
        let _in_flight = state
            .admission
            .reserve_concurrency(Some((&key.id, key.max_concurrent_requests)), None)
            .unwrap();
        let app = crate::router::build(state.clone());

        let response = request_usage(
            &app,
            "/v1/usage?key_id=other-key-id",
            Some("own-client-key"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["limits"]["rpm"], 20);
        assert_eq!(body["limits"]["tpm"], 1000);
        assert_eq!(body["limits"]["concurrency"], 2);
        assert_eq!(body["limits"]["daily_budget_usd"], 5.0);
        assert_eq!(body["limits"]["monthly_budget_usd"], 50.0);
        assert_eq!(body["admission"]["in_flight"], 1);
        for period in ["daily", "monthly"] {
            assert_eq!(body["usage"][period]["requests"], 2);
            assert!(body["usage"][period]["input_tokens"].is_null());
            assert_eq!(body["usage"][period]["output_tokens"], 6);
            assert_eq!(body["usage"][period]["known_cost_usd"], 0.5);
            assert_eq!(body["usage"][period]["unknown_cost_requests"], 1);
            assert_eq!(body["usage"][period]["unknown_usage_requests"], 1);
        }
        assert!(body["remaining"]["daily_budget_usd"].is_null());
        assert!(body["remaining"]["monthly_budget_usd"].is_null());
        assert!(body["periods"]["daily"]["from"].is_string());
        assert!(body["periods"]["monthly"]["from"].is_string());
        assert!(body["resets"]["daily"].is_string());
        assert!(body["resets"]["monthly"].is_string());

        let serialized = body.to_string();
        for private_value in [
            "other-key-id",
            "private key name",
            "private-model",
            "private-route-id",
            "private-route-name",
            "private-account-id",
            "private-account-label",
            "private-provider-name",
            "private-upstream-request-id",
            "private-opaque-route-id",
        ] {
            assert!(
                !serialized.contains(private_value),
                "leaked {private_value}"
            );
        }
        drop(_in_flight);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn usage_shows_full_remaining_budget_when_no_usage_exists() {
        let (state, root) = test_state("empty").await;
        let key = test_key("empty-key", "empty-client-key", "active");
        db::insert_virtual_key(&state.pool, &key).await.unwrap();
        let app = crate::router::build(state.clone());
        let response = request_usage(&app, "/v1/usage", Some("empty-client-key")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        for period in ["daily", "monthly"] {
            assert_eq!(body["usage"][period]["requests"], 0);
            assert_eq!(body["usage"][period]["input_tokens"], 0);
            assert_eq!(body["usage"][period]["output_tokens"], 0);
            assert_eq!(body["usage"][period]["known_cost_usd"], 0.0);
        }
        assert_eq!(body["remaining"]["daily_budget_usd"], 5.0);
        assert_eq!(body["remaining"]["monthly_budget_usd"], 50.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unknown_cost_has_zero_known_subtotal_and_unknown_remaining_budget() {
        let (state, root) = test_state("unknown-cost-only").await;
        let key = test_key("unknown-cost-key", "unknown-cost-client-key", "active");
        db::insert_virtual_key(&state.pool, &key).await.unwrap();
        db::insert_usage_log(&state.pool, &usage_row(&key.id, None, None, None))
            .await
            .unwrap();
        let app = crate::router::build(state.clone());

        let response = request_usage(&app, "/v1/usage", Some("unknown-cost-client-key")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        for period in ["daily", "monthly"] {
            assert_eq!(body["usage"][period]["known_cost_usd"], 0.0);
            assert_eq!(body["usage"][period]["unknown_cost_requests"], 1);
        }
        assert!(body["remaining"]["daily_budget_usd"].is_null());
        assert!(body["remaining"]["monthly_budget_usd"].is_null());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn known_cost_reduces_daily_and_monthly_remaining_budgets() {
        let (state, root) = test_state("known-cost").await;
        let key = test_key("priced-key", "priced-client-key", "active");
        db::insert_virtual_key(&state.pool, &key).await.unwrap();
        db::insert_usage_log(
            &state.pool,
            &usage_row(&key.id, Some(3), Some(4), Some(1.25)),
        )
        .await
        .unwrap();
        let app = crate::router::build(state.clone());

        let response = request_usage(&app, "/v1/usage", Some("priced-client-key")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["remaining"]["daily_budget_usd"], 3.75);
        assert_eq!(body["remaining"]["monthly_budget_usd"], 48.75);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn usage_requires_a_valid_active_unexpired_virtual_key() {
        let (state, root) = test_state("auth").await;
        let revoked = test_key("revoked-key", "revoked-client-key", "revoked");
        let disabled = test_key("disabled-key", "disabled-client-key", "disabled");
        let mut expired = test_key("expired-key", "expired-client-key", "active");
        expired.expires_at = Some((chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339());
        for key in [&revoked, &disabled, &expired] {
            db::insert_virtual_key(&state.pool, key).await.unwrap();
        }
        let app = crate::router::build(state.clone());

        assert_eq!(
            request_usage(&app, "/v1/usage", None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request_usage(&app, "/v1/usage", Some("unknown-client-key"))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request_usage(&app, "/v1/usage", Some("revoked-client-key"))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request_usage(&app, "/v1/usage", Some("expired-client-key"))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request_usage(&app, "/v1/usage", Some("disabled-client-key"))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
