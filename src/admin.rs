//! Admin API (FR-8.4). Every dashboard action is available here; the dashboard
//! is just a client. Protected by `AdminAuth`.

use std::sync::atomic::Ordering;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapters::{
    normalize_plugin_reasoning_capability, normalize_plugin_reasoning_capability_v1,
    normalize_reasoning_capability, parse_plugin_opaque_state_capability, plugin_capability_flags,
    plugin_capability_flags_v1, plugin_identity, plugin_identity_hint,
    plugin_opaque_state_capability, plugin_provider_variant, plugin_reasoning_support,
    reasoning_metadata_declared, thinking_map_for_reasoning_with_wire, ModelCapabilityFlags,
    UpstreamContext,
};
use crate::app::AppState;
use crate::auth::{self, AdminAuth, SESSION_COOKIE};
use crate::crypto;
use crate::db::{self, Pool};
use crate::frontends::FrontendFormat;
use crate::limits;
use crate::pipeline;
use crate::types::{AuthScheme, Prices, ThinkingMap, WireFormat};

type ApiResult = Result<Json<Value>, ApiError>;

#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.into())
    }
    fn not_found(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::NOT_FOUND, msg.into())
    }
    fn internal(e: impl std::fmt::Display) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

// ===========================================================================
// Auth / session
// ===========================================================================

#[derive(Deserialize)]
pub struct LoginBody {
    pub username: Option<String>,
    pub password: String,
}

pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(body): Json<LoginBody>,
) -> Result<(CookieJar, Json<Value>), ApiError> {
    // Admin authentication is a control-plane action: if the store is
    // unavailable it must fail closed, not fall through (NFR-2.7).
    if !db_healthy(&state).await {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "admin authentication unavailable: control plane degraded".into(),
        ));
    }
    if !auth::verify_admin_password(&state, &body.password).await {
        let _ = db::insert_audit(
            &state.pool,
            "unknown",
            "admin_login_failed",
            "system",
            "",
            "Admin Console",
            "Rejected an admin login with an incorrect password.",
        )
        .await;
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid admin password".into(),
        ));
    }
    // Sessions are in-memory with a TTL: a restart drops them all, so a browser
    // must log in again rather than staying signed in forever.
    let token = state.sessions.create();
    let cookie = Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .build();
    let actor = body.username.unwrap_or_else(|| "admin".to_string());
    let _ = db::insert_audit(
        &state.pool,
        &actor,
        "admin_login",
        "system",
        "",
        "Admin Console",
        "Administrator session started.",
    )
    .await;
    Ok((jar.add(cookie), Json(json!({ "ok": true, "user": actor }))))
}

pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> (CookieJar, Json<Value>) {
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "admin_logout",
        "system",
        "",
        "Admin Console",
        "Administrator session ended.",
    )
    .await;
    if let Some(c) = jar.get(SESSION_COOKIE) {
        state.sessions.revoke(c.value());
    }
    let cookie = Cookie::build((SESSION_COOKIE, "")).path("/").build();
    (jar.add(cookie), Json(json!({ "ok": true })))
}

/// `POST /admin/api/password` — change the dashboard password. Requires the
/// current password (so a hijacked session cannot silently rotate it), stores a
/// hash, and revokes every session including the caller's.
#[derive(serde::Deserialize)]
pub struct PasswordBody {
    current_password: String,
    new_password: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<PasswordBody>,
) -> Result<Json<Value>, ApiError> {
    if !auth::verify_admin_password(&state, &body.current_password).await {
        let _ = db::insert_audit(
            &state.pool,
            "admin",
            "admin_password_change_failed",
            "system",
            "",
            "Admin Console",
            "Rejected a password change: current password incorrect.",
        )
        .await;
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "current password is incorrect".into(),
        ));
    }
    if body.new_password.trim().len() < 8 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "new password must be at least 8 characters".into(),
        ));
    }
    auth::set_admin_password(&state, &body.new_password)
        .await
        .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "admin_password_changed",
        "system",
        "",
        "Admin Console",
        "Administrator password changed; all sessions invalidated.",
    )
    .await;
    Ok(Json(
        json!({ "ok": true, "note": "all sessions invalidated; please log in again" }),
    ))
}

pub async fn me(_auth: AdminAuth) -> Json<Value> {
    Json(json!({ "authenticated": true, "user": "admin" }))
}

const PUBLIC_BASE_URL_SETTING: &str = "public_base_url";

fn normalize_public_base_url(value: &str) -> Result<String, ApiError> {
    let value = value.trim().trim_end_matches('/');
    let parsed =
        url::Url::parse(value).map_err(|_| ApiError::bad("public base URL is not a valid URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad("public base URL must use http or https"));
    }
    if parsed.host_str().is_none() {
        return Err(ApiError::bad("public base URL must include a host"));
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ApiError::bad(
            "public base URL must not contain credentials, a query, or a fragment",
        ));
    }
    Ok(value.to_string())
}

async fn effective_public_base_url(state: &AppState) -> Result<String, ApiError> {
    Ok(db::get_setting(&state.pool, PUBLIC_BASE_URL_SETTING)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| state.config.public_base_url.clone()))
}

#[derive(Deserialize)]
pub struct PublicBaseUrlBody {
    pub public_base_url: String,
}

pub async fn get_public_base_url(
    State(state): State<AppState>,
    _auth: AdminAuth,
) -> Result<Json<Value>, ApiError> {
    let configured = db::get_setting(&state.pool, PUBLIC_BASE_URL_SETTING)
        .await
        .map_err(ApiError::internal)?;
    let (value, source) = match configured {
        Some(value) => (value, "dashboard"),
        None => (state.config.public_base_url.clone(), "environment"),
    };
    Ok(Json(json!({
        "public_base_url": value,
        "source": source,
        "environment_default": state.config.public_base_url,
    })))
}

pub async fn update_public_base_url(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<PublicBaseUrlBody>,
) -> Result<Json<Value>, ApiError> {
    let value = normalize_public_base_url(&body.public_base_url)?;
    db::set_setting(&state.pool, PUBLIC_BASE_URL_SETTING, &value)
        .await
        .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "public_base_url_changed",
        "system",
        "",
        "Public Base URL",
        &format!("Public base URL changed to {value}."),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "public_base_url": value,
        "source": "dashboard",
    })))
}

const MODEL_RECONCILIATION_INTERVAL_SETTING: &str = "model_reconciliation_interval_secs";
const MODEL_PRICING_SYNC_INTERVAL_SETTING: &str = "model_pricing_sync_interval_secs";
const MODEL_LIFECYCLE_JITTER_SETTING: &str = "model_lifecycle_jitter_secs";
const MODEL_PROBE_FRESHNESS_SETTING: &str = "model_probe_freshness_secs";

fn provider_discovery_observations_key(provider_id: &str) -> String {
    format!("model_discovery_observations:{provider_id}")
}

fn lifecycle_setting_key(lane: &str, field: &str, provider_id: &str) -> String {
    format!("model_{lane}_{field}:{provider_id}")
}

async fn lifecycle_lane_status(
    pool: &Pool,
    lane: &str,
    provider_id: &str,
) -> Result<Value, ApiError> {
    let last_attempt = db::get_setting(
        pool,
        &lifecycle_setting_key(lane, "last_attempt", provider_id),
    )
    .await
    .map_err(ApiError::internal)?;
    let last_success = db::get_setting(
        pool,
        &lifecycle_setting_key(lane, "last_success", provider_id),
    )
    .await
    .map_err(ApiError::internal)?;
    let last_failure = db::get_setting(
        pool,
        &lifecycle_setting_key(lane, "last_failure", provider_id),
    )
    .await
    .map_err(ApiError::internal)?;
    let last_error = db::get_setting(
        pool,
        &lifecycle_setting_key(lane, "last_error", provider_id),
    )
    .await
    .map_err(ApiError::internal)?
    .filter(|value| !value.is_empty());
    Ok(json!({
        "last_attempt": last_attempt,
        "last_success": last_success,
        "last_failure": last_failure,
        "last_error": last_error,
    }))
}

async fn provider_lifecycle_status(pool: &Pool, provider_id: &str) -> Result<Value, ApiError> {
    Ok(json!({
        "reconciliation": lifecycle_lane_status(pool, "reconciliation", provider_id).await?,
        "pricing_sync": lifecycle_lane_status(pool, "pricing_sync", provider_id).await?,
    }))
}

async fn record_lifecycle_success(
    pool: &Pool,
    lane: &str,
    provider_id: &str,
) -> anyhow::Result<()> {
    db::set_setting(
        pool,
        &lifecycle_setting_key(lane, "last_success", provider_id),
        &db::now_iso(),
    )
    .await?;
    db::set_setting(
        pool,
        &lifecycle_setting_key(lane, "last_error", provider_id),
        "",
    )
    .await?;
    Ok(())
}

async fn record_lifecycle_failure(
    pool: &Pool,
    lane: &str,
    provider_id: &str,
    error: &str,
) -> anyhow::Result<()> {
    db::set_setting(
        pool,
        &lifecycle_setting_key(lane, "last_failure", provider_id),
        &db::now_iso(),
    )
    .await?;
    db::set_setting(
        pool,
        &lifecycle_setting_key(lane, "last_error", provider_id),
        &truncate(&crypto::redact(error), 400),
    )
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ModelLifecycleLane {
    Reconciliation,
    PricingSync,
}

impl ModelLifecycleLane {
    fn name(self) -> &'static str {
        match self {
            Self::Reconciliation => "reconciliation",
            Self::PricingSync => "pricing_sync",
        }
    }
}

async fn run_model_lifecycle_lane(
    state: &AppState,
    provider_id: &str,
    lane: ModelLifecycleLane,
) -> Result<Value, ApiError> {
    let lane_name = lane.name();
    db::set_setting(
        &state.pool,
        &lifecycle_setting_key(lane_name, "last_attempt", provider_id),
        &db::now_iso(),
    )
    .await
    .map_err(ApiError::internal)?;

    let result = match lane {
        ModelLifecycleLane::Reconciliation => reconcile_provider_id(state, provider_id).await,
        ModelLifecycleLane::PricingSync => sync_provider_pricing_id(state, provider_id).await,
    };

    match result {
        Ok(mut payload) => {
            record_lifecycle_success(&state.pool, lane_name, provider_id)
                .await
                .map_err(ApiError::internal)?;
            let lifecycle = provider_lifecycle_status(&state.pool, provider_id).await?;
            if let Some(object) = payload.as_object_mut() {
                object.insert("lifecycle".to_string(), lifecycle);
                Ok(payload)
            } else {
                Ok(json!({
                    "result": payload,
                    "lifecycle": lifecycle,
                }))
            }
        }
        Err(error) => {
            if let Err(record_error) =
                record_lifecycle_failure(&state.pool, lane_name, provider_id, &error.1).await
            {
                tracing::warn!(
                    provider = %provider_id,
                    lane = lane_name,
                    %record_error,
                    "failed to persist model lifecycle failure state"
                );
            }
            Err(error)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ModelLifecycleSettings {
    reconciliation_interval_secs: u64,
    pricing_sync_interval_secs: u64,
    jitter_secs: u64,
    probe_freshness_secs: u64,
}

async fn read_u64_setting(pool: &Pool, key: &str, default: u64) -> Result<u64, ApiError> {
    Ok(db::get_setting(pool, key)
        .await
        .map_err(ApiError::internal)?
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default))
}

async fn model_lifecycle_settings(state: &AppState) -> Result<ModelLifecycleSettings, ApiError> {
    Ok(ModelLifecycleSettings {
        reconciliation_interval_secs: read_u64_setting(
            &state.pool,
            MODEL_RECONCILIATION_INTERVAL_SETTING,
            0,
        )
        .await?,
        pricing_sync_interval_secs: read_u64_setting(
            &state.pool,
            MODEL_PRICING_SYNC_INTERVAL_SETTING,
            0,
        )
        .await?,
        jitter_secs: read_u64_setting(&state.pool, MODEL_LIFECYCLE_JITTER_SETTING, 300).await?,
        probe_freshness_secs: read_u64_setting(
            &state.pool,
            MODEL_PROBE_FRESHNESS_SETTING,
            30 * 24 * 3600,
        )
        .await?,
    })
}

#[derive(Deserialize)]
pub struct ModelLifecycleSettingsBody {
    #[serde(default)]
    pub reconciliation_interval_secs: Option<u64>,
    #[serde(default)]
    pub pricing_sync_interval_secs: Option<u64>,
    #[serde(default)]
    pub jitter_secs: Option<u64>,
    #[serde(default)]
    pub probe_freshness_secs: Option<u64>,
}

fn validate_lifecycle_interval(name: &str, value: u64) -> Result<(), ApiError> {
    if value != 0 && !(300..=365 * 24 * 3600).contains(&value) {
        return Err(ApiError::bad(format!(
            "{name} must be 0 (disabled) or between 300 and 31536000 seconds"
        )));
    }
    Ok(())
}

pub async fn get_model_lifecycle_settings(
    State(state): State<AppState>,
    _auth: AdminAuth,
) -> ApiResult {
    let settings = model_lifecycle_settings(&state).await?;
    Ok(Json(json!({
        "reconciliation_interval_secs": settings.reconciliation_interval_secs,
        "pricing_sync_interval_secs": settings.pricing_sync_interval_secs,
        "jitter_secs": settings.jitter_secs,
        "probe_freshness_secs": settings.probe_freshness_secs,
    })))
}

pub async fn update_model_lifecycle_settings(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<ModelLifecycleSettingsBody>,
) -> ApiResult {
    if let Some(value) = body.reconciliation_interval_secs {
        validate_lifecycle_interval("reconciliation_interval_secs", value)?;
        db::set_setting(
            &state.pool,
            MODEL_RECONCILIATION_INTERVAL_SETTING,
            &value.to_string(),
        )
        .await
        .map_err(ApiError::internal)?;
    }
    if let Some(value) = body.pricing_sync_interval_secs {
        validate_lifecycle_interval("pricing_sync_interval_secs", value)?;
        db::set_setting(
            &state.pool,
            MODEL_PRICING_SYNC_INTERVAL_SETTING,
            &value.to_string(),
        )
        .await
        .map_err(ApiError::internal)?;
    }
    if let Some(value) = body.jitter_secs {
        if value > 3600 {
            return Err(ApiError::bad("jitter_secs must be <= 3600"));
        }
        db::set_setting(
            &state.pool,
            MODEL_LIFECYCLE_JITTER_SETTING,
            &value.to_string(),
        )
        .await
        .map_err(ApiError::internal)?;
    }
    if let Some(value) = body.probe_freshness_secs {
        if !(60..=365 * 24 * 3600).contains(&value) {
            return Err(ApiError::bad(
                "probe_freshness_secs must be between 60 and 31536000",
            ));
        }
        db::set_setting(
            &state.pool,
            MODEL_PROBE_FRESHNESS_SETTING,
            &value.to_string(),
        )
        .await
        .map_err(ApiError::internal)?;
    }

    let settings = model_lifecycle_settings(&state).await?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "model_lifecycle_settings_changed",
        "system",
        "",
        "Model Lifecycle",
        "Updated model reconciliation, pricing-sync, or probe-freshness scheduling.",
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "reconciliation_interval_secs": settings.reconciliation_interval_secs,
        "pricing_sync_interval_secs": settings.pricing_sync_interval_secs,
        "jitter_secs": settings.jitter_secs,
        "probe_freshness_secs": settings.probe_freshness_secs,
    })))
}

fn stable_schedule_jitter(key: &str, max_secs: u64) -> u64 {
    if max_secs == 0 {
        return 0;
    }
    let hash = key.bytes().fold(1469598103934665603_u64, |hash, byte| {
        hash.wrapping_mul(1099511628211).wrapping_add(byte as u64)
    });
    hash % (max_secs + 1)
}

async fn scheduled_due(pool: &Pool, key: &str, interval_secs: u64, jitter_secs: u64) -> bool {
    if interval_secs == 0 {
        return false;
    }

    let jitter = stable_schedule_jitter(key, jitter_secs);
    let now = chrono::Utc::now();
    if let Some(last) = db::get_setting(pool, key).await.ok().flatten() {
        if let Ok(last) = chrono::DateTime::parse_from_rfc3339(&last) {
            let elapsed = now
                .signed_duration_since(last.with_timezone(&chrono::Utc))
                .num_seconds()
                .max(0) as u64;
            return elapsed >= interval_secs.saturating_add(jitter);
        }
    }

    // Keep first-run jitter durable without pretending an attempt already ran.
    let anchor_key = format!("{key}:schedule_anchor");
    if let Some(anchor) = db::get_setting(pool, &anchor_key).await.ok().flatten() {
        if let Ok(anchor) = chrono::DateTime::parse_from_rfc3339(&anchor) {
            let elapsed = now
                .signed_duration_since(anchor.with_timezone(&chrono::Utc))
                .num_seconds()
                .max(0) as u64;
            return elapsed >= jitter;
        }
    }

    if db::set_setting(pool, &anchor_key, &now.to_rfc3339())
        .await
        .is_err()
    {
        return false;
    }
    jitter == 0
}

/// Best-effort scheduled lifecycle pass. Each provider has an independent last
/// attempt timestamp and deterministic jitter, avoiding synchronized catalog or
/// provider discovery bursts across a large installation.
pub(crate) async fn run_scheduled_model_lifecycle(state: &AppState) {
    let settings = match model_lifecycle_settings(state).await {
        Ok(settings) => settings,
        Err(error) => {
            tracing::warn!(error = ?error, "failed to read model lifecycle settings");
            return;
        }
    };
    if settings.reconciliation_interval_secs == 0 && settings.pricing_sync_interval_secs == 0 {
        return;
    }

    let providers = match db::list_providers(&state.pool).await {
        Ok(providers) => providers,
        Err(error) => {
            tracing::warn!(%error, "failed to list providers for model lifecycle scheduler");
            return;
        }
    };

    for provider in providers {
        let reconcile_key = lifecycle_setting_key("reconciliation", "last_attempt", &provider.id);
        let pricing_key = lifecycle_setting_key("pricing_sync", "last_attempt", &provider.id);
        let reconcile_due = scheduled_due(
            &state.pool,
            &reconcile_key,
            settings.reconciliation_interval_secs,
            settings.jitter_secs,
        )
        .await;
        let pricing_due = scheduled_due(
            &state.pool,
            &pricing_key,
            settings.pricing_sync_interval_secs,
            settings.jitter_secs,
        )
        .await;

        if reconcile_due {
            if let Err(error) =
                run_model_lifecycle_lane(state, &provider.id, ModelLifecycleLane::Reconciliation)
                    .await
            {
                tracing::warn!(
                    provider = %provider.id,
                    error = ?error,
                    "scheduled model reconciliation failed; existing state left intact"
                );
            }
        }

        if pricing_due {
            if let Err(error) =
                run_model_lifecycle_lane(state, &provider.id, ModelLifecycleLane::PricingSync).await
            {
                tracing::warn!(
                    provider = %provider.id,
                    error = ?error,
                    "scheduled pricing sync failed; preserving last-known metadata and prices"
                );
            }
        }
    }
}

/// `POST /admin/api/test-stream` — run a real request through the pipeline for a
/// chosen virtual key + model and stream the encoded result back to the
/// browser. The raw virtual key never leaves the server (it is stored hashed).
pub async fn test_stream(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<TestStreamBody>,
) -> Response {
    let key = match db::get_virtual_key_by_id(&state.pool, &body.key_id)
        .await
        .map_err(ApiError::internal)
    {
        Ok(Some(k)) => k,
        Ok(None) => return ApiError::not_found("key not found").into_response(),
        Err(e) => return e.into_response(),
    };

    let format = if body.format.as_deref() == Some("anthropic") {
        FrontendFormat::Anthropic
    } else {
        FrontendFormat::OpenAi
    };
    let stream = body.stream.unwrap_or(true);

    // Build a minimal internal request from the tester form.
    let mut system_prompts = Vec::new();
    if let Some(system) = body.system.filter(|s| !s.trim().is_empty()) {
        system_prompts.push(system);
    }
    let messages = vec![crate::types::Message {
        role: crate::types::Role::User,
        parts: vec![crate::types::Part::Text(body.prompt)],
    }];

    let req = crate::types::InternalRequest {
        requested_model: body.model.clone(),
        system: system_prompts,
        messages,
        tools: vec![],
        tool_choice: None,
        tool_choice_name: None,
        params: crate::types::SamplingParams {
            max_tokens: body.max_tokens,
            temperature: body.temperature,
            ..Default::default()
        },
        stream,
        include_usage: false,
        thinking: None,
        extra: Default::default(),
        raw_body: None,
    };

    let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
    if let Err(e) = limits::validate(&key, &req.requested_model) {
        return crate::api::error_response(format, &request_id, e);
    }

    match pipeline::run(
        &state,
        format,
        Some(key),
        req,
        request_id.clone(),
        true,
        None,
        Vec::new(),
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => crate::api::error_response(format, &request_id, e),
    }
}

#[derive(Deserialize)]
pub struct TestStreamBody {
    pub key_id: String,
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
}

// ===========================================================================
// Overview / metrics
// ===========================================================================

pub async fn overview(State(state): State<AppState>, _auth: AdminAuth) -> ApiResult {
    let summary = db::usage_summary(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let keys = db::list_virtual_keys(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let accounts = db::list_accounts(&state.pool)
        .await
        .map_err(ApiError::internal)?;

    // Active streams comes from the in-memory live view (NFR-4.2), not the
    // usage-log queue depth.
    let active_streams = state.live.live_count();
    let fallback_rate = {
        let reqs = summary["requests"].as_i64().unwrap_or(0);
        let hops = summary["fallback_hops"].as_i64().unwrap_or(0);
        if reqs > 0 {
            hops as f64 / reqs as f64
        } else {
            0.0
        }
    };

    Ok(Json(json!({
        "active_streams": active_streams,
        "live_requests": state.live.snapshot().len(),
        "live_dropped": state.live.dropped(),
        "total_requests": summary["requests"],
        "total_tokens": summary["input_tokens"].as_i64().unwrap_or(0) + summary["output_tokens"].as_i64().unwrap_or(0),
        "total_spend_usd": summary["cost_usd"],
        "cached_tokens": summary["cached_tokens"],
        "cache_write_tokens": summary["cache_write_tokens"],
        "thinking_tokens": summary["thinking_tokens"],
        "fallback_rate": fallback_rate,
        "avg_latency_ms": summary["avg_latency_ms"],
        "unknown_usage_requests": summary["unknown_usage_requests"],
        "estimated_usage_requests": summary["estimated_usage_requests"],
        "unknown_cost_requests": summary["unknown_cost_requests"],
        "keys_active": keys.iter().filter(|k| k.status == "active").count(),
        "keys_total": keys.len(),
        "accounts_healthy": accounts.iter().filter(|a| a.status == "healthy").count(),
        "accounts_total": accounts.len(),
        "log_queue_depth": state.log_queue.depth(),
        "log_queue_dropped": state.log_queue.dropped(),
        "uptime_secs": state.uptime_secs(),
        "tunnel_status": "connected",
    })))
}

#[derive(Debug, Deserialize)]
pub struct RuntimeHealthQuery {
    pub window: Option<String>,
}

pub async fn runtime_health(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Query(query): Query<RuntimeHealthQuery>,
) -> ApiResult {
    let window_secs = match query.window.as_deref().unwrap_or("1h") {
        "5m" => 5 * 60,
        "1h" => 60 * 60,
        "24h" => 24 * 60 * 60,
        other => {
            return Err(ApiError::bad(format!(
                "unsupported health window '{other}'"
            )))
        }
    };
    let telemetry = state
        .target_telemetry
        .summaries(&state.pool, window_secs)
        .await
        .map_err(ApiError::internal)?;

    let mut provider_circuits = state.provider_circuits.snapshots();
    provider_circuits.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));

    let routing_quota = state
        .quota
        .routing_observations()
        .into_iter()
        .map(|(provider_id, account_id, snapshot, fresh)| {
            (
                (provider_id, account_id),
                json!({
                    "scope": "account-global",
                    "remaining_fraction": snapshot.remaining_fraction,
                    "reset_at": snapshot.reset_at,
                    "observed_at": snapshot.observed_at,
                    "source": snapshot.source,
                    "max_age_secs": snapshot.max_age_secs,
                    "freshness": if fresh { "fresh" } else { "stale" },
                    "routing_eligible": fresh,
                }),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();

    let mut quota = state
        .quota
        .observations()
        .into_iter()
        .map(|(provider_id, account_id, snapshot, fresh)| {
            let routing = routing_quota
                .get(&(provider_id.clone(), account_id.clone()))
                .cloned()
                .unwrap_or(Value::Null);
            json!({
                "provider_id": provider_id,
                "account_id": account_id,
                "remaining_fraction": snapshot.remaining_fraction,
                "reset_at": snapshot.reset_at,
                "observed_at": snapshot.observed_at,
                "source": snapshot.source,
                "max_age_secs": snapshot.max_age_secs,
                "freshness": if fresh { "fresh" } else { "stale" },
                "routing": routing,
            })
        })
        .collect::<Vec<_>>();
    quota.sort_by(|a, b| {
        a["provider_id"]
            .as_str()
            .cmp(&b["provider_id"].as_str())
            .then_with(|| a["account_id"].as_str().cmp(&b["account_id"].as_str()))
    });

    Ok(Json(json!({
        "window": query.window.unwrap_or_else(|| "1h".into()),
        "telemetry": telemetry,
        "provider_circuits": provider_circuits,
        "quota": quota,
        "dropped": {
            "queue": state.target_telemetry.dropped_queue(),
            "persistence": state.target_telemetry.dropped_persistence(),
        },
    })))
}

// ===========================================================================
// Virtual keys
// ===========================================================================

pub async fn list_keys(State(state): State<AppState>, _auth: AdminAuth) -> ApiResult {
    let keys = db::list_virtual_keys(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let (by_key, _) = db::lifetime_totals(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let mut out = Vec::new();
    for k in keys {
        let (daily, monthly) = limits::spend_snapshot(&state.pool, &k).await;
        let (requests, tokens) = by_key.get(&k.id).copied().unwrap_or((0, 0));
        out.push(key_json(&k, daily, monthly, requests, tokens));
    }
    Ok(Json(json!({ "keys": out })))
}

fn key_json(
    k: &db::VirtualKeyRow,
    daily_spend: f64,
    monthly_spend: f64,
    total_requests: i64,
    total_tokens: i64,
) -> Value {
    json!({
        "id": k.id,
        "name": k.name,
        "owner": k.owner,
        "tag": k.tag,
        "allowed_models": k.allowed_models(),
        "allowed_providers": k.allowed_providers(),
        "rpm_limit": k.rpm_limit,
        "tpm_limit": k.tpm_limit,
        "daily_budget": k.daily_budget,
        "monthly_budget": k.monthly_budget,
        "current_daily_spend": daily_spend,
        "current_monthly_spend": monthly_spend,
        "expires_at": k.expires_at,
        "status": k.status,
        "allowed_ips": k.allowed_ips(),
        "body_logging": k.body_logging != 0,
        "created_at": k.created_at,
        "key_mask": mask_hash(&k.key_hash),
        "total_requests": total_requests,
        "total_tokens": total_tokens,
    })
}

fn mask_hash(_hash: &str) -> String {
    "sk-kinetix-•••• (hidden)".to_string()
}

#[derive(Deserialize)]
pub struct CreateKeyBody {
    pub name: String,
    pub owner: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub daily_budget: Option<f64>,
    pub monthly_budget: Option<f64>,
    pub expires_at: Option<String>,
    #[serde(default)]
    pub allowed_ips: Vec<String>,
    #[serde(default)]
    pub body_logging: bool,
}

pub async fn create_key(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<CreateKeyBody>,
) -> ApiResult {
    let full_key = crypto::generate_virtual_key();
    let hash = crypto::hash_virtual_key(&full_key);
    let allowed = if body.allowed_models.is_empty() {
        vec!["*".to_string()]
    } else {
        body.allowed_models.clone()
    };
    let row = db::VirtualKeyRow {
        id: format!("key_{}", uuid::Uuid::new_v4().simple()),
        key_hash: hash,
        name: body.name.clone(),
        owner: body.owner.clone(),
        tag: body.tag.clone(),
        allowed_models: serde_json::to_string(&allowed).unwrap(),
        allowed_providers: serde_json::to_string(&body.allowed_providers).unwrap(),
        rpm_limit: body.rpm_limit,
        tpm_limit: body.tpm_limit,
        daily_budget: body.daily_budget,
        monthly_budget: body.monthly_budget,
        expires_at: body.expires_at.clone(),
        status: "active".to_string(),
        allowed_ips: serde_json::to_string(&body.allowed_ips).unwrap(),
        body_logging: body.body_logging as i64,
        created_at: db::now_iso(),
        revoked_at: None,
    };
    db::insert_virtual_key(&state.pool, &row)
        .await
        .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "key_created",
        "key",
        &row.id,
        &row.name,
        &format!("Issued virtual key for {} (owner {})", row.name, row.owner),
    )
    .await;
    // The full key is shown exactly once (FR-3.1).
    Ok(Json(json!({
        "key": key_json(&row, 0.0, 0.0, 0, 0),
        "full_key": full_key,
    })))
}

#[derive(Deserialize)]
pub struct UpdateKeyBody {
    pub status: Option<String>,
    pub name: Option<String>,
    pub owner: Option<String>,
    pub tag: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    pub allowed_providers: Option<Vec<String>>,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub daily_budget: Option<f64>,
    pub monthly_budget: Option<f64>,
    pub expires_at: Option<String>,
    pub body_logging: Option<bool>,
    /// Per-key IP allowlist (FR-3.4). An explicit empty list clears it.
    pub allowed_ips: Option<Vec<String>>,
}

pub async fn update_key(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<UpdateKeyBody>,
) -> ApiResult {
    if let Some(status) = &body.status {
        db::set_virtual_key_status(&state.pool, &id, status)
            .await
            .map_err(ApiError::internal)?;
        let _ = db::insert_audit(
            &state.pool,
            "admin",
            &format!("key_status_{status}"),
            "key",
            &id,
            "",
            &format!("Changed key status to {status}."),
        )
        .await;
    }
    // Field updates (limits/budgets/etc.) rebuild the row.
    let existing = db::list_virtual_keys(&state.pool)
        .await
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|k| k.id == id)
        .ok_or_else(|| ApiError::not_found("key not found"))?;

    let name = body.name.unwrap_or(existing.name.clone());
    let owner = body.owner.unwrap_or(existing.owner.clone());
    let tag = body.tag.unwrap_or(existing.tag.clone());
    let allowed_models = body
        .allowed_models
        .map(|v| serde_json::to_string(&v).unwrap())
        .unwrap_or(existing.allowed_models.clone());
    let allowed_providers = body
        .allowed_providers
        .map(|v| serde_json::to_string(&v).unwrap())
        .unwrap_or(existing.allowed_providers.clone());
    let body_logging = body
        .body_logging
        .map(|b| b as i64)
        .unwrap_or(existing.body_logging);
    // FR-3.4: persist the per-key IP allowlist. Only touch it when provided,
    // so an update that omits it leaves the existing allowlist intact.
    let allowed_ips = body
        .allowed_ips
        .map(|v| serde_json::to_string(&v).unwrap())
        .unwrap_or(existing.allowed_ips.clone());

    sqlx::query(
        "UPDATE virtual_keys SET name=?, owner=?, tag=?, allowed_models=?, allowed_providers=?,
         rpm_limit=?, tpm_limit=?, daily_budget=?, monthly_budget=?, expires_at=?, body_logging=?, allowed_ips=? WHERE id=?",
    )
    .bind(name)
    .bind(owner)
    .bind(tag)
    .bind(allowed_models)
    .bind(allowed_providers)
    .bind(body.rpm_limit.or(existing.rpm_limit))
    .bind(body.tpm_limit.or(existing.tpm_limit))
    .bind(body.daily_budget.or(existing.daily_budget))
    .bind(body.monthly_budget.or(existing.monthly_budget))
    .bind(body.expires_at.or(existing.expires_at))
    .bind(body_logging)
    .bind(allowed_ips)
    .bind(&id)
    .execute(&state.pool)
    .await
    .map_err(ApiError::internal)?;

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "key_updated",
        "key",
        &id,
        &existing.name,
        "Updated key limits/budgets.",
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_key(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    db::delete_virtual_key_cascade(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "key_deleted",
        "key",
        &id,
        "",
        "Deleted key.",
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

// ===========================================================================
// Providers
// ===========================================================================

/// FR-8.4: return a single provider's full configuration so the dashboard can
/// populate an edit form (list_providers is intentionally summary-shaped).
pub async fn get_provider(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let p = db::get_provider(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    Ok(Json(provider_json_with_enrollment(&state, &p).await))
}

pub async fn list_providers(State(state): State<AppState>, _auth: AdminAuth) -> ApiResult {
    let providers = db::list_providers(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let accounts = db::list_accounts(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let models = db::list_models(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let mut out = Vec::with_capacity(providers.len());
    for p in &providers {
        let visible_accounts = accounts.iter().filter(|a| {
            a.provider_id == p.id
                && !(p.credential_mode == "none" && a.label == "__kinetix_noauth__")
        });
        let accounts_count = visible_accounts.clone().count();
        let healthy_accounts = visible_accounts.filter(|a| a.status == "healthy").count();
        let mut v = provider_json_with_enrollment(&state, p).await;
        v["accounts_count"] = json!(accounts_count);
        v["models_count"] = json!(models.iter().filter(|m| m.provider_id == p.id).count());
        v["healthy_accounts"] = json!(healthy_accounts);
        out.push(v);
    }
    Ok(Json(json!({ "providers": out })))
}

/// Full provider configuration (used by both the list and single-provider
/// endpoints). The dashboard's edit form is populated from this shape, so every
/// field an admin can set must be present here (FR-8.4/8.6).
fn provider_json(p: &db::ProviderRow) -> Value {
    let (action_label, available) = match p.credential_mode.as_str() {
        "auth_flow" => (Some("Connect account"), false),
        "none" => (None, true),
        _ => (Some("Add API Key"), true),
    };
    json!({
        "id": p.id,
        "name": p.name,
        "base_url": p.base_url,
        "wire_format": p.wire_format,
        "auth_scheme": p.auth_scheme,
        "custom_header_name": p.custom_header_name,
        "custom_param_name": p.custom_param_name,
        "extra_headers": p.extra_headers_map(),
        "timeout_ms": p.timeout_ms,
        "capability_mode": p.capability_mode,
        "models_path": p.models_path,
        "rate_limit_rules": serde_json::from_str::<Value>(&p.rate_limit_rules).unwrap_or(json!({})),
        "enabled": p.enabled != 0,
        "follow_redirects": p.follow_redirects != 0,
        "credential_hosts": p.credential_hosts,
        "allow_insecure_tls": p.allow_insecure_tls != 0,
        "wire_plugin": p.wire_plugin,
        "credential_plugin": p.credential_plugin,
        "model_source_plugin": p.model_source_plugin,
        "credential_mode": p.credential_mode,
        "source_plugin_id": p.source_plugin_id,
        "source_integration_id": p.source_integration_id,
        "credential_enrollment": {
            "mode": p.credential_mode,
            "action_label": action_label,
            "available": available,
        },
        "created_at": p.created_at,
    })
}

async fn provider_json_with_enrollment(state: &AppState, p: &db::ProviderRow) -> Value {
    let mut value = provider_json(p);
    if p.credential_mode != "auth_flow" {
        return value;
    }

    let mut label = "Connect account".to_string();
    let mut available = false;
    if let (Some(plugin_id), Some(integration_id), Some(manager)) = (
        p.source_plugin_id.as_deref(),
        p.source_integration_id.as_deref(),
        state.plugin_manager(),
    ) {
        if let Ok(Some(row)) = manager.get(plugin_id).await {
            if row.enabled != 0 {
                if let Some(manifest) = row.manifest() {
                    if let Some(integration) = manifest
                        .integrations
                        .iter()
                        .find(|integration| integration.id == integration_id)
                    {
                        available = integration.auth_flow.is_some()
                            && integration.credential_strategy.is_some();
                        if let Some(action) = manifest.ui.actions.iter().find(|action| {
                            action.kind == "auth" && action.integration == integration_id
                        }) {
                            label = action.label.clone();
                        }
                    }
                }
            }
        }
    }
    value["credential_enrollment"] = json!({
        "mode": "auth_flow",
        "action_label": label,
        "available": available,
    });
    value
}

#[derive(Deserialize)]
pub struct ProviderBody {
    pub name: String,
    pub base_url: String,
    pub wire_format: String,
    #[serde(default = "default_bearer")]
    pub auth_scheme: String,
    pub custom_header_name: Option<String>,
    pub custom_param_name: Option<String>,
    #[serde(default)]
    pub extra_headers: serde_json::Map<String, Value>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: i64,
    #[serde(default = "default_permissive")]
    pub capability_mode: String,
    pub models_path: Option<String>,
    /// Optional provider-specific failure classification overrides. Rules are
    /// matched against status/code/message before fallback state is updated.
    #[serde(default)]
    pub rate_limit_rules: Option<Value>,
    /// NFR-3.10: redirects are never followed unless explicitly enabled.
    #[serde(default)]
    pub follow_redirects: bool,
    /// NFR-3.11: comma-separated authorized hosts for the credential.
    #[serde(default)]
    pub credential_hosts: String,
    /// NFR-3.12: explicit dev-mode opt-out of TLS verification.
    #[serde(default)]
    pub allow_insecure_tls: bool,
    /// §6.0: plugin capability bindings (`plugin:<id>/<cap>` or empty).
    #[serde(default)]
    pub wire_plugin: String,
    #[serde(default)]
    pub credential_plugin: String,
    #[serde(default)]
    pub model_source_plugin: String,
    #[serde(default)]
    pub pricing_scope: Option<String>,
    /// Optional initial credential.
    pub api_key: Option<String>,
    pub account_label: Option<String>,
}

fn default_bearer() -> String {
    "bearer".into()
}
fn default_timeout() -> i64 {
    120_000
}
fn default_permissive() -> String {
    "permissive".into()
}

async fn provider_plugin_binding_problems(state: &AppState, body: &ProviderBody) -> Vec<String> {
    use crate::plugins::Capability;

    let bindings = [
        (
            "wire_plugin",
            body.wire_plugin.as_str(),
            Capability::ProviderAdapter,
        ),
        (
            "credential_plugin",
            body.credential_plugin.as_str(),
            Capability::CredentialStrategy,
        ),
    ];

    let mut problems = Vec::new();
    for (field, reference, capability) in bindings {
        let reference = reference.trim();
        if reference.is_empty() {
            continue;
        }
        if crate::plugins::PluginRef::parse(reference).is_none() {
            problems.push(format!(
                "{field} must use plugin:<id>/<capability-name> syntax"
            ));
            continue;
        }
        let Some(manager) = state.plugin_manager() else {
            problems.push(format!(
                "{field} references '{reference}' but the plugin host is unavailable"
            ));
            continue;
        };
        if manager
            .resolve_binding(reference, capability)
            .await
            .is_none()
        {
            problems.push(format!(
                "{field} reference '{reference}' does not resolve to an installed, enabled, approved plugin providing {}",
                capability.manifest_key()
            ));
        }
    }

    let model_reference = body.model_source_plugin.trim();
    if !model_reference.is_empty() {
        if crate::plugins::PluginRef::parse(model_reference).is_none() {
            problems
                .push("model_source_plugin must use plugin:<id>/<capability-name> syntax".into());
        } else if let Some(manager) = state.plugin_manager() {
            let account_aware = manager
                .resolve_binding(model_reference, Capability::AccountModelSource)
                .await
                .is_some();
            let legacy = manager
                .resolve_binding(model_reference, Capability::ModelSource)
                .await
                .is_some();
            if !account_aware && !legacy {
                problems.push(format!(
                    "model_source_plugin reference '{model_reference}' does not resolve to an installed, enabled, approved plugin providing account_model_sources or model_sources"
                ));
            }
        } else {
            problems.push(format!(
                "model_source_plugin references '{model_reference}' but the plugin host is unavailable"
            ));
        }
    }

    problems
}
pub async fn create_provider(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<ProviderBody>,
) -> ApiResult {
    validate_outbound_url(&state, &body.base_url)?;
    let binding_problems = provider_plugin_binding_problems(&state, &body).await;
    if !binding_problems.is_empty() {
        return Err(ApiError::bad(binding_problems.join("; ")));
    }
    let wire =
        WireFormat::parse(&body.wire_format).ok_or_else(|| ApiError::bad("invalid wire_format"))?;
    if wire == WireFormat::Plugin && body.wire_plugin.trim().is_empty() {
        return Err(ApiError::bad(
            "wire_format 'plugin' requires a wire_plugin binding",
        ));
    }
    let auth =
        AuthScheme::parse(&body.auth_scheme).ok_or_else(|| ApiError::bad("invalid auth_scheme"))?;
    let conservative_scope = db::conservative_provider_pricing_scope(
        "manual",
        None,
        None,
        &body.wire_plugin,
        &body.credential_plugin,
        &body.model_source_plugin,
    );
    if let Some(scope) = body.pricing_scope.as_deref() {
        if !matches!(scope, "direct_api" | "integration") {
            return Err(ApiError::bad(
                "pricing_scope must be 'direct_api' or 'integration'",
            ));
        }
        if scope == "direct_api" && conservative_scope != "direct_api" {
            return Err(ApiError::bad(
                "pricing_scope 'direct_api' is not allowed for plugin-backed generic providers",
            ));
        }
    }

    let id = db::insert_provider(
        &state.pool,
        &db::NewProvider {
            name: &body.name,
            base_url: &body.base_url,
            wire_format: wire,
            auth_scheme: auth,
            custom_header_name: body.custom_header_name.as_deref(),
            custom_param_name: body.custom_param_name.as_deref(),
            extra_headers: Value::Object(body.extra_headers),
            timeout_ms: body.timeout_ms,
            capability_mode: &body.capability_mode,
            models_path: body.models_path.as_deref(),
            rate_limit_rules: body.rate_limit_rules.clone().unwrap_or_else(|| json!({})),
            follow_redirects: body.follow_redirects,
            credential_hosts: &body.credential_hosts,
            allow_insecure_tls: body.allow_insecure_tls,
            wire_plugin: &body.wire_plugin,
            credential_plugin: &body.credential_plugin,
            model_source_plugin: &body.model_source_plugin,
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .map_err(ApiError::internal)?;
    if body.pricing_scope.as_deref() == Some("integration") {
        db::update_provider_pricing_scope(&state.pool, &id, "integration")
            .await
            .map_err(ApiError::internal)?;
    }

    if let Some(api_key) = body.api_key.filter(|k| !k.trim().is_empty()) {
        let enc = state.crypto.encrypt(&api_key).map_err(ApiError::internal)?;
        db::insert_account(
            &state.pool,
            &id,
            &account_label_or_default(&body.name, body.account_label.as_deref()),
            &enc,
            &crypto::mask_secret(&api_key),
            1,
            1,
            None,
            "none",
        )
        .await
        .map_err(ApiError::internal)?;
    }

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "provider_created",
        "provider",
        &id,
        &body.name,
        &format!(
            "Added {} upstream ({} wire format at {}).",
            body.name, body.wire_format, body.base_url
        ),
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "id": id })))
}

fn validate_auth_flow_binding_edit(
    provider: &db::ProviderRow,
    expected_binding: &str,
    proposed_binding: &str,
) -> Result<(), ApiError> {
    if provider.credential_mode == "auth_flow"
        && proposed_binding != provider.credential_plugin
        && proposed_binding != expected_binding
    {
        return Err(ApiError::bad(
            "credential_plugin conflicts with the provider's authentication-flow source integration",
        ));
    }
    Ok(())
}

async fn validate_provider_credential_binding_edit(
    state: &AppState,
    provider: &db::ProviderRow,
    proposed_binding: &str,
) -> Result<(), ApiError> {
    if provider.credential_mode != "auth_flow" || proposed_binding == provider.credential_plugin {
        return Ok(());
    }

    let plugin_id = provider
        .source_plugin_id
        .as_deref()
        .ok_or_else(|| ApiError::bad("provider auth integration provenance is unavailable"))?;
    let integration_id = provider
        .source_integration_id
        .as_deref()
        .ok_or_else(|| ApiError::bad("provider auth integration provenance is unavailable"))?;
    let manager = plugin_manager(state)?;
    let row = manager
        .get(plugin_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::bad("provider authentication plugin is not installed"))?;
    let manifest = row
        .manifest()
        .ok_or_else(|| ApiError::bad("plugin manifest is unreadable"))?;
    let integration = manifest
        .integrations
        .iter()
        .find(|integration| integration.id == integration_id)
        .ok_or_else(|| ApiError::bad("provider authentication integration is unavailable"))?;
    let credential_strategy = integration
        .credential_strategy
        .as_deref()
        .ok_or_else(|| ApiError::bad("provider integration has no credential strategy"))?;
    let expected_binding = format!("plugin:{plugin_id}/{credential_strategy}");

    validate_auth_flow_binding_edit(provider, &expected_binding, proposed_binding)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectApiManifestTrust {
    Trusted,
    PluginUnavailable,
}

fn normalize_provider_endpoint_identity(base_url: &str) -> Option<String> {
    let mut url = url::Url::parse(base_url).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    let trimmed = url.path().trim_end_matches('/').to_string();
    url.set_path(&trimmed);
    Some(url.to_string().trim_end_matches('/').to_string())
}

fn validate_direct_api_integration_endpoint(
    name: &str,
    base_url: &str,
    integration: &crate::plugins::Integration,
) -> Result<(), String> {
    let template = integration.provider.as_ref().ok_or_else(|| {
        format!(
            "provider '{name}': source integration '{}' has no provider template",
            integration.id
        )
    })?;
    if template.pricing_scope != crate::plugins::PricingScope::DirectApi {
        return Err(format!(
            "provider '{name}': source integration '{}' does not declare pricing_scope 'direct_api'",
            integration.id
        ));
    }
    let expected = normalize_provider_endpoint_identity(&template.base_url).ok_or_else(|| {
        format!(
            "provider '{name}': source integration '{}' has an invalid provider base_url",
            integration.id
        )
    })?;
    let actual = normalize_provider_endpoint_identity(base_url)
        .ok_or_else(|| format!("provider '{name}': base_url is invalid"))?;
    if actual != expected {
        return Err(format!(
            "provider '{name}': pricing_scope 'direct_api' is bound to integration '{}' base_url '{}', not '{}'",
            integration.id, template.base_url, base_url
        ));
    }
    Ok(())
}

async fn direct_api_manifest_trust(
    state: &AppState,
    name: &str,
    base_url: &str,
    source_plugin_id: Option<&str>,
    source_integration_id: Option<&str>,
) -> Result<DirectApiManifestTrust, String> {
    let plugin_id = source_plugin_id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            format!("provider '{name}': pricing_scope 'direct_api' requires source_plugin_id")
        })?;
    let integration_id = source_integration_id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            format!("provider '{name}': pricing_scope 'direct_api' requires source_integration_id")
        })?;
    let Some(manager) = state.plugin_manager() else {
        return Ok(DirectApiManifestTrust::PluginUnavailable);
    };
    let Some(row) = manager
        .get(plugin_id)
        .await
        .map_err(|error| format!("provider '{name}': {error}"))?
    else {
        return Ok(DirectApiManifestTrust::PluginUnavailable);
    };
    let manifest = row
        .manifest()
        .ok_or_else(|| format!("provider '{name}': source plugin manifest is unreadable"))?;
    let integration = manifest
        .integrations
        .iter()
        .find(|integration| integration.id == integration_id)
        .ok_or_else(|| {
            format!("provider '{name}': source integration '{integration_id}' is unavailable")
        })?;
    validate_direct_api_integration_endpoint(name, base_url, integration)?;
    Ok(DirectApiManifestTrust::Trusted)
}

pub async fn update_provider(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<ProviderBody>,
) -> ApiResult {
    validate_outbound_url(&state, &body.base_url)?;
    let lock = model_reconciliation_lock(&id);
    let _guard = lock.lock().await;
    let existing = db::get_provider(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    if body
        .api_key
        .as_deref()
        .is_some_and(|api_key| !api_key.trim().is_empty())
    {
        if let Some(error) = manual_account_enrollment_error(&existing.credential_mode) {
            return Err(ApiError::bad(error));
        }
    }
    let rate_limit_rules = body.rate_limit_rules.clone().unwrap_or_else(|| {
        serde_json::from_str(&existing.rate_limit_rules).unwrap_or_else(|_| json!({}))
    });
    validate_provider_credential_binding_edit(&state, &existing, &body.credential_plugin).await?;
    let binding_problems = provider_plugin_binding_problems(&state, &body).await;
    if !binding_problems.is_empty() {
        return Err(ApiError::bad(binding_problems.join("; ")));
    }
    let wire =
        WireFormat::parse(&body.wire_format).ok_or_else(|| ApiError::bad("invalid wire_format"))?;
    if wire == WireFormat::Plugin && body.wire_plugin.trim().is_empty() {
        return Err(ApiError::bad(
            "wire_format 'plugin' requires a wire_plugin binding",
        ));
    }
    let auth =
        AuthScheme::parse(&body.auth_scheme).ok_or_else(|| ApiError::bad("invalid auth_scheme"))?;
    let conservative_scope = db::conservative_provider_pricing_scope(
        &existing.credential_mode,
        existing.source_plugin_id.as_deref(),
        existing.source_integration_id.as_deref(),
        &body.wire_plugin,
        &body.credential_plugin,
        &body.model_source_plugin,
    );
    if let Some(scope) = body.pricing_scope.as_deref() {
        if !matches!(scope, "direct_api" | "integration") {
            return Err(ApiError::bad(
                "pricing_scope must be 'direct_api' or 'integration'",
            ));
        }
    }
    let effective_scope = body
        .pricing_scope
        .as_deref()
        .unwrap_or(existing.pricing_scope.as_str());
    let scope_drivers_unchanged = existing.wire_plugin == body.wire_plugin
        && existing.credential_plugin == body.credential_plugin
        && existing.model_source_plugin == body.model_source_plugin;
    let endpoint_unchanged = normalize_provider_endpoint_identity(&existing.base_url)
        == normalize_provider_endpoint_identity(&body.base_url);
    if effective_scope == "direct_api" && conservative_scope != "direct_api" {
        let reusable_existing_trust = existing.pricing_scope == "direct_api"
            && scope_drivers_unchanged
            && endpoint_unchanged;
        match direct_api_manifest_trust(
            &state,
            &body.name,
            &body.base_url,
            existing.source_plugin_id.as_deref(),
            existing.source_integration_id.as_deref(),
        )
        .await
        {
            Ok(DirectApiManifestTrust::Trusted) => {}
            Ok(DirectApiManifestTrust::PluginUnavailable) if reusable_existing_trust => {}
            Ok(DirectApiManifestTrust::PluginUnavailable) => {
                return Err(ApiError::bad(
                    "pricing_scope 'direct_api' requires the source integration plugin to be installed when provider identity changes",
                ));
            }
            Err(problem) => return Err(ApiError::bad(problem)),
        }
    }
    let provider = db::NewProvider {
        name: &body.name,
        base_url: &body.base_url,
        wire_format: wire,
        auth_scheme: auth,
        custom_header_name: body.custom_header_name.as_deref(),
        custom_param_name: body.custom_param_name.as_deref(),
        extra_headers: Value::Object(body.extra_headers),
        timeout_ms: body.timeout_ms,
        capability_mode: &body.capability_mode,
        models_path: body.models_path.as_deref(),
        rate_limit_rules,
        follow_redirects: body.follow_redirects,
        credential_hosts: &body.credential_hosts,
        allow_insecure_tls: body.allow_insecure_tls,
        wire_plugin: &body.wire_plugin,
        credential_plugin: &body.credential_plugin,
        model_source_plugin: &body.model_source_plugin,
        credential_mode: &existing.credential_mode,
        source_plugin_id: existing.source_plugin_id.as_deref(),
        source_integration_id: existing.source_integration_id.as_deref(),
    };
    db::update_provider(&state.pool, &id, &provider, body.pricing_scope.as_deref())
        .await
        .map_err(ApiError::internal)?;
    if let Some(api_key) = body.api_key.filter(|k| !k.trim().is_empty()) {
        let enc = state.crypto.encrypt(&api_key).map_err(ApiError::internal)?;
        db::insert_account(
            &state.pool,
            &id,
            &account_label_or_default(&body.name, body.account_label.as_deref()),
            &enc,
            &crypto::mask_secret(&api_key),
            1,
            1,
            None,
            "none",
        )
        .await
        .map_err(ApiError::internal)?;
    }
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "provider_updated",
        "provider",
        &id,
        &body.name,
        "Updated provider configuration.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_provider(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    db::delete_provider(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "provider_deleted",
        "provider",
        &id,
        "",
        "Deleted provider and its models/accounts.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Clone)]
struct DiscoveredObservation {
    model: crate::adapters::DiscoveredModel,
    #[cfg_attr(not(test), allow(dead_code))]
    reasoning_support: Option<bool>,
    reasoning: Option<crate::adapters::ReasoningCapability>,
    thinking_map: Option<ThinkingMap>,
    capabilities: ModelCapabilityFlags,
    capability_sources: Value,
    modalities: Option<Value>,
    prices: Prices,
    price_sources: Value,
    raw_metadata: Option<Value>,
    raw_metadata_truncated: bool,
    transport: Option<String>,
    transport_source: Option<String>,
    canonical_identity: Option<Value>,
    canonical_model_id: Option<String>,
    canonical_match: Option<String>,
    provider_variant: Option<Value>,
    opaque_state: Option<Value>,
    model_type: Option<String>,
    execution_supported: bool,
    catalog: Option<Value>,
}

fn reasoning_wire_context(provider: &db::ProviderRow) -> WireFormat {
    if provider.wire_plugin_ref().is_some() {
        WireFormat::Plugin
    } else {
        provider.wire()
    }
}

fn explicit_reasoning_support(metadata: &Value) -> Option<bool> {
    metadata
        .get("reasoning")
        .and_then(|reasoning| {
            reasoning
                .as_bool()
                .or_else(|| reasoning.get("supported").and_then(Value::as_bool))
        })
        .or_else(|| {
            metadata
                .get("reasoning_capability")
                .and_then(|reasoning| reasoning.get("supported"))
                .and_then(Value::as_bool)
        })
        // Gemini models.list exposes support as a top-level boolean while
        // leaving supported thinking levels to the model documentation.
        .or_else(|| metadata.get("thinking").and_then(Value::as_bool))
}

fn provider_capability_flags(metadata: &Value) -> ModelCapabilityFlags {
    fn first_bool(metadata: &Value, pointers: &[&str]) -> Option<bool> {
        pointers
            .iter()
            .find_map(|pointer| metadata.pointer(pointer).and_then(Value::as_bool))
    }

    let text = first_bool(metadata, &["/capabilities/text", "/text/supported"]).or_else(|| {
        metadata
            .get("supportedGenerationMethods")
            .and_then(Value::as_array)
            .filter(|methods| !methods.is_empty())
            .map(|methods| {
                methods.iter().any(|method| {
                    method
                        .as_str()
                        .is_some_and(|method| method.contains("generateContent"))
                })
            })
    });

    ModelCapabilityFlags {
        text,
        reasoning: explicit_reasoning_support(metadata),
        vision: first_bool(
            metadata,
            &["/capabilities/vision", "/vision/input", "/vision"],
        ),
        tool_calling: first_bool(
            metadata,
            &[
                "/capabilities/tool_calling",
                "/capabilities/tools",
                "/tools/supported",
            ],
        ),
        structured_output: first_bool(
            metadata,
            &[
                "/capabilities/structured_output",
                "/structured_output/supported",
            ],
        ),
    }
}

fn overlay_capability_flags(base: &mut ModelCapabilityFlags, overlay: &ModelCapabilityFlags) {
    if overlay.text.is_some() {
        base.text = overlay.text;
    }
    if overlay.reasoning.is_some() {
        base.reasoning = overlay.reasoning;
    }
    if overlay.vision.is_some() {
        base.vision = overlay.vision;
    }
    if overlay.tool_calling.is_some() {
        base.tool_calling = overlay.tool_calling;
    }
    if overlay.structured_output.is_some() {
        base.structured_output = overlay.structured_output;
    }
}

fn capability_source(
    provider: Option<bool>,
    plugin: Option<bool>,
    catalog: Option<bool>,
    catalog_source: Option<&str>,
) -> Option<String> {
    if provider.is_some() {
        Some("provider_metadata".to_string())
    } else if plugin.is_some() {
        Some("plugin_capabilities_json".to_string())
    } else if catalog.is_some() {
        catalog_source.map(str::to_string)
    } else {
        None
    }
}

fn valid_discovery_price(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn discovery_prices(metadata: &Value) -> Prices {
    let Some(prices) = metadata.get("prices") else {
        return Prices::default();
    };
    Prices {
        input_per_1m: valid_discovery_price(prices.get("input_per_1m")),
        output_per_1m: valid_discovery_price(prices.get("output_per_1m")),
        cached_per_1m: valid_discovery_price(prices.get("cached_per_1m")),
        cache_write_per_1m: valid_discovery_price(prices.get("cache_write_per_1m")),
        thinking_per_1m: valid_discovery_price(prices.get("thinking_per_1m")),
    }
}

fn overlay_prices(base: &mut Prices, overlay: &Prices) {
    if overlay.input_per_1m.is_some() {
        base.input_per_1m = overlay.input_per_1m;
    }
    if overlay.output_per_1m.is_some() {
        base.output_per_1m = overlay.output_per_1m;
    }
    if overlay.cached_per_1m.is_some() {
        base.cached_per_1m = overlay.cached_per_1m;
    }
    if overlay.cache_write_per_1m.is_some() {
        base.cache_write_per_1m = overlay.cache_write_per_1m;
    }
    if overlay.thinking_per_1m.is_some() {
        base.thinking_per_1m = overlay.thinking_per_1m;
    }
}

fn price_source(
    provider: Option<f64>,
    plugin: Option<f64>,
    catalog: Option<f64>,
    catalog_source: Option<&str>,
) -> Option<String> {
    if provider.is_some() {
        Some("provider_metadata".to_string())
    } else if plugin.is_some() {
        Some("plugin_capabilities_json".to_string())
    } else if catalog.is_some() {
        catalog_source.map(str::to_string)
    } else {
        None
    }
}

const MAX_RAW_DISCOVERY_METADATA_BYTES: usize = 4 * 1024 * 1024;

fn bounded_raw_metadata(metadata: Option<&Value>) -> (Option<Value>, bool) {
    let Some(metadata) = metadata else {
        return (None, false);
    };
    match serde_json::to_vec(metadata) {
        Ok(encoded) if encoded.len() <= MAX_RAW_DISCOVERY_METADATA_BYTES => {
            (Some(metadata.clone()), false)
        }
        Ok(_) | Err(_) => (None, true),
    }
}

fn normalized_modalities(metadata: &Value) -> Option<Value> {
    fn direction(metadata: &Value, key: &str) -> Option<Vec<String>> {
        let values = metadata.get("modalities")?.get(key)?.as_array()?;
        let mut out = Vec::new();
        for value in values {
            let Some(value) = value.as_str() else {
                continue;
            };
            let value = value.trim().to_ascii_lowercase();
            if matches!(value.as_str(), "text" | "image" | "audio" | "video" | "pdf")
                && !out.iter().any(|existing| existing == &value)
            {
                out.push(value);
            }
        }
        Some(out)
    }

    let input = direction(metadata, "input");
    let output = direction(metadata, "output");
    if input.is_none() && output.is_none() {
        None
    } else {
        Some(json!({"input": input, "output": output}))
    }
}

#[cfg(test)]
fn discovered_observation(
    model: crate::adapters::DiscoveredModel,
    provider_metadata: Option<Value>,
    fallback_metadata: Option<Value>,
    wire: WireFormat,
) -> DiscoveredObservation {
    discovered_observation_with_catalog(model, provider_metadata, fallback_metadata, wire, None)
}

fn discovered_observation_with_catalog(
    mut model: crate::adapters::DiscoveredModel,
    provider_metadata: Option<Value>,
    fallback_metadata: Option<Value>,
    wire: WireFormat,
    catalog: Option<crate::model_catalog::CatalogResolution>,
) -> DiscoveredObservation {
    fn has_reasoning_details(capability: &crate::adapters::ReasoningCapability) -> bool {
        capability.mode.is_some() || !capability.levels.is_empty() || capability.default.is_some()
    }

    let plugin_identity_metadata = fallback_metadata.as_ref().and_then(plugin_identity);
    let provider_variant = fallback_metadata.as_ref().and_then(plugin_provider_variant);
    let opaque_state = fallback_metadata
        .as_ref()
        .and_then(plugin_opaque_state_capability);
    let suppress_canonical_prices =
        wire == WireFormat::Plugin && plugin_identity_metadata.is_some();

    let catalog_layers = catalog
        .as_ref()
        .map(crate::model_catalog::CatalogResolution::layers)
        .unwrap_or_default();

    let mut catalog_flags = ModelCapabilityFlags::default();
    let mut catalog_text_source = None;
    let mut catalog_reasoning_source = None;
    let mut catalog_vision_source = None;
    let mut catalog_tools_source = None;
    let mut catalog_structured_source = None;

    let mut catalog_context_window = None;
    let mut catalog_context_source = None;
    let mut catalog_max_output_tokens = None;
    let mut catalog_max_output_source = None;
    let mut catalog_modalities = None;
    let mut catalog_model_type = None;
    let mut catalog_model_type_source = None;

    let mut catalog_prices = Prices::default();
    let mut catalog_input_price_source = None;
    let mut catalog_output_price_source = None;
    let mut catalog_cached_price_source = None;
    let mut catalog_cache_write_price_source = None;
    let mut catalog_thinking_price_source = None;

    let mut catalog_reasoning = None;
    let mut catalog_detailed_reasoning = None;

    for layer in &catalog_layers {
        let source = layer.provenance().to_string();
        let layer_flags = plugin_capability_flags_v1(layer.capabilities_json).unwrap_or_default();

        if layer_flags.text.is_some() {
            catalog_flags.text = layer_flags.text;
            catalog_text_source = Some(source.clone());
        }
        if layer_flags.reasoning.is_some() {
            catalog_flags.reasoning = layer_flags.reasoning;
            catalog_reasoning_source = Some(source.clone());
        }
        if layer_flags.vision.is_some() {
            catalog_flags.vision = layer_flags.vision;
            catalog_vision_source = Some(source.clone());
        }
        if layer_flags.tool_calling.is_some() {
            catalog_flags.tool_calling = layer_flags.tool_calling;
            catalog_tools_source = Some(source.clone());
        }
        if layer_flags.structured_output.is_some() {
            catalog_flags.structured_output = layer_flags.structured_output;
            catalog_structured_source = Some(source.clone());
        }

        if let Some(value) = layer.context_window {
            catalog_context_window = Some(value);
            catalog_context_source = Some(source.clone());
        }
        if let Some(value) = layer.max_output_tokens {
            catalog_max_output_tokens = Some(value);
            catalog_max_output_source = Some(source.clone());
        }
        if let Some(value) = layer.modalities {
            catalog_modalities = Some(value.clone());
        }
        if let Some(value) = layer.model_type {
            catalog_model_type = Some(value.to_string());
            catalog_model_type_source = Some(source.clone());
        }

        if let Some(layer_prices) = layer.prices {
            let canonical_subscription_price = suppress_canonical_prices
                && layer.kind == crate::model_catalog::CatalogLayerKind::Canonical;
            if !canonical_subscription_price {
                if layer_prices.input_per_1m.is_some() {
                    catalog_input_price_source = Some(source.clone());
                }
                if layer_prices.output_per_1m.is_some() {
                    catalog_output_price_source = Some(source.clone());
                }
                if layer_prices.cached_per_1m.is_some() {
                    catalog_cached_price_source = Some(source.clone());
                }
                if layer_prices.cache_write_per_1m.is_some() {
                    catalog_cache_write_price_source = Some(source.clone());
                }
                if layer_prices.thinking_per_1m.is_some() {
                    catalog_thinking_price_source = Some(source.clone());
                }
                overlay_prices(&mut catalog_prices, layer_prices);
            }
        }

        if let Some(mut reasoning) =
            normalize_plugin_reasoning_capability_v1(layer.capabilities_json)
        {
            if layer.kind == crate::model_catalog::CatalogLayerKind::Provider
                && wire == WireFormat::Gemini
            {
                reasoning.upstream_format = "gemini_thinking_level".to_string();
            }
            catalog_reasoning = Some((reasoning.clone(), source.clone()));
            if has_reasoning_details(&reasoning) {
                catalog_detailed_reasoning = Some((reasoning, source));
            }
        }
    }

    let plugin_flags = fallback_metadata
        .as_ref()
        .and_then(plugin_capability_flags)
        .unwrap_or_default();
    let provider_flags = provider_metadata
        .as_ref()
        .map(provider_capability_flags)
        .unwrap_or_default();

    let provider_prices = provider_metadata
        .as_ref()
        .map(discovery_prices)
        .unwrap_or_default();
    let plugin_prices = fallback_metadata
        .as_ref()
        .map(discovery_prices)
        .unwrap_or_default();
    let mut prices = catalog_prices.clone();
    overlay_prices(&mut prices, &plugin_prices);
    overlay_prices(&mut prices, &provider_prices);
    let price_sources = json!({
        "input_per_1m": price_source(
            provider_prices.input_per_1m,
            plugin_prices.input_per_1m,
            catalog_prices.input_per_1m,
            catalog_input_price_source.as_deref(),
        ),
        "output_per_1m": price_source(
            provider_prices.output_per_1m,
            plugin_prices.output_per_1m,
            catalog_prices.output_per_1m,
            catalog_output_price_source.as_deref(),
        ),
        "cached_per_1m": price_source(
            provider_prices.cached_per_1m,
            plugin_prices.cached_per_1m,
            catalog_prices.cached_per_1m,
            catalog_cached_price_source.as_deref(),
        ),
        "cache_write_per_1m": price_source(
            provider_prices.cache_write_per_1m,
            plugin_prices.cache_write_per_1m,
            catalog_prices.cache_write_per_1m,
            catalog_cache_write_price_source.as_deref(),
        ),
        "thinking_per_1m": price_source(
            provider_prices.thinking_per_1m,
            plugin_prices.thinking_per_1m,
            catalog_prices.thinking_per_1m,
            catalog_thinking_price_source.as_deref(),
        ),
    });
    let modalities = provider_metadata
        .as_ref()
        .and_then(normalized_modalities)
        .or_else(|| fallback_metadata.as_ref().and_then(normalized_modalities))
        .or(catalog_modalities);
    let (raw_metadata, raw_metadata_truncated) = bounded_raw_metadata(provider_metadata.as_ref());
    let provider_transport = provider_metadata
        .as_ref()
        .and_then(|metadata| metadata.pointer("/transport/format"))
        .and_then(Value::as_str);
    let plugin_transport = fallback_metadata
        .as_ref()
        .and_then(|metadata| metadata.pointer("/transport/format"))
        .and_then(Value::as_str);
    let (transport, transport_source) = if let Some(format) = provider_transport {
        (
            Some(format.to_string()),
            Some("provider_metadata".to_string()),
        )
    } else if let Some(format) = plugin_transport {
        (
            Some(format.to_string()),
            Some("plugin_capabilities_json".to_string()),
        )
    } else {
        (None, None)
    };

    let provider_declares_reasoning = provider_metadata
        .as_ref()
        .is_some_and(reasoning_metadata_declared);
    let provider_reasoning = provider_metadata
        .as_ref()
        .filter(|_| provider_declares_reasoning)
        .and_then(normalize_reasoning_capability);
    let provider_support = provider_reasoning
        .as_ref()
        .map(|_| true)
        .or(provider_flags.reasoning);

    let plugin_reasoning = fallback_metadata
        .as_ref()
        .and_then(normalize_plugin_reasoning_capability);
    let plugin_support = fallback_metadata
        .as_ref()
        .and_then(plugin_reasoning_support);
    let catalog_support = catalog_flags.reasoning;

    let provider_reasoning_is_authoritative =
        provider_declares_reasoning || provider_flags.reasoning.is_some();
    let (reasoning_support, support_source) = if provider_reasoning_is_authoritative {
        (provider_support, Some("provider_metadata".to_string()))
    } else if plugin_support.is_some() {
        (plugin_support, Some("plugin_capabilities_json".to_string()))
    } else {
        (catalog_support, catalog_reasoning_source.clone())
    };

    let detailed_reasoning = provider_reasoning
        .as_ref()
        .filter(|capability| has_reasoning_details(capability))
        .cloned()
        .map(|capability| (capability, "provider_metadata".to_string()))
        .or_else(|| {
            plugin_reasoning
                .as_ref()
                .filter(|capability| has_reasoning_details(capability))
                .cloned()
                .map(|capability| (capability, "plugin_capabilities_json".to_string()))
        })
        .or(catalog_detailed_reasoning);

    let supported_only_reasoning = provider_reasoning
        .clone()
        .map(|capability| (capability, "provider_metadata".to_string()))
        .or_else(|| {
            plugin_reasoning
                .clone()
                .map(|capability| (capability, "plugin_capabilities_json".to_string()))
        })
        .or(catalog_reasoning);

    let provider_blocks_fallback = provider_declares_reasoning
        && provider_reasoning.is_none()
        && provider_flags.reasoning.is_none();
    let (reasoning, reasoning_source) = if provider_blocks_fallback
        || reasoning_support == Some(false)
    {
        (None, support_source)
    } else if let Some((capability, detail_source)) =
        detailed_reasoning.or(supported_only_reasoning)
    {
        let source = match support_source.as_deref() {
            Some(support) if support != detail_source => Some(format!("{support}+{detail_source}")),
            Some(support) => Some(support.to_string()),
            None => Some(detail_source),
        };
        (Some(capability), source)
    } else {
        (None, support_source)
    };

    let context_source = if model.context_window.is_some() {
        Some("upstream_discovery".to_string())
    } else if let Some(value) = catalog_context_window {
        model.context_window = Some(value);
        catalog_context_source
    } else {
        None
    };
    let max_output_source = if model.max_output_tokens.is_some() {
        Some("upstream_discovery".to_string())
    } else if let Some(value) = catalog_max_output_tokens {
        model.max_output_tokens = Some(value);
        catalog_max_output_source
    } else {
        None
    };

    let mut capabilities = catalog_flags.clone();
    overlay_capability_flags(&mut capabilities, &plugin_flags);
    overlay_capability_flags(&mut capabilities, &provider_flags);
    capabilities.reasoning = reasoning_support;

    let capability_sources = json!({
        "context_window": context_source,
        "max_output_tokens": max_output_source,
        "text": capability_source(
            provider_flags.text,
            plugin_flags.text,
            catalog_flags.text,
            catalog_text_source.as_deref(),
        ),
        "reasoning": reasoning_source,
        "vision": capability_source(
            provider_flags.vision,
            plugin_flags.vision,
            catalog_flags.vision,
            catalog_vision_source.as_deref(),
        ),
        "tool_calling": capability_source(
            provider_flags.tool_calling,
            plugin_flags.tool_calling,
            catalog_flags.tool_calling,
            catalog_tools_source.as_deref(),
        ),
        "structured_output": capability_source(
            provider_flags.structured_output,
            plugin_flags.structured_output,
            catalog_flags.structured_output,
            catalog_structured_source.as_deref(),
        ),
        "model_type": catalog_model_type_source,
    });

    let canonical_identity = catalog.as_ref().map(|entry| entry.identity.to_json());
    let canonical_model_id = catalog
        .as_ref()
        .and_then(|entry| entry.identity.canonical_model_id.clone());
    let canonical_match = catalog.as_ref().and_then(|entry| {
        entry
            .identity
            .match_kind
            .map(crate::model_catalog::CanonicalMatchKind::label)
            .map(str::to_string)
    });
    let catalog_json = catalog
        .as_ref()
        .map(crate::model_catalog::CatalogResolution::catalog_json);
    let execution_supported = execution_supported_for_model_type(catalog_model_type.as_deref());
    let thinking_map = reasoning.as_ref().and_then(|capability| {
        if let Some(transport) = transport
            .as_deref()
            .and_then(crate::adapters::TargetTransport::parse)
        {
            crate::adapters::thinking_map_for_transport(capability, &transport)
        } else {
            thinking_map_for_reasoning_with_wire(capability, wire)
        }
    });

    DiscoveredObservation {
        model,
        reasoning_support,
        reasoning,
        thinking_map,
        capabilities,
        capability_sources,
        modalities,
        prices,
        price_sources,
        raw_metadata,
        raw_metadata_truncated,
        transport,
        transport_source,
        canonical_identity,
        canonical_model_id,
        canonical_match,
        provider_variant,
        opaque_state,
        model_type: catalog_model_type,
        execution_supported,
        catalog: catalog_json,
    }
}

fn discovered_capabilities(observation: &DiscoveredObservation) -> Value {
    json!({
        "text": observation.capabilities.text,
        "reasoning": observation.capabilities.reasoning,
        "vision": observation.capabilities.vision,
        "tool_calling": observation.capabilities.tool_calling,
        "structured_output": observation.capabilities.structured_output,
    })
}

async fn persist_model_discovery_update(
    pool: &Pool,
    row: &db::ModelRow,
    fresh: Value,
) -> anyhow::Result<()> {
    db::merge_model_discovery(pool, &row.id, &fresh).await
}

async fn persist_provider_discovery_observations(
    pool: &Pool,
    provider_id: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    db::set_setting(
        pool,
        &provider_discovery_observations_key(provider_id),
        &payload.to_string(),
    )
    .await
}

async fn load_provider_discovery_observations(
    pool: &Pool,
    provider_id: &str,
) -> anyhow::Result<Option<Value>> {
    db::get_setting(pool, &provider_discovery_observations_key(provider_id))
        .await?
        .map(|value| serde_json::from_str::<Value>(&value))
        .transpose()
        .map_err(Into::into)
}

fn discovery_object(row: &db::ModelRow) -> Value {
    serde_json::from_str::<Value>(&row.discovery).unwrap_or_else(|_| json!({}))
}

fn latest_reconciliation_observation(discovery: &Value) -> &Value {
    discovery
        .get("latest_observation")
        .filter(|value| value.is_object())
        .unwrap_or(discovery)
}

fn provenance_mentions_models_dev(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|source| source.split('+').any(|part| part.starts_with("models.dev")))
}

fn provenance_is_live_observation(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|source| {
        source.contains("provider_metadata")
            || source.contains("plugin_capabilities_json")
            || source == "upstream_discovery"
    })
}

fn set_provenance_field(target: &mut Value, field: &str, source: &Value) {
    if let Some(fields) = target.as_object_mut() {
        fields.insert(field.to_string(), source.clone());
    }
}

fn set_observed_capability(
    capabilities: &mut ModelCapabilityFlags,
    field: &str,
    value: Option<bool>,
) {
    match field {
        "text" => capabilities.text = value,
        "reasoning" => capabilities.reasoning = value,
        "vision" => capabilities.vision = value,
        "tool_calling" => capabilities.tool_calling = value,
        "structured_output" => capabilities.structured_output = value,
        _ => {}
    }
}

/// When models.dev is unavailable, absence of fresh catalog data is not
/// evidence that previously observed catalog facts disappeared. Reuse only
/// persisted models.dev-owned fields, and never override fresh provider/plugin
/// observations.
fn preserve_last_known_catalog_observation(
    row: &db::ModelRow,
    observation: &mut DiscoveredObservation,
) {
    let discovery = discovery_object(row);
    let previous = latest_reconciliation_observation(&discovery);
    let previous_sources = previous
        .get("capability_sources")
        .cloned()
        .unwrap_or_else(|| json!({}));

    for field in ["context_window", "max_output_tokens"] {
        let previous_source = previous_sources.get(field);
        let current_source = observation.capability_sources.get(field);
        if !provenance_mentions_models_dev(previous_source)
            || provenance_is_live_observation(current_source)
        {
            continue;
        }
        match field {
            "context_window" => {
                observation.model.context_window = previous.get(field).and_then(Value::as_i64);
            }
            "max_output_tokens" => {
                observation.model.max_output_tokens = previous.get(field).and_then(Value::as_i64);
            }
            _ => {}
        }
        if let Some(source) = previous_source {
            set_provenance_field(&mut observation.capability_sources, field, source);
        }
    }

    let previous_capabilities = previous
        .get("capabilities")
        .cloned()
        .unwrap_or_else(|| json!({}));
    for field in [
        "text",
        "reasoning",
        "vision",
        "tool_calling",
        "structured_output",
    ] {
        let previous_source = previous_sources.get(field);
        let current_source = observation.capability_sources.get(field);
        if !provenance_mentions_models_dev(previous_source)
            || provenance_is_live_observation(current_source)
        {
            continue;
        }
        set_observed_capability(
            &mut observation.capabilities,
            field,
            previous_capabilities.get(field).and_then(Value::as_bool),
        );
        if field == "reasoning" {
            observation.reasoning_support =
                previous_capabilities.get(field).and_then(Value::as_bool);
            observation.reasoning = previous
                .get("reasoning_capability")
                .filter(|value| !value.is_null())
                .and_then(|value| {
                    normalize_reasoning_capability(&json!({
                        "reasoning_capability": value
                    }))
                });
            observation.thinking_map = previous
                .get("thinking_map")
                .cloned()
                .and_then(|value| serde_json::from_value::<ThinkingMap>(value).ok());
        }
        if let Some(source) = previous_source {
            set_provenance_field(&mut observation.capability_sources, field, source);
        }
    }

    let previous_prices = previous
        .get("prices")
        .cloned()
        .and_then(|value| serde_json::from_value::<Prices>(value).ok())
        .unwrap_or_default();
    let previous_price_sources = previous
        .get("price_sources")
        .cloned()
        .unwrap_or_else(|| json!({}));
    for field in PRICE_FIELDS {
        let previous_source = previous_price_sources.get(field);
        let current_source = observation.price_sources.get(field);
        if !provenance_mentions_models_dev(previous_source)
            || provenance_is_live_observation(current_source)
        {
            continue;
        }
        set_price_field(
            &mut observation.prices,
            field,
            price_field(&previous_prices, field),
        );
        if let Some(source) = previous_source {
            set_provenance_field(&mut observation.price_sources, field, source);
        }
    }

    let previous_catalog_is_models_dev = previous
        .pointer("/catalog/source_state/source")
        .and_then(Value::as_str)
        == Some("models.dev");
    if previous_catalog_is_models_dev {
        if observation.modalities.is_none() {
            observation.modalities = previous.get("modalities").cloned();
        }
        let previous_model_type_source = previous_sources.get("model_type");
        let current_model_type_source = observation.capability_sources.get("model_type");
        if provenance_mentions_models_dev(previous_model_type_source)
            && !provenance_is_live_observation(current_model_type_source)
        {
            observation.model_type = previous
                .get("model_type")
                .and_then(Value::as_str)
                .map(str::to_string);
            observation.execution_supported = previous
                .get("execution_supported")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| {
                    execution_supported_for_model_type(observation.model_type.as_deref())
                });
            if let Some(source) = previous_model_type_source {
                set_provenance_field(&mut observation.capability_sources, "model_type", source);
            }
        }

        observation.catalog = previous.get("catalog").cloned().map(|mut catalog| {
            if let Some(source_state) = catalog
                .get_mut("source_state")
                .and_then(Value::as_object_mut)
            {
                source_state.insert("freshness".into(), json!("stale"));
            }
            catalog
        });
        observation.canonical_identity = previous.get("canonical_identity").cloned();
        observation.canonical_model_id = previous
            .get("canonical_model_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        observation.canonical_match = previous
            .get("canonical_match")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
}

fn metadata_marks_deprecated(value: &Value) -> bool {
    value.get("deprecated").and_then(Value::as_bool) == Some(true)
        || value
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("deprecated"))
        || value
            .pointer("/lifecycle/status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("deprecated"))
}

fn first_metadata_value(value: &Value, pointers: &[&str]) -> Option<Value> {
    pointers.iter().find_map(|pointer| {
        value
            .pointer(pointer)
            .filter(|value| !value.is_null())
            .cloned()
    })
}

fn deprecation_details(observation: &DiscoveredObservation) -> Option<Value> {
    let candidates = [
        ("provider_metadata", observation.raw_metadata.as_ref()),
        (
            "catalog_provider",
            observation
                .catalog
                .as_ref()
                .and_then(|catalog| catalog.pointer("/provider/metadata")),
        ),
        (
            "catalog_canonical",
            observation
                .catalog
                .as_ref()
                .and_then(|catalog| catalog.pointer("/canonical/metadata")),
        ),
    ];

    for (source, metadata) in candidates {
        let Some(metadata) = metadata else {
            continue;
        };
        if !metadata_marks_deprecated(metadata) {
            continue;
        }

        let mut details = serde_json::Map::new();
        details.insert("source".into(), json!(source));
        if let Some(value) = first_metadata_value(
            metadata,
            &[
                "/end_date",
                "/endDate",
                "/sunset_at",
                "/sunsetAt",
                "/lifecycle/end_date",
                "/lifecycle/endDate",
            ],
        ) {
            details.insert("end_date".into(), value);
        }
        if let Some(value) = first_metadata_value(
            metadata,
            &[
                "/effective_date",
                "/effectiveDate",
                "/deprecated_at",
                "/deprecatedAt",
                "/lifecycle/effective_date",
                "/lifecycle/effectiveDate",
            ],
        ) {
            details.insert("effective_date".into(), value);
        }
        if let Some(value) = first_metadata_value(
            metadata,
            &[
                "/replacement",
                "/replacement_model",
                "/replacementModel",
                "/replacement_model_id",
                "/replacementModelId",
                "/lifecycle/replacement",
            ],
        ) {
            details.insert("replacement".into(), value);
        }
        return Some(Value::Object(details));
    }

    None
}

fn explicit_deprecation(observation: &DiscoveredObservation) -> bool {
    deprecation_details(observation).is_some()
}

fn reconciliation_diff(row: &db::ModelRow, observation: &DiscoveredObservation) -> Vec<Value> {
    fn push_diff(
        out: &mut Vec<Value>,
        field: &str,
        configured: Value,
        observed: Value,
        source: Value,
    ) {
        if configured != observed && !observed.is_null() {
            out.push(json!({
                "field": field,
                "configured": configured,
                "observed": observed,
                "source": source,
            }));
        }
    }

    let mut diff = Vec::new();
    let discovery = discovery_object(row);
    push_diff(
        &mut diff,
        "display_name",
        json!(row.display_name),
        json!(observation.model.display_name),
        json!("upstream_discovery"),
    );
    if let Some(value) = observation.model.context_window {
        push_diff(
            &mut diff,
            "context_window",
            json!(row.context_window),
            json!(value),
            observation
                .capability_sources
                .get("context_window")
                .cloned()
                .unwrap_or(Value::Null),
        );
    }
    if let Some(value) = observation.model.max_output_tokens {
        push_diff(
            &mut diff,
            "max_output_tokens",
            json!(row.max_output_tokens),
            json!(value),
            observation
                .capability_sources
                .get("max_output_tokens")
                .cloned()
                .unwrap_or(Value::Null),
        );
    }

    let configured_caps =
        serde_json::from_str::<Value>(&row.capabilities).unwrap_or_else(|_| json!({}));
    let observed_caps = discovered_capabilities(observation);
    for field in [
        "text",
        "reasoning",
        "vision",
        "tool_calling",
        "structured_output",
    ] {
        if let Some(observed) = observed_caps.get(field).filter(|value| !value.is_null()) {
            push_diff(
                &mut diff,
                &format!("capabilities.{field}"),
                configured_caps.get(field).cloned().unwrap_or(Value::Null),
                observed.clone(),
                observation
                    .capability_sources
                    .get(field)
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
    }

    if let Some(reasoning) = observation.reasoning.as_ref() {
        push_diff(
            &mut diff,
            "reasoning_capability",
            discovery
                .get("reasoning_capability")
                .cloned()
                .unwrap_or(Value::Null),
            serde_json::to_value(reasoning).unwrap_or(Value::Null),
            observation
                .capability_sources
                .get("reasoning")
                .cloned()
                .unwrap_or(Value::Null),
        );
    }

    if let Some(modalities) = observation.modalities.as_ref() {
        push_diff(
            &mut diff,
            "modalities",
            discovery.get("modalities").cloned().unwrap_or(Value::Null),
            modalities.clone(),
            observation
                .capability_sources
                .get("modalities")
                .cloned()
                .unwrap_or_else(|| json!("discovery")),
        );
    }

    if let Some(thinking_map) = observation.thinking_map.as_ref() {
        let configured =
            serde_json::from_str::<Value>(&row.thinking_map).unwrap_or_else(|_| json!({}));
        let observed = serde_json::to_value(thinking_map).unwrap_or_else(|_| json!({}));
        push_diff(
            &mut diff,
            "thinking_map",
            configured,
            observed,
            observation
                .capability_sources
                .get("reasoning")
                .cloned()
                .unwrap_or(Value::Null),
        );
    }

    let configured_prices = row.prices();
    for (field, configured, observed) in [
        (
            "prices.input_per_1m",
            configured_prices.input_per_1m,
            observation.prices.input_per_1m,
        ),
        (
            "prices.output_per_1m",
            configured_prices.output_per_1m,
            observation.prices.output_per_1m,
        ),
        (
            "prices.cached_per_1m",
            configured_prices.cached_per_1m,
            observation.prices.cached_per_1m,
        ),
        (
            "prices.cache_write_per_1m",
            configured_prices.cache_write_per_1m,
            observation.prices.cache_write_per_1m,
        ),
        (
            "prices.thinking_per_1m",
            configured_prices.thinking_per_1m,
            observation.prices.thinking_per_1m,
        ),
    ] {
        if let Some(observed) = observed {
            let source_key = field.trim_start_matches("prices.");
            push_diff(
                &mut diff,
                field,
                configured.map(Value::from).unwrap_or(Value::Null),
                Value::from(observed),
                observation
                    .price_sources
                    .get(source_key)
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
    }

    if let Some(observed) = observation.transport.as_ref() {
        let configured = discovery
            .get("configured_transport")
            .and_then(Value::as_str)
            .or_else(|| {
                discovery
                    .pointer("/transport/format")
                    .and_then(Value::as_str)
            });
        push_diff(
            &mut diff,
            "transport",
            configured.map(Value::from).unwrap_or(Value::Null),
            json!(observed),
            json!(observation.transport_source),
        );
    }

    let pinned: std::collections::HashSet<String> = discovery
        .pointer("/reconciliation/pinned_fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    diff.retain(|item| {
        item.get("field")
            .and_then(Value::as_str)
            .is_none_or(|field| !pinned.contains(field))
    });
    diff
}

fn reconciliation_state(
    row: &db::ModelRow,
    observation: &DiscoveredObservation,
    checked_at: &str,
) -> Value {
    let discovery = discovery_object(row);
    let previous = discovery
        .get("reconciliation")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let diff = Value::Array(reconciliation_diff(row, observation));
    let ignored = previous
        .get("ignored_diff")
        .is_some_and(|value| value == &diff);
    let deprecation = deprecation_details(observation);
    let deprecated = deprecation.is_some();
    let status = if deprecated {
        "deprecated"
    } else if diff.as_array().is_none_or(Vec::is_empty) {
        "unchanged"
    } else if ignored {
        "ignored"
    } else {
        "changed"
    };

    json!({
        "status": status,
        "checked_at": checked_at,
        "last_success_at": checked_at,
        "diff": diff,
        "ignored_diff": previous.get("ignored_diff").cloned(),
        "pinned_fields": previous
            .get("pinned_fields")
            .cloned()
            .unwrap_or_else(|| json!([])),
        "deprecation": deprecation,
        "provenance": {
            "capabilities": observation.capability_sources,
            "prices": observation.price_sources,
            "transport": observation.transport_source,
        },
    })
}

fn missing_reconciliation_state(row: &db::ModelRow, checked_at: &str) -> Value {
    let discovery = discovery_object(row);
    let previous = discovery
        .get("reconciliation")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let pinned_fields = previous
        .get("pinned_fields")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let pinned: std::collections::HashSet<String> = pinned_fields
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();

    let mut diff = vec![json!({
        "field": "availability",
        "configured": "present",
        "observed": "missing",
        "source": "upstream_discovery",
    })];
    diff.retain(|item| {
        item.get("field")
            .and_then(Value::as_str)
            .is_none_or(|field| !pinned.contains(field))
    });
    let diff = Value::Array(diff);
    let ignored = !diff.as_array().is_none_or(Vec::is_empty)
        && previous
            .get("ignored_diff")
            .is_some_and(|value| value == &diff);

    json!({
        "status": if ignored { "ignored" } else { "missing" },
        "checked_at": checked_at,
        "last_success_at": checked_at,
        "diff": diff,
        "ignored_diff": previous.get("ignored_diff").cloned(),
        "pinned_fields": pinned_fields,
    })
}

fn raw_discovery_metadata<'a>(payload: &'a Value, model_id: &str) -> Option<&'a Value> {
    fn matches_model(value: &Value, model_id: &str) -> bool {
        value
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| value.get("name").and_then(Value::as_str))
            .is_some_and(|candidate| {
                candidate == model_id
                    || candidate
                        .strip_prefix("models/")
                        .is_some_and(|stripped| stripped == model_id)
            })
    }

    for key in ["data", "models"] {
        if let Some(values) = payload.get(key).and_then(Value::as_array) {
            if let Some(value) = values.iter().find(|value| matches_model(value, model_id)) {
                return Some(value);
            }
        }
    }
    payload
        .as_array()
        .and_then(|values| values.iter().find(|value| matches_model(value, model_id)))
}

fn extend_unique_by_id<T>(target: &mut Vec<T>, incoming: Vec<T>, id: impl Fn(&T) -> String) {
    let mut seen: std::collections::HashSet<String> = target.iter().map(|item| id(item)).collect();
    for item in incoming {
        if seen.insert(id(&item)) {
            target.push(item);
        }
    }
}

fn model_reconciliation_locks(
) -> &'static dashmap::DashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>> {
    static LOCKS: std::sync::OnceLock<
        dashmap::DashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
    > = std::sync::OnceLock::new();
    LOCKS.get_or_init(dashmap::DashMap::new)
}

fn model_reconciliation_lock(provider_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    model_reconciliation_locks()
        .entry(provider_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// `POST /admin/api/providers/:id/discover` — fetch the upstream model list
/// using the provider's credentials (FR-10.4).
pub(crate) async fn reconcile_provider_id(state: &AppState, id: &str) -> Result<Value, ApiError> {
    // Manual discovery and scheduled reconciliation reuse this path. Serialize
    // work per provider so overlapping refreshes cannot race observation,
    // ignore, or pin state. Pricing sync shares the same provider lock but has
    // an independent state machine and never invokes reconciliation implicitly.
    let lock = model_reconciliation_lock(id);
    let _guard = lock.lock().await;
    let provider = db::get_provider(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;

    // A provider bound to a plugin model source (§6.2) discovers through that
    // plugin instead of the built-in adapter. A bound-but-unavailable plugin
    // fails closed rather than silently falling back to native discovery
    // (§6.0).
    let (mut discovered, models_dev_available) = if let Some(pref) =
        provider.model_source_plugin_ref()
    {
        let manager = plugin_manager(&state)?;
        let reference = format!("plugin:{}/{}", pref.plugin_id, pref.capability);
        let account_aware = manager
            .resolve_binding(&reference, crate::plugins::Capability::AccountModelSource)
            .await
            .is_some();
        let legacy = manager
            .resolve_binding(&reference, crate::plugins::Capability::ModelSource)
            .await
            .is_some();
        if !account_aware && !legacy {
            return Err(ApiError::bad(format!(
                "provider is bound to unavailable plugin model source '{reference}'"
            )));
        }

        let models_path = provider.models_path.clone().unwrap_or_default();
        let list = if account_aware {
            let accounts = db::accounts_for_provider(&state.pool, &provider.id)
                .await
                .map_err(ApiError::internal)?;
            if accounts.is_empty() {
                return Err(ApiError::bad(
                    "provider has no credentials to discover with",
                ));
            }

            let mut combined = Vec::new();
            for account in accounts {
                let account_models = manager
                    .account_model_discover(
                        &pref.plugin_id,
                        &provider.id,
                        &account.id,
                        &provider.base_url,
                        &models_path,
                    )
                    .await
                    .map_err(|fault| {
                        ApiError::bad(format!(
                            "plugin model discovery failed for account '{}': {}",
                            account.label,
                            crate::crypto::redact(&fault.message())
                        ))
                    })?;
                extend_unique_by_id(&mut combined, account_models, |model| model.id.clone());
            }
            combined
        } else {
            manager
                .model_discover(
                    &pref.plugin_id,
                    &provider.id,
                    &provider.base_url,
                    &models_path,
                )
                .await
                .map_err(|fault| {
                    ApiError::bad(format!(
                        "plugin model discovery failed: {}",
                        crate::crypto::redact(&fault.message())
                    ))
                })?
        };

        let models_dev =
            crate::model_catalog::ModelsDevCatalog::fetch(&state.http, &provider.base_url).await;
        let models_dev_available = models_dev.is_some();
        let discovered = list
            .into_iter()
            .map(|m| {
                let provider_metadata = m.raw_metadata.as_deref().map(|value| {
                    serde_json::from_str::<Value>(value)
                        .unwrap_or_else(|_| Value::String(value.to_string()))
                });
                let fallback_metadata = m
                    .capabilities_json
                    .as_deref()
                    .and_then(|value| serde_json::from_str::<Value>(value).ok());
                let canonical_hint = fallback_metadata.as_ref().and_then(plugin_identity_hint);
                let catalog = crate::model_catalog::resolve_with_hint(
                    &provider.base_url,
                    &m.id,
                    canonical_hint.as_deref(),
                    models_dev.as_ref(),
                );
                discovered_observation_with_catalog(
                    crate::adapters::DiscoveredModel {
                        id: m.id,
                        display_name: m.display_name,
                        context_window: m.context_window.map(|v| v as i64),
                        max_output_tokens: m.max_output_tokens.map(|v| v as i64),
                    },
                    provider_metadata,
                    fallback_metadata,
                    reasoning_wire_context(&provider),
                    Some(catalog),
                )
            })
            .collect();
        (discovered, models_dev_available)
    } else {
        discover_models_native(&state, &provider).await?
    };

    // Mark which are already imported and record the observation (FR-10.5).
    // Discovery never overwrites admin-edited fields. Plugin opaque-state
    // provenance is host-owned and may be refreshed alongside discovery.
    let discovery_plugin_id = provider
        .model_source_plugin_ref()
        .map(|reference| reference.plugin_id);
    let existing = db::models_for_provider(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    if !models_dev_available {
        for observation in &mut discovered {
            if let Some(row) = existing
                .iter()
                .find(|row| row.upstream_id == observation.model.id)
            {
                preserve_last_known_catalog_observation(row, observation);
            }
        }
    }
    let now = db::now_iso();
    let discovered_ids: std::collections::HashSet<String> = discovered
        .iter()
        .map(|item| item.model.id.clone())
        .collect();
    let mut out: Vec<Value> = Vec::new();
    for observation in &discovered {
        let m = &observation.model;
        if let Some(row) = existing.iter().find(|e| e.upstream_id == m.id) {
            let reconciliation = reconciliation_state(row, observation, &now);
            persist_model_discovery_update(
                &state.pool,
                row,
                json!({
                    "last_seen": now,
                    "latest_observation": {
                        "observed_at": now,
                        "context_window": m.context_window,
                        "max_output_tokens": m.max_output_tokens,
                        "display_name": m.display_name,
                        "capabilities": discovered_capabilities(observation),
                        "reasoning_capability": &observation.reasoning,
                        "thinking_map": &observation.thinking_map,
                        "transport": observation
                            .transport
                            .as_ref()
                            .map(|format| json!({ "format": format })),
                        "transport_source": &observation.transport_source,
                        "capability_sources": &observation.capability_sources,
                        "modalities": &observation.modalities,
                        "prices": &observation.prices,
                        "price_sources": &observation.price_sources,
                        "raw_metadata": &observation.raw_metadata,
                        "raw_metadata_truncated": observation.raw_metadata_truncated,
                        "canonical_identity": &observation.canonical_identity,
                        "canonical_model_id": &observation.canonical_model_id,
                        "canonical_match": &observation.canonical_match,
                        "provider_variant": &observation.provider_variant,
                        "opaque_state": &observation.opaque_state,
                        "model_type": &observation.model_type,
                        "execution_supported": observation.execution_supported,
                        "catalog": &observation.catalog,
                    },
                    // Opaque-state capability is host-owned protocol metadata,
                    // not an operator model-semantic override. Keep it live so
                    // plugin continuation handling remains correct.
                    "opaque_state": &observation.opaque_state,
                    "reconciliation": &reconciliation,
                    "disappeared": false,
                }),
            )
            .await
            .map_err(ApiError::internal)?;
            if let Some(plugin_id) = discovery_plugin_id.as_deref() {
                let provenance = if observation.opaque_state.is_some() {
                    plugin_id
                } else {
                    ""
                };
                db::set_model_opaque_state_plugin(&state.pool, &row.id, provenance)
                    .await
                    .map_err(ApiError::internal)?;
            }
        }
        out.push(json!({
            "id": m.id,
            "display_name": m.display_name,
            "context_window": m.context_window,
            "max_output_tokens": m.max_output_tokens,
            "capabilities": discovered_capabilities(observation),
            "reasoning_capability": &observation.reasoning,
            "thinking_map": &observation.thinking_map,
            "transport": &observation.transport,
            "transport_source": &observation.transport_source,
            "capability_sources": &observation.capability_sources,
            "modalities": &observation.modalities,
            "prices": &observation.prices,
            "price_sources": &observation.price_sources,
            "raw_metadata": &observation.raw_metadata,
            "raw_metadata_truncated": observation.raw_metadata_truncated,
            "canonical_identity": &observation.canonical_identity,
            "canonical_model_id": &observation.canonical_model_id,
            "canonical_match": &observation.canonical_match,
            "provider_variant": &observation.provider_variant,
            "opaque_state": &observation.opaque_state,
            "model_type": &observation.model_type,
            "execution_supported": observation.execution_supported,
            "catalog": &observation.catalog,
            "reconciliation": existing
                .iter()
                .find(|row| row.upstream_id == m.id)
                .map(|row| reconciliation_state(row, observation, &now))
                .unwrap_or_else(|| json!({
                    "status": if explicit_deprecation(observation) { "deprecated" } else { "new" },
                    "checked_at": now,
                    "last_success_at": now,
                    "diff": [],
                    "provenance": {
                        "capabilities": observation.capability_sources,
                        "prices": observation.price_sources,
                        "transport": observation.transport_source,
                    },
                })),
            "already_imported": existing.iter().any(|e| e.upstream_id == m.id),
        }));
    }
    // Flag imported models that are no longer advertised upstream.
    let mut disappeared: Vec<Value> = Vec::new();
    for row in &existing {
        if discovered_ids.contains(&row.upstream_id) {
            continue;
        }
        persist_model_discovery_update(
            &state.pool,
            row,
            json!({
                "disappeared": true,
                "flagged_at": now,
                "reconciliation": missing_reconciliation_state(row, &now),
            }),
        )
        .await
        .map_err(ApiError::internal)?;
        disappeared.push(json!({
            "upstream_id": row.upstream_id,
            "display_name": row.display_name,
            "model_id": row.id,
        }));
    }
    let result = json!({ "models": out, "disappeared": disappeared });
    persist_provider_discovery_observations(&state.pool, id, &result)
        .await
        .map_err(ApiError::internal)?;
    Ok(result)
}

pub async fn cached_model_discovery(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let payload = load_provider_discovery_observations(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| json!({ "models": [], "disappeared": [] }));
    let lifecycle = provider_lifecycle_status(&state.pool, &id).await?;
    let mut payload = payload;
    payload
        .as_object_mut()
        .ok_or_else(|| ApiError::internal("invalid cached model discovery payload"))?
        .insert("lifecycle".into(), lifecycle);
    Ok(Json(payload))
}

/// Manual discovery and reconciliation share one implementation so scheduled
/// checks cannot drift from the dashboard/API behavior.
pub async fn discover_models(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    run_model_lifecycle_lane(&state, &id, ModelLifecycleLane::Reconciliation)
        .await
        .map(Json)
}

pub async fn reconcile_models(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    run_model_lifecycle_lane(&state, &id, ModelLifecycleLane::Reconciliation)
        .await
        .map(Json)
}

#[derive(Deserialize)]
pub struct ReconciliationActionBody {
    pub action: String,
    #[serde(default)]
    pub fields: Vec<String>,
}

fn remaining_reconciliation_diff(reconciliation: &Value, selected: &[String]) -> Vec<Value> {
    reconciliation
        .get("diff")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("field")
                .and_then(Value::as_str)
                .is_none_or(|field| !selected.iter().any(|candidate| candidate == field))
        })
        .cloned()
        .collect()
}

fn reconciliation_fields(reconciliation: &Value) -> Vec<String> {
    reconciliation
        .get("diff")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("field").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

const PRICE_FIELDS: [&str; 5] = [
    "input_per_1m",
    "output_per_1m",
    "cached_per_1m",
    "cache_write_per_1m",
    "thinking_per_1m",
];

fn price_field(prices: &Prices, field: &str) -> Option<f64> {
    match field {
        "input_per_1m" => prices.input_per_1m,
        "output_per_1m" => prices.output_per_1m,
        "cached_per_1m" => prices.cached_per_1m,
        "cache_write_per_1m" => prices.cache_write_per_1m,
        "thinking_per_1m" => prices.thinking_per_1m,
        _ => None,
    }
}

fn set_price_field(prices: &mut Prices, field: &str, value: Option<f64>) {
    match field {
        "input_per_1m" => prices.input_per_1m = value,
        "output_per_1m" => prices.output_per_1m = value,
        "cached_per_1m" => prices.cached_per_1m = value,
        "cache_write_per_1m" => prices.cache_write_per_1m = value,
        "thinking_per_1m" => prices.thinking_per_1m = value,
        _ => {}
    }
}

fn effective_price_fields(discovery: &Value, current: &Prices) -> serde_json::Map<String, Value> {
    let mut fields = discovery
        .pointer("/effective_pricing/fields")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let legacy_source = discovery
        .pointer("/effective_pricing/source")
        .and_then(Value::as_str)
        .unwrap_or("operator");
    let legacy_metadata = discovery
        .pointer("/effective_pricing/metadata")
        .cloned()
        .unwrap_or_else(|| json!({}));

    for field in PRICE_FIELDS {
        if price_field(current, field).is_some() && !fields.contains_key(field) {
            fields.insert(
                field.to_string(),
                json!({
                    "source": legacy_source,
                    "metadata": legacy_metadata,
                }),
            );
        }
    }
    fields
}

fn set_price_field_provenance(
    fields: &mut serde_json::Map<String, Value>,
    field: &str,
    source: &str,
    metadata: Value,
) {
    fields.insert(
        field.to_string(),
        json!({
            "source": source,
            "metadata": metadata,
        }),
    );
}

fn price_field_operator_owned(
    fields: &serde_json::Map<String, Value>,
    current: &Prices,
    field: &str,
) -> bool {
    if let Some(source) = fields
        .get(field)
        .and_then(|value| value.get("source"))
        .and_then(Value::as_str)
    {
        return !is_automatic_price_source(source);
    }
    // Legacy configured prices predate per-field provenance and are operator
    // owned. An absent value without provenance remains probe/sync-refinable.
    price_field(current, field).is_some()
}

fn has_automatic_price_observation(observed: &Prices, discovery: &Value) -> bool {
    observed.is_configured()
        || PRICE_FIELDS.iter().any(|field| {
            discovery
                .pointer(&format!("/price_sources/{field}"))
                .is_some_and(Value::is_null)
        })
}

fn effective_price_source(fields: &serde_json::Map<String, Value>, prices: &Prices) -> String {
    let sources: std::collections::BTreeSet<&str> = PRICE_FIELDS
        .iter()
        .filter(|field| price_field(prices, field).is_some())
        .filter_map(|field| {
            fields
                .get(*field)
                .and_then(|value| value.get("source"))
                .and_then(Value::as_str)
        })
        .collect();
    match sources.len() {
        0 => "untracked".into(),
        1 => sources
            .into_iter()
            .next()
            .unwrap_or("untracked")
            .to_string(),
        _ => "mixed".into(),
    }
}

fn operator_price_provenance(prices: &Prices) -> (String, Value) {
    let mut fields = serde_json::Map::new();
    for field in PRICE_FIELDS {
        if price_field(prices, field).is_some() {
            set_price_field_provenance(
                &mut fields,
                field,
                "operator",
                json!({ "configured_by": "admin" }),
            );
        }
    }
    (
        effective_price_source(&fields, prices),
        json!({ "fields": fields }),
    )
}

fn effective_price_metadata(fields: &serde_json::Map<String, Value>, observation: &Value) -> Value {
    let catalog_contributes = fields.values().any(|field| {
        field
            .get("source")
            .and_then(Value::as_str)
            .is_some_and(crate::model_catalog::is_external_catalog_price_source)
    });
    let mut metadata = json!({ "fields": fields });
    if catalog_contributes {
        metadata["catalog_source_state"] = observation
            .pointer("/catalog/source_state")
            .cloned()
            .unwrap_or(Value::Null);
    }
    metadata
}

fn catalog_provider_price_identity(observation: &Value) -> Value {
    let Some(provider) = observation
        .pointer("/catalog/provider")
        .and_then(Value::as_object)
    else {
        return Value::Null;
    };
    let mut identity = serde_json::Map::new();
    for field in ["reference", "provider_id", "model_id"] {
        if let Some(value) = provider.get(field) {
            identity.insert(field.to_string(), value.clone());
        }
    }
    if identity.is_empty() {
        Value::Null
    } else {
        Value::Object(identity)
    }
}

fn automatic_price_provenance(prices: &Prices, observation: &Value) -> (String, Value) {
    let provider_observed_at = observation.get("last_seen").cloned().unwrap_or(Value::Null);
    let catalog_source_state = observation
        .pointer("/catalog/source_state")
        .cloned()
        .unwrap_or(Value::Null);
    let catalog_observed_at = catalog_source_state
        .get("retrieved_at")
        .cloned()
        .unwrap_or(Value::Null);
    let mut fields = serde_json::Map::new();

    for field in PRICE_FIELDS {
        if price_field(prices, field).is_none() {
            continue;
        }
        let source = observation
            .pointer(&format!("/price_sources/{field}"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| automatic_price_source(observation));
        let (observed_at, source_state, catalog_provider) =
            if crate::model_catalog::is_external_catalog_price_source(&source) {
                (
                    catalog_observed_at.clone(),
                    catalog_source_state.clone(),
                    catalog_provider_price_identity(observation),
                )
            } else {
                (provider_observed_at.clone(), Value::Null, Value::Null)
            };
        set_price_field_provenance(
            &mut fields,
            field,
            &source,
            json!({
                "observed_at": observed_at,
                "catalog_source_state": source_state,
                "catalog_provider": catalog_provider,
            }),
        );
    }

    let source = effective_price_source(&fields, prices);
    let metadata = effective_price_metadata(&fields, observation);
    (source, metadata)
}

fn automatic_prices_for_provider_scope(
    prices: &Prices,
    observation: &Value,
    pricing_scope: &str,
) -> Prices {
    if pricing_scope != "integration" {
        return prices.clone();
    }
    let mut effective = prices.clone();
    for field in PRICE_FIELDS {
        if price_field(&effective, field).is_none() {
            continue;
        }
        let source = observation
            .pointer(&format!("/price_sources/{field}"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| automatic_price_source(observation));
        if crate::model_catalog::is_external_catalog_price_source(&source) {
            set_price_field(&mut effective, field, None);
        }
    }
    effective
}

fn merge_automatic_price_observation(
    current: &Prices,
    observed: &Prices,
    observation: &Value,
    ownership: &Value,
) -> (Prices, serde_json::Map<String, Value>, bool) {
    let mut effective = current.clone();
    // Ownership is stored on the model's top-level discovery envelope, while
    // fresh automatic values live under latest_observation. Keep those two
    // concerns separate so reconciliation snapshots cannot hide operator pins.
    let mut fields = effective_price_fields(ownership, current);
    let mut preserved_manual = false;
    let provider_observed_at = observation.get("last_seen").cloned().unwrap_or(Value::Null);
    let catalog_source_state = observation
        .pointer("/catalog/source_state")
        .cloned()
        .unwrap_or(Value::Null);
    let catalog_observed_at = catalog_source_state
        .get("retrieved_at")
        .cloned()
        .unwrap_or(Value::Null);

    for field in PRICE_FIELDS {
        let observed_value = price_field(observed, field);
        let authoritative_absence = observed_value.is_none()
            && observation
                .pointer(&format!("/price_sources/{field}"))
                .is_some_and(Value::is_null);
        if observed_value.is_none() && !authoritative_absence {
            continue;
        }
        if price_field_operator_owned(&fields, current, field) {
            preserved_manual = true;
            continue;
        }
        let Some(value) = observed_value else {
            set_price_field(&mut effective, field, None);
            fields.remove(field);
            continue;
        };
        set_price_field(&mut effective, field, Some(value));
        let source = observation
            .pointer(&format!("/price_sources/{field}"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| automatic_price_source(observation));
        let (observed_at, source_state, catalog_provider) =
            if crate::model_catalog::is_external_catalog_price_source(&source) {
                (
                    catalog_observed_at.clone(),
                    catalog_source_state.clone(),
                    catalog_provider_price_identity(observation),
                )
            } else {
                (provider_observed_at.clone(), Value::Null, Value::Null)
            };
        set_price_field_provenance(
            &mut fields,
            field,
            &source,
            json!({
                "observed_at": observed_at,
                "catalog_source_state": source_state,
                "catalog_provider": catalog_provider,
            }),
        );
    }
    (effective, fields, preserved_manual)
}

fn automatic_price_source(discovery: &Value) -> String {
    let mut sources = std::collections::BTreeSet::new();
    if let Some(values) = discovery.get("price_sources").and_then(Value::as_object) {
        for source in values.values().filter_map(Value::as_str) {
            sources.insert(source.to_string());
        }
    }
    if sources.len() == 1 {
        sources
            .into_iter()
            .next()
            .unwrap_or_else(|| "discovery".into())
    } else if sources.is_empty() {
        "discovery".into()
    } else {
        "mixed".into()
    }
}

fn is_automatic_price_source(source: &str) -> bool {
    matches!(
        source,
        "provider_metadata"
            | "plugin_capabilities_json"
            | "models.dev"
            | "bundled_catalog"
            | "mixed"
            | "discovery"
    ) || source.starts_with("models.dev:")
        || source.starts_with("bundled_catalog:")
}

fn merge_selected_capability_overrides(
    discovery: &Value,
    capabilities: &Value,
    selected: &[String],
) -> serde_json::Map<String, Value> {
    let Some(configured) = capabilities.as_object() else {
        return serde_json::Map::new();
    };
    let mut overrides = match discovery.get("operator_capability_overrides") {
        Some(value) => value.as_object().cloned().unwrap_or_default(),
        None => {
            // Materializing ownership for a legacy model must not silently
            // demote its other configured capabilities to probe-refinable.
            configured.clone()
        }
    };
    for field in selected {
        let Some(key) = field.strip_prefix("capabilities.") else {
            continue;
        };
        overrides.insert(
            key.to_string(),
            configured.get(key).cloned().unwrap_or(Value::Null),
        );
    }
    overrides
}

fn merge_selected_reasoning_overrides(
    discovery: &Value,
    source: &Value,
    selected: &[String],
) -> serde_json::Map<String, Value> {
    let mut overrides = discovery
        .get("operator_reasoning_overrides")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if selected.iter().any(|field| field == "reasoning_capability") {
        overrides.insert(
            "reasoning_capability".into(),
            source
                .get("reasoning_capability")
                .cloned()
                .unwrap_or(Value::Null),
        );
    }
    overrides
}

fn merge_operator_thinking_map_override(
    discovery: &Value,
    thinking_map: &Value,
) -> serde_json::Map<String, Value> {
    let mut overrides = discovery
        .get("operator_thinking_overrides")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    overrides.insert("thinking_map".into(), thinking_map.clone());
    overrides
}

fn pin_selected_price_fields(
    discovery: &Value,
    current: &Prices,
    selected: &[String],
) -> serde_json::Map<String, Value> {
    let mut fields = effective_price_fields(discovery, current);
    let pinned_at = db::now_iso();
    for field in selected {
        let Some(price_field_name) = field.strip_prefix("prices.") else {
            continue;
        };
        let previous_source = fields
            .get(price_field_name)
            .and_then(|value| value.get("source"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "untracked".to_string());
        set_price_field_provenance(
            &mut fields,
            price_field_name,
            "operator_pin",
            json!({
                "pinned_at": pinned_at,
                "previous_source": previous_source,
            }),
        );
    }
    fields
}

fn operator_parameter_support_overrides(parameters: &Value) -> Value {
    let mut overrides = serde_json::Map::new();
    if let Some(parameters) = parameters.as_object() {
        for (name, spec) in parameters {
            if let Some(supported) = spec.get("supported").and_then(Value::as_bool) {
                overrides.insert(name.clone(), Value::Bool(supported));
            }
        }
    }
    Value::Object(overrides)
}

/// Apply an explicit reconciliation decision. Observations never reach this
/// path on their own; every mutation here represents an operator action.
pub async fn update_model_reconciliation(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<ReconciliationActionBody>,
) -> ApiResult {
    let provider_id = db::get_model(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("model not found"))?
        .provider_id;
    let lock = model_reconciliation_lock(&provider_id);
    let _guard = lock.lock().await;
    let row = db::get_model(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("model not found"))?;
    let discovery = discovery_object(&row);
    let observed = latest_reconciliation_observation(&discovery).clone();
    let mut reconciliation = discovery
        .get("reconciliation")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let available = reconciliation_fields(&reconciliation);
    let selected = if body.fields.is_empty() {
        available.clone()
    } else {
        body.fields.clone()
    };
    if !body.fields.is_empty() {
        let unknown: Vec<&str> = selected
            .iter()
            .filter(|field| !available.iter().any(|candidate| candidate == *field))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(ApiError::bad(format!(
                "reconciliation fields are not currently actionable: {}",
                unknown.join(", ")
            )));
        }
    }

    match body.action.as_str() {
        "ignore" => {
            let current = reconciliation
                .get("diff")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let object = reconciliation
                .as_object_mut()
                .ok_or_else(|| ApiError::internal("invalid reconciliation metadata"))?;
            object.insert("ignored_diff".into(), current);
            object.insert("status".into(), json!("ignored"));
            object.insert("decision_at".into(), json!(db::now_iso()));
        }
        "pin" => {
            let pins = if selected.is_empty() {
                return Err(ApiError::bad("pin requires at least one observed field"));
            } else {
                selected
            };
            let mut merged: std::collections::BTreeSet<String> = reconciliation
                .get("pinned_fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            for field in &pins {
                if available.iter().any(|candidate| candidate == field) {
                    merged.insert(field.clone());
                }
            }
            let remaining = remaining_reconciliation_diff(&reconciliation, &pins);
            let object = reconciliation
                .as_object_mut()
                .ok_or_else(|| ApiError::internal("invalid reconciliation metadata"))?;
            object.insert(
                "pinned_fields".into(),
                Value::Array(merged.into_iter().map(Value::String).collect()),
            );
            object.insert("diff".into(), Value::Array(remaining.clone()));
            object.insert(
                "status".into(),
                json!(if remaining.is_empty() {
                    "unchanged"
                } else {
                    "changed"
                }),
            );
            object.insert("decision_at".into(), json!(db::now_iso()));

            let configured_capabilities =
                serde_json::from_str::<Value>(&row.capabilities).unwrap_or_else(|_| json!({}));
            let parameters =
                serde_json::from_str::<Value>(&row.parameters).unwrap_or_else(|_| json!({}));
            let thinking_map = serde_json::to_value(row.thinking()).map_err(ApiError::internal)?;
            let extra_request = row.extra_request_value();
            let current_prices = row.prices();
            let has_capability_pins = pins.iter().any(|field| field.starts_with("capabilities."));
            let has_reasoning_pins = pins.iter().any(|field| field == "reasoning_capability");
            let has_thinking_map_pin = pins.iter().any(|field| field == "thinking_map");
            let has_price_pins = pins.iter().any(|field| field.starts_with("prices."));

            let mut discovery_patch = serde_json::Map::new();
            discovery_patch.insert("reconciliation".into(), reconciliation.clone());
            if has_capability_pins {
                let overrides = merge_selected_capability_overrides(
                    &discovery,
                    &configured_capabilities,
                    &pins,
                );
                discovery_patch.insert(
                    "operator_capability_overrides".into(),
                    Value::Object(overrides),
                );
            }
            if has_reasoning_pins {
                let overrides = merge_selected_reasoning_overrides(&discovery, &discovery, &pins);
                discovery_patch.insert(
                    "operator_reasoning_overrides".into(),
                    Value::Object(overrides),
                );
            }
            if has_thinking_map_pin {
                discovery_patch.insert(
                    "operator_thinking_overrides".into(),
                    Value::Object(merge_operator_thinking_map_override(
                        &discovery,
                        &thinking_map,
                    )),
                );
            }

            let mut pinned_price_source = String::new();
            let mut pinned_price_metadata = Value::Null;
            if has_price_pins {
                let fields = pin_selected_price_fields(&discovery, &current_prices, &pins);
                pinned_price_source = if current_prices.is_configured() {
                    effective_price_source(&fields, &current_prices)
                } else {
                    "operator_pin".to_string()
                };
                pinned_price_metadata = json!({ "fields": fields });
            }
            let pricing = if has_price_pins {
                Some(db::ModelPricingMutation {
                    prices: &current_prices,
                    source: &pinned_price_source,
                    metadata: &pinned_price_metadata,
                })
            } else {
                None
            };
            let discovery_patch = Value::Object(discovery_patch);
            db::commit_model_operator_mutation(
                &state.pool,
                &db::ModelOperatorMutation {
                    id: &id,
                    display_name: &row.display_name,
                    enabled: row.enabled != 0,
                    context_window: row.context_window,
                    max_output_tokens: row.max_output_tokens,
                    capabilities: &configured_capabilities,
                    parameters: &parameters,
                    thinking_map: &thinking_map,
                    extra_request: &extra_request,
                    update_transport: false,
                    transport: None,
                    discovery_patch: &discovery_patch,
                    pricing,
                },
            )
            .await
            .map_err(ApiError::internal)?;
            state
                .registry
                .reload(&state.pool)
                .await
                .map_err(ApiError::internal)?;
            return Ok(Json(json!({ "ok": true })));
        }
        "accept" => {
            let safe_default: Vec<String> = available
                .iter()
                .filter(|field| field.as_str() != "transport")
                .cloned()
                .collect();
            let selected = if body.fields.is_empty() {
                safe_default
            } else {
                selected
            };

            let mut display_name = row.display_name.clone();
            let mut context_window = row.context_window;
            let mut max_output_tokens = row.max_output_tokens;
            let mut capabilities =
                serde_json::from_str::<Value>(&row.capabilities).unwrap_or_else(|_| json!({}));
            if !capabilities.is_object() {
                capabilities = json!({});
            }
            let parameters =
                serde_json::from_str::<Value>(&row.parameters).unwrap_or_else(|_| json!({}));
            let mut thinking_map = row.thinking();
            let extra_request = row.extra_request_value();
            let mut prices = row.prices();
            let mut accepted_prices = false;
            let mut accepted_transport: Option<String> = None;
            let mut accepted_discovery = serde_json::Map::new();

            for field in &selected {
                match field.as_str() {
                    "display_name" => {
                        if let Some(value) = observed.get("display_name").and_then(Value::as_str) {
                            display_name = value.to_string();
                        }
                    }
                    "context_window" => {
                        if let Some(value) = observed.get("context_window").and_then(Value::as_i64)
                        {
                            context_window = Some(value);
                        }
                    }
                    "max_output_tokens" => {
                        if let Some(value) =
                            observed.get("max_output_tokens").and_then(Value::as_i64)
                        {
                            max_output_tokens = Some(value);
                        }
                    }
                    "thinking_map" => {
                        if let Some(value) = observed.get("thinking_map") {
                            if let Ok(parsed) = serde_json::from_value::<ThinkingMap>(value.clone())
                            {
                                validate_thinking_map(&parsed)?;
                                thinking_map = parsed;
                            }
                        }
                    }
                    "reasoning_capability" | "modalities" => {
                        if let Some(value) = observed.get(field) {
                            accepted_discovery.insert(field.clone(), value.clone());
                        }
                    }
                    "transport" => {
                        accepted_transport = observed
                            .pointer("/transport/format")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    field if field.starts_with("capabilities.") => {
                        let key = field.trim_start_matches("capabilities.");
                        if let Some(value) = observed
                            .pointer(&format!("/capabilities/{key}"))
                            .and_then(Value::as_bool)
                        {
                            capabilities
                                .as_object_mut()
                                .expect("capabilities normalized to object")
                                .insert(key.to_string(), Value::Bool(value));
                        }
                    }
                    field if field.starts_with("prices.") => {
                        let key = field.trim_start_matches("prices.");
                        let value = observed
                            .pointer(&format!("/prices/{key}"))
                            .and_then(Value::as_f64);
                        match key {
                            "input_per_1m" => prices.input_per_1m = value,
                            "output_per_1m" => prices.output_per_1m = value,
                            "cached_per_1m" => prices.cached_per_1m = value,
                            "cache_write_per_1m" => prices.cache_write_per_1m = value,
                            "thinking_per_1m" => prices.thinking_per_1m = value,
                            _ => {}
                        }
                        accepted_prices = true;
                    }
                    _ => {}
                }
            }

            let transport_update = if let Some(transport) = accepted_transport.as_deref() {
                let provider = db::get_provider(&state.pool, &row.provider_id)
                    .await
                    .map_err(ApiError::internal)?
                    .ok_or_else(|| ApiError::not_found("provider not found"))?;
                Some(validate_model_transport_override(
                    &provider,
                    Some(transport),
                )?)
            } else {
                None
            };

            if selected
                .iter()
                .any(|field| field.starts_with("capabilities."))
            {
                let capability_overrides =
                    merge_selected_capability_overrides(&discovery, &capabilities, &selected);
                accepted_discovery.insert(
                    "operator_capability_overrides".into(),
                    Value::Object(capability_overrides),
                );
            }
            if selected.iter().any(|field| field == "reasoning_capability") {
                let reasoning_overrides =
                    merge_selected_reasoning_overrides(&discovery, &observed, &selected);
                accepted_discovery.insert(
                    "operator_reasoning_overrides".into(),
                    Value::Object(reasoning_overrides),
                );
            }
            if selected.iter().any(|field| field == "thinking_map") {
                let accepted_thinking_map =
                    serde_json::to_value(&thinking_map).map_err(ApiError::internal)?;
                accepted_discovery.insert(
                    "operator_thinking_overrides".into(),
                    Value::Object(merge_operator_thinking_map_override(
                        &discovery,
                        &accepted_thinking_map,
                    )),
                );
            }

            let remaining = remaining_reconciliation_diff(&reconciliation, &selected);
            let object = reconciliation
                .as_object_mut()
                .ok_or_else(|| ApiError::internal("invalid reconciliation metadata"))?;
            object.insert(
                "status".into(),
                json!(if remaining.is_empty() {
                    "accepted"
                } else {
                    "changed"
                }),
            );
            object.insert("diff".into(), Value::Array(remaining));
            object.remove("ignored_diff");
            object.insert("decision_at".into(), json!(db::now_iso()));
            accepted_discovery.insert("reconciliation".into(), reconciliation.clone());

            let mut price_source = String::new();
            let mut price_metadata = Value::Null;
            if accepted_prices {
                let mut fields = effective_price_fields(&discovery, &row.prices());
                for selected_field in selected
                    .iter()
                    .filter_map(|field| field.strip_prefix("prices."))
                {
                    let accepted_from = observed
                        .pointer(&format!("/price_sources/{selected_field}"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    set_price_field_provenance(
                        &mut fields,
                        selected_field,
                        "operator_accept",
                        json!({ "accepted_from": accepted_from }),
                    );
                }
                price_source = if prices.is_configured() {
                    effective_price_source(&fields, &prices)
                } else {
                    "operator_accept".to_string()
                };
                price_metadata = json!({ "fields": fields });
            }
            let pricing = if accepted_prices {
                Some(db::ModelPricingMutation {
                    prices: &prices,
                    source: &price_source,
                    metadata: &price_metadata,
                })
            } else {
                None
            };
            let capabilities = normalize_model_capabilities(&capabilities);
            let thinking_map = serde_json::to_value(&thinking_map).map_err(ApiError::internal)?;
            let discovery_patch = Value::Object(accepted_discovery);
            db::commit_model_operator_mutation(
                &state.pool,
                &db::ModelOperatorMutation {
                    id: &id,
                    display_name: &display_name,
                    enabled: row.enabled != 0,
                    context_window,
                    max_output_tokens,
                    capabilities: &capabilities,
                    parameters: &parameters,
                    thinking_map: &thinking_map,
                    extra_request: &extra_request,
                    update_transport: transport_update.is_some(),
                    transport: transport_update
                        .as_ref()
                        .and_then(|transport| transport.as_deref()),
                    discovery_patch: &discovery_patch,
                    pricing,
                },
            )
            .await
            .map_err(ApiError::internal)?;
            state
                .registry
                .reload(&state.pool)
                .await
                .map_err(ApiError::internal)?;
            return Ok(Json(json!({ "ok": true })));
        }
        other => {
            return Err(ApiError::bad(format!(
                "unsupported reconciliation action '{other}'"
            )))
        }
    }

    db::merge_model_discovery(
        &state.pool,
        &id,
        &json!({ "reconciliation": reconciliation }),
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

fn models_dev_pricing_patch(
    discovery: &Value,
    resolution: &crate::model_catalog::CatalogResolution,
    direct_api_pricing_eligible: bool,
) -> Value {
    let mut observed: Prices = discovery
        .get("prices")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let mut sources = discovery
        .get("price_sources")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let provider_prices = direct_api_pricing_eligible
        .then_some(resolution.provider.as_ref())
        .flatten()
        .filter(|provider| provider.source == crate::model_catalog::CatalogSource::ModelsDev);

    for field in PRICE_FIELDS {
        let existing_source = sources.get(field).and_then(Value::as_str);
        if matches!(
            existing_source,
            Some("provider_metadata" | "plugin_capabilities_json")
        ) {
            continue;
        }

        let value = provider_prices.and_then(|provider| price_field(&provider.prices, field));
        if let Some(value) = value {
            set_price_field(&mut observed, field, Some(value));
            sources.insert(
                field.to_string(),
                Value::String("models.dev:provider".to_string()),
            );
        } else if existing_source.is_some_and(|source| source.starts_with("models.dev")) {
            set_price_field(&mut observed, field, None);
            sources.insert(field.to_string(), Value::Null);
        }
    }

    json!({
        "prices": observed,
        "price_sources": Value::Object(sources),
        "catalog": resolution.catalog_json(),
    })
}

fn apply_top_level_discovery_patch(discovery: &mut Value, patch: &Value) {
    let Some(target) = discovery.as_object_mut() else {
        *discovery = patch.clone();
        return;
    };
    let Some(fields) = patch.as_object() else {
        return;
    };
    for (key, value) in fields {
        target.insert(key.clone(), value.clone());
    }
}

async fn run_if_pricing_refresh_committable<T>(
    outcome: crate::model_catalog::ModelsDevRefreshOutcome,
    operation: impl std::future::Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    if matches!(
        outcome,
        crate::model_catalog::ModelsDevRefreshOutcome::StaleFallback
    ) {
        return Err(ApiError::bad(
            "models.dev pricing refresh failed; stale cached catalog preserved last-known observations and effective prices",
        ));
    }
    operation.await
}

/// Explicit pricing synchronization. This lane refreshes models.dev pricing
/// independently of provider/plugin discovery and never runs reconciliation.
/// Existing provider/plugin observations retain precedence; manual/operator-owned
/// effective fields always win.
struct StagedProviderPricing {
    model_id: String,
    latest_observation: Value,
    effective: Option<Prices>,
    source: String,
    metadata: Value,
}

async fn apply_provider_pricing_sync(
    state: &AppState,
    provider: &db::ProviderRow,
    models_dev: &crate::model_catalog::ModelsDevCatalog,
) -> Result<Value, ApiError> {
    let models = db::models_for_provider(&state.pool, &provider.id)
        .await
        .map_err(ApiError::internal)?;
    let pricing_scope = db::provider_pricing_scope(&state.pool, &provider.id)
        .await
        .map_err(ApiError::internal)?;
    let direct_api_pricing_eligible = pricing_scope == "direct_api";
    let mut staged = Vec::with_capacity(models.len());
    let mut updated = Vec::new();
    let mut skipped_manual = Vec::new();

    for row in models {
        let discovery = discovery_object(&row);
        let mut observation = latest_reconciliation_observation(&discovery).clone();
        let resolution =
            crate::model_catalog::resolve(&provider.base_url, &row.upstream_id, Some(models_dev));
        let pricing_patch =
            models_dev_pricing_patch(&observation, &resolution, direct_api_pricing_eligible);
        apply_top_level_discovery_patch(&mut observation, &pricing_patch);

        let observed: Prices = observation
            .get("prices")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        let observed = automatic_prices_for_provider_scope(&observed, &observation, &pricing_scope);

        if !has_automatic_price_observation(&observed, &observation) {
            staged.push(StagedProviderPricing {
                model_id: row.id,
                latest_observation: observation,
                effective: None,
                source: String::new(),
                metadata: Value::Null,
            });
            continue;
        }

        let current = row.prices();
        let (effective, fields, preserved_manual) =
            merge_automatic_price_observation(&current, &observed, &observation, &discovery);
        if preserved_manual {
            skipped_manual.push(row.id.clone());
        }
        let source = effective_price_source(&fields, &effective);
        let metadata = effective_price_metadata(&fields, &observation);
        updated.push(row.id.clone());
        staged.push(StagedProviderPricing {
            model_id: row.id,
            latest_observation: observation,
            effective: Some(effective),
            source,
            metadata,
        });
    }

    let mutations: Vec<db::ProviderPricingMutation<'_>> = staged
        .iter()
        .map(|stage| db::ProviderPricingMutation {
            model_id: &stage.model_id,
            latest_observation: &stage.latest_observation,
            pricing: stage
                .effective
                .as_ref()
                .map(|prices| db::ModelPricingMutation {
                    prices,
                    source: &stage.source,
                    metadata: &stage.metadata,
                }),
        })
        .collect();
    db::commit_provider_pricing_batch(&state.pool, &mutations)
        .await
        .map_err(ApiError::internal)?;

    if !staged.is_empty() {
        if let Err(error) = state.registry.reload(&state.pool).await {
            tracing::warn!(
                provider = %provider.id,
                %error,
                "pricing sync committed but immediate registry activation failed; background reload will retry"
            );
        }
    }

    Ok(json!({
        "ok": true,
        "updated": updated,
        "skipped_manual": skipped_manual,
    }))
}

pub(crate) async fn sync_provider_pricing_id(
    state: &AppState,
    id: &str,
) -> Result<Value, ApiError> {
    let lock = model_reconciliation_lock(id);
    let _guard = lock.lock().await;

    let provider = db::get_provider(&state.pool, id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    let models_dev_fetch =
        crate::model_catalog::ModelsDevCatalog::fetch_with_outcome(&state.http, &provider.base_url)
            .await
            .ok_or_else(|| {
                ApiError::bad(
                    "models.dev pricing refresh unavailable; preserving last-known observations and effective prices",
                )
            })?;
    let outcome = models_dev_fetch.outcome;
    let models_dev = models_dev_fetch.catalog;

    run_if_pricing_refresh_committable(
        outcome,
        apply_provider_pricing_sync(state, &provider, &models_dev),
    )
    .await
}

pub async fn sync_provider_pricing(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    run_model_lifecycle_lane(&state, &id, ModelLifecycleLane::PricingSync)
        .await
        .map(Json)
}

async fn credential_for_admin_action(
    state: &AppState,
    provider: &db::ProviderRow,
    account: &db::AccountRow,
    context: &'static str,
) -> Result<String, ApiError> {
    match state.credential_for(provider, account).await {
        Ok(credential) => {
            crate::alerts::record_credential_success();
            Ok(credential.secret)
        }
        Err(error) => {
            crate::alerts::record_credential_failure();
            if let Err(disable_error) = state
                .disable_invalid_credential(account, &error, context)
                .await
            {
                tracing::error!(
                    provider = %provider.id,
                    account = %account.id,
                    credential_error = %error,
                    %disable_error,
                    "failed to disable account after admin credential resolution confirmed invalid"
                );
                return Err(ApiError::internal(disable_error));
            }
            Err(ApiError::internal(error))
        }
    }
}

/// Built-in adapter discovery: resolve a credential, call the provider's models
/// endpoint, and parse the list. Extracted so the plugin path can share the
/// surrounding import/flag logic.
async fn discover_models_native(
    state: &AppState,
    provider: &db::ProviderRow,
) -> Result<(Vec<DiscoveredObservation>, bool), ApiError> {
    let accounts = db::accounts_for_provider(&state.pool, &provider.id)
        .await
        .map_err(ApiError::internal)?;
    if accounts.is_empty() {
        return Err(ApiError::bad(
            "provider has no credentials to discover with",
        ));
    }

    let adapter = state.adapters.for_provider(provider);
    let path = provider
        .models_path
        .clone()
        .unwrap_or_else(|| adapter.default_models_path().to_string());
    let base = provider.base_url.trim_end_matches('/');
    let url = if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    };
    let parsed_url =
        url::Url::parse(&url).map_err(|e| ApiError::bad(format!("invalid discovery URL: {e}")))?;
    let models_dev =
        crate::model_catalog::ModelsDevCatalog::fetch(&state.http, &provider.base_url).await;
    let models_dev_available = models_dev.is_some();
    let dummy_model = db::ModelRow {
        id: "discovery".into(),
        provider_id: provider.id.clone(),
        upstream_id: "discovery".into(),
        display_name: "discovery".into(),
        enabled: 1,
        context_window: None,
        max_output_tokens: None,
        capabilities: "{}".into(),
        prices: "{}".into(),
        parameters: "{}".into(),
        thinking_map: "{}".into(),
        extra_request: "{}".into(),
        discovery: "{}".into(),
        created_at: db::now_iso(),
        opaque_state_plugin: String::new(),
    };

    let mut discovered = Vec::new();
    for account in accounts {
        let credential = credential_for_admin_action(
            state,
            provider,
            &account,
            "native model discovery credential resolution",
        )
        .await?;
        let ctx = UpstreamContext {
            provider,
            model: &dummy_model,
            account_id: Some(account.id.as_str()),
            credential,
        };
        let resp = crate::outbound::send_provider_request(
            &state.outbound_clients,
            state.config.allow_private_upstreams,
            state.config.allow_insecure_tls,
            &adapter,
            &ctx,
            crate::outbound::ProviderRequest {
                method: reqwest::Method::GET,
                url: parsed_url.clone(),
                json_body: None,
                accept_event_stream: false,
                request_id: None,
                headers: Vec::new(),
                total_timeout: Some(std::time::Duration::from_millis(
                    provider.timeout_ms.max(1) as u64
                )),
            },
        )
        .await
        .map_err(|e| {
            ApiError::bad(format!(
                "discovery request failed for account '{}': {}",
                account.label,
                crate::crypto::redact(&e.message)
            ))
        })?;
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ApiError::bad(format!(
                "upstream returned HTTP {} for account '{}': {}",
                status.as_u16(),
                account.label,
                crate::crypto::redact(&truncate(&body_text, 400))
            )));
        }
        let parsed: Value = serde_json::from_str(&body_text)
            .map_err(|e| ApiError::bad(format!("invalid discovery response: {e}")))?;
        let account_observations = adapter
            .parse_model_list(&parsed)
            .into_iter()
            .map(|model| {
                let provider_metadata = raw_discovery_metadata(&parsed, &model.id).cloned();
                let catalog = crate::model_catalog::resolve(
                    &provider.base_url,
                    &model.id,
                    models_dev.as_ref(),
                );
                discovered_observation_with_catalog(
                    model,
                    provider_metadata,
                    None,
                    reasoning_wire_context(provider),
                    Some(catalog),
                )
            })
            .collect();
        extend_unique_by_id(
            &mut discovered,
            account_observations,
            |observation: &DiscoveredObservation| observation.model.id.clone(),
        );
    }

    Ok((discovered, models_dev_available))
}

/// `POST /admin/api/providers/:id/test` — send a minimal probe (FR-10.11).
pub async fn test_provider(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<TestBody>,
) -> ApiResult {
    let provider = db::get_provider(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    let account = if let Some(account_id) = body.account_id.as_deref() {
        let account = db::get_account(&state.pool, account_id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("account not found"))?;
        if account.provider_id != id {
            return Err(ApiError::bad("account does not belong to provider"));
        }
        account
    } else {
        db::accounts_for_provider(&state.pool, &id)
            .await
            .map_err(ApiError::internal)?
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::bad("provider has no credentials to test with"))?
    };
    let credential = credential_for_admin_action(
        &state,
        &provider,
        &account,
        "provider test credential resolution",
    )
    .await?;

    let upstream_id = if let Some(model) = body.model.clone().filter(|m| !m.trim().is_empty()) {
        model
    } else {
        db::models_for_provider(&state.pool, &id)
            .await
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|model| model.enabled != 0)
            .map(|model| model.upstream_id)
            .ok_or_else(|| ApiError::bad("provider has no enabled model to test"))?
    };
    let model = db::find_model_by_upstream(&state.pool, &id, &upstream_id)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| db::ModelRow {
            id: "probe".into(),
            provider_id: id.clone(),
            upstream_id: upstream_id.clone(),
            display_name: upstream_id.clone(),
            enabled: 1,
            context_window: None,
            max_output_tokens: Some(64),
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: "{}".into(),
            thinking_map: "{}".into(),
            extra_request: "{}".into(),
            discovery: "{}".into(),
            created_at: db::now_iso(),
            opaque_state_plugin: String::new(),
        });

    let profile = crate::adapters::resolve_execution_profile_for_target(
        &provider,
        &model,
        Some(account.id.as_str()),
    )
    .map_err(|error| ApiError::bad(error.message))?;
    let adapter = state
        .adapters
        .for_transport(&profile.transport)
        .map_err(|error| ApiError::bad(error.message))?;
    let ctx = UpstreamContext {
        provider: &provider,
        model: &model,
        account_id: Some(account.id.as_str()),
        credential,
    };
    let mut internal = crate::types::InternalRequest {
        requested_model: upstream_id.clone(),
        system: vec![],
        messages: vec![crate::types::Message {
            role: crate::types::Role::User,
            parts: vec![crate::types::Part::Text(
                "Reply with the single word: ok".into(),
            )],
        }],
        tools: vec![],
        tool_choice: None,
        tool_choice_name: None,
        params: crate::types::SamplingParams {
            max_tokens: Some(64),
            ..Default::default()
        },
        stream: false,
        include_usage: false,
        thinking: None,
        extra: Default::default(),
        raw_body: None,
    };
    internal.stream = false;

    let url = adapter
        .build_url(&ctx)
        .map_err(|e| ApiError::bad(e.message))?;
    let outbound = adapter
        .build_body(&ctx, &internal)
        .map_err(|e| ApiError::internal(e.message))?;
    let parsed_url =
        url::Url::parse(&url).map_err(|e| ApiError::bad(format!("invalid probe URL: {e}")))?;

    let started = std::time::Instant::now();
    match crate::outbound::send_provider_request(
        &state.outbound_clients,
        state.config.allow_private_upstreams,
        state.config.allow_insecure_tls,
        &adapter,
        &ctx,
        crate::outbound::ProviderRequest {
            method: reqwest::Method::POST,
            url: parsed_url,
            json_body: Some(outbound),
            accept_event_stream: false,
            request_id: None,
            headers: Vec::new(),
            total_timeout: Some(std::time::Duration::from_millis(
                provider.timeout_ms.max(1) as u64
            )),
        },
    )
    .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let latency = started.elapsed().as_millis() as i64;
            if !(200..300).contains(&status) {
                let text = resp.text().await.unwrap_or_default();
                let native = adapter.classify_error(status, &text, &axum::http::HeaderMap::new());
                let failure =
                    crate::pipeline::apply_provider_failure_rules(&provider, status, &text, native);
                return Ok(Json(json!({
                    "ok": false, "status": status, "latency_ms": latency,
                    "error": failure.message,
                })));
            }
            // Read a bounded amount of the (possibly streaming) response so a
            // probe never hangs on a long-lived SSE connection (FR-10.11).
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let preview = read_probe_preview(resp, content_type.contains("event-stream")).await;
            Ok(Json(json!({
                "ok": true, "status": status, "latency_ms": latency,
                "response_preview": truncate(&preview, 400),
            })))
        }
        Err(e) => Ok(Json(json!({
            "ok": false, "status": 0,
            "error": e.message,
        }))),
    }
}

/// Read a bounded probe response. For SSE, parse the frames and return the
/// concatenated text deltas; otherwise return the (bounded) body text.
async fn read_probe_preview(resp: reqwest::Response, sse: bool) -> String {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut framer = crate::sse::SseFramer::new();
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let next = tokio::time::timeout_at(deadline, stream.next()).await;
        let chunk = match next {
            Ok(Some(Ok(c))) => c,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break, // bounded read; a probe must not hang
        };
        if !sse {
            buf.extend_from_slice(&chunk);
            if buf.len() > 8192 {
                break;
            }
            continue;
        }
        let frames = match framer.push(&chunk) {
            Ok(frames) => frames,
            Err(_) => break,
        };
        for frame in frames {
            if let Some(data) = crate::sse::extract_data(&frame) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
                    // OpenAI shape (text, then reasoning as a fallback label)
                    if let Some(c) = v
                        .pointer("/choices/0/delta/content")
                        .and_then(|c| c.as_str())
                    {
                        text.push_str(c);
                    } else if let Some(r) = v
                        .pointer("/choices/0/delta/reasoning_content")
                        .and_then(|c| c.as_str())
                    {
                        if !r.is_empty() && !text.starts_with("[reasoning] ") {
                            text = format!("[reasoning] {text}");
                        }
                    }
                    // Gemini shape
                    if let Some(parts) = v
                        .pointer("/candidates/0/content/parts")
                        .and_then(|p| p.as_array())
                    {
                        for p in parts {
                            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                                text.push_str(t);
                            }
                        }
                    }
                }
            }
        }
        if !text.trim_start_matches("[reasoning] ").is_empty() || buf.len() > 8192 {
            break;
        }
    }
    if sse {
        if text.is_empty() {
            "[streaming response: no text delta within the probe window]".to_string()
        } else {
            text
        }
    } else {
        String::from_utf8_lossy(&buf).to_string()
    }
}

#[derive(Deserialize)]
pub struct TestBody {
    pub model: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
}

#[derive(Deserialize)]
pub struct CapabilityProbeBody {
    #[serde(default)]
    pub account_id: Option<String>,
    pub capability: String,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    #[serde(default)]
    pub transport: Option<String>,
}

fn probe_execution_model(
    provider: &db::ProviderRow,
    model: &db::ModelRow,
    candidate_transport: Option<&str>,
) -> Result<db::ModelRow, ApiError> {
    let Some(transport) = validate_model_transport_override(provider, candidate_transport)? else {
        return Ok(model.clone());
    };
    let mut execution_model = model.clone();
    let mut discovery = discovery_object(&execution_model);
    if !discovery.is_object() {
        discovery = json!({});
    }
    discovery
        .as_object_mut()
        .expect("probe discovery normalized to object")
        .insert("configured_transport".into(), Value::String(transport));
    execution_model.discovery = discovery.to_string();
    Ok(execution_model)
}

fn probe_thinking_level(value: Option<&Value>) -> Result<crate::types::ThinkingLevel, ApiError> {
    let level = value.and_then(Value::as_str).unwrap_or("low");
    match level {
        "off" | "none" => Ok(crate::types::ThinkingLevel::Off),
        "default" => Ok(crate::types::ThinkingLevel::Default),
        "minimal" => Ok(crate::types::ThinkingLevel::Minimal),
        "low" => Ok(crate::types::ThinkingLevel::Low),
        "medium" => Ok(crate::types::ThinkingLevel::Medium),
        "high" => Ok(crate::types::ThinkingLevel::High),
        "xhigh" => Ok(crate::types::ThinkingLevel::XHigh),
        "max" => Ok(crate::types::ThinkingLevel::Max),
        other => Err(ApiError::bad(format!(
            "unsupported canonical reasoning level '{other}'"
        ))),
    }
}

fn reasoning_disable_probe_thinking_map(
    transport: &crate::adapters::TargetTransport,
) -> Option<ThinkingMap> {
    let level_field = match transport {
        crate::adapters::TargetTransport::OpenAiChat => "reasoning_effort",
        crate::adapters::TargetTransport::OpenAiResponses => "reasoning.effort",
        _ => return None,
    };
    Some(ThinkingMap {
        levels: [("off".to_string(), json!("none"))].into_iter().collect(),
        mode: Some(crate::types::ThinkingMode::Level),
        budget_field: None,
        level_field: Some(level_field.to_string()),
    })
}

fn normalize_capability_probe_value(
    capability: &str,
    value: Option<&Value>,
) -> Result<Option<Value>, ApiError> {
    if capability != "reasoning_disable" {
        return Ok(value.cloned());
    }

    match value {
        None => Ok(Some(json!("off"))),
        Some(Value::String(level)) => match level.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Ok(Some(json!("off"))),
            _ => Err(ApiError::bad(
                "reasoning_disable probe only accepts value 'off'/'none' or an omitted value",
            )),
        },
        Some(_) => Err(ApiError::bad(
            "reasoning_disable probe only accepts value 'off'/'none' or an omitted value",
        )),
    }
}

fn numeric_probe_budget(value: &Value) -> Option<u64> {
    match value {
        Value::Number(value) => value.as_u64(),
        Value::Object(fields) => {
            let mut found = false;
            let mut total = 0_u64;
            for value in fields.values() {
                if let Some(bound) = numeric_probe_budget(value) {
                    found = true;
                    total = total.saturating_add(bound);
                }
            }
            found.then_some(total)
        }
        _ => None,
    }
}

const PROBE_RESPONSE_ALLOWANCE_TOKENS: u64 = 16;

fn probe_reasoning_token_bound(
    thinking_map: &ThinkingMap,
    thinking: Option<crate::types::ThinkingLevel>,
) -> Option<u64> {
    let Some(level) = thinking else {
        return Some(0);
    };
    if level == crate::types::ThinkingLevel::Off {
        return Some(0);
    }
    let mapping = thinking_map.levels.get(level.as_key())?;
    Some(numeric_probe_budget(mapping).unwrap_or(0))
}

fn probe_max_tokens_for_thinking(
    thinking_map: &ThinkingMap,
    thinking: Option<crate::types::ThinkingLevel>,
    model_max_output_tokens: Option<i64>,
) -> Result<u32, String> {
    let reasoning_tokens = probe_reasoning_token_bound(thinking_map, thinking)
        .ok_or_else(|| "requested reasoning level has no executable mapping".to_string())?;
    if reasoning_tokens == 0 {
        return Ok(PROBE_RESPONSE_ALLOWANCE_TOKENS as u32);
    }

    let required = reasoning_tokens
        .checked_add(PROBE_RESPONSE_ALLOWANCE_TOKENS)
        .ok_or_else(|| "reasoning budget is too large to construct a bounded probe".to_string())?;
    if let Some(ceiling) = model_max_output_tokens {
        if ceiling <= 0 || required > ceiling as u64 {
            return Err(format!(
                "reasoning budget requires max_tokens > {reasoning_tokens}, but model output ceiling is {ceiling}"
            ));
        }
    }
    u32::try_from(required)
        .map_err(|_| "reasoning budget exceeds the supported probe token range".to_string())
}

fn probe_cost_upper_bound(
    prices: &Prices,
    outbound: &Value,
    max_tokens: u32,
    thinking_map: &ThinkingMap,
    thinking: Option<crate::types::ThinkingLevel>,
) -> Option<f64> {
    let input_rate = prices.input_per_1m?;
    let output_rate = prices.output_per_1m?;
    // Serialized bytes intentionally overestimate token count; this is a
    // ceiling, not billing estimation.
    let input_tokens = serde_json::to_vec(outbound).ok()?.len() as u64;
    let reasoning_tokens = probe_reasoning_token_bound(thinking_map, thinking)?;
    let generated_tokens =
        (max_tokens as u64).max(reasoning_tokens.saturating_add(PROBE_RESPONSE_ALLOWANCE_TOKENS));
    let generated_rate = if reasoning_tokens > 0 {
        output_rate.max(prices.thinking_per_1m.unwrap_or(output_rate))
    } else {
        output_rate
    };
    Some(
        (input_tokens as f64 * input_rate + generated_tokens as f64 * generated_rate) / 1_000_000.0,
    )
}

#[cfg(test)]
mod model_lifecycle_regression_tests {
    use super::*;

    #[tokio::test]
    async fn pricing_refresh_failure_does_not_advance_last_success() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-pricing-lifecycle-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let url = format!("sqlite://{}?mode=rwc", root.join("test.db").display());
        let pool = db::connect(&url).await.unwrap();
        db::migrate(&pool).await.unwrap();

        let provider_id = "provider-pricing-stale";
        let lane = "pricing_sync";
        let previous_success = "2026-09-26T00:00:00Z";
        db::set_setting(
            &pool,
            &lifecycle_setting_key(lane, "last_success", provider_id),
            previous_success,
        )
        .await
        .unwrap();
        db::set_setting(
            &pool,
            &lifecycle_setting_key(lane, "last_attempt", provider_id),
            "2026-09-27T00:00:00Z",
        )
        .await
        .unwrap();

        record_lifecycle_failure(
            &pool,
            lane,
            provider_id,
            "models.dev refresh failed; stale cached catalog retained",
        )
        .await
        .unwrap();

        let status = lifecycle_lane_status(&pool, lane, provider_id)
            .await
            .unwrap();
        assert_eq!(status["last_success"].as_str(), Some(previous_success));
        assert_eq!(
            status["last_attempt"].as_str(),
            Some("2026-09-27T00:00:00Z")
        );
        assert!(status["last_failure"].as_str().is_some());
        assert!(status["last_error"]
            .as_str()
            .is_some_and(|error| error.contains("stale cached catalog")));

        drop(pool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_pricing_refresh_does_not_poll_any_price_mutation() {
        let cases = [
            ("unset", Value::Null, "unset"),
            ("automatic", json!(4.0), "models.dev:provider"),
            ("manual", json!(4.0), "operator"),
        ];

        for (label, effective_price, ownership) in cases {
            let mut state = json!({
                "effective_price": effective_price,
                "ownership": ownership,
                "observation_marker": "cached",
            });
            let before = state.clone();

            let result = run_if_pricing_refresh_committable(
                crate::model_catalog::ModelsDevRefreshOutcome::StaleFallback,
                async {
                    state["effective_price"] = json!(5.0);
                    state["ownership"] = json!("models.dev:provider");
                    state["observation_marker"] = json!("sync-write");
                    Ok::<(), ApiError>(())
                },
            )
            .await;

            assert!(result.is_err(), "{label}");
            assert_eq!(state, before, "{label}");
        }
    }

    #[test]
    fn stale_catalog_observation_preserves_effective_automatic_price() {
        let current = Prices {
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let observed = Prices {
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let discovery = json!({
            "price_sources": {
                "output_per_1m": "models.dev:provider"
            },
            "effective_pricing": {
                "source": "models.dev:provider",
                "fields": {
                    "output_per_1m": {"source": "models.dev:provider", "metadata": {}}
                }
            },
            "catalog": {
                "source_state": {
                    "source": "models.dev",
                    "retrieved_at": "2026-09-26T00:00:00Z",
                    "freshness": "stale"
                }
            }
        });

        let (effective, fields, preserved_manual) =
            merge_automatic_price_observation(&current, &observed, &discovery, &discovery);
        assert_eq!(effective.output_per_1m, Some(5.0));
        assert!(!preserved_manual);
        assert_eq!(fields["output_per_1m"]["source"], "models.dev:provider");
    }

    #[test]
    fn pinned_price_field_survives_later_automatic_observation() {
        let current = Prices {
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let mut discovery = json!({
            "price_sources": {
                "output_per_1m": "models.dev:provider"
            },
            "effective_pricing": {
                "source": "models.dev:provider",
                "fields": {
                    "output_per_1m": {
                        "source": "models.dev:provider",
                        "metadata": {}
                    }
                }
            }
        });
        let selected = vec!["prices.output_per_1m".to_string()];
        let pinned = pin_selected_price_fields(&discovery, &current, &selected);
        discovery.as_object_mut().unwrap().insert(
            "effective_pricing".into(),
            json!({
                "source": "operator_pin",
                "fields": pinned
            }),
        );
        let observed = Prices {
            output_per_1m: Some(6.0),
            ..Default::default()
        };

        let (effective, fields, preserved_manual) =
            merge_automatic_price_observation(&current, &observed, &discovery, &discovery);
        assert_eq!(effective.output_per_1m, Some(5.0));
        assert!(preserved_manual);
        assert_eq!(fields["output_per_1m"]["source"], "operator_pin");
    }

    #[test]
    fn mixed_price_ownership_preserves_manual_input_and_updates_automatic_output() {
        let current = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(4.0),
            ..Default::default()
        };
        let observed = Prices {
            input_per_1m: Some(1.5),
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let discovery = json!({
            "last_seen": "2026-09-27T00:00:00Z",
            "price_sources": {
                "input_per_1m": "models.dev:provider",
                "output_per_1m": "models.dev:provider"
            },
            "effective_pricing": {
                "source": "mixed",
                "fields": {
                    "input_per_1m": {"source": "operator", "metadata": {}},
                    "output_per_1m": {"source": "models.dev:provider", "metadata": {}}
                }
            },
            "catalog": {
                "source_state": {
                    "source": "models.dev",
                    "retrieved_at": "2026-09-27T00:00:00Z",
                    "freshness": "fresh"
                }
            }
        });
        let (effective, fields, preserved_manual) =
            merge_automatic_price_observation(&current, &observed, &discovery, &discovery);
        assert_eq!(effective.input_per_1m, Some(1.0));
        assert_eq!(effective.output_per_1m, Some(5.0));
        assert!(preserved_manual);
        assert_eq!(fields["input_per_1m"]["source"], "operator");
        assert_eq!(fields["output_per_1m"]["source"], "models.dev:provider");
    }

    #[test]
    fn authoritative_price_absence_clears_automatic_field() {
        let current = Prices {
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let observed = Prices::default();
        let discovery = json!({
            "price_sources": {
                "output_per_1m": null
            },
            "effective_pricing": {
                "source": "models.dev:provider",
                "fields": {
                    "output_per_1m": {"source": "models.dev:provider", "metadata": {}}
                }
            },
            "catalog": {
                "source_state": {
                    "source": "models.dev",
                    "retrieved_at": "2026-09-27T01:00:00Z",
                    "freshness": "fresh"
                }
            }
        });

        assert!(has_automatic_price_observation(&observed, &discovery));
        let (effective, fields, preserved_manual) =
            merge_automatic_price_observation(&current, &observed, &discovery, &discovery);

        assert_eq!(effective.output_per_1m, None);
        assert!(!preserved_manual);
        assert!(!fields.contains_key("output_per_1m"));
    }

    #[test]
    fn authoritative_price_absence_preserves_operator_field() {
        let current = Prices {
            output_per_1m: Some(7.0),
            ..Default::default()
        };
        let observed = Prices::default();
        let discovery = json!({
            "price_sources": {
                "output_per_1m": null
            },
            "effective_pricing": {
                "source": "operator",
                "fields": {
                    "output_per_1m": {
                        "source": "operator",
                        "metadata": {"configured_by": "admin"}
                    }
                }
            }
        });

        let (effective, fields, preserved_manual) =
            merge_automatic_price_observation(&current, &observed, &discovery, &discovery);

        assert_eq!(effective.output_per_1m, Some(7.0));
        assert!(preserved_manual);
        assert_eq!(fields["output_per_1m"]["source"], "operator");
    }

    #[test]
    fn partial_capability_accept_only_promotes_selected_keys() {
        let discovery = json!({
            "operator_capability_overrides": {
                "reasoning": true
            }
        });
        let configured = json!({
            "tool_calling": true,
            "vision": false,
            "reasoning": true
        });
        let selected = vec!["capabilities.tool_calling".to_string()];
        let overrides = merge_selected_capability_overrides(&discovery, &configured, &selected);
        assert_eq!(overrides.get("tool_calling"), Some(&json!(true)));
        assert_eq!(overrides.get("reasoning"), Some(&json!(true)));
        assert!(!overrides.contains_key("vision"));
    }

    #[test]
    fn models_dev_pricing_patch_does_not_touch_reconciliation_state() {
        let discovery = json!({
            "reconciliation": {
                "status": "missing",
                "checked_at": "2026-09-27T00:00:00Z"
            },
            "prices": {},
            "price_sources": {}
        });
        let resolution = crate::model_catalog::CatalogResolution {
            identity: crate::model_catalog::CanonicalIdentity {
                status: crate::model_catalog::CanonicalIdentityStatus::Unresolved,
                upstream_model_id: "model".into(),
                canonical_model_id: None,
                match_kind: None,
                candidates: vec![],
                source: None,
            },
            canonical: None,
            provider: Some(crate::model_catalog::ProviderModelMatch {
                source: crate::model_catalog::CatalogSource::ModelsDev,
                provider_id: "provider".into(),
                host: "example.test".into(),
                model_id: "model".into(),
                context_window: None,
                max_input_tokens: None,
                max_output_tokens: None,
                capabilities_json: json!({}),
                modalities: None,
                prices: Prices {
                    input_per_1m: Some(1.0),
                    output_per_1m: Some(2.0),
                    ..Default::default()
                },
                model_type: None,
                metadata: json!({}),
                source_url: None,
            }),
            fallback_canonical: None,
            fallback_provider: None,
            catalog_provenance: Some(json!({
                "source": "models.dev",
                "retrieved_at": "2026-09-27T01:00:00Z",
                "freshness": "fresh"
            })),
        };
        let patch = models_dev_pricing_patch(&discovery, &resolution, true);
        assert_eq!(patch["prices"]["input_per_1m"], json!(1.0));
        assert_eq!(
            patch["price_sources"]["input_per_1m"],
            json!("models.dev:provider")
        );
        assert!(patch.get("reconciliation").is_none());
        assert!(patch.get("disappeared").is_none());
        assert!(patch.get("last_seen").is_none());
    }

    #[test]
    fn partial_reconciliation_selection_keeps_unselected_drift() {
        let reconciliation = json!({
            "diff": [
                {"field":"context_window","configured":1,"observed":2},
                {"field":"capabilities.reasoning","configured":false,"observed":true}
            ]
        });
        let selected = vec!["context_window".to_string()];
        let remaining = remaining_reconciliation_diff(&reconciliation, &selected);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0]["field"], "capabilities.reasoning");
    }

    fn model_row_with_reconciliation(reconciliation: Value) -> db::ModelRow {
        db::ModelRow {
            id: "model".into(),
            provider_id: "provider".into(),
            upstream_id: "model".into(),
            display_name: "Model".into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: None,
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: "{}".into(),
            thinking_map: "{}".into(),
            extra_request: "{}".into(),
            discovery: json!({ "reconciliation": reconciliation }).to_string(),
            created_at: db::now_iso(),
            opaque_state_plugin: String::new(),
        }
    }

    #[test]
    fn missing_reconciliation_keeps_identical_ignored_diff_ignored() {
        let missing_diff = json!([{
            "field": "availability",
            "configured": "present",
            "observed": "missing",
            "source": "upstream_discovery",
        }]);
        let row = model_row_with_reconciliation(json!({
            "status": "ignored",
            "ignored_diff": missing_diff,
            "pinned_fields": [],
        }));

        let state = missing_reconciliation_state(&row, "2026-09-27T00:00:00Z");
        assert_eq!(state["status"], "ignored");
        assert_eq!(state["diff"], state["ignored_diff"]);
    }

    #[test]
    fn missing_reconciliation_filters_pinned_availability() {
        let row = model_row_with_reconciliation(json!({
            "status": "unchanged",
            "pinned_fields": ["availability"],
        }));

        let state = missing_reconciliation_state(&row, "2026-09-27T00:00:00Z");
        assert_eq!(state["status"], "missing");
        assert_eq!(state["diff"], json!([]));
        assert_eq!(state["pinned_fields"], json!(["availability"]));
    }

    #[tokio::test]
    async fn provider_lifecycle_lock_serializes_operator_mutations() {
        let provider_id = format!("provider-lock-{}", uuid::Uuid::new_v4().simple());
        let automatic = model_reconciliation_lock(&provider_id);
        let automatic_guard = automatic.lock().await;
        let operator = model_reconciliation_lock(&provider_id);

        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(10), operator.lock()).await;
        assert!(blocked.is_err());

        drop(automatic_guard);
        let operator_guard =
            tokio::time::timeout(std::time::Duration::from_millis(100), operator.lock())
                .await
                .expect("operator mutation should proceed after lifecycle work releases the lock");
        drop(operator_guard);
        model_reconciliation_locks().remove(&provider_id);
    }

    #[tokio::test]
    async fn scheduled_discovery_payload_survives_for_later_api_read() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-model-lifecycle-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let url = format!("sqlite://{}?mode=rwc", root.join("test.db").display());
        let pool = db::connect(&url).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let payload = json!({
            "models": [{
                "id": "new-upstream-model",
                "already_imported": false,
                "reconciliation": {"status": "new"}
            }],
            "disappeared": []
        });
        persist_provider_discovery_observations(&pool, "provider-a", &payload)
            .await
            .unwrap();
        let loaded = load_provider_discovery_observations(&pool, "provider-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded, payload);
        drop(pool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scoped_probe_evidence_retains_multiple_accounts_and_replaces_same_scope() {
        let mut evidence = serde_json::Map::new();
        let key = "tool_calling".to_string();
        let account_a = json!({
            "status": "supported",
            "scope": {
                "provider_id": "provider",
                "account_id": "account-a",
                "model_id": "model",
                "transport": "openai"
            }
        });
        let account_b = json!({
            "status": "unsupported",
            "scope": {
                "provider_id": "provider",
                "account_id": "account-b",
                "model_id": "model",
                "transport": "openai"
            }
        });
        upsert_probe_evidence(&mut evidence, key.clone(), account_a);
        upsert_probe_evidence(&mut evidence, key.clone(), account_b);
        assert_eq!(evidence[&key].as_array().unwrap().len(), 2);

        let account_a_new = json!({
            "status": "unsupported",
            "scope": {
                "provider_id": "provider",
                "account_id": "account-a",
                "model_id": "model",
                "transport": "openai"
            }
        });
        upsert_probe_evidence(&mut evidence, key.clone(), account_a_new);
        let entries = evidence[&key].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|item| {
            item.pointer("/scope/account_id").and_then(Value::as_str) == Some("account-a")
                && item.get("status").and_then(Value::as_str) == Some("unsupported")
        }));
        assert!(entries.iter().any(|item| {
            item.pointer("/scope/account_id").and_then(Value::as_str) == Some("account-b")
                && item.get("status").and_then(Value::as_str) == Some("unsupported")
        }));
    }

    #[test]
    fn inconclusive_probe_keeps_previous_conclusive_same_scope() {
        let mut evidence = serde_json::Map::new();
        let key = "tool_calling".to_string();
        let scope = json!({
            "provider_id": "provider",
            "account_id": "account-a",
            "model_id": "model",
            "transport": "openai"
        });
        upsert_probe_evidence(
            &mut evidence,
            key.clone(),
            json!({
                "status": "supported",
                "fresh_until": "2999-01-01T00:00:00Z",
                "scope": scope.clone()
            }),
        );
        upsert_probe_evidence(
            &mut evidence,
            key.clone(),
            json!({
                "status": "inconclusive",
                "reason": "503",
                "fresh_until": "2999-01-01T00:00:00Z",
                "scope": scope.clone()
            }),
        );
        upsert_probe_evidence(
            &mut evidence,
            key.clone(),
            json!({
                "status": "inconclusive",
                "reason": "timeout",
                "fresh_until": "2999-01-01T00:00:00Z",
                "scope": scope
            }),
        );

        let entries = evidence[&key].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["status"], "supported");
        assert_eq!(entries[1]["status"], "inconclusive");
        assert_eq!(entries[1]["reason"], "timeout");
    }

    #[test]
    fn account_scoped_discovery_union_keeps_models_from_either_account() {
        #[derive(Debug)]
        struct Item {
            id: String,
        }

        let mut union = Vec::new();
        extend_unique_by_id(
            &mut union,
            vec![Item {
                id: "model-1".into(),
            }],
            |item| item.id.clone(),
        );
        extend_unique_by_id(
            &mut union,
            vec![
                Item {
                    id: "model-1".into(),
                },
                Item {
                    id: "model-2".into(),
                },
            ],
            |item| item.id.clone(),
        );

        let ids: std::collections::HashSet<_> = union.into_iter().map(|item| item.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("model-1"));
        assert!(ids.contains("model-2"));
    }

    #[tokio::test]
    async fn concurrent_probe_persistence_keeps_both_capability_keys() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-probe-evidence-race-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let url = format!("sqlite://{}?mode=rwc", root.join("test.db").display());
        let pool = db::connect(&url).await.unwrap();
        db::migrate(&pool).await.unwrap();

        let provider_id = db::insert_provider(
            &pool,
            &db::NewProvider {
                name: "Provider",
                base_url: "https://example.test",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 120_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "example.test",
                allow_insecure_tls: false,
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
        let model_id = db::insert_model(
            &pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "model",
                display_name: "Model",
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
        let scope = json!({
            "provider_id": provider_id,
            "account_id": "account-a",
            "model_id": model_id,
            "transport": "openai",
        });
        let first = persist_probe_evidence_observation(
            &pool,
            &provider_id,
            &model_id,
            "tool_calling".into(),
            json!({
                "status": "supported",
                "fresh_until": "2999-01-01T00:00:00Z",
                "scope": scope.clone(),
            }),
        );
        let second = persist_probe_evidence_observation(
            &pool,
            &provider_id,
            &model_id,
            "structured_output".into(),
            json!({
                "status": "supported",
                "fresh_until": "2999-01-01T00:00:00Z",
                "scope": scope,
            }),
        );
        let (first, second) = tokio::join!(first, second);
        first.unwrap();
        second.unwrap();

        let row = db::get_model(&pool, &model_id).await.unwrap().unwrap();
        let discovery = discovery_object(&row);
        let evidence = discovery["probe_evidence"].as_object().unwrap();
        assert!(evidence.contains_key("tool_calling"));
        assert!(evidence.contains_key("structured_output"));

        model_reconciliation_locks().remove(&provider_id);
        drop(pool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn discovery_persistence_failure_is_not_silently_successful() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-model-lifecycle-failure-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let url = format!("sqlite://{}?mode=rwc", root.join("test.db").display());
        let pool = db::connect(&url).await.unwrap();
        db::migrate(&pool).await.unwrap();
        pool.close().await;
        assert!(persist_provider_discovery_observations(
            &pool,
            "provider-a",
            &json!({"models":[],"disappeared":[]}),
        )
        .await
        .is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn catalog_outage_preserves_persisted_models_dev_observation() {
        let row = db::ModelRow {
            id: "model".into(),
            provider_id: "provider".into(),
            upstream_id: "model".into(),
            display_name: "Model".into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: None,
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: "{}".into(),
            thinking_map: "{}".into(),
            extra_request: "{}".into(),
            discovery: json!({
                "context_window": 200000,
                "max_output_tokens": 32768,
                "capabilities": {
                    "text": true,
                    "reasoning": true,
                    "tool_calling": true
                },
                "reasoning_capability": {
                    "mode": "level",
                    "levels": ["low", "high"],
                    "can_disable": false,
                    "upstream_format": "openai_effort"
                },
                "thinking_map": {
                    "mode": "level",
                    "levels": {"low": "low", "high": "high"},
                    "level_field": "reasoning_effort"
                },
                "capability_sources": {
                    "context_window": "models.dev:provider",
                    "max_output_tokens": "models.dev:provider",
                    "text": "models.dev:canonical",
                    "reasoning": "models.dev:provider",
                    "tool_calling": "models.dev:provider",
                    "model_type": "models.dev:provider"
                },
                "prices": {
                    "input_per_1m": 1.0,
                    "output_per_1m": 4.0
                },
                "price_sources": {
                    "input_per_1m": "models.dev:provider",
                    "output_per_1m": "models.dev:provider"
                },
                "modalities": {"input": ["text"], "output": ["text"]},
                "model_type": "chat",
                "execution_supported": true,
                "canonical_identity": {"status": "resolved"},
                "canonical_model_id": "openai/model",
                "canonical_match": "exact_model_id",
                "catalog": {
                    "source_state": {
                        "source": "models.dev",
                        "retrieved_at": "2026-09-27T00:00:00Z",
                        "freshness": "fresh"
                    }
                }
            })
            .to_string(),
            created_at: db::now_iso(),
            opaque_state_plugin: String::new(),
        };
        let mut observation = discovered_observation(
            crate::adapters::DiscoveredModel {
                id: "model".into(),
                display_name: Some("Model".into()),
                context_window: None,
                max_output_tokens: None,
            },
            Some(json!({
                "capabilities": {"tool_calling": false}
            })),
            None,
            WireFormat::Openai,
        );

        preserve_last_known_catalog_observation(&row, &mut observation);

        assert_eq!(observation.model.context_window, Some(200000));
        assert_eq!(observation.model.max_output_tokens, Some(32768));
        assert_eq!(observation.capabilities.reasoning, Some(true));
        assert_eq!(observation.capabilities.tool_calling, Some(false));
        assert_eq!(
            observation.capability_sources["reasoning"],
            "models.dev:provider"
        );
        assert_eq!(
            observation.capability_sources["tool_calling"],
            "provider_metadata"
        );
        assert_eq!(observation.prices.input_per_1m, Some(1.0));
        assert_eq!(observation.prices.output_per_1m, Some(4.0));
        assert_eq!(
            observation.catalog.as_ref().unwrap()["source_state"]["source"],
            "models.dev"
        );
        assert_eq!(
            observation.catalog.as_ref().unwrap()["source_state"]["retrieved_at"],
            "2026-09-27T00:00:00Z"
        );
        assert_eq!(
            observation.catalog.as_ref().unwrap()["source_state"]["freshness"],
            "stale"
        );
        assert_eq!(
            observation.canonical_model_id.as_deref(),
            Some("openai/model")
        );
    }

    #[test]
    fn reasoning_disable_probe_defaults_to_off_and_rejects_non_off_values() {
        let omitted: CapabilityProbeBody =
            serde_json::from_value(json!({"capability": "reasoning_disable"})).unwrap();
        let omitted_value =
            normalize_capability_probe_value(&omitted.capability, omitted.value.as_ref()).unwrap();
        assert_eq!(omitted_value, Some(json!("off")));
        assert_eq!(
            probe_thinking_level(omitted_value.as_ref()).unwrap(),
            crate::types::ThinkingLevel::Off
        );

        let none: CapabilityProbeBody = serde_json::from_value(json!({
            "capability": "reasoning_disable",
            "value": "none"
        }))
        .unwrap();
        assert_eq!(
            normalize_capability_probe_value(&none.capability, none.value.as_ref()).unwrap(),
            Some(json!("off"))
        );

        let invalid: CapabilityProbeBody = serde_json::from_value(json!({
            "capability": "reasoning_disable",
            "value": "low"
        }))
        .unwrap();
        let error = normalize_capability_probe_value(&invalid.capability, invalid.value.as_ref())
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn reasoning_disable_probe_uses_transport_specific_candidate_mapping() {
        let responses = reasoning_disable_probe_thinking_map(
            &crate::adapters::TargetTransport::OpenAiResponses,
        )
        .unwrap();
        assert_eq!(responses.level_field.as_deref(), Some("reasoning.effort"));
        assert_eq!(responses.levels.get("off"), Some(&json!("none")));

        let chat =
            reasoning_disable_probe_thinking_map(&crate::adapters::TargetTransport::OpenAiChat)
                .unwrap();
        assert_eq!(chat.level_field.as_deref(), Some("reasoning_effort"));
        assert_eq!(chat.levels.get("off"), Some(&json!("none")));

        assert!(
            reasoning_disable_probe_thinking_map(&crate::adapters::TargetTransport::Anthropic)
                .is_none()
        );
    }

    #[tokio::test]
    async fn initial_scheduled_pass_waits_for_stable_jitter_without_fake_attempt() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-lifecycle-jitter-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let url = format!("sqlite://{}?mode=rwc", root.join("test.db").display());
        let pool = db::connect(&url).await.unwrap();
        db::migrate(&pool).await.unwrap();

        let mut key = lifecycle_setting_key("reconciliation", "last_attempt", "provider-jitter");
        while stable_schedule_jitter(&key, 300) == 0 {
            key.push('x');
        }

        assert!(!scheduled_due(&pool, &key, 3600, 300).await);
        assert!(db::get_setting(&pool, &key).await.unwrap().is_none());
        assert!(db::get_setting(&pool, &format!("{key}:schedule_anchor"))
            .await
            .unwrap()
            .is_some());
        assert!(!scheduled_due(&pool, &key, 3600, 300).await);

        drop(pool);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod probe_cost_regression_tests {
    use super::*;

    #[test]
    fn numeric_reasoning_budget_is_included_in_cost_ceiling() {
        let prices = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            thinking_per_1m: Some(3.0),
            ..Default::default()
        };
        let thinking_map = ThinkingMap {
            levels: [("high".to_string(), json!(4096))].into_iter().collect(),
            mode: Some(crate::types::ThinkingMode::ManualBudget),
            budget_field: Some("thinking.budget_tokens".into()),
            level_field: None,
        };
        let outbound = json!({"messages":[{"role":"user","content":"ok"}]});
        let cost = probe_cost_upper_bound(
            &prices,
            &outbound,
            16,
            &thinking_map,
            Some(crate::types::ThinkingLevel::High),
        )
        .unwrap();
        assert!(cost >= (4112.0 * 3.0) / 1_000_000.0);
    }

    #[test]
    fn numeric_reasoning_budget_builds_valid_probe_output_limit() {
        let thinking_map = ThinkingMap {
            levels: [("high".to_string(), json!(4096))].into_iter().collect(),
            mode: Some(crate::types::ThinkingMode::ManualBudget),
            budget_field: Some("thinking.budget_tokens".into()),
            level_field: None,
        };
        let max_tokens = probe_max_tokens_for_thinking(
            &thinking_map,
            Some(crate::types::ThinkingLevel::High),
            Some(8192),
        )
        .unwrap();
        assert_eq!(max_tokens, 4112);
        assert!(max_tokens > 4096);

        let error = probe_max_tokens_for_thinking(
            &thinking_map,
            Some(crate::types::ThinkingLevel::High),
            Some(4096),
        )
        .unwrap_err();
        assert!(error.contains("output ceiling"));
    }

    #[test]
    fn effort_reasoning_keeps_probe_tiny_without_numeric_budget() {
        let thinking_map = ThinkingMap {
            levels: [("high".to_string(), json!("high"))].into_iter().collect(),
            mode: Some(crate::types::ThinkingMode::Level),
            budget_field: None,
            level_field: Some("reasoning_effort".into()),
        };
        assert_eq!(
            probe_max_tokens_for_thinking(
                &thinking_map,
                Some(crate::types::ThinkingLevel::High),
                Some(65536),
            )
            .unwrap(),
            16
        );
    }

    #[test]
    fn outbound_schema_bytes_contribute_to_input_bound() {
        let prices = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(1.0),
            ..Default::default()
        };
        let small = probe_cost_upper_bound(
            &prices,
            &json!({"messages":[]}),
            16,
            &ThinkingMap::default(),
            None,
        )
        .unwrap();
        let large = probe_cost_upper_bound(
            &prices,
            &json!({"tools":[{"parameters":{"schema":"x".repeat(2048)}}]}),
            16,
            &ThinkingMap::default(),
            None,
        )
        .unwrap();
        assert!(large > small);
    }
}

fn merge_probe_extra_request(base: &Value, overlay: &Value) -> Value {
    fn merge(target: &mut Value, overlay: &Value) {
        match (target, overlay) {
            (Value::Object(target), Value::Object(overlay)) => {
                for (key, value) in overlay {
                    if let Some(existing) = target.get_mut(key) {
                        merge(existing, value);
                    } else {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
            (target, overlay) => *target = overlay.clone(),
        }
    }

    let mut merged = if base.is_object() {
        base.clone()
    } else {
        json!({})
    };
    merge(&mut merged, overlay);
    merged
}

fn structured_output_probe_contract(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == 1 && object.get("ok").and_then(Value::as_str).is_some()
}

fn reasoning_disable_probe_contract(events: &[crate::types::StreamEvent]) -> Option<bool> {
    events.iter().find_map(|event| match event {
        crate::types::StreamEvent::Usage(usage) => {
            usage.thinking.map(|reasoning_tokens| reasoning_tokens == 0)
        }
        _ => None,
    })
}

#[cfg(test)]
mod probe_rejection_regression_tests {
    use super::*;

    #[test]
    fn coupled_parameter_invalid_value_is_not_deterministic_rejection() {
        for body in [
            "invalid value for max_tokens when reasoning is enabled",
            "invalid value 'high' for max_tokens when reasoning_effort is enabled",
        ] {
            assert!(!deterministic_probe_rejection(
                "reasoning",
                Some(&json!("high")),
                400,
                body,
            ));
        }
    }

    #[test]
    fn explicit_reasoning_parameter_rejection_is_deterministic() {
        assert!(deterministic_probe_rejection(
            "reasoning",
            Some(&json!("high")),
            400,
            "unsupported value 'high' for reasoning_effort",
        ));
    }

    #[test]
    fn parameter_value_rejection_is_inconclusive_for_parameter_support() {
        assert!(!deterministic_probe_rejection(
            "parameter.temperature",
            Some(&json!(2)),
            400,
            "invalid value 2 for temperature",
        ));
        assert!(!deterministic_probe_rejection(
            "parameter.temperature",
            Some(&json!(2)),
            422,
            "temperature value 2 is unsupported",
        ));
    }

    #[test]
    fn explicit_parameter_rejection_is_deterministic() {
        assert!(deterministic_probe_rejection(
            "parameter.temperature",
            Some(&json!(2)),
            400,
            "temperature is unsupported",
        ));
        assert!(deterministic_probe_rejection(
            "parameter.temperature",
            Some(&json!(2)),
            422,
            "unknown parameter temperature",
        ));
    }

    #[test]
    fn forced_tool_choice_rejection_is_inconclusive_for_base_tool_support() {
        for body in [
            "tool_choice is unsupported",
            "tool choice is not supported",
            "function_call is not supported",
            "Tool 'kinetix_probe_noop' not found in provided tools",
        ] {
            assert!(!deterministic_probe_rejection(
                "tool_calling",
                None,
                400,
                body,
            ));
        }
    }

    #[test]
    fn explicit_tool_declaration_rejection_is_deterministic() {
        for body in [
            "tools are unsupported",
            "tool declarations are not supported",
            "function declarations are unsupported",
        ] {
            assert!(deterministic_probe_rejection(
                "tool_calling",
                None,
                400,
                body,
            ));
        }
    }

    #[test]
    fn transport_probe_requires_explicit_endpoint_or_transport_rejection() {
        for (status, body) in [
            (400, "transport is unsupported"),
            (404, "unsupported endpoint for this transport"),
            (405, "endpoint is not supported"),
        ] {
            assert!(deterministic_probe_rejection(
                "transport",
                None,
                status,
                body,
            ));
        }

        for (status, body) in [
            (400, "bad request"),
            (404, "not found"),
            (405, "method not allowed"),
            (502, "unsupported endpoint"),
            (503, "transport is unsupported"),
            (504, "transport is unsupported"),
        ] {
            assert!(!deterministic_probe_rejection(
                "transport",
                None,
                status,
                body,
            ));
        }
    }

    #[test]
    fn structured_output_probe_merges_configured_extra_request() {
        let base = json!({
            "metadata": {"trace_id": "keep-me"},
            "generationConfig": {
                "temperature": 0.25,
                "responseMimeType": "text/plain"
            }
        });
        let probe = json!({
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseJsonSchema": {"type": "object"}
            }
        });

        let merged = merge_probe_extra_request(&base, &probe);
        assert_eq!(merged["metadata"]["trace_id"], "keep-me");
        assert_eq!(merged["generationConfig"]["temperature"], 0.25);
        assert_eq!(
            merged["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(
            merged["generationConfig"]["responseJsonSchema"]["type"],
            "object"
        );
    }

    #[test]
    fn structured_output_probe_requires_exact_object_shape() {
        assert!(structured_output_probe_contract(&json!({"ok": "yes"})));
        assert!(!structured_output_probe_contract(
            &json!({"ok": "yes", "extra": true})
        ));
        assert!(!structured_output_probe_contract(&json!({"ok": 1})));
        assert!(!structured_output_probe_contract(&json!(["ok"])));
    }

    #[test]
    fn reasoning_disable_probe_requires_explicit_zero_reasoning_usage() {
        let zero = vec![crate::types::StreamEvent::Usage(crate::types::TokenUsage {
            thinking: Some(0),
            ..Default::default()
        })];
        assert_eq!(reasoning_disable_probe_contract(&zero), Some(true));

        let nonzero = vec![crate::types::StreamEvent::Usage(crate::types::TokenUsage {
            thinking: Some(7),
            ..Default::default()
        })];
        assert_eq!(reasoning_disable_probe_contract(&nonzero), Some(false));

        let missing = vec![crate::types::StreamEvent::Usage(
            crate::types::TokenUsage::default(),
        )];
        assert_eq!(reasoning_disable_probe_contract(&missing), None);
    }

    #[test]
    fn capability_probe_response_body_enforces_hard_byte_limit() {
        let mut bytes = vec![b'x'; MAX_CAPABILITY_PROBE_RESPONSE_BYTES - 1];
        assert!(append_capability_probe_response_chunk(&mut bytes, b"x").is_ok());
        assert_eq!(bytes.len(), MAX_CAPABILITY_PROBE_RESPONSE_BYTES);

        assert!(append_capability_probe_response_chunk(&mut bytes, b"x").is_err());
        assert_eq!(bytes.len(), MAX_CAPABILITY_PROBE_RESPONSE_BYTES);
    }
}

fn deterministic_probe_rejection(
    capability: &str,
    value: Option<&Value>,
    status: u16,
    body: &str,
) -> bool {
    if capability == "transport" {
        if !matches!(status, 400 | 404 | 405 | 422) {
            return false;
        }
        let body = body.to_ascii_lowercase();
        return [
            "transport is unsupported",
            "transport is not supported",
            "transport not supported",
            "unsupported transport",
            "endpoint is unsupported",
            "endpoint is not supported",
            "endpoint not supported",
            "unsupported endpoint",
            "api endpoint is unsupported",
            "api endpoint is not supported",
            "this endpoint does not support this transport",
        ]
        .iter()
        .any(|pattern| body.contains(pattern));
    }

    if !matches!(status, 400 | 422) {
        return false;
    }

    let body = body.to_ascii_lowercase();
    let parameter_terms: Vec<String> = match capability {
        "reasoning" | "reasoning_disable" => vec![
            "reasoning_effort".into(),
            "reasoning.effort".into(),
            "reasoning effort".into(),
            "thinking_level".into(),
            "thinking level".into(),
            "thinkingconfig.thinkinglevel".into(),
            "thinking effort".into(),
        ],
        "tool_calling" => vec![
            "tool calling".into(),
            "tools".into(),
            "tool declaration".into(),
            "tool declarations".into(),
            "function declaration".into(),
            "function declarations".into(),
        ],
        "structured_output" => vec![
            "response_format".into(),
            "response format".into(),
            "json_schema".into(),
            "json schema".into(),
            "structured output".into(),
        ],
        capability if capability.starts_with("parameter.") => {
            let parameter = capability
                .trim_start_matches("parameter.")
                .to_ascii_lowercase();
            vec![parameter.clone(), parameter.replace('_', " ")]
        }
        _ => Vec::new(),
    };
    if parameter_terms.is_empty() {
        return false;
    }

    let explicitly_rejects_parameter = parameter_terms.iter().any(|term| {
        [
            format!("{term} is unsupported"),
            format!("{term} are unsupported"),
            format!("{term} unsupported"),
            format!("{term}: unsupported"),
            format!("{term} is not supported"),
            format!("{term} are not supported"),
            format!("{term} not supported"),
            format!("{term} is not allowed"),
            format!("{term} not allowed"),
            format!("unsupported parameter {term}"),
            format!("unsupported parameter: {term}"),
            format!("unknown parameter {term}"),
            format!("unknown parameter: {term}"),
            format!("unrecognized parameter {term}"),
            format!("unrecognized parameter: {term}"),
            format!("invalid parameter {term}"),
            format!("invalid parameter: {term}"),
        ]
        .iter()
        .any(|pattern| body.contains(pattern))
    });
    if explicitly_rejects_parameter {
        return true;
    }

    // Rejection of one probed value does not establish that a general model
    // parameter is unsupported. Reasoning efforts are intentionally different:
    // their evidence is value-scoped by probe_evidence_key().
    if capability.starts_with("parameter.") {
        return false;
    }

    let Some(value) = value else {
        return false;
    };
    let value = match value {
        Value::String(value) => value.to_ascii_lowercase(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null => "null".to_string(),
        _ => return false,
    };
    if value.is_empty() {
        return false;
    }

    parameter_terms.iter().any(|term| {
        let value_forms = [value.clone(), format!("'{value}'"), format!("\"{value}\"")];
        value_forms.iter().any(|value| {
            [
                format!("invalid value {value} for {term}"),
                format!("unsupported value {value} for {term}"),
                format!("{term}: invalid value {value}"),
                format!("{term}: unsupported value {value}"),
                format!("{term} has invalid value {value}"),
                format!("{term} value {value} is unsupported"),
                format!("{term} value {value} is not supported"),
                format!("{term} value {value} is not allowed"),
                format!("{term} {value} is unsupported"),
                format!("{term} {value} is not supported"),
                format!("{term} {value} is not allowed"),
            ]
            .iter()
            .any(|pattern| body.contains(pattern))
        })
    })
}

fn probe_evidence_key(capability: &str, value: Option<&Value>) -> String {
    if capability == "reasoning" {
        if let Some(level) = value.and_then(Value::as_str) {
            let level = level.trim().to_ascii_lowercase();
            if !level.is_empty()
                && level
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            {
                return format!("reasoning_effort_{}", level.replace('-', "_"));
            }
        }
    }
    capability.replace('.', "_")
}

fn same_probe_scope(a: &Value, b: &Value) -> bool {
    a.get("scope") == b.get("scope")
}

fn upsert_probe_evidence(
    evidence: &mut serde_json::Map<String, Value>,
    key: String,
    evidence_value: Value,
) {
    let mut entries = match evidence.remove(&key) {
        Some(Value::Array(entries)) => entries,
        Some(existing @ Value::Object(_)) => vec![existing],
        _ => Vec::new(),
    };
    match evidence_value.get("status").and_then(Value::as_str) {
        Some("supported" | "unsupported") => {
            entries.retain(|existing| !same_probe_scope(existing, &evidence_value));
        }
        Some("inconclusive") => {
            entries.retain(|existing| {
                !same_probe_scope(existing, &evidence_value)
                    || existing.get("status").and_then(Value::as_str) != Some("inconclusive")
            });
        }
        _ => {}
    }
    entries.push(evidence_value);
    evidence.insert(key, Value::Array(entries));
}

async fn persist_probe_evidence_observation(
    pool: &Pool,
    provider_id: &str,
    model_id: &str,
    key: String,
    evidence_value: Value,
) -> anyhow::Result<()> {
    let lock = model_reconciliation_lock(provider_id);
    let _guard = lock.lock().await;
    let latest = db::get_model(pool, model_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("model disappeared while persisting probe evidence"))?;
    let discovery = discovery_object(&latest);
    let mut evidence = discovery
        .get("probe_evidence")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    upsert_probe_evidence(&mut evidence, key, evidence_value);
    db::merge_model_discovery(
        pool,
        model_id,
        &json!({ "probe_evidence": Value::Object(evidence) }),
    )
    .await
}

const MAX_CAPABILITY_PROBE_RESPONSE_BYTES: usize = 128 * 1024;

fn append_capability_probe_response_chunk(bytes: &mut Vec<u8>, chunk: &[u8]) -> Result<(), ()> {
    if bytes.len().saturating_add(chunk.len()) > MAX_CAPABILITY_PROBE_RESPONSE_BYTES {
        return Err(());
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

async fn read_capability_probe_response(mut response: reqwest::Response) -> Result<String, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CAPABILITY_PROBE_RESPONSE_BYTES as u64)
    {
        return Err(format!(
            "upstream probe response exceeded the {} byte limit",
            MAX_CAPABILITY_PROBE_RESPONSE_BYTES
        ));
    }

    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_CAPABILITY_PROBE_RESPONSE_BYTES as u64) as usize,
    );
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| format!("failed to read upstream probe response: {error}"))?;
        let Some(chunk) = chunk else {
            break;
        };
        append_capability_probe_response_chunk(&mut bytes, &chunk).map_err(|()| {
            format!(
                "upstream probe response exceeded the {} byte limit",
                MAX_CAPABILITY_PROBE_RESPONSE_BYTES
            )
        })?;
    }

    String::from_utf8(bytes).map_err(|_| "upstream probe response was not valid UTF-8".to_string())
}

/// Run one explicit, bounded upstream capability probe against the selected
/// provider/account/model/transport execution profile. No automatic path calls
/// this handler.
pub async fn probe_model_capability(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<CapabilityProbeBody>,
) -> ApiResult {
    let probe_value = normalize_capability_probe_value(&body.capability, body.value.as_ref())?;

    let model = db::get_model(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("model not found"))?;
    let provider = db::get_provider(&state.pool, &model.provider_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    let account = if let Some(account_id) = body.account_id.as_deref() {
        let account = db::get_account(&state.pool, account_id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("account not found"))?;
        if account.provider_id != provider.id {
            return Err(ApiError::bad("account does not belong to model provider"));
        }
        account
    } else {
        db::accounts_for_provider(&state.pool, &provider.id)
            .await
            .map_err(ApiError::internal)?
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::bad("provider has no credentials to probe with"))?
    };

    let mut execution_model = probe_execution_model(&provider, &model, body.transport.as_deref())?;
    let profile = crate::adapters::resolve_execution_profile(&provider, &execution_model)
        .map_err(|error| ApiError::bad(error.message))?;
    let adapter = state
        .adapters
        .for_transport(&profile.transport)
        .map_err(|error| ApiError::bad(error.message))?;
    let credential = credential_for_admin_action(
        &state,
        &provider,
        &account,
        "capability probe credential resolution",
    )
    .await?;

    let mut effective_prices = model.prices();
    if !effective_prices.is_configured() {
        let discovery = discovery_object(&model);
        if let Some(prices) = discovery
            .get("prices")
            .cloned()
            .and_then(|value| serde_json::from_value::<Prices>(value).ok())
        {
            effective_prices = prices;
        }
    }
    let mut probe_thinking_map = profile.thinking_map.clone();
    let mut probe_parameters = profile.parameters.clone();

    let mut internal = crate::types::InternalRequest {
        requested_model: model.upstream_id.clone(),
        system: vec![],
        messages: vec![crate::types::Message {
            role: crate::types::Role::User,
            parts: vec![crate::types::Part::Text(
                "Reply with the single word: ok".into(),
            )],
        }],
        tools: vec![],
        tool_choice: None,
        tool_choice_name: None,
        params: crate::types::SamplingParams {
            max_tokens: Some(16),
            ..Default::default()
        },
        stream: false,
        include_usage: false,
        thinking: None,
        extra: Default::default(),
        raw_body: None,
    };

    match body.capability.as_str() {
        "transport" => {}
        "reasoning" => {
            let level = probe_thinking_level(probe_value.as_ref())?;
            if !probe_thinking_map.level_is_executable(level.as_key()) {
                return Ok(Json(json!({
                    "status": "inconclusive",
                    "reason": "resolved target has no executable mapping for the requested canonical reasoning level",
                    "transport": profile.transport.as_str(),
                    "level": level.as_key(),
                })));
            }
            internal.thinking = Some(level);
        }
        "reasoning_disable" => {
            let level = probe_thinking_level(probe_value.as_ref())?;
            let Some(candidate) = reasoning_disable_probe_thinking_map(&profile.transport) else {
                return Ok(Json(json!({
                    "status": "inconclusive",
                    "reason": "no conservative reasoning-disable probe mapping for this transport",
                    "transport": profile.transport.as_str(),
                    "level": level.as_key(),
                })));
            };
            probe_thinking_map = candidate;
            internal.thinking = Some(level);
        }
        "tool_calling" => {
            internal.tools.push(crate::types::ToolDef {
                name: "kinetix_probe_noop".into(),
                description: Some("Capability probe only; never executed.".into()),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: None,
            });
            // Require the inert synthetic tool so a successful probe proves
            // actual tool-call generation rather than merely accepting a tool schema.
            // The returned call is inspected only; it is never executed.
            internal.tool_choice = Some(crate::types::ToolChoice::Required);
        }
        "structured_output" => {
            let schema = json!({
                "type": "object",
                "properties": { "ok": { "type": "string" } },
                "required": ["ok"],
                "additionalProperties": false,
            });
            let extra = match &profile.transport {
                crate::adapters::TargetTransport::OpenAiChat => json!({
                    "response_format": {
                        "type": "json_schema",
                        "json_schema": {
                            "name": "kinetix_probe",
                            "strict": true,
                            "schema": schema,
                        }
                    }
                }),
                crate::adapters::TargetTransport::OpenAiResponses => json!({
                    "text": {
                        "format": {
                            "type": "json_schema",
                            "name": "kinetix_probe",
                            "strict": true,
                            "schema": schema,
                        }
                    }
                }),
                crate::adapters::TargetTransport::Gemini => json!({
                    "generationConfig": {
                        "responseMimeType": "application/json",
                        "responseJsonSchema": schema,
                    }
                }),
                _ => {
                    return Ok(Json(json!({
                        "status": "inconclusive",
                        "reason": "no conservative structured-output probe mapping for this transport",
                        "transport": profile.transport.as_str(),
                    })))
                }
            };
            let merged_extra =
                merge_probe_extra_request(&execution_model.extra_request_value(), &extra);
            execution_model.extra_request = merged_extra.to_string();
        }
        capability if capability.starts_with("parameter.") => {
            let parameter = capability.trim_start_matches("parameter.");
            let value = body
                .value
                .as_ref()
                .and_then(Value::as_f64)
                .ok_or_else(|| ApiError::bad("parameter probe requires a numeric value"))?;
            match parameter {
                "temperature" => internal.params.temperature = Some(value),
                "top_p" => internal.params.top_p = Some(value),
                "top_k" => internal.params.top_k = Some(value),
                "seed" => internal.params.seed = Some(value as i64),
                "presence_penalty" => internal.params.presence_penalty = Some(value),
                "frequency_penalty" => internal.params.frequency_penalty = Some(value),
                other => {
                    return Err(ApiError::bad(format!(
                        "unsupported safe parameter probe '{other}'"
                    )))
                }
            }
            probe_parameters.insert(
                parameter.to_string(),
                crate::types::ParamSpec {
                    supported: true,
                    min: None,
                    max: None,
                    default: None,
                    policy: crate::types::ParamPolicy::Forward,
                },
            );
        }
        other => {
            return Err(ApiError::bad(format!(
                "unsupported capability probe '{other}'"
            )))
        }
    }

    if internal.thinking.is_some() {
        let max_tokens = match probe_max_tokens_for_thinking(
            &probe_thinking_map,
            internal.thinking,
            model.max_output_tokens,
        ) {
            Ok(max_tokens) => max_tokens,
            Err(reason) => {
                return Ok(Json(json!({
                    "status": "inconclusive",
                    "reason": reason,
                    "transport": profile.transport.as_str(),
                    "scope": {
                        "provider_id": provider.id,
                        "account_id": account.id,
                        "model_id": model.id,
                        "transport": profile.transport.as_str(),
                    }
                })))
            }
        };
        internal.params.max_tokens = Some(max_tokens);
    }

    execution_model.thinking_map =
        serde_json::to_string(&probe_thinking_map).map_err(ApiError::internal)?;
    execution_model.parameters =
        serde_json::to_string(&probe_parameters).map_err(ApiError::internal)?;

    let ctx = UpstreamContext {
        provider: &provider,
        model: &execution_model,
        account_id: Some(account.id.as_str()),
        credential,
    };
    let url = adapter
        .build_url(&ctx)
        .map_err(|error| ApiError::bad(error.message))?;
    let outbound = adapter
        .build_body(&ctx, &internal)
        .map_err(|failure| ApiError::bad(failure.message))?;

    let max_cost = body.max_cost_usd.unwrap_or(0.05);
    if !max_cost.is_finite() || max_cost < 0.0 {
        return Err(ApiError::bad(
            "max_cost_usd must be a finite non-negative number",
        ));
    }
    let estimated_cost = probe_cost_upper_bound(
        &effective_prices,
        &outbound,
        internal.params.max_tokens.unwrap_or(16),
        &probe_thinking_map,
        internal.thinking,
    )
    .ok_or_else(|| {
        ApiError::bad(
            "cannot conservatively bound probe cost from pricing and reasoning limits; configure/sync pricing and an output ceiling first",
        )
    })?;
    if estimated_cost > max_cost {
        return Err(ApiError::bad(format!(
            "probe upper-bound cost ${estimated_cost:.6} exceeds max_cost_usd ${max_cost:.6}"
        )));
    }
    let parsed_url = url::Url::parse(&url)
        .map_err(|error| ApiError::bad(format!("invalid probe URL: {error}")))?;

    let started = std::time::Instant::now();
    let response = crate::outbound::send_provider_request(
        &state.outbound_clients,
        state.config.allow_private_upstreams,
        state.config.allow_insecure_tls,
        &adapter,
        &ctx,
        crate::outbound::ProviderRequest {
            method: reqwest::Method::POST,
            url: parsed_url,
            json_body: Some(outbound),
            accept_event_stream: false,
            request_id: None,
            headers: Vec::new(),
            total_timeout: Some(std::time::Duration::from_secs(10)),
        },
    )
    .await;

    let verified_at = chrono::Utc::now();
    let freshness_secs = db::get_setting(&state.pool, "model_probe_freshness_secs")
        .await
        .ok()
        .flatten()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(30 * 24 * 3600);
    let fresh_until = verified_at + chrono::Duration::seconds(freshness_secs);
    let (status, status_code, detail) = match response {
        Ok(response) => {
            let status_code = response.status().as_u16();
            let (text, body_read_error) = match read_capability_probe_response(response).await {
                Ok(text) => (text, None),
                Err(error) => (String::new(), Some(error)),
            };
            if let Some(error) = body_read_error {
                (
                    "inconclusive",
                    status_code,
                    Some(truncate(&crypto::redact(&error), 400)),
                )
            } else if (200..300).contains(&status_code) {
                if body.capability == "reasoning_disable" {
                    let verified = serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|payload| adapter.parse_full_response(&payload).ok())
                        .and_then(|events| reasoning_disable_probe_contract(&events));
                    match verified {
                        Some(true) => ("supported", status_code, None),
                        Some(false) => (
                            "unsupported",
                            status_code,
                            Some(
                                "upstream accepted the disable request but reported non-zero reasoning tokens"
                                    .to_string(),
                            ),
                        ),
                        None => (
                            "inconclusive",
                            status_code,
                            Some(
                                "upstream accepted the disable request but did not report reasoning-token usage"
                                    .to_string(),
                            ),
                        ),
                    }
                } else {
                    let contract_verified = match body.capability.as_str() {
                        "tool_calling" => serde_json::from_str::<Value>(&text)
                            .ok()
                            .and_then(|payload| adapter.parse_full_response(&payload).ok())
                            .is_some_and(|events| {
                                events.iter().any(|event| {
                                    matches!(
                                        event,
                                        crate::types::StreamEvent::ToolCallStart { name, .. }
                                            if name == "kinetix_probe_noop"
                                    )
                                })
                            }),
                        "structured_output" => serde_json::from_str::<Value>(&text)
                            .ok()
                            .and_then(|payload| adapter.parse_full_response(&payload).ok())
                            .map(|events| {
                                events
                                    .into_iter()
                                    .filter_map(|event| match event {
                                        crate::types::StreamEvent::TextDelta(text) => Some(text),
                                        _ => None,
                                    })
                                    .collect::<String>()
                            })
                            .and_then(|text| serde_json::from_str::<Value>(text.trim()).ok())
                            .is_some_and(|value| structured_output_probe_contract(&value)),
                        _ => true,
                    };
                    if contract_verified {
                        ("supported", status_code, None)
                    } else {
                        (
                            "inconclusive",
                            status_code,
                            Some(format!(
                                "upstream returned success but did not satisfy the {} probe contract",
                                body.capability
                            )),
                        )
                    }
                }
            } else {
                let redacted = crypto::redact(&text);
                let status = if deterministic_probe_rejection(
                    &body.capability,
                    probe_value.as_ref(),
                    status_code,
                    &redacted,
                ) {
                    "unsupported"
                } else {
                    "inconclusive"
                };
                (status, status_code, Some(truncate(&redacted, 400)))
            }
        }
        Err(error) => (
            "inconclusive",
            0,
            Some(truncate(&crypto::redact(&error.message), 400)),
        ),
    };

    let evidence_value = json!({
        "status": status,
        "source": "probe",
        "value": probe_value.as_ref(),
        "verified_at": verified_at.to_rfc3339(),
        "fresh_until": fresh_until.to_rfc3339(),
        "scope": {
            "provider_id": provider.id,
            "account_id": account.id,
            "model_id": model.id,
            "transport": profile.transport.as_str(),
        },
        "status_code": status_code,
        "latency_ms": started.elapsed().as_millis() as i64,
        "estimated_max_cost_usd": estimated_cost,
        "detail": detail,
    });
    persist_probe_evidence_observation(
        &state.pool,
        &provider.id,
        &model.id,
        probe_evidence_key(&body.capability, probe_value.as_ref()),
        evidence_value.clone(),
    )
    .await
    .map_err(ApiError::internal)?;

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "model_capability_probe",
        "model",
        &model.id,
        &model.display_name,
        &format!(
            "Capability probe '{}' completed as {status} with status {status_code}.",
            body.capability
        ),
    )
    .await;

    Ok(Json(json!({
        "status": status,
        "evidence": evidence_value,
    })))
}

/// `POST /admin/api/accounts/:id/test` — run a minimal real proxy-style probe
/// through one specific credential instead of the provider pool default.
pub async fn test_account(
    State(state): State<AppState>,
    auth: AdminAuth,
    Path(id): Path<String>,
    Json(mut body): Json<TestBody>,
) -> ApiResult {
    let account = db::get_account(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("account not found"))?;
    let provider_id = account.provider_id.clone();
    body.account_id = Some(id.clone());

    let result = test_provider(State(state.clone()), auth, Path(provider_id), Json(body)).await?;
    let ok = result.0.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let status = result.0.get("status").and_then(Value::as_u64).unwrap_or(0);
    let latency = result
        .0
        .get("latency_ms")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    if ok {
        let _ = db::touch_probe_at(&state.pool, &id).await;
        let _ = db::reset_account_failures(&state.pool, &id).await;
    }
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        if ok {
            "account_probe_succeeded"
        } else {
            "account_probe_failed"
        },
        "account",
        &id,
        &account.label,
        &format!("Account probe completed with status {status} in {latency} ms."),
    )
    .await;

    Ok(result)
}

// ===========================================================================
// Models
// ===========================================================================

pub async fn list_models(State(state): State<AppState>, _auth: AdminAuth) -> ApiResult {
    let models = db::list_models(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let providers = db::list_providers(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let out: Vec<Value> = models.iter().map(|m| model_json(m, &providers)).collect();
    Ok(Json(json!({ "models": out })))
}

fn model_json(m: &db::ModelRow, providers: &[db::ProviderRow]) -> Value {
    let provider_name = providers
        .iter()
        .find(|p| p.id == m.provider_id)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    json!({
        "id": m.id,
        "provider_id": m.provider_id,
        "provider_name": provider_name,
        "upstream_id": m.upstream_id,
        "display_name": m.display_name,
        "enabled": m.enabled != 0,
        "context_window": m.context_window,
        "max_output_tokens": m.max_output_tokens,
        "capabilities": serde_json::from_str::<Value>(&m.capabilities).unwrap_or(json!({})),
        "prices": m.prices(),
        "parameters": m.params(),
        "thinking_map": m.thinking(),
        "extra_request": m.extra_request_value(),
        "transport_override": serde_json::from_str::<Value>(&m.discovery)
            .ok()
            .and_then(|discovery| discovery.get("configured_transport").cloned()),
        // Discovery-suggested values (FR-10.5). Surfaced so an admin can see
        // what the last discovery observed, including models that have since
        // disappeared upstream (flagged, never silently deleted).
        "discovery": serde_json::from_str::<Value>(&m.discovery).unwrap_or(json!({})),
    })
}

#[derive(Deserialize)]
pub struct ModelBody {
    pub upstream_id: String,
    pub display_name: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(default)]
    pub prices: Value,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default)]
    pub thinking_map: ThinkingMap,
    #[serde(default)]
    pub extra_request: Value,
    #[serde(default)]
    pub discovery: Value,
    #[serde(default)]
    pub transport_override: Option<String>,
}

fn validate_model_transport_override(
    provider: &db::ProviderRow,
    transport: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let Some(transport) = transport
        .map(str::trim)
        .filter(|transport| !transport.is_empty())
    else {
        return Ok(None);
    };
    let parsed = crate::adapters::TargetTransport::parse(transport)
        .ok_or_else(|| ApiError::bad(format!("unsupported model transport '{transport}'")))?;
    if let Some(reference) = provider.wire_plugin_ref() {
        let bound = crate::adapters::TargetTransport::Plugin(reference.to_string_ref());
        if parsed != bound {
            return Err(ApiError::bad(
                "model transport override conflicts with the provider's explicit plugin adapter",
            ));
        }
    }
    Ok(Some(parsed.as_str().to_string()))
}

fn default_true() -> bool {
    true
}

fn normalize_model_capabilities(value: &Value) -> Value {
    let Some(input) = value.as_object() else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for (canonical, aliases) in [
        ("text", &["text"][..]),
        ("vision", &["vision"][..]),
        ("reasoning", &["reasoning"][..]),
        (
            "tool_calling",
            &["tool_calling", "toolCalling", "tools", "tool_calls"][..],
        ),
        ("audio", &["audio"][..]),
        (
            "structured_output",
            &["structured_output", "structuredOutput"][..],
        ),
    ] {
        if let Some(value) = aliases
            .iter()
            .find_map(|key| input.get(*key))
            .and_then(Value::as_bool)
        {
            out.insert(canonical.to_string(), Value::Bool(value));
        }
    }
    Value::Object(out)
}

fn validate_thinking_map(thinking_map: &ThinkingMap) -> Result<(), ApiError> {
    let problems = thinking_map.validation_errors();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ApiError::bad(format!(
            "invalid thinking_map: {}",
            problems.join("; ")
        )))
    }
}

fn execution_supported_for_model_type(model_type: Option<&str>) -> bool {
    model_type.is_none()
}

fn validate_discovery_execution(discovery: &Value) -> Result<(), ApiError> {
    let imported_from_discovery = discovery
        .get("imported_from_discovery")
        .and_then(Value::as_bool)
        == Some(true);
    let execution_supported = discovery
        .get("execution_supported")
        .and_then(Value::as_bool);
    let model_type = discovery.get("model_type").and_then(Value::as_str);

    if imported_from_discovery && execution_supported != Some(true) {
        return Err(ApiError::bad(
            "discovery import requires explicit execution_supported: true",
        ));
    }

    if execution_supported == Some(false) || !execution_supported_for_model_type(model_type) {
        let model_type = model_type.unwrap_or("specialized");
        return Err(ApiError::bad(format!(
            "cannot import unsupported discovered model type: {model_type}"
        )));
    }

    Ok(())
}

pub async fn create_model(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(provider_id): Path<String>,
    Json(body): Json<ModelBody>,
) -> ApiResult {
    let lock = model_reconciliation_lock(&provider_id);
    let _guard = lock.lock().await;
    let provider = db::get_provider(&state.pool, &provider_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    let transport_override =
        validate_model_transport_override(&provider, body.transport_override.as_deref())?;
    let caps = normalize_model_capabilities(&body.capabilities);
    let prices: Prices = serde_json::from_value(body.prices.clone()).unwrap_or_default();
    validate_thinking_map(&body.thinking_map)?;
    validate_discovery_execution(&body.discovery)?;
    let thinking_map =
        serde_json::to_value(&body.thinking_map).expect("ThinkingMap serialization is infallible");
    let thinking_map_configured = !body.thinking_map.levels.is_empty()
        || body.thinking_map.mode.is_some()
        || body.thinking_map.budget_field.is_some()
        || body.thinking_map.level_field.is_some();
    let imported_from_discovery = body
        .discovery
        .get("imported_from_discovery")
        .and_then(Value::as_bool)
        == Some(true);
    let effective_prices = if imported_from_discovery {
        automatic_prices_for_provider_scope(&prices, &body.discovery, &provider.pricing_scope)
    } else {
        prices.clone()
    };

    let mut discovery_patch = serde_json::Map::new();
    if imported_from_discovery {
        discovery_patch.insert("operator_capability_overrides".into(), json!({}));
        discovery_patch.insert("operator_parameter_overrides".into(), json!({}));
        discovery_patch.insert("operator_thinking_overrides".into(), json!({}));
    } else {
        discovery_patch.insert("operator_capability_overrides".into(), caps.clone());
        discovery_patch.insert(
            "operator_parameter_overrides".into(),
            operator_parameter_support_overrides(&body.parameters),
        );
        discovery_patch.insert(
            "operator_thinking_overrides".into(),
            if thinking_map_configured {
                json!({ "thinking_map": thinking_map.clone() })
            } else {
                json!({})
            },
        );
    }
    if !effective_prices.is_configured() {
        discovery_patch.insert("effective_pricing".into(), Value::Null);
    }
    let discovery_patch = Value::Object(discovery_patch);

    let opaque_state_plugin = if imported_from_discovery
        && body
            .discovery
            .get("opaque_state")
            .and_then(parse_plugin_opaque_state_capability)
            .is_some()
    {
        provider
            .model_source_plugin_ref()
            .map(|reference| reference.plugin_id)
    } else {
        None
    };

    let pricing_values = if effective_prices.is_configured() {
        Some(if imported_from_discovery {
            automatic_price_provenance(&effective_prices, &body.discovery)
        } else {
            operator_price_provenance(&effective_prices)
        })
    } else {
        None
    };
    let pricing = pricing_values
        .as_ref()
        .map(|(source, metadata)| db::ModelPricingMutation {
            prices: &effective_prices,
            source,
            metadata,
        });

    let (id, _) = db::commit_model_creation(
        &state.pool,
        &db::ModelCreation {
            model: db::NewModel {
                provider_id: &provider_id,
                upstream_id: &body.upstream_id,
                display_name: body.display_name.as_deref().unwrap_or(&body.upstream_id),
                enabled: body.enabled,
                context_window: body.context_window,
                max_output_tokens: body.max_output_tokens,
                capabilities: caps,
                prices: serde_json::to_value(&effective_prices).unwrap(),
                parameters: body.parameters.clone(),
                thinking_map,
                extra_request: body.extra_request.clone(),
                discovery: body.discovery.clone(),
            },
            transport: transport_override.as_deref(),
            discovery_patch: &discovery_patch,
            opaque_state_plugin: opaque_state_plugin.as_deref(),
            pricing,
        },
    )
    .await
    .map_err(ApiError::internal)?;

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "model_configured",
        "model",
        &id,
        &body.upstream_id,
        "Configured upstream model.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "id": id })))
}

pub async fn update_model(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<ModelBody>,
) -> ApiResult {
    let provider_id = db::get_model(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("model not found"))?
        .provider_id;
    let lock = model_reconciliation_lock(&provider_id);
    let _guard = lock.lock().await;
    let model = db::get_model(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("model not found"))?;
    let provider = db::get_provider(&state.pool, &model.provider_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    let transport_override =
        validate_model_transport_override(&provider, body.transport_override.as_deref())?;
    let caps = normalize_model_capabilities(&body.capabilities);
    let prices: Prices = serde_json::from_value(body.prices.clone()).unwrap_or_default();
    let existing_discovery = discovery_object(&model);
    let previous_prices = model.prices();
    let mut price_fields = effective_price_fields(&existing_discovery, &previous_prices);
    for field in PRICE_FIELDS {
        let previous = price_field(&previous_prices, field);
        let next = price_field(&prices, field);
        if previous != next {
            if next.is_some() {
                set_price_field_provenance(
                    &mut price_fields,
                    field,
                    "operator",
                    json!({ "configured_by": "admin" }),
                );
            } else {
                price_fields.remove(field);
            }
        }
    }
    let price_source = if prices.is_configured() {
        effective_price_source(&price_fields, &prices)
    } else {
        "operator".to_string()
    };
    let price_metadata = json!({ "fields": price_fields });

    validate_thinking_map(&body.thinking_map)?;
    let existing_caps = normalize_model_capabilities(
        &serde_json::from_str::<Value>(&model.capabilities).unwrap_or_else(|_| json!({})),
    );
    let existing_parameters =
        serde_json::from_str::<Value>(&model.parameters).unwrap_or_else(|_| json!({}));
    let existing_thinking_map =
        serde_json::to_value(model.thinking()).expect("ThinkingMap serialization is infallible");
    let thinking_map =
        serde_json::to_value(&body.thinking_map).expect("ThinkingMap serialization is infallible");
    let mut discovery_patch = serde_json::Map::new();
    if existing_caps != caps {
        discovery_patch.insert("operator_capability_overrides".into(), caps.clone());
    }
    if existing_parameters != body.parameters {
        discovery_patch.insert(
            "operator_parameter_overrides".into(),
            operator_parameter_support_overrides(&body.parameters),
        );
    }
    if existing_thinking_map != thinking_map {
        discovery_patch.insert(
            "operator_thinking_overrides".into(),
            Value::Object(merge_operator_thinking_map_override(
                &existing_discovery,
                &thinking_map,
            )),
        );
    }
    let display_name = body.display_name.as_deref().unwrap_or(&body.upstream_id);
    let discovery_patch = Value::Object(discovery_patch);
    db::commit_model_operator_mutation(
        &state.pool,
        &db::ModelOperatorMutation {
            id: &id,
            display_name,
            enabled: body.enabled,
            context_window: body.context_window,
            max_output_tokens: body.max_output_tokens,
            capabilities: &caps,
            parameters: &body.parameters,
            thinking_map: &thinking_map,
            extra_request: &body.extra_request,
            update_transport: true,
            transport: transport_override.as_deref(),
            discovery_patch: &discovery_patch,
            pricing: Some(db::ModelPricingMutation {
                prices: &prices,
                source: &price_source,
                metadata: &price_metadata,
            }),
        },
    )
    .await
    .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "model_updated",
        "model",
        &id,
        &body.upstream_id,
        "Updated model configuration.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_model(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    db::delete_model(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "model_deleted",
        "model",
        &id,
        "",
        "Removed model.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

// ===========================================================================
// Accounts
// ===========================================================================

#[derive(Deserialize)]
pub struct AccountListQuery {
    pub provider_id: Option<String>,
}

pub async fn list_accounts(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Query(query): Query<AccountListQuery>,
) -> ApiResult {
    let accounts = if let Some(provider_id) = query.provider_id.as_deref() {
        db::accounts_for_provider(&state.pool, provider_id)
            .await
            .map_err(ApiError::internal)?
    } else {
        db::list_accounts(&state.pool)
            .await
            .map_err(ApiError::internal)?
    };
    let providers = db::list_providers(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let (_, by_account) = db::lifetime_totals(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let out: Vec<Value> = accounts
        .iter()
        .filter(|a| {
            providers
                .iter()
                .find(|p| p.id == a.provider_id)
                .map(|p| !(p.credential_mode == "none" && a.label == "__kinetix_noauth__"))
                .unwrap_or(true)
        })
        .map(|a| {
            let (requests, tokens) = by_account.get(&a.id).copied().unwrap_or((0, 0));
            account_json(a, &providers, requests, tokens)
        })
        .collect();
    Ok(Json(json!({ "accounts": out })))
}

fn account_json(
    a: &db::AccountRow,
    providers: &[db::ProviderRow],
    requests_count: i64,
    tokens_count: i64,
) -> Value {
    let provider_name = providers
        .iter()
        .find(|p| p.id == a.provider_id)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    json!({
        "id": a.id,
        "provider_id": a.provider_id,
        "provider_name": provider_name,
        "label": a.label,
        "key_mask": a.key_mask,
        "status": a.status,
        "cooldown_until": a.cooldown_until,
        "quota_reset_at": a.quota_reset_at,
        "quota_type": a.quota_type,
        "soft_quota_usd": a.soft_quota_usd,
        "priority": a.priority,
        "weight": a.weight,
        "last_error": a.last_error,
        "created_at": a.created_at,
        "requests_count": requests_count,
        "tokens_count": tokens_count,
    })
}

#[derive(Deserialize)]
pub struct AccountBody {
    pub provider_id: String,
    pub label: String,
    pub api_key: Option<String>,
    #[serde(default = "one")]
    pub priority: i64,
    #[serde(default = "one")]
    pub weight: i64,
    pub soft_quota_usd: Option<f64>,
    #[serde(default = "default_quota_type")]
    pub quota_type: String,
    pub status: Option<String>,
}

fn one() -> i64 {
    1
}
fn default_quota_type() -> String {
    "none".into()
}

fn manual_account_enrollment_error(mode: &str) -> Option<&'static str> {
    match mode {
        "auth_flow" => Some("provider uses an authentication flow; connect an account instead"),
        "none" => Some("provider does not require user credentials"),
        _ => None,
    }
}

pub async fn create_account(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<AccountBody>,
) -> ApiResult {
    let provider = db::get_provider(&state.pool, &body.provider_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    if let Some(error) = manual_account_enrollment_error(&provider.credential_mode) {
        return Err(ApiError::bad(error));
    }

    let api_key = body
        .api_key
        .clone()
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| ApiError::bad("api_key is required"))?;
    let enc = state.crypto.encrypt(&api_key).map_err(ApiError::internal)?;
    let id = db::insert_account(
        &state.pool,
        &body.provider_id,
        &body.label,
        &enc,
        &crypto::mask_secret(&api_key),
        body.priority,
        body.weight,
        body.soft_quota_usd,
        &body.quota_type,
    )
    .await
    .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "account_credential_added",
        "account",
        &id,
        &body.label,
        "Enrolled a new credential into the pool.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "id": id })))
}

pub async fn update_account(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<AccountBody>,
) -> ApiResult {
    db::update_account(
        &state.pool,
        &id,
        &body.label,
        body.status.as_deref().unwrap_or("healthy"),
        body.priority,
        body.weight,
        body.soft_quota_usd,
        &body.quota_type,
    )
    .await
    .map_err(ApiError::internal)?;
    // Optionally rotate a manually enrolled credential.
    if let Some(api_key) = body.api_key.filter(|k| !k.trim().is_empty()) {
        let account = db::get_account(&state.pool, &id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("account not found"))?;
        let provider = db::get_provider(&state.pool, &account.provider_id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("provider not found"))?;
        if let Some(error) = manual_account_enrollment_error(&provider.credential_mode) {
            return Err(ApiError::bad(error));
        }
        let enc = state.crypto.encrypt(&api_key).map_err(ApiError::internal)?;
        sqlx::query("UPDATE accounts SET secret_enc=?, key_mask=? WHERE id=?")
            .bind(enc)
            .bind(crypto::mask_secret(&api_key))
            .bind(&id)
            .execute(&state.pool)
            .await
            .map_err(ApiError::internal)?;
    }
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "account_updated",
        "account",
        &id,
        &body.label,
        "Updated account configuration.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

/// `POST /admin/api/accounts/:id/reset` — clear cooldown/exhaustion.
pub async fn reset_account(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    crate::pool::mark_healthy(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let _ = crate::pool::clear_circuit(&state.pool, &id).await;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "account_reset",
        "account",
        &id,
        "",
        "Cleared cooldown/exhaustion/circuit state.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_account(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    db::delete_account(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "account_credential_removed",
        "account",
        &id,
        "",
        "Removed credential from the pool.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

// ===========================================================================
// Routes
// ===========================================================================

pub async fn list_routes(State(state): State<AppState>, _auth: AdminAuth) -> ApiResult {
    let routes = db::list_routes(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let accounts = db::list_accounts(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let models = db::list_models(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let mut out = Vec::new();
    for c in &routes {
        let targets = db::route_targets(&state.pool, &c.id)
            .await
            .map_err(ApiError::internal)?;
        let targets_json: Vec<Value> = targets
            .iter()
            .map(|t| {
                let model = models.iter().find(|m| m.id == t.model_id);
                let account = t.account_id.as_ref().and_then(|aid| accounts.iter().find(|a| a.id == *aid));
                json!({
                    "id": t.id,
                    "account_id": t.account_id,
                    "account_label": account.map(|a| a.label.clone()),
                    "model_id": t.model_id,
                    "model_display_name": model.map(|m| m.display_name.clone()),
                    "provider_id": model.map(|m| m.provider_id.clone()),
                    "priority": t.priority,
                    "weight": t.weight,
                    "predicate": serde_json::from_str::<Value>(&t.predicate).unwrap_or(json!({})),
                    "param_overrides": serde_json::from_str::<Value>(&t.param_overrides).unwrap_or(json!({})),
                })
            })
            .collect();
        out.push(json!({
            "id": c.id,
            "name": c.name,
            "description": c.description,
            "strategy": c.strategy,
            "fallback_triggers": serde_json::from_str::<Value>(&c.fallback_triggers).unwrap_or(json!({})),
            "portability_policy": c.portability_policy,
            "sticky_routing": c.sticky_routing != 0,
            "cache_affinity": c.cache_affinity != 0,
            "max_attempts": c.max_attempts,
            "enabled": c.enabled != 0,
            "targets": targets_json,
        }));
    }
    Ok(Json(json!({ "routes": out })))
}

#[derive(Deserialize)]
pub struct RouteBody {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "priority_strategy")]
    pub strategy: String,
    #[serde(default)]
    pub fallback_triggers: Value,
    /// FR-2.11: reject | strip_with_warning.
    #[serde(default = "portability_default")]
    pub portability_policy: String,
    #[serde(default)]
    pub sticky_routing: bool,
    /// FR-7.3: cache-aware sticky routing.
    #[serde(default)]
    pub cache_affinity: bool,
    pub max_attempts: Option<i64>,
    #[serde(default)]
    pub targets: Vec<RouteTargetBody>,
}

fn priority_strategy() -> String {
    "priority".into()
}
fn portability_default() -> String {
    "strip_with_warning".into()
}

fn validate_route_body(body: &RouteBody) -> Result<(), ApiError> {
    if !matches!(
        body.strategy.as_str(),
        "priority" | "round-robin" | "weighted" | "least-used" | "adaptive"
    ) {
        return Err(ApiError::bad("invalid route strategy"));
    }
    if !matches!(
        body.portability_policy.as_str(),
        "reject" | "strip_with_warning"
    ) {
        return Err(ApiError::bad(
            "portability_policy must be 'reject' or 'strip_with_warning'",
        ));
    }
    if !body.fallback_triggers.is_null() {
        let Some(triggers) = body.fallback_triggers.as_object() else {
            return Err(ApiError::bad("fallback_triggers must be a JSON object"));
        };
        for key in ["on429", "onQuota", "on5xx", "onTimeout"] {
            if let Some(value) = triggers.get(key) {
                if !value.is_boolean() {
                    return Err(ApiError::bad(format!(
                        "fallback_triggers.{key} must be boolean"
                    )));
                }
            }
        }
    }
    for target in &body.targets {
        if !target.param_overrides.is_null() && !target.param_overrides.is_object() {
            return Err(ApiError::bad(
                "route target param_overrides must be a JSON object",
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct RouteTargetBody {
    pub account_id: Option<String>,
    pub model_id: String,
    #[serde(default = "one")]
    pub priority: i64,
    #[serde(default = "one")]
    pub weight: i64,
    /// Typed eligibility predicate (FR-12.3). Empty/absent = always eligible.
    #[serde(default)]
    pub predicate: Value,
    /// Per-target parameter overrides (FR-12.2).
    #[serde(default)]
    pub param_overrides: Value,
}

pub async fn create_route(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<RouteBody>,
) -> ApiResult {
    validate_route_body(&body)?;
    let id = db::insert_route(
        &state.pool,
        &db::NewRoute {
            name: &body.name,
            description: &body.description,
            strategy: &body.strategy,
            fallback_triggers: if body.fallback_triggers.is_null() {
                json!({"on429": true, "onQuota": true, "on5xx": true, "onTimeout": true})
            } else {
                body.fallback_triggers.clone()
            },
            portability_policy: &body.portability_policy,
            sticky_routing: body.sticky_routing,
            cache_affinity: body.cache_affinity,
            max_attempts: body.max_attempts,
        },
    )
    .await
    .map_err(ApiError::internal)?;
    write_route_targets(&state.pool, &id, &body.targets).await?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "route_created",
        "route",
        &id,
        &body.name,
        &format!("Created route with {} targets.", body.targets.len()),
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "id": id })))
}

pub async fn update_route(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<RouteBody>,
) -> ApiResult {
    validate_route_body(&body)?;
    db::update_route(
        &state.pool,
        &id,
        &body.description,
        &body.strategy,
        body.fallback_triggers.clone(),
        &body.portability_policy,
        body.sticky_routing,
        body.cache_affinity,
        body.max_attempts,
    )
    .await
    .map_err(ApiError::internal)?;
    db::clear_route_targets(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    write_route_targets(&state.pool, &id, &body.targets).await?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "route_updated",
        "route",
        &id,
        &body.name,
        "Updated route configuration.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

async fn write_route_targets(
    pool: &Pool,
    route_id: &str,
    targets: &[RouteTargetBody],
) -> Result<(), ApiError> {
    for t in targets {
        let predicate = if t.predicate.is_null() {
            "{}".to_string()
        } else {
            t.predicate.to_string()
        };
        let overrides = if t.param_overrides.is_null() {
            "{}".to_string()
        } else {
            t.param_overrides.to_string()
        };
        db::insert_route_target(
            pool,
            route_id,
            t.account_id.as_deref(),
            &t.model_id,
            t.priority,
            t.weight,
            &predicate,
            &overrides,
        )
        .await
        .map_err(ApiError::internal)?;
    }
    Ok(())
}

pub async fn delete_route(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    db::delete_route(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "route_deleted",
        "route",
        &id,
        "",
        "Deleted route.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

/// `POST /admin/api/routes/dry-run` (FR-8.7): evaluate routing for a
/// representative request descriptor without mutating anything.
#[derive(Deserialize)]
pub struct DryRunBody {
    pub model: String,
    #[serde(flatten, default)]
    pub descriptor: pipeline::DryRunRequest,
}

pub async fn dry_run_route(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<DryRunBody>,
) -> ApiResult {
    let out = pipeline::dry_run(&state, &body.model, &body.descriptor)
        .await
        .map_err(|e| ApiError::bad(e.message))?;
    Ok(Json(out))
}

/// `POST /admin/api/validate` (FR-8.6): validate a provider endpoint (schema,
/// TLS/SSRF, credential-host binding) and, optionally, connectivity + resolved
/// IP/ASN. Never mutates production state.
#[derive(Deserialize)]
pub struct ValidateBody {
    pub base_url: String,
    #[serde(default)]
    pub check_connectivity: bool,
}

/// `POST /admin/api/validate/provider` (FR-8.6): full schema + outbound-security
/// validation of a proposed provider, without creating it.
pub async fn validate_provider(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<ProviderBody>,
) -> ApiResult {
    let mut problems = crate::validate::validate_provider_schema(
        &body.name,
        &body.base_url,
        &body.wire_format,
        &body.auth_scheme,
        body.custom_header_name.as_deref(),
        body.custom_param_name.as_deref(),
    );
    problems.extend(provider_plugin_binding_problems(&state, &body).await);
    if body.wire_format == "plugin" && body.wire_plugin.trim().is_empty() {
        problems.push("wire_format 'plugin' requires a wire_plugin binding".into());
    }
    let mut warnings: Vec<String> = Vec::new();
    let mut security: Value = Value::String("not_checked".into());
    if body.base_url.trim().is_empty() {
        // already reported as a schema problem
    } else {
        match validate_outbound_url(&state, &body.base_url) {
            Ok(()) => security = Value::String("passed".into()),
            Err(ApiError(_, msg)) => problems.push(msg),
        }
    }
    if body.wire_format == "anthropic"
        && !body
            .extra_headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("anthropic-version"))
    {
        warnings.push(
            "anthropic wire format: set an 'anthropic-version' extra header (Kinetix adds no hidden defaults)"
                .into(),
        );
    }
    // Credential-host binding (NFR-3.11): the credential is bound to the
    // provider's base host plus any explicitly authorized hosts. Flag malformed
    // entries (a scheme/path/port is not a host) so a misconfigured binding is
    // caught before Apply.
    let mut binding: Vec<String> = Vec::new();
    if let Ok(parsed) = url::Url::parse(&body.base_url) {
        if let Some(h) = parsed.host_str() {
            binding.push(h.to_string());
        }
    }
    for entry in body.credential_hosts.split(',') {
        let host = entry.trim();
        if host.is_empty() {
            continue;
        }
        if host.contains('/') || host.contains(' ') || host.contains("://") {
            problems.push(format!(
                "credential_hosts entry '{host}' is not a bare host (drop the scheme/path)"
            ));
        } else {
            binding.push(host.to_string());
        }
    }
    Ok(Json(json!({
        "valid": problems.is_empty(),
        "problems": problems,
        "warnings": warnings,
        "outbound_security": security,
        "credential_host_binding": binding,
        "note": "Validate only: no provider was created and no upstream call was made (FR-8.6).",
    })))
}

/// `POST /admin/api/validate/model` (FR-8.6): schema + metadata validation of a
/// proposed model (unknown price/capability data reported, never assumed).
pub async fn validate_model_edit(
    State(_state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<ModelBody>,
) -> ApiResult {
    let mut out = crate::validate::validate_model(
        &body.upstream_id,
        body.context_window,
        body.max_output_tokens,
        &body.capabilities,
        &body.prices,
        &body.parameters,
    );
    let mut validation_problems = body.thinking_map.validation_errors();
    if let Some(transport) = body
        .transport_override
        .as_deref()
        .map(str::trim)
        .filter(|transport| !transport.is_empty())
    {
        if crate::adapters::TargetTransport::parse(transport).is_none() {
            validation_problems.push(format!("unsupported model transport '{transport}'"));
        }
    }
    if !validation_problems.is_empty() {
        if let Some(problems) = out.get_mut("problems").and_then(Value::as_array_mut) {
            problems.extend(validation_problems.into_iter().map(Value::String));
        }
        out["valid"] = Value::Bool(false);
    }
    Ok(Json(out))
}

/// `POST /admin/api/validate/account` (FR-8.6): schema validation of a proposed
/// account. The credential is not stored; only its presence is checked.
pub async fn validate_account_edit(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<AccountBody>,
) -> ApiResult {
    let mut problems =
        crate::validate::validate_account(&body.label, body.api_key.as_deref(), &body.quota_type);
    if let Some(provider) = db::get_provider(&state.pool, &body.provider_id)
        .await
        .map_err(ApiError::internal)?
    {
        if let Some(error) = manual_account_enrollment_error(&provider.credential_mode) {
            problems.push(error.into());
        }
    } else {
        problems.push("provider not found".into());
    }
    Ok(Json(json!({
        "valid": problems.is_empty(),
        "problems": problems,
        "note": "Validate only: no account was created (FR-8.6).",
    })))
}

pub async fn validate_endpoint(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<ValidateBody>,
) -> ApiResult {
    validate_outbound_url(&state, &body.base_url)?;
    let parsed =
        url::Url::parse(&body.base_url).map_err(|e| ApiError::bad(format!("invalid URL: {e}")))?;
    let host = parsed.host_str().unwrap_or("").to_string();
    let mut resolved: Vec<String> = Vec::new();
    let mut asn: Value = Value::String("unknown".into());
    let mut reachable: Value = Value::String("not_checked".into());
    if body.check_connectivity {
        let port = parsed.port_or_known_default().unwrap_or(443);
        match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(addrs) => {
                for a in addrs {
                    resolved.push(a.ip().to_string());
                }
                reachable = Value::Bool(true);
            }
            Err(e) => {
                reachable = Value::String(format!("dns_error: {e}"));
            }
        }
        // ASN lookup is not implemented; the requirement says show unknown
        // rather than guess (FR-8.8).
        asn = Value::String("unknown".into());
    }
    Ok(Json(json!({
        "valid": true,
        "scheme": parsed.scheme(),
        "host": host,
        "resolved_ips": resolved,
        "asn": asn,
        "connectivity": reachable,
        "note": "ASN is reported as unknown when it cannot be established; Kinetix never guesses (FR-8.8)."
    })))
}

// ===========================================================================
// Aliases
// ===========================================================================

pub async fn list_aliases(State(state): State<AppState>, _auth: AdminAuth) -> ApiResult {
    let aliases = db::list_aliases(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let models = db::list_models(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let routes = db::list_routes(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let out: Vec<Value> = aliases
        .iter()
        .map(|a| {
            let display = if a.target_type == "route" {
                routes
                    .iter()
                    .find(|c| c.id == a.target_id)
                    .map(|c| format!("Route: {}", c.name))
            } else {
                models
                    .iter()
                    .find(|m| m.id == a.target_id)
                    .map(|m| format!("Model: {}", m.display_name))
            };
            json!({
                "id": a.id,
                "alias": a.alias,
                "target_type": a.target_type,
                "target_id": a.target_id,
                "target_display_name": display,
                "description": a.description,
            })
        })
        .collect();
    Ok(Json(json!({ "aliases": out })))
}

#[derive(Deserialize)]
pub struct AliasBody {
    pub alias: String,
    pub target_type: String,
    pub target_id: String,
    #[serde(default)]
    pub description: String,
}

pub async fn create_alias(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<AliasBody>,
) -> ApiResult {
    let id = db::upsert_alias(
        &state.pool,
        &body.alias,
        &body.target_type,
        &body.target_id,
        &body.description,
    )
    .await
    .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "alias_upserted",
        "alias",
        &id,
        &body.alias,
        "Upserted model alias.",
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "id": id })))
}

pub async fn delete_alias(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    db::delete_alias(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

// ===========================================================================
// Usage / requests / audit
// ===========================================================================

#[derive(Deserialize)]
pub struct LimitQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    200
}

pub async fn usage(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Query(q): Query<LimitQuery>,
) -> ApiResult {
    let rows = db::recent_usage(&state.pool, q.limit.min(2000))
        .await
        .map_err(ApiError::internal)?;
    let summary = db::usage_summary(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let out: Vec<Value> = rows.iter().map(usage_json).collect();
    Ok(Json(json!({ "usage": out, "summary": summary })))
}

fn usage_json(u: &db::UsageLogRow) -> Value {
    json!({
        "id": u.id,
        "request_id": u.request_id,
        "timestamp": u.ts,
        "key_id": u.key_id,
        "key_name": u.key_name,
        "client_format": u.client_format,
        "requested_model": u.requested_model,
        "effective_model": u.effective_model,
        "route_id": u.route_id,
        "route_name": u.route_name,
        "fallback_hops": u.fallback_hops,
        "fallback_path": serde_json::from_str::<Value>(&u.fallback_path).unwrap_or(json!([])),
        "status": u.status,
        "status_code": u.status_code,
        "latency_ms": u.latency_ms,
        "ttft_ms": u.ttft_ms,
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cached_tokens": u.cached_tokens,
        "cache_write_tokens": u.cache_write_tokens,
        "thinking_tokens": u.thinking_tokens,
        "cost_usd": u.cost_usd,
        "cost_known": u.cost_known != 0,
        "cache_status": u.cache_status,
        "serving_account_id": u.serving_account_id,
        "serving_account": u.serving_account,
        "serving_provider": u.serving_provider,
        "flagged": u.flagged != 0,
        "error_message": u.error_message,
        "usage_confidence": u.usage_confidence,
        "commit_state": u.commit_state,
        "retry_count": u.retry_count,
        "opaque_route_id": u.opaque_route_id,
    })
}

/// `GET /admin/api/requests/{id}/route-trace` (FR-12.14, NFR-4.3).
pub async fn request_route_trace(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(request_id): Path<String>,
) -> ApiResult {
    let trace = db::get_route_trace_by_request(&state.pool, &request_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("no route trace for that request id"))?;
    Ok(Json(route_trace_json(&trace)))
}

/// `GET /admin/api/route-traces/{opaque_id}` (FR-12.15): resolve the opaque
/// `X-Kinetix-Route-Id` a client received back to its Route Trace. Serving
/// topology is admin-only, so this never leaks to the client itself.
pub async fn route_trace_by_opaque(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(opaque_id): Path<String>,
) -> ApiResult {
    let trace = db::get_route_trace_by_opaque(&state.pool, &opaque_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("no route trace for that opaque route id"))?;
    Ok(Json(route_trace_json(&trace)))
}

/// `GET /admin/api/requests/{id}/diagnostics` (FR-13.4): correlate the Route
/// Trace with the flight-recorder events for one request.
pub async fn request_diagnostics(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(request_id): Path<String>,
) -> ApiResult {
    let trace = db::get_route_trace_by_request(&state.pool, &request_id)
        .await
        .map_err(ApiError::internal)?;
    let flight = state.flight.events(&request_id);
    let usage = db::recent_usage(&state.pool, 2000)
        .await
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|u| u.request_id == request_id)
        .map(|u| usage_json(&u));
    Ok(Json(json!({
        "request_id": request_id,
        "route_trace": trace.as_ref().map(route_trace_json),
        "flight_events": flight,
        "usage": usage,
        "flight_recorder": {
            "tracked_requests": state.flight.request_count(),
            "dropped_requests": state.flight.dropped_requests(),
            "dropped_events": state.flight.dropped_events(),
        }
    })))
}

/// Live in-flight request view (FR-8.3). Control-plane only: served from an
/// in-memory registry so it never touches the data plane, and DB enrichment is
/// best-effort so a degraded store cannot fail the view (NFR-2.6/2.7).
pub async fn live_requests(State(state): State<AppState>, _auth: AdminAuth) -> ApiResult {
    let mut rows = state.live.snapshot();
    // Best-effort enrichment: for a finished request still in the tail, attach
    // the persisted commit state / status / tokens if the DB is reachable.
    if db_healthy(&state).await {
        if let Ok(recent) = db::recent_usage(&state.pool, 200).await {
            let by_id: std::collections::HashMap<&str, &db::UsageLogRow> =
                recent.iter().map(|u| (u.request_id.as_str(), u)).collect();
            for r in rows.iter_mut() {
                if let Some(u) = by_id.get(r.request_id.as_str()) {
                    if r.finished {
                        r.status = u.status.clone();
                        r.commit_state = u.commit_state.clone();
                        r.retry_count = u.retry_count.max(0) as u32;
                        r.fallback_hops = u.fallback_hops.max(0) as u32;
                        r.input_tokens = u.input_tokens.map(|v| v.max(0) as u64);
                        r.output_tokens = u.output_tokens.map(|v| v.max(0) as u64);
                    }
                }
            }
        }
    }
    Ok(Json(json!({
        "live": rows,
        "live_count": state.live.live_count(),
        "dropped": state.live.dropped(),
    })))
}

fn route_trace_json(t: &db::RouteTraceRow) -> Value {
    json!({
        "request_id": t.request_id,
        "opaque_route_id": t.opaque_route_id,
        "timestamp": t.ts,
        "requested_model": t.requested_model,
        "route_id": t.route_id,
        "route_name": t.route_name,
        "final_target": t.final_target,
        "commit_state": t.commit_state,
        "outcome": t.outcome,
        "steps": serde_json::from_str::<Value>(&t.steps).unwrap_or(json!([])),
        "warnings": serde_json::from_str::<Value>(&t.warnings).unwrap_or(json!([])),
    })
}

/// `GET /admin/api/exports` — list exported usage/log files on disk, plus the
/// per-day usage available for export.
pub async fn list_exports(State(state): State<AppState>, _auth: AdminAuth) -> ApiResult {
    let dir = state.config.paths.exports_dir();
    let files = crate::export::list_files(&dir);
    let days = db::usage_days(&state.pool)
        .await
        .map_err(ApiError::internal)?
        .into_iter()
        .map(
            |(day, requests, tokens)| json!({ "day": day, "requests": requests, "tokens": tokens }),
        )
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "dir": dir.display().to_string(),
        "retention_days": state.config.export_retention_days,
        "files": files,
        "days": days,
    })))
}

#[derive(serde::Deserialize)]
pub struct ExportDayBody {
    /// `YYYY-MM-DD`; defaults to yesterday (UTC) when omitted.
    #[serde(default)]
    day: Option<String>,
}

/// `POST /admin/api/exports` — write one day's usage to disk on demand.
pub async fn export_usage_day(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<ExportDayBody>,
) -> ApiResult {
    let day = body.day.unwrap_or_else(|| {
        (chrono::Utc::now().date_naive() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string()
    });
    let dir = state.config.paths.exports_dir();
    let (jsonl, csv) = crate::export::export_day(&state.pool, &dir, &day)
        .await
        .map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "usage_exported",
        "system",
        &day,
        "Usage Export",
        &format!("Exported usage for {day} to disk."),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "day": day,
        "jsonl": jsonl.display().to_string(),
        "csv": csv.display().to_string(),
    })))
}

/// `DELETE /admin/api/exports/{name}` — remove one exported file from disk.
pub async fn delete_export(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(name): Path<String>,
) -> ApiResult {
    let dir = state.config.paths.exports_dir();
    let removed = crate::export::delete_file(&dir, &name).map_err(ApiError::internal)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "export_deleted",
        "system",
        &name,
        "Usage Export",
        "Deleted an exported usage file.",
    )
    .await;
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

pub async fn audit(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Query(q): Query<LimitQuery>,
) -> ApiResult {
    let rows = db::recent_audit(&state.pool, q.limit.min(2000))
        .await
        .map_err(ApiError::internal)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "timestamp": a.ts,
                "actor": a.actor,
                "action": a.action,
                "target_type": a.target_type,
                "target_id": a.target_id,
                "target_name": a.target_name,
                "details": a.details,
            })
        })
        .collect();
    Ok(Json(json!({ "audit": out })))
}

/// Prometheus-format metrics (NFR-4.2).
/// True when the control-plane database answers a trivial query.
pub async fn db_healthy(state: &AppState) -> bool {
    sqlx::query("SELECT 1").fetch_one(&state.pool).await.is_ok()
}

/// Admin-router middleware: every state-changing request (anything that is not
/// GET/HEAD) must fail closed when the control-plane store is unavailable
/// (NFR-2.7: "admin mutations fail closed"). Reads are allowed so an operator
/// can still inspect what is cached in memory while the store is degraded.
pub async fn require_control_plane(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    if method != axum::http::Method::GET
        && method != axum::http::Method::HEAD
        && !db_healthy(&state).await
    {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "admin mutation unavailable: control-plane store is degraded"})),
        )
            .into_response();
    }
    next.run(req).await
}

pub async fn metrics(State(state): State<AppState>, _auth: AdminAuth) -> Response {
    // Serve whatever is available from memory even when the store is down; the
    // control plane degrades, the data plane does not (NFR-2.6/2.7).
    let healthy = db_healthy(&state).await;
    let summary = if healthy {
        db::usage_summary(&state.pool).await.unwrap_or(json!({}))
    } else {
        json!({})
    };
    let accounts = if healthy {
        db::list_accounts(&state.pool).await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut body = String::new();
    body.push_str(
        "# HELP kinetix_control_plane_degraded 1 when the control-plane store is unavailable\n",
    );
    body.push_str("# TYPE kinetix_control_plane_degraded gauge\n");
    body.push_str(&format!(
        "kinetix_control_plane_degraded {}\n",
        if healthy { 0 } else { 1 }
    ));
    body.push_str("# HELP kinetix_requests_total Total proxied requests\n");
    body.push_str("# TYPE kinetix_requests_total counter\n");
    body.push_str(&format!(
        "kinetix_requests_total {}\n",
        summary["requests"].as_i64().unwrap_or(0)
    ));
    // Allocation accounting (NFR-1.8). Zero unless built with `alloc-stats`, so
    // the value honestly reads "not measured" rather than fabricating a number.
    body.push_str("# HELP kinetix_allocations_total Allocation calls (0 unless built with --features alloc-stats)\n");
    body.push_str("# TYPE kinetix_allocations_total counter\n");
    body.push_str(&format!(
        "kinetix_allocations_total {}\n",
        crate::alloc::allocations()
    ));
    body.push_str("# HELP kinetix_alloc_bytes_total Bytes allocated (0 unless built with --features alloc-stats)\n");
    body.push_str("# TYPE kinetix_alloc_bytes_total counter\n");
    body.push_str(&format!(
        "kinetix_alloc_bytes_total {}\n",
        crate::alloc::alloc_bytes()
    ));
    // Request/error rate (NFR-4.2): errors over total requests, including
    // client disconnects, so the ratio matches the alert loop's definition.
    {
        let reqs = summary["requests"].as_i64().unwrap_or(0).max(1);
        let errs = summary["error_requests"].as_i64().unwrap_or(0);
        body.push_str("# HELP kinetix_error_rate Request error ratio (0..1)\n");
        body.push_str("# TYPE kinetix_error_rate gauge\n");
        body.push_str(&format!(
            "kinetix_error_rate {}\n",
            errs as f64 / reqs as f64
        ));
    }
    body.push_str(
        "# HELP kinetix_ip_rate_limited_total Requests rejected by the per-IP limiter (NFR-3.6)\n",
    );
    body.push_str("# TYPE kinetix_ip_rate_limited_total counter\n");
    body.push_str(&format!(
        "kinetix_ip_rate_limited_total {}\n",
        state.ip_limiter.limited_total()
    ));
    body.push_str("# HELP kinetix_cost_usd_total Total computed cost in USD\n");
    body.push_str("# TYPE kinetix_cost_usd_total counter\n");
    body.push_str(&format!(
        "kinetix_cost_usd_total {}\n",
        summary["cost_usd"].as_f64().unwrap_or(0.0)
    ));
    body.push_str("# HELP kinetix_log_queue_depth Pending usage-log rows\n");
    body.push_str("# TYPE kinetix_log_queue_depth gauge\n");
    body.push_str(&format!(
        "kinetix_log_queue_depth {}\n",
        state.log_queue.depth()
    ));
    body.push_str("# HELP kinetix_log_queue_dropped_total Dropped usage-log rows\n");
    body.push_str("# TYPE kinetix_log_queue_dropped_total counter\n");
    body.push_str(&format!(
        "kinetix_log_queue_dropped_total {}\n",
        state.log_queue.dropped()
    ));
    body.push_str("# HELP kinetix_account_status Account health by status\n");
    body.push_str("# TYPE kinetix_account_status gauge\n");
    for status in ["healthy", "cooldown", "exhausted", "disabled"] {
        let n = accounts.iter().filter(|a| a.status == status).count();
        body.push_str(&format!(
            "kinetix_account_status{{status=\"{status}\"}} {n}\n"
        ));
    }
    // Commit-point failure counters (FR-4.9, NFR-4.2).
    body.push_str("# HELP kinetix_failures_pre_commit_total Failures before the commit point\n");
    body.push_str("# TYPE kinetix_failures_pre_commit_total counter\n");
    body.push_str(&format!(
        "kinetix_failures_pre_commit_total {}\n",
        state.failures_pre_commit.load(Ordering::Relaxed)
    ));
    body.push_str("# HELP kinetix_failures_post_commit_total Failures after the commit point\n");
    body.push_str("# TYPE kinetix_failures_post_commit_total counter\n");
    body.push_str(&format!(
        "kinetix_failures_post_commit_total {}\n",
        state.failures_post_commit.load(Ordering::Relaxed)
    ));
    body.push_str("# HELP kinetix_cancellations_total Client-disconnect cancellations\n");
    body.push_str("# TYPE kinetix_cancellations_total counter\n");
    body.push_str(&format!(
        "kinetix_cancellations_total {}\n",
        state.cancellations.load(Ordering::Relaxed)
    ));
    let cancel_total = state.cancellations.load(Ordering::Relaxed);
    let cancel_ms = state.cancellation_latency_ms_total.load(Ordering::Relaxed);
    let avg_cancel = if cancel_total > 0 {
        cancel_ms as f64 / cancel_total as f64
    } else {
        0.0
    };
    body.push_str("# HELP kinetix_cancellation_latency_ms Average cancellation latency (ms)\n");
    body.push_str("# TYPE kinetix_cancellation_latency_ms gauge\n");
    body.push_str(&format!("kinetix_cancellation_latency_ms {avg_cancel}\n"));
    body.push_str("# HELP kinetix_avg_latency_ms Average end-to-end latency (ms)\n");
    body.push_str("# TYPE kinetix_avg_latency_ms gauge\n");
    body.push_str(&format!(
        "kinetix_avg_latency_ms {}\n",
        summary["avg_latency_ms"].as_f64().unwrap_or(0.0)
    ));
    body.push_str("# HELP kinetix_avg_ttft_ms Average time-to-first-token (ms)\n");
    body.push_str("# TYPE kinetix_avg_ttft_ms gauge\n");
    body.push_str(&format!(
        "kinetix_avg_ttft_ms {}\n",
        summary["avg_ttft_ms"].as_f64().unwrap_or(0.0)
    ));
    body.push_str("# HELP kinetix_cached_tokens_total Provider-reported cached prompt tokens\n");
    body.push_str("# TYPE kinetix_cached_tokens_total counter\n");
    body.push_str(&format!(
        "kinetix_cached_tokens_total {}\n",
        summary["cached_tokens"].as_i64().unwrap_or(0)
    ));
    body.push_str(
        "# HELP kinetix_cache_write_tokens_total Provider-reported cache-write prompt tokens\n",
    );
    body.push_str("# TYPE kinetix_cache_write_tokens_total counter\n");
    body.push_str(&format!(
        "kinetix_cache_write_tokens_total {}\n",
        summary["cache_write_tokens"].as_i64().unwrap_or(0)
    ));
    // Accounting confidence (FR-6.8, NFR-4.2): usage rows whose tokens were not
    // provider-reported. Unknown/estimated rows must never be read as exact.
    body.push_str("# HELP kinetix_usage_unknown_total Requests whose token usage is unknown\n");
    body.push_str("# TYPE kinetix_usage_unknown_total counter\n");
    body.push_str(&format!(
        "kinetix_usage_unknown_total {}\n",
        summary["unknown_usage_requests"].as_i64().unwrap_or(0)
    ));
    body.push_str("# HELP kinetix_usage_estimated_total Requests whose token usage is estimated\n");
    body.push_str("# TYPE kinetix_usage_estimated_total counter\n");
    body.push_str(&format!(
        "kinetix_usage_estimated_total {}\n",
        summary["estimated_usage_requests"].as_i64().unwrap_or(0)
    ));
    body.push_str(
        "# HELP kinetix_usage_unknown_cost_total Requests whose cost is unknown (no prices)\n",
    );
    body.push_str("# TYPE kinetix_usage_unknown_cost_total counter\n");
    body.push_str(&format!(
        "kinetix_usage_unknown_cost_total {}\n",
        summary["unknown_cost_requests"].as_i64().unwrap_or(0)
    ));
    body.push_str("# HELP kinetix_credential_failures_total Credential-strategy failures\n");
    body.push_str("# TYPE kinetix_credential_failures_total counter\n");
    body.push_str(&format!(
        "kinetix_credential_failures_total {}\n",
        crate::alerts::credential_failures()
    ));
    body.push_str("# HELP kinetix_fallback_hops_total Total fallback hops across requests\n");
    body.push_str("# TYPE kinetix_fallback_hops_total counter\n");
    body.push_str(&format!(
        "kinetix_fallback_hops_total {}\n",
        summary["fallback_hops"].as_i64().unwrap_or(0)
    ));
    body.push_str(
        "# HELP kinetix_route_fallbacks_total Requests served after at least one fallback hop\n",
    );
    body.push_str("# TYPE kinetix_route_fallbacks_total counter\n");
    body.push_str(&format!(
        "kinetix_route_fallbacks_total {}\n",
        state
            .route_fallbacks
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    body.push_str(
        "# HELP kinetix_route_skip_total Route targets skipped during eligibility filtering\n",
    );
    body.push_str("# TYPE kinetix_route_skip_total counter\n");
    body.push_str(&format!(
        "kinetix_route_skip_total {}\n",
        state.route_skips.load(std::sync::atomic::Ordering::Relaxed)
    ));
    body.push_str(
        "# HELP kinetix_provider_circuit_open 1 when a provider circuit is open or half-open\n",
    );
    body.push_str("# TYPE kinetix_provider_circuit_open gauge\n");
    body.push_str(
        "# HELP kinetix_provider_circuit_opens_total Provider circuit open transitions\n",
    );
    body.push_str("# TYPE kinetix_provider_circuit_opens_total counter\n");
    body.push_str(
        "# HELP kinetix_provider_circuit_recoveries_total Successful half-open recoveries\n",
    );
    body.push_str("# TYPE kinetix_provider_circuit_recoveries_total counter\n");
    body.push_str(
        "# HELP kinetix_provider_circuit_rejects_total Target candidates rejected by an open provider circuit; one request may count more than once\n",
    );
    body.push_str("# TYPE kinetix_provider_circuit_rejects_total counter\n");
    body.push_str(
        "# HELP kinetix_provider_circuit_half_open_probes_total Half-open provider probes started\n",
    );
    body.push_str("# TYPE kinetix_provider_circuit_half_open_probes_total counter\n");
    for circuit in state.provider_circuits.snapshots() {
        let provider_id = circuit
            .provider_id
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        let open = if circuit.state == crate::provider_circuit::ProviderCircuitState::Closed {
            0
        } else {
            1
        };
        body.push_str(&format!(
            "kinetix_provider_circuit_open{{provider_id=\"{provider_id}\"}} {open}\n"
        ));
        body.push_str(&format!(
            "kinetix_provider_circuit_opens_total{{provider_id=\"{provider_id}\"}} {}\n",
            circuit.opens
        ));
        body.push_str(&format!(
            "kinetix_provider_circuit_recoveries_total{{provider_id=\"{provider_id}\"}} {}\n",
            circuit.recoveries
        ));
        body.push_str(&format!(
            "kinetix_provider_circuit_rejects_total{{provider_id=\"{provider_id}\"}} {}\n",
            circuit.rejects
        ));
        body.push_str(&format!(
            "kinetix_provider_circuit_half_open_probes_total{{provider_id=\"{provider_id}\"}} {}\n",
            circuit.half_open_probes
        ));
    }
    body.push_str(
        "# HELP kinetix_target_telemetry_queue_dropped_total Target telemetry events dropped because the queue was full\n",
    );
    body.push_str("# TYPE kinetix_target_telemetry_queue_dropped_total counter\n");
    body.push_str(&format!(
        "kinetix_target_telemetry_queue_dropped_total {}\n",
        state.target_telemetry.dropped_queue()
    ));
    body.push_str(
        "# HELP kinetix_target_telemetry_persistence_dropped_total Target telemetry events lost on persistence failure\n",
    );
    body.push_str("# TYPE kinetix_target_telemetry_persistence_dropped_total counter\n");
    body.push_str(&format!(
        "kinetix_target_telemetry_persistence_dropped_total {}\n",
        state.target_telemetry.dropped_persistence()
    ));

    body.push_str(
        "# HELP kinetix_flight_recorder_requests Requests tracked by the flight recorder\n",
    );
    body.push_str("# TYPE kinetix_flight_recorder_requests gauge\n");
    body.push_str(&format!(
        "kinetix_flight_recorder_requests {}\n",
        state.flight.request_count()
    ));
    body.push_str("# HELP kinetix_active_streams Requests currently in flight (live view)\n");
    body.push_str("# TYPE kinetix_active_streams gauge\n");
    body.push_str(&format!(
        "kinetix_active_streams {}\n",
        state.live.live_count()
    ));
    body.push_str("# HELP kinetix_live_view_dropped_total Live-view entries evicted under load\n");
    body.push_str("# TYPE kinetix_live_view_dropped_total counter\n");
    body.push_str(&format!(
        "kinetix_live_view_dropped_total {}\n",
        state.live.dropped()
    ));
    body.push_str(
        "# HELP kinetix_flight_recorder_dropped_total Diagnostics dropped when saturated\n",
    );
    body.push_str("# TYPE kinetix_flight_recorder_dropped_total counter\n");
    body.push_str(&format!(
        "kinetix_flight_recorder_dropped_total {}\n",
        state.flight.dropped_requests() + state.flight.dropped_events()
    ));
    // Opaque provider-state persistence (Gemini thoughtSignature replay).
    // These are counts and bucket sizes only; no signature, tool-call id, or
    // session identifier is ever exported.
    {
        let m = state.opaque_state.metrics();
        body.push_str("# HELP kinetix_opaque_state_entries Cached opaque provider-state rows (memory + store)\n");
        body.push_str("# TYPE kinetix_opaque_state_entries gauge\n");
        body.push_str(&format!("kinetix_opaque_state_entries {}\n", m.entries));
        body.push_str(
            "# HELP kinetix_opaque_state_captured_total Opaque provider-state rows captured\n",
        );
        body.push_str("# TYPE kinetix_opaque_state_captured_total counter\n");
        body.push_str(&format!(
            "kinetix_opaque_state_captured_total {}\n",
            m.capture_stored
        ));
        body.push_str("# HELP kinetix_opaque_state_replaced_total Captured rows that replaced an existing value\n");
        body.push_str("# TYPE kinetix_opaque_state_replaced_total counter\n");
        body.push_str(&format!(
            "kinetix_opaque_state_replaced_total {}\n",
            m.capture_replaced
        ));
        body.push_str("# HELP kinetix_opaque_state_capture_dropped_total Captures whose durability job was dropped because the async queue was full\n");
        body.push_str("# TYPE kinetix_opaque_state_capture_dropped_total counter\n");
        body.push_str(&format!(
            "kinetix_opaque_state_capture_dropped_total {}\n",
            m.capture_dropped
        ));
        body.push_str("# HELP kinetix_opaque_state_capture_storage_errors_total Async durability failures while capturing\n");
        body.push_str("# TYPE kinetix_opaque_state_capture_storage_errors_total counter\n");
        body.push_str(&format!(
            "kinetix_opaque_state_capture_storage_errors_total {}\n",
            m.capture_storage_error
        ));
        for (label, value) in [
            ("hit", m.lookup_hit),
            ("miss", m.lookup_miss),
            ("expired", m.lookup_expired),
            ("incompatible", m.lookup_incompatible),
            ("session_mismatch", m.lookup_session_mismatch),
            ("tool_name_mismatch", m.lookup_tool_name_mismatch),
            ("decrypt_error", m.lookup_decrypt_error),
        ] {
            body.push_str(&format!(
                "kinetix_opaque_state_lookups_total{{outcome=\"{label}\"}} {value}\n"
            ));
        }
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response()
}

// ===========================================================================
// Helpers
// ===========================================================================

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

/// Label for an account auto-created alongside a provider. Uses the supplied
/// label when given; otherwise derives it from the provider name so the pool key
/// is not misleadingly called "Default key".
fn account_label_or_default(provider_name: &str, label: Option<&str>) -> String {
    match label.map(str::trim).filter(|l| !l.is_empty()) {
        Some(l) => l.to_string(),
        None => format!("{} (primary)", provider_name.trim()),
    }
}

/// Sanitize a plugin-supplied account label before it is persisted and shown in
/// the dashboard: drop control characters, collapse whitespace, and cap length.
fn sanitize_account_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    cleaned.trim().chars().take(120).collect()
}

/// Guardrail for admin-supplied endpoints (NFR-3.9): HTTPS by default, and
/// loopback/link-local/private/metadata ranges blocked unless explicitly allowed.
fn validate_outbound_url(state: &AppState, url: &str) -> Result<(), ApiError> {
    let parsed = url::Url::parse(url).map_err(|e| ApiError::bad(format!("invalid URL: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| ApiError::bad("URL must have a host"))?;
    // TLS is mandatory except in the explicit, visibly-marked dev mode
    // (NFR-3.12). KINETIX_ALLOW_INSECURE_TLS is that override — the
    // private-upstreams flag must NOT silently disable TLS.
    if parsed.scheme() != "https" && !state.config.allow_insecure_tls {
        return Err(ApiError::bad(
            "endpoint must use https (set KINETIX_ALLOW_INSECURE_TLS=true to override for local development)",
        ));
    }
    // The private-upstreams flag only relaxes the blocked-host check (NFR-3.9).
    if state.config.allow_private_upstreams {
        return Ok(());
    }
    if crate::net::is_blocked_host(host) {
        return Err(ApiError::bad(format!(
            "host '{host}' resolves to a blocked private/metadata range; set KINETIX_ALLOW_PRIVATE_UPSTREAMS=true to allow"
        )));
    }
    Ok(())
}

/// Whether an IP literal falls in a blocked private/link-local/metadata range
/// (NFR-3.9). Shared with the connect-time DNS re-check in the pipeline and the
/// plugin host-mediated HTTP guard.
pub use crate::net::is_blocked_ip;

// ===========================================================================
// Configuration export / import (FR-10.12)
//
// User-authored, secret-free by default. Export produces a portable JSON
// document; import is a two-phase Validate/Dry Run + Apply so an operator can
// see the plan before touching production (FR-8.6). Import matches by name and
// never deletes: a name that already exists is updated, a new name is created.
// Secrets are excluded unless `include_secrets` is set, in which case the
// AES-GCM encrypted `secret_enc` blobs are carried so a restore is possible
// without re-entering keys.
// ===========================================================================

fn portable_price_provenance_metadata(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut stable = serde_json::Map::new();
            for (key, value) in fields {
                if matches!(
                    key.as_str(),
                    "observed_at" | "retrieved_at" | "etag" | "last_modified" | "freshness"
                ) {
                    continue;
                }
                stable.insert(key.clone(), portable_price_provenance_metadata(value));
            }
            Value::Object(stable)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(portable_price_provenance_metadata)
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn portable_model_ownership(
    discovery: &Value,
    capabilities: &Value,
    parameters: &Value,
    thinking_map: &Value,
    prices: &Prices,
) -> Value {
    let capability_overrides = discovery
        .get("operator_capability_overrides")
        .cloned()
        .unwrap_or_else(|| normalize_model_capabilities(capabilities));
    let parameter_overrides = discovery
        .get("operator_parameter_overrides")
        .cloned()
        .unwrap_or_else(|| operator_parameter_support_overrides(parameters));
    let reasoning_overrides = discovery
        .get("operator_reasoning_overrides")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let thinking_overrides = discovery
        .get("operator_thinking_overrides")
        .cloned()
        .unwrap_or_else(|| {
            if thinking_map
                .as_object()
                .is_some_and(|thinking_map| !thinking_map.is_empty())
            {
                json!({ "thinking_map": thinking_map })
            } else {
                json!({})
            }
        });

    let effective_pricing = discovery
        .get("effective_pricing")
        .filter(|value| !value.is_null())
        .and_then(Value::as_object)
        .and_then(|effective| {
            let source = effective.get("source")?.as_str()?;
            let mut metadata = effective
                .get("metadata")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if metadata.get("fields").is_none() {
                if let Some(fields) = effective.get("fields") {
                    metadata["fields"] = fields.clone();
                }
            }
            Some(json!({
                "source": source,
                "metadata": portable_price_provenance_metadata(&metadata),
            }))
        })
        .or_else(|| {
            prices.is_configured().then(|| {
                let (source, metadata) = operator_price_provenance(prices);
                json!({
                    "source": source,
                    "metadata": portable_price_provenance_metadata(&metadata),
                })
            })
        })
        .unwrap_or(Value::Null);

    json!({
        "operator_capability_overrides": capability_overrides,
        "operator_parameter_overrides": parameter_overrides,
        "operator_reasoning_overrides": reasoning_overrides,
        "operator_thinking_overrides": thinking_overrides,
        "effective_pricing": effective_pricing,
    })
}

#[derive(Clone)]
struct ImportedModelOwnership {
    discovery_patch: Value,
    pricing: Option<(String, Value)>,
}

fn parse_imported_model_ownership(model: &Value) -> Result<Option<ImportedModelOwnership>, String> {
    let Some(raw) = model.get("ownership") else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let ownership = raw
        .as_object()
        .ok_or_else(|| "ownership must be an object or null".to_string())?;
    let mut discovery_patch = serde_json::Map::new();
    for key in [
        "operator_capability_overrides",
        "operator_parameter_overrides",
        "operator_reasoning_overrides",
        "operator_thinking_overrides",
    ] {
        let value = ownership.get(key).cloned().unwrap_or_else(|| json!({}));
        if !value.is_object() {
            return Err(format!("ownership.{key} must be an object"));
        }
        discovery_patch.insert(key.to_string(), value);
    }

    let prices: Prices = serde_json::from_value(model["prices"].clone()).unwrap_or_default();
    let pricing = match ownership.get("effective_pricing") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let effective = value.as_object().ok_or_else(|| {
                "ownership.effective_pricing must be an object or null".to_string()
            })?;
            let source = effective
                .get("source")
                .and_then(Value::as_str)
                .filter(|source| !source.trim().is_empty())
                .ok_or_else(|| {
                    "ownership.effective_pricing.source must be a non-empty string".to_string()
                })?
                .to_string();
            let metadata = effective
                .get("metadata")
                .cloned()
                .ok_or_else(|| "ownership.effective_pricing.metadata is required".to_string())?;
            if !metadata.is_object() {
                return Err("ownership.effective_pricing.metadata must be an object".to_string());
            }
            Some((source, metadata))
        }
    };
    if prices.is_configured() && pricing.is_none() {
        return Err("configured prices require ownership.effective_pricing provenance".to_string());
    }

    Ok(Some(ImportedModelOwnership {
        discovery_patch: Value::Object(discovery_patch),
        pricing,
    }))
}

fn filter_imported_ownership_pricing_for_scope(
    prices: &Prices,
    ownership: &ImportedModelOwnership,
    pricing_scope: &str,
) -> (Prices, ImportedModelOwnership, Vec<String>) {
    if pricing_scope != "integration" {
        return (prices.clone(), ownership.clone(), Vec::new());
    }
    let Some((fallback_source, metadata)) = ownership.pricing.as_ref() else {
        return (prices.clone(), ownership.clone(), Vec::new());
    };

    let mut effective = prices.clone();
    let mut filtered_metadata = metadata.clone();
    let mut fields = filtered_metadata
        .get("fields")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut suppressed = Vec::new();

    for field in PRICE_FIELDS {
        if price_field(&effective, field).is_none() {
            continue;
        }
        let source = fields
            .get(field)
            .and_then(|value| value.get("source"))
            .and_then(Value::as_str)
            .unwrap_or(fallback_source);
        if crate::model_catalog::is_external_catalog_price_source(source) {
            set_price_field(&mut effective, field, None);
            fields.remove(field);
            suppressed.push(field.to_string());
        }
    }

    filtered_metadata["fields"] = Value::Object(fields.clone());
    let catalog_still_contributes = fields.values().any(|field| {
        field
            .get("source")
            .and_then(Value::as_str)
            .is_some_and(crate::model_catalog::is_external_catalog_price_source)
    });
    if !catalog_still_contributes {
        if let Some(metadata) = filtered_metadata.as_object_mut() {
            metadata.remove("catalog_source_state");
            metadata.remove("catalog_provider");
        }
    }

    let mut filtered = ownership.clone();
    filtered.pricing = Some((effective_price_source(&fields, &effective), filtered_metadata));
    (effective, filtered, suppressed)
}

#[derive(Deserialize)]
pub struct ExportQuery {
    /// Include encrypted credential blobs (still ciphertext, still keyed by the
    /// master key). Off by default so exports are safe to share.
    #[serde(default)]
    pub include_secrets: bool,
}

pub async fn export_config(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Query(q): Query<ExportQuery>,
) -> ApiResult {
    let providers = db::list_providers(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let mut accounts = Vec::new();
    let mut models = Vec::new();
    for p in &providers {
        for a in db::accounts_for_provider(&state.pool, &p.id)
            .await
            .map_err(ApiError::internal)?
        {
            if a.label != "__kinetix_noauth__" {
                accounts.push(a);
            }
        }
        for m in db::models_for_provider(&state.pool, &p.id)
            .await
            .map_err(ApiError::internal)?
        {
            models.push(m);
        }
    }
    let routes = db::list_routes(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let aliases = db::list_aliases(&state.pool)
        .await
        .map_err(ApiError::internal)?;

    let provider_name = |id: &str| -> String {
        providers
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    };
    let model_label = |id: &str| -> String {
        models
            .iter()
            .find(|m| m.id == id)
            .map(|m| format!("{}/{}", provider_name(&m.provider_id), m.upstream_id))
            .unwrap_or_default()
    };

    let providers_json: Vec<Value> = providers
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "base_url": p.base_url,
                "wire_format": p.wire_format,
                "auth_scheme": p.auth_scheme,
                "custom_header_name": p.custom_header_name,
                "custom_param_name": p.custom_param_name,
                "extra_headers": serde_json::from_str::<Value>(&p.extra_headers).unwrap_or(json!({})),
                "timeout_ms": p.timeout_ms,
                "capability_mode": p.capability_mode,
                "models_path": p.models_path,
                "rate_limit_rules": serde_json::from_str::<Value>(&p.rate_limit_rules).unwrap_or(json!({})),
                "follow_redirects": p.follow_redirects != 0,
                "credential_hosts": p.credential_hosts,
                "allow_insecure_tls": p.allow_insecure_tls != 0,
                "wire_plugin": p.wire_plugin,
                "credential_plugin": p.credential_plugin,
                "model_source_plugin": p.model_source_plugin,
                "credential_mode": p.credential_mode,
                "source_plugin_id": p.source_plugin_id,
                "source_integration_id": p.source_integration_id,
                "pricing_scope": p.pricing_scope,
                "enabled": p.enabled != 0,
            })
        })
        .collect();

    let accounts_json: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let mut v = json!({
                "provider": provider_name(&a.provider_id),
                "label": a.label,
                "key_mask": a.key_mask,
                "status": a.status,
                "quota_type": a.quota_type,
                "soft_quota_usd": a.soft_quota_usd,
                "priority": a.priority,
                "weight": a.weight,
            });
            if q.include_secrets {
                v["secret_enc"] = json!(a.secret_enc);
            }
            v
        })
        .collect();

    let models_json: Vec<Value> = models
        .iter()
        .map(|m| {
            let discovery =
                serde_json::from_str::<Value>(&m.discovery).unwrap_or_else(|_| json!({}));
            let capabilities =
                serde_json::from_str::<Value>(&m.capabilities).unwrap_or_else(|_| json!({}));
            let prices_value =
                serde_json::from_str::<Value>(&m.prices).unwrap_or_else(|_| json!({}));
            let prices: Prices = serde_json::from_value(prices_value.clone()).unwrap_or_default();
            let parameters =
                serde_json::from_str::<Value>(&m.parameters).unwrap_or_else(|_| json!({}));
            let thinking_map =
                serde_json::from_str::<Value>(&m.thinking_map).unwrap_or_else(|_| json!({}));
            json!({
                "provider": provider_name(&m.provider_id),
                "upstream_id": m.upstream_id,
                "display_name": m.display_name,
                "enabled": m.enabled != 0,
                "context_window": m.context_window,
                "max_output_tokens": m.max_output_tokens,
                "capabilities": capabilities,
                "prices": prices_value,
                "parameters": parameters,
                "thinking_map": thinking_map,
                "extra_request": serde_json::from_str::<Value>(&m.extra_request).unwrap_or(json!({})),
                "transport_override": discovery.get("configured_transport").cloned(),
                "ownership": portable_model_ownership(
                    &discovery,
                    &capabilities,
                    &parameters,
                    &thinking_map,
                    &prices,
                ),
            })
        })
        .collect();

    let mut routes_json = Vec::new();
    for r in &routes {
        let targets = db::route_targets(&state.pool, &r.id)
            .await
            .map_err(ApiError::internal)?;
        let targets_json: Vec<Value> = targets
            .iter()
            .map(|t| {
                json!({
                    "model": model_label(&t.model_id),
                    "account_id": t.account_id,
                    "priority": t.priority,
                    "weight": t.weight,
                    "predicate": serde_json::from_str::<Value>(&t.predicate).unwrap_or(json!({})),
                    "param_overrides": serde_json::from_str::<Value>(&t.param_overrides).unwrap_or(json!({})),
                })
            })
            .collect();
        routes_json.push(json!({
            "name": r.name,
            "description": r.description,
            "strategy": r.strategy,
            "fallback_triggers": serde_json::from_str::<Value>(&r.fallback_triggers).unwrap_or(json!({})),
            "portability_policy": r.portability_policy,
            "sticky_routing": r.sticky_routing != 0,
            "cache_affinity": r.cache_affinity != 0,
            "max_attempts": r.max_attempts,
            "enabled": r.enabled != 0,
            "targets": targets_json,
        }));
    }

    let aliases_json: Vec<Value> = aliases
        .iter()
        .map(|a| {
            json!({
                "alias": a.alias,
                "target_type": a.target_type,
                "target": if a.target_type == "route" {
                    routes.iter().find(|r| r.id == a.target_id).map(|r| r.name.clone()).unwrap_or_default()
                } else {
                    model_label(&a.target_id)
                },
                "description": a.description,
            })
        })
        .collect();

    Ok(Json(json!({
        "kinetix_config_version": 1,
        "exported_at": db::now_iso(),
        "secrets_included": q.include_secrets,
        "providers": providers_json,
        "accounts": accounts_json,
        "models": models_json,
        "routes": routes_json,
        "aliases": aliases_json,
    })))
}

async fn validate_imported_provider_credential_semantics(
    state: &AppState,
    name: &str,
    credential_mode: crate::plugins::CredentialMode,
    credential_plugin: &str,
    source_plugin_id: Option<&str>,
    source_integration_id: Option<&str>,
) -> Result<(), String> {
    match credential_mode {
        crate::plugins::CredentialMode::None => {
            if !credential_plugin.is_empty() {
                return Err(format!(
                    "provider '{name}': credential_mode 'none' may not declare credential_plugin"
                ));
            }
        }
        crate::plugins::CredentialMode::AuthFlow => {
            let plugin_id = source_plugin_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    format!(
                        "provider '{name}': credential_mode 'auth_flow' requires source_plugin_id"
                    )
                })?;
            let integration_id = source_integration_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    format!(
                        "provider '{name}': credential_mode 'auth_flow' requires source_integration_id"
                    )
                })?;
            let binding = crate::plugins::PluginRef::parse(credential_plugin).ok_or_else(|| {
                format!(
                    "provider '{name}': credential_mode 'auth_flow' requires a credential_plugin binding"
                )
            })?;
            if binding.plugin_id != plugin_id {
                return Err(format!(
                    "provider '{name}': credential_plugin does not match source_plugin_id"
                ));
            }

            // Preserve portable exports when the plugin is not installed yet.
            // Once present, its manifest becomes authoritative for integration
            // identity and the exact credential strategy binding.
            let installed_plugin = match state.plugin_manager() {
                Some(manager) => manager
                    .get(plugin_id)
                    .await
                    .map_err(|error| format!("provider '{name}': {error}"))?,
                None => None,
            };
            if let Some(row) = installed_plugin {
                let manifest = row.manifest().ok_or_else(|| {
                    format!("provider '{name}': source plugin manifest is unreadable")
                })?;
                let integration = manifest
                    .integrations
                    .iter()
                    .find(|integration| integration.id == integration_id)
                    .ok_or_else(|| {
                        format!(
                            "provider '{name}': source integration '{integration_id}' is unavailable"
                        )
                    })?;
                if integration.effective_credential_mode(&manifest.permissions)
                    != crate::plugins::CredentialMode::AuthFlow
                {
                    return Err(format!(
                        "provider '{name}': source integration '{integration_id}' is not an auth_flow integration"
                    ));
                }
                let strategy = integration.credential_strategy.as_deref().ok_or_else(|| {
                    format!(
                        "provider '{name}': source integration '{integration_id}' has no credential strategy"
                    )
                })?;
                let expected_binding = format!("plugin:{plugin_id}/{strategy}");
                if credential_plugin != expected_binding {
                    return Err(format!(
                        "provider '{name}': credential_plugin does not match source integration '{integration_id}'"
                    ));
                }
            }
        }
        crate::plugins::CredentialMode::Manual => {}
    }

    Ok(())
}

async fn resolve_imported_provider_pricing_scope(
    state: &AppState,
    name: &str,
    base_url: &str,
    requested_scope: Option<&str>,
    credential_mode: crate::plugins::CredentialMode,
    source_plugin_id: Option<&str>,
    source_integration_id: Option<&str>,
    wire_plugin: &str,
    credential_plugin: &str,
    model_source_plugin: &str,
) -> Result<String, String> {
    let requested_scope = requested_scope.unwrap_or_else(|| {
        db::conservative_provider_pricing_scope(
            credential_mode.as_str(),
            source_plugin_id,
            source_integration_id,
            wire_plugin,
            credential_plugin,
            model_source_plugin,
        )
    });
    if requested_scope != "direct_api" {
        return Ok(requested_scope.to_string());
    }

    let conservative_scope = db::conservative_provider_pricing_scope(
        credential_mode.as_str(),
        source_plugin_id,
        source_integration_id,
        wire_plugin,
        credential_plugin,
        model_source_plugin,
    );
    if conservative_scope == "direct_api" {
        return Ok("direct_api".to_string());
    }

    match direct_api_manifest_trust(
        state,
        name,
        base_url,
        source_plugin_id,
        source_integration_id,
    )
    .await?
    {
        DirectApiManifestTrust::Trusted => Ok("direct_api".to_string()),
        DirectApiManifestTrust::PluginUnavailable => Ok("integration".to_string()),
    }
}

#[derive(Deserialize)]
pub struct ImportBody {
    pub config: Value,
    /// When false (default) only plan the changes and return them.
    #[serde(default)]
    pub apply: bool,
}

pub async fn import_config(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<ImportBody>,
) -> ApiResult {
    let cfg = &body.config;
    let mut plan: Vec<Value> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut effective_provider_scopes: std::collections::HashMap<String, String> =
        Default::default();

    let empty = Vec::new();
    let providers = cfg["providers"].as_array().unwrap_or(&empty);
    let accounts = cfg["accounts"].as_array().unwrap_or(&empty);
    let models = cfg["models"].as_array().unwrap_or(&empty);
    let routes = cfg["routes"].as_array().unwrap_or(&empty);
    let aliases = cfg["aliases"].as_array().unwrap_or(&empty);

    if providers.is_empty() && models.is_empty() && routes.is_empty() {
        return Err(ApiError::bad(
            "config has no providers, models, or routes to import",
        ));
    }

    // ---- Validate phase (FR-8.6): schema + outbound security, no writes ----
    for p in providers {
        let name = p["name"].as_str().unwrap_or("");
        let base_url = p["base_url"].as_str().unwrap_or("");
        if name.is_empty() || base_url.is_empty() {
            problems.push("a provider entry is missing name or base_url".into());
            continue;
        }
        if let Err(e) = validate_outbound_url(&state, base_url) {
            problems.push(format!("provider '{name}': {}", e.1));
        }
        if WireFormat::parse(p["wire_format"].as_str().unwrap_or("")).is_none() {
            problems.push(format!(
                "provider '{name}': invalid wire_format '{}'",
                p["wire_format"].as_str().unwrap_or("")
            ));
        }
        if let Some(scope) = p.get("pricing_scope").and_then(Value::as_str) {
            if !matches!(scope, "direct_api" | "integration") {
                problems.push(format!(
                    "provider '{name}': pricing_scope must be 'direct_api' or 'integration'"
                ));
            }
        } else if p.get("pricing_scope").is_some() && !p["pricing_scope"].is_null() {
            problems.push(format!(
                "provider '{name}': pricing_scope must be 'direct_api' or 'integration'"
            ));
        }
        let existing_provider = db::list_providers(&state.pool)
            .await
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|provider| provider.name == name);
        let explicit_mode = match p.get("credential_mode").filter(|mode| !mode.is_null()) {
            Some(mode) => match mode
                .as_str()
                .and_then(crate::plugins::CredentialMode::parse)
            {
                Some(mode) => Some(mode),
                None => {
                    problems.push(format!(
                        "provider '{name}': credential_mode must be 'manual', 'auth_flow', or 'none'"
                    ));
                    None
                }
            },
            None => None,
        };
        let effective_mode = explicit_mode
            .or_else(|| {
                existing_provider.as_ref().and_then(|provider| {
                    crate::plugins::CredentialMode::parse(&provider.credential_mode)
                })
            })
            .unwrap_or(crate::plugins::CredentialMode::Manual);
        let source_plugin_id = if p.get("source_plugin_id").is_some() {
            p["source_plugin_id"].as_str()
        } else {
            existing_provider
                .as_ref()
                .and_then(|provider| provider.source_plugin_id.as_deref())
        };
        let source_integration_id = if p.get("source_integration_id").is_some() {
            p["source_integration_id"].as_str()
        } else {
            existing_provider
                .as_ref()
                .and_then(|provider| provider.source_integration_id.as_deref())
        };
        let wire_plugin = p["wire_plugin"].as_str().unwrap_or("");
        let credential_plugin = p["credential_plugin"].as_str().unwrap_or("");
        let model_source_plugin = p["model_source_plugin"].as_str().unwrap_or("");
        if let Err(problem) = validate_imported_provider_credential_semantics(
            &state,
            name,
            effective_mode,
            credential_plugin,
            source_plugin_id,
            source_integration_id,
        )
        .await
        {
            problems.push(problem);
        }
        let requested_pricing_scope = p["pricing_scope"].as_str().map(str::to_string);
        let effective_pricing_scope = match resolve_imported_provider_pricing_scope(
            &state,
            name,
            base_url,
            p["pricing_scope"].as_str(),
            effective_mode,
            source_plugin_id,
            source_integration_id,
            wire_plugin,
            credential_plugin,
            model_source_plugin,
        )
        .await
        {
            Ok(scope) => {
                if requested_pricing_scope.as_deref() == Some("direct_api")
                    && scope == "integration"
                {
                    warnings.push(format!(
                        "provider '{name}': requested pricing_scope 'direct_api' will be restored as 'integration' until the source plugin can attest the provider endpoint"
                    ));
                }
                effective_provider_scopes.insert(name.to_string(), scope.clone());
                Some(scope)
            }
            Err(problem) => {
                problems.push(problem);
                None
            }
        };

        plan.push(json!({
            "kind": "provider",
            "name": name,
            "action": if existing_provider.is_some() { "update" } else { "create" },
            "requested_pricing_scope": requested_pricing_scope,
            "effective_pricing_scope": effective_pricing_scope,
        }));
    }
    for m in models {
        let provider = m["provider"].as_str().unwrap_or("");
        let upstream = m["upstream_id"].as_str().unwrap_or("");
        if provider.is_empty() || upstream.is_empty() {
            problems.push("a model entry is missing provider or upstream_id".into());
        }
        if let Some(value) = m.get("transport_override").filter(|value| !value.is_null()) {
            match value.as_str() {
                Some(transport)
                    if transport.trim().is_empty()
                        || crate::adapters::TargetTransport::parse(transport.trim()).is_some() => {}
                Some(transport) => problems.push(format!(
                    "model '{provider}/{upstream}' has unsupported transport override '{transport}'"
                )),
                None => problems.push(format!(
                    "model '{provider}/{upstream}' transport_override must be a string or null"
                )),
            }
        }
        let mut suppressed_external_price_fields = Vec::new();
        match parse_imported_model_ownership(m) {
            Ok(Some(ownership)) => {
                if let Some(scope) = effective_provider_scopes.get(provider) {
                    let prices: Prices =
                        serde_json::from_value(m["prices"].clone()).unwrap_or_default();
                    let (_, _, suppressed) =
                        filter_imported_ownership_pricing_for_scope(&prices, &ownership, scope);
                    if !suppressed.is_empty() {
                        warnings.push(format!(
                            "model '{provider}/{upstream}': external catalog price fields will not be restored while provider pricing_scope is 'integration': {}",
                            suppressed.join(", ")
                        ));
                        suppressed_external_price_fields = suppressed;
                    }
                }
            }
            Ok(None) => {}
            Err(problem) => {
                problems.push(format!(
                    "model '{provider}/{upstream}' has invalid ownership: {problem}"
                ));
            }
        }
        plan.push(json!({
            "kind": "model",
            "name": format!("{provider}/{upstream}"),
            "action": "upsert",
            "suppressed_external_price_fields": suppressed_external_price_fields,
        }));
    }
    for r in routes {
        let name = r["name"].as_str().unwrap_or("");
        if name.is_empty() {
            problems.push("a route entry is missing a name".into());
        }
        let targets = r["targets"].as_array().map(|a| a.len()).unwrap_or(0);
        if targets == 0 {
            problems.push(format!("route '{name}' has no targets"));
        }
        plan.push(json!({"kind": "route", "name": name, "action": "upsert", "targets": targets}));
    }

    if !body.apply {
        return Ok(Json(json!({
            "valid": problems.is_empty(),
            "problems": problems,
            "warnings": warnings,
            "plan": plan,
            "note": "dry run: no changes were applied",
        })));
    }
    if !problems.is_empty() {
        return Err(ApiError::bad(format!(
            "config validation failed: {}",
            problems.join("; ")
        )));
    }

    // ---- Apply phase ----
    let mut provider_ids: std::collections::HashMap<String, String> = Default::default();
    for p in &db::list_providers(&state.pool)
        .await
        .map_err(ApiError::internal)?
    {
        provider_ids.insert(p.name.clone(), p.id.clone());
    }

    for p in providers {
        let name = p["name"].as_str().unwrap_or("");
        let base_url = p["base_url"].as_str().unwrap_or("");
        let wire = WireFormat::parse(p["wire_format"].as_str().unwrap_or(""))
            .ok_or_else(|| ApiError::bad("invalid wire_format"))?;
        let auth = AuthScheme::parse(p["auth_scheme"].as_str().unwrap_or("bearer"))
            .unwrap_or(AuthScheme::Bearer);
        let extra_headers = p["extra_headers"].clone();
        let rate_limit_rules = p["rate_limit_rules"].clone();
        let timeout_ms = p["timeout_ms"].as_i64().unwrap_or(120_000);
        let capability_mode = p["capability_mode"].as_str().unwrap_or("permissive");
        let models_path = p["models_path"].as_str();
        let follow_redirects = p["follow_redirects"].as_bool().unwrap_or(false);
        let credential_hosts = p["credential_hosts"].as_str().unwrap_or("");
        let allow_insecure_tls = p["allow_insecure_tls"].as_bool().unwrap_or(false);
        let custom_header = p["custom_header_name"].as_str();
        let custom_param = p["custom_param_name"].as_str();
        let explicit_mode = p["credential_mode"]
            .as_str()
            .and_then(crate::plugins::CredentialMode::parse);

        if let Some(existing_id) = provider_ids.get(name).cloned() {
            let lock = model_reconciliation_lock(&existing_id);
            let _guard = lock.lock().await;
            let existing = db::get_provider(&state.pool, &existing_id)
                .await
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::not_found("provider not found"))?;
            let credential_mode = explicit_mode
                .or_else(|| crate::plugins::CredentialMode::parse(&existing.credential_mode))
                .unwrap_or(crate::plugins::CredentialMode::Manual);
            let source_plugin_id = if p.get("source_plugin_id").is_some() {
                p["source_plugin_id"].as_str().map(str::to_string)
            } else {
                existing.source_plugin_id.clone()
            };
            let source_integration_id = if p.get("source_integration_id").is_some() {
                p["source_integration_id"].as_str().map(str::to_string)
            } else {
                existing.source_integration_id.clone()
            };

            let provider = db::NewProvider {
                name,
                base_url,
                wire_format: wire,
                auth_scheme: auth,
                custom_header_name: custom_header,
                custom_param_name: custom_param,
                extra_headers,
                timeout_ms,
                capability_mode,
                models_path,
                rate_limit_rules,
                follow_redirects,
                credential_hosts,
                allow_insecure_tls,
                wire_plugin: p["wire_plugin"].as_str().unwrap_or(""),
                credential_plugin: p["credential_plugin"].as_str().unwrap_or(""),
                model_source_plugin: p["model_source_plugin"].as_str().unwrap_or(""),
                credential_mode: credential_mode.as_str(),
                source_plugin_id: source_plugin_id.as_deref(),
                source_integration_id: source_integration_id.as_deref(),
            };
            let pricing_scope = resolve_imported_provider_pricing_scope(
                &state,
                name,
                base_url,
                p["pricing_scope"].as_str(),
                credential_mode,
                source_plugin_id.as_deref(),
                source_integration_id.as_deref(),
                p["wire_plugin"].as_str().unwrap_or(""),
                p["credential_plugin"].as_str().unwrap_or(""),
                p["model_source_plugin"].as_str().unwrap_or(""),
            )
            .await
            .map_err(ApiError::bad)?;
            db::update_provider(&state.pool, &existing_id, &provider, Some(&pricing_scope))
                .await
                .map_err(ApiError::internal)?;
            reconcile_provider_account_mode(&state, &existing_id, credential_mode).await?;
        } else {
            let credential_mode = explicit_mode.unwrap_or(crate::plugins::CredentialMode::Manual);
            let pricing_scope = resolve_imported_provider_pricing_scope(
                &state,
                name,
                base_url,
                p["pricing_scope"].as_str(),
                credential_mode,
                p["source_plugin_id"].as_str(),
                p["source_integration_id"].as_str(),
                p["wire_plugin"].as_str().unwrap_or(""),
                p["credential_plugin"].as_str().unwrap_or(""),
                p["model_source_plugin"].as_str().unwrap_or(""),
            )
            .await
            .map_err(ApiError::bad)?;
            let id = db::insert_provider(
                &state.pool,
                &db::NewProvider {
                    name,
                    base_url,
                    wire_format: wire,
                    auth_scheme: auth,
                    custom_header_name: custom_header,
                    custom_param_name: custom_param,
                    extra_headers,
                    timeout_ms,
                    capability_mode,
                    models_path,
                    rate_limit_rules,
                    follow_redirects,
                    credential_hosts,
                    allow_insecure_tls,
                    wire_plugin: p["wire_plugin"].as_str().unwrap_or(""),
                    credential_plugin: p["credential_plugin"].as_str().unwrap_or(""),
                    model_source_plugin: p["model_source_plugin"].as_str().unwrap_or(""),
                    credential_mode: credential_mode.as_str(),
                    source_plugin_id: p["source_plugin_id"].as_str(),
                    source_integration_id: p["source_integration_id"].as_str(),
                },
            )
            .await
            .map_err(ApiError::internal)?;
            db::update_provider_pricing_scope(&state.pool, &id, &pricing_scope)
                .await
                .map_err(ApiError::internal)?;
            reconcile_provider_account_mode(&state, &id, credential_mode).await?;
            provider_ids.insert(name.to_string(), id);
        }
    }

    // Accounts: only restored when the provider mode permits credentials and
    // the export carries an encrypted secret blob. Internal no-auth accounts
    // are always synthesized by reconciliation, never imported as user state.
    for a in accounts {
        let provider = a["provider"].as_str().unwrap_or("");
        let Some(pid) = provider_ids.get(provider) else {
            continue;
        };
        let provider_row = db::get_provider(&state.pool, pid)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("provider not found"))?;
        let label = a["label"].as_str().unwrap_or("Default key");
        if provider_row.credential_mode == "none" || label == "__kinetix_noauth__" {
            continue;
        }
        let Some(secret_enc) = a["secret_enc"].as_str() else {
            continue;
        };
        let exists = db::accounts_for_provider(&state.pool, pid)
            .await
            .map_err(ApiError::internal)?
            .into_iter()
            .any(|x| x.label == label);
        if exists {
            continue; // never overwrite an existing credential
        }
        db::insert_account(
            &state.pool,
            pid,
            label,
            secret_enc,
            a["key_mask"].as_str().unwrap_or("••••"),
            a["priority"].as_i64().unwrap_or(1),
            a["weight"].as_i64().unwrap_or(1),
            a["soft_quota_usd"].as_f64(),
            a["quota_type"].as_str().unwrap_or("none"),
        )
        .await
        .map_err(ApiError::internal)?;
    }

    let mut model_ids: std::collections::HashMap<String, String> = Default::default();
    for m in &db::list_models(&state.pool)
        .await
        .map_err(ApiError::internal)?
    {
        let pname = db::list_providers(&state.pool)
            .await
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|p| p.id == m.provider_id)
            .map(|p| p.name)
            .unwrap_or_default();
        model_ids.insert(format!("{}/{}", pname, m.upstream_id), m.id.clone());
    }

    for m in models {
        let provider = m["provider"].as_str().unwrap_or("");
        let upstream = m["upstream_id"].as_str().unwrap_or("");
        let Some(pid) = provider_ids.get(provider) else {
            continue;
        };
        let caps = normalize_model_capabilities(&m["capabilities"]);
        let prices: Prices = serde_json::from_value(m["prices"].clone()).unwrap_or_default();
        let parameters = m["parameters"].clone();
        let thinking: ThinkingMap =
            serde_json::from_value(m.get("thinking_map").cloned().unwrap_or_else(|| json!({})))
                .map_err(|error| {
                    ApiError::bad(format!(
                        "model '{provider}/{upstream}' has invalid thinking_map: {error}"
                    ))
                })?;
        validate_thinking_map(&thinking)?;
        let thinking_map =
            serde_json::to_value(&thinking).expect("ThinkingMap serialization is infallible");
        let extra_request = m["extra_request"].clone();
        let display = m["display_name"].as_str().unwrap_or(upstream);
        let enabled = m["enabled"].as_bool().unwrap_or(true);
        let context_window = m["context_window"].as_i64();
        let max_output_tokens = m["max_output_tokens"].as_i64();
        let requested_transport = m["transport_override"].as_str();

        let lock = model_reconciliation_lock(pid);
        let _guard = lock.lock().await;
        let provider_row = db::get_provider(&state.pool, pid)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("provider not found"))?;
        let transport_override =
            validate_model_transport_override(&provider_row, requested_transport)?;

        let model_key = format!("{provider}/{upstream}");
        let imported_ownership = parse_imported_model_ownership(m).map_err(ApiError::bad)?;
        let (prices, imported_ownership) = if let Some(ownership) = imported_ownership {
            let (filtered_prices, filtered_ownership, _) =
                filter_imported_ownership_pricing_for_scope(
                    &prices,
                    &ownership,
                    &provider_row.pricing_scope,
                );
            (filtered_prices, Some(filtered_ownership))
        } else {
            (prices, None)
        };
        let existing = db::find_model_by_upstream(&state.pool, pid, upstream)
            .await
            .map_err(ApiError::internal)?;
        if let Some(existing) = existing {
            let existing_id = existing.id.clone();
            model_ids.insert(model_key.clone(), existing_id.clone());
            let existing_discovery = discovery_object(&existing);
            let previous_prices = existing.prices();
            let (price_source, price_metadata, discovery_patch) = if let Some(ownership) =
                imported_ownership.as_ref()
            {
                let (source, metadata) = ownership
                    .pricing
                    .clone()
                    .unwrap_or_else(|| ("untracked".to_string(), json!({ "fields": {} })));
                (source, metadata, ownership.discovery_patch.clone())
            } else {
                let mut price_fields =
                    effective_price_fields(&existing_discovery, &previous_prices);
                for field in PRICE_FIELDS {
                    let previous = price_field(&previous_prices, field);
                    let next = price_field(&prices, field);
                    if previous != next {
                        if next.is_some() {
                            set_price_field_provenance(
                                &mut price_fields,
                                field,
                                "operator",
                                json!({ "configured_by": "config_import" }),
                            );
                        } else {
                            price_fields.remove(field);
                        }
                    }
                }
                let price_source = if prices.is_configured() {
                    effective_price_source(&price_fields, &prices)
                } else {
                    "operator".to_string()
                };
                let price_metadata = json!({ "fields": price_fields });

                let existing_caps = normalize_model_capabilities(
                    &serde_json::from_str::<Value>(&existing.capabilities)
                        .unwrap_or_else(|_| json!({})),
                );
                let existing_parameters = serde_json::from_str::<Value>(&existing.parameters)
                    .unwrap_or_else(|_| json!({}));
                let existing_thinking_map = serde_json::to_value(existing.thinking())
                    .expect("ThinkingMap serialization is infallible");
                let mut discovery_patch = serde_json::Map::new();
                if existing_caps != caps {
                    discovery_patch.insert("operator_capability_overrides".into(), caps.clone());
                }
                if existing_parameters != parameters {
                    discovery_patch.insert(
                        "operator_parameter_overrides".into(),
                        operator_parameter_support_overrides(&parameters),
                    );
                }
                if existing_thinking_map != thinking_map {
                    discovery_patch.insert(
                        "operator_thinking_overrides".into(),
                        Value::Object(merge_operator_thinking_map_override(
                            &existing_discovery,
                            &thinking_map,
                        )),
                    );
                }
                (price_source, price_metadata, Value::Object(discovery_patch))
            };

            db::commit_model_operator_mutation(
                &state.pool,
                &db::ModelOperatorMutation {
                    id: &existing_id,
                    display_name: display,
                    enabled,
                    context_window,
                    max_output_tokens,
                    capabilities: &caps,
                    parameters: &parameters,
                    thinking_map: &thinking_map,
                    extra_request: &extra_request,
                    update_transport: true,
                    transport: transport_override.as_deref(),
                    discovery_patch: &discovery_patch,
                    pricing: Some(db::ModelPricingMutation {
                        prices: &prices,
                        source: &price_source,
                        metadata: &price_metadata,
                    }),
                },
            )
            .await
            .map_err(ApiError::internal)?;
        } else {
            let (discovery_patch, pricing_values) =
                if let Some(ownership) = imported_ownership.as_ref() {
                    (ownership.discovery_patch.clone(), ownership.pricing.clone())
                } else {
                    let thinking_map_configured = !thinking.levels.is_empty()
                        || thinking.mode.is_some()
                        || thinking.budget_field.is_some()
                        || thinking.level_field.is_some();
                    let mut discovery_patch = serde_json::Map::new();
                    discovery_patch.insert("operator_capability_overrides".into(), caps.clone());
                    discovery_patch.insert(
                        "operator_parameter_overrides".into(),
                        operator_parameter_support_overrides(&parameters),
                    );
                    discovery_patch.insert(
                        "operator_thinking_overrides".into(),
                        if thinking_map_configured {
                            json!({ "thinking_map": thinking_map.clone() })
                        } else {
                            json!({})
                        },
                    );
                    if !prices.is_configured() {
                        discovery_patch.insert("effective_pricing".into(), Value::Null);
                    }
                    (
                        Value::Object(discovery_patch),
                        prices
                            .is_configured()
                            .then(|| operator_price_provenance(&prices)),
                    )
                };
            let pricing =
                pricing_values
                    .as_ref()
                    .map(|(source, metadata)| db::ModelPricingMutation {
                        prices: &prices,
                        source,
                        metadata,
                    });
            let (id, _) = db::commit_model_creation(
                &state.pool,
                &db::ModelCreation {
                    model: db::NewModel {
                        provider_id: pid,
                        upstream_id: upstream,
                        display_name: display,
                        enabled,
                        context_window,
                        max_output_tokens,
                        capabilities: caps,
                        prices: serde_json::to_value(&prices).unwrap(),
                        parameters,
                        thinking_map,
                        extra_request,
                        discovery: json!({}),
                    },
                    transport: transport_override.as_deref(),
                    discovery_patch: &discovery_patch,
                    opaque_state_plugin: None,
                    pricing,
                },
            )
            .await
            .map_err(ApiError::internal)?;
            model_ids.insert(model_key, id);
        }
    }

    // Routes (upsert by name) + targets.
    for r in routes {
        let name = r["name"].as_str().unwrap_or("");
        let legacy_reject = r["continuity_policy"].as_str() == Some("error");
        let body = RouteBody {
            name: name.to_string(),
            description: r["description"].as_str().unwrap_or("").to_string(),
            strategy: r["strategy"].as_str().unwrap_or("priority").to_string(),
            fallback_triggers: r["fallback_triggers"].clone(),
            portability_policy: r["portability_policy"]
                .as_str()
                .unwrap_or(if legacy_reject {
                    "reject"
                } else {
                    "strip_with_warning"
                })
                .to_string(),
            sticky_routing: r["sticky_routing"].as_bool().unwrap_or(false),
            cache_affinity: r["cache_affinity"].as_bool().unwrap_or(false),
            max_attempts: r["max_attempts"].as_i64(),
            targets: Vec::new(),
        };
        let mut target_bodies = Vec::new();
        for t in r["targets"].as_array().unwrap_or(&empty) {
            let model_key = t["model"].as_str().unwrap_or("");
            let Some(mid) = model_ids.get(model_key) else {
                continue;
            };
            target_bodies.push(RouteTargetBody {
                account_id: t["account_id"].as_str().map(|s| s.to_string()),
                model_id: mid.clone(),
                priority: t["priority"].as_i64().unwrap_or(1),
                weight: t["weight"].as_i64().unwrap_or(1),
                predicate: t["predicate"].clone(),
                param_overrides: t["param_overrides"].clone(),
            });
        }
        let existing = db::get_route_by_name(&state.pool, name)
            .await
            .map_err(ApiError::internal)?;
        let rid = match existing {
            Some(route) => {
                let mut body = body;
                body.targets = target_bodies;
                db::update_route(
                    &state.pool,
                    &route.id,
                    &body.description,
                    &body.strategy,
                    body.fallback_triggers.clone(),
                    &body.portability_policy,
                    body.sticky_routing,
                    body.cache_affinity,
                    body.max_attempts,
                )
                .await
                .map_err(ApiError::internal)?;
                db::clear_route_targets(&state.pool, &route.id)
                    .await
                    .map_err(ApiError::internal)?;
                write_route_targets(&state.pool, &route.id, &body.targets).await?;
                route.id
            }
            None => {
                let id = db::insert_route(
                    &state.pool,
                    &db::NewRoute {
                        name,
                        description: &body.description,
                        strategy: &body.strategy,
                        fallback_triggers: if body.fallback_triggers.is_null() {
                            json!({"on429": true, "onQuota": true, "on5xx": true, "onTimeout": true})
                        } else {
                            body.fallback_triggers.clone()
                        },
                        portability_policy: &body.portability_policy,
                        sticky_routing: body.sticky_routing,
                        cache_affinity: body.cache_affinity,
                        max_attempts: body.max_attempts,
                    },
                )
                .await
                .map_err(ApiError::internal)?;
                write_route_targets(&state.pool, &id, &target_bodies).await?;
                id
            }
        };
        let _ = rid;
    }

    // Aliases (upsert by alias name).
    for a in aliases {
        let alias = a["alias"].as_str().unwrap_or("");
        if alias.is_empty() {
            continue;
        }
        let ttype = a["target_type"].as_str().unwrap_or("model");
        let target = a["target"].as_str().unwrap_or("");
        let tid = if ttype == "route" {
            db::get_route_by_name(&state.pool, target)
                .await
                .map_err(ApiError::internal)?
                .map(|r| r.id)
        } else {
            model_ids.get(target).cloned()
        };
        let Some(tid) = tid else { continue };
        db::upsert_alias(
            &state.pool,
            alias,
            ttype,
            &tid,
            a["description"].as_str().unwrap_or(""),
        )
        .await
        .map_err(ApiError::internal)?;
    }

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "config_imported",
        "system",
        "config",
        "config",
        &format!(
            "Imported {} providers, {} models, {} routes, {} aliases.",
            providers.len(),
            models.len(),
            routes.len(),
            aliases.len()
        ),
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true, "applied": plan })))
}

// ===========================================================================
// Plugins
// ===========================================================================

#[derive(Deserialize)]
pub struct PluginInstallBody {
    /// Base64-encoded `.kxp` package (dashboard/API upload).
    #[serde(default)]
    pub package_base64: Option<String>,
    /// Downloadable URL to a `.kxp` package.
    #[serde(default)]
    pub url: Option<String>,
    /// Server-side path to a `.kxp` (operator convenience).
    #[serde(default)]
    pub path: Option<String>,
    /// Expected SHA-256 for a URL/remote install (§11).
    #[serde(default)]
    pub sha256: Option<String>,
    /// Trusted Ed25519 publisher public keys, base64 (§12).
    #[serde(default)]
    pub trusted_keys: Vec<String>,
    /// Explicit override to install an untrusted signature (§12).
    #[serde(default)]
    pub allow_untrusted_signature: bool,
}

fn plugin_bad(e: anyhow::Error) -> ApiError {
    ApiError::bad(e.to_string())
}

fn plugin_manager(
    state: &AppState,
) -> Result<&std::sync::Arc<crate::plugins::PluginManager>, ApiError> {
    state.plugin_manager().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "plugin host is not available".into(),
        )
    })
}

/// Register the runtime capability objects an enabled plugin provides (§6.0).
///
/// A plugin that declares a `credential_strategies` capability gets a
/// `PluginCredentialStrategy` so a provider bound to it resolves through the
/// plugin; a plugin that declares `provider_adapters` gets a plugin-backed
/// adapter for each declared name. Registration is idempotent — enabling an
/// already-enabled plugin simply re-registers the same object.
pub(crate) async fn register_enabled_plugin_capabilities(state: &AppState, id: &str) {
    let Some(manager) = state.plugin_manager().cloned() else {
        return;
    };
    let crypto = state.crypto.clone();
    let pool = state.pool.clone();
    // `enable` has already succeeded, so the row exists and is enabled.
    let provides = match manager.get(id).await {
        Ok(Some(row)) => row.manifest().map(|m| m.provides).unwrap_or_default(),
        _ => return,
    };
    if !provides.credential_strategies.is_empty() {
        let strategy: std::sync::Arc<dyn crate::credentials::CredentialStrategy> =
            std::sync::Arc::new(crate::plugins::credential::PluginCredentialStrategy::new(
                manager.clone(),
                pool,
                crypto,
                id,
            ));
        state.register_plugin_credential_strategy(id, strategy);
    }
    // ProviderAdapter registration (§6.3, §7.1): a plugin adapter is a pure
    // translation library — core still owns the outbound streaming send. The
    // `plugin-adapter` world imports no network capability, so registering it
    // does not widen the plugin's authority.
    if !provides.provider_adapters.is_empty() {
        if let Err(e) = crate::plugins::adapter::register_declared_adapters(
            &state.adapters,
            (*manager).clone(),
            id,
            &provides,
        )
        .await
        {
            tracing::warn!(
                plugin = %id,
                error = %e,
                "plugin declares provider_adapters but its adapter world could not be loaded; bound providers will fail closed"
            );
        }
    }

    auto_provision_plugin_providers(state, id).await;
}

async fn reconcile_provider_account_mode(
    state: &AppState,
    provider_id: &str,
    credential_mode: crate::plugins::CredentialMode,
) -> Result<(), ApiError> {
    let legacy_public_mask = crate::crypto::mask_secret("public");
    match credential_mode {
        crate::plugins::CredentialMode::None => {
            // Credential-free providers must have exactly one internal empty
            // account so routing can execute without exposing a fake user key.
            let accounts: Vec<_> = db::list_accounts(&state.pool)
                .await
                .map_err(ApiError::internal)?
                .into_iter()
                .filter(|account| account.provider_id == provider_id)
                .collect();
            let empty_secret = state.crypto.encrypt("").map_err(ApiError::internal)?;

            if accounts.len() == 1
                && (accounts[0].label == "__kinetix_noauth__"
                    || (accounts[0].label == "public"
                        && accounts[0].key_mask == legacy_public_mask))
            {
                sqlx::query(
                    "UPDATE accounts
                     SET label='__kinetix_noauth__', secret_enc=?, key_mask='',
                         status='healthy', cooldown_until=NULL, quota_reset_at=NULL,
                         quota_type='none', quota_window_s=NULL, soft_quota_usd=NULL,
                         priority=1, weight=1, last_error=NULL, last_probe_at=NULL,
                         circuit_open_until=NULL, consecutive_failures=0
                     WHERE id=?",
                )
                .bind(&empty_secret)
                .bind(&accounts[0].id)
                .execute(&state.pool)
                .await
                .map_err(ApiError::internal)?;
            } else {
                sqlx::query(
                    "UPDATE route_targets SET account_id=NULL
                     WHERE account_id IN (SELECT id FROM accounts WHERE provider_id=?)",
                )
                .bind(provider_id)
                .execute(&state.pool)
                .await
                .map_err(ApiError::internal)?;
                sqlx::query("DELETE FROM accounts WHERE provider_id=?")
                    .bind(provider_id)
                    .execute(&state.pool)
                    .await
                    .map_err(ApiError::internal)?;
                db::insert_account(
                    &state.pool,
                    provider_id,
                    "__kinetix_noauth__",
                    &empty_secret,
                    "",
                    1,
                    1,
                    None,
                    "none",
                )
                .await
                .map_err(ApiError::internal)?;
            }
        }
        crate::plugins::CredentialMode::Manual | crate::plugins::CredentialMode::AuthFlow => {
            // Credential-bearing modes must never route through synthetic
            // no-auth state left by a previous credential-free configuration.
            sqlx::query(
                "UPDATE route_targets SET account_id=NULL
                 WHERE account_id IN (
                     SELECT id FROM accounts
                     WHERE provider_id=?
                       AND (label='__kinetix_noauth__' OR (label='public' AND key_mask=?))
                 )",
            )
            .bind(provider_id)
            .bind(&legacy_public_mask)
            .execute(&state.pool)
            .await
            .map_err(ApiError::internal)?;
            sqlx::query(
                "DELETE FROM accounts
                 WHERE provider_id=?
                   AND (label='__kinetix_noauth__' OR (label='public' AND key_mask=?))",
            )
            .bind(provider_id)
            .bind(&legacy_public_mask)
            .execute(&state.pool)
            .await
            .map_err(ApiError::internal)?;
        }
    }

    Ok(())
}

async fn reconcile_provider_credential_semantics(
    state: &AppState,
    provider_id: &str,
    credential_mode: crate::plugins::CredentialMode,
    source_plugin_id: &str,
    source_integration_id: &str,
) -> Result<(), ApiError> {
    let lock = model_reconciliation_lock(provider_id);
    let _guard = lock.lock().await;
    db::update_provider_credential_semantics(
        &state.pool,
        provider_id,
        credential_mode.as_str(),
        Some(source_plugin_id),
        Some(source_integration_id),
    )
    .await
    .map_err(ApiError::internal)?;

    reconcile_provider_account_mode(state, provider_id, credential_mode).await
}

async fn reconcile_provider_integration_semantics(
    state: &AppState,
    provider_id: &str,
    credential_mode: crate::plugins::CredentialMode,
    source_plugin_id: &str,
    source_integration_id: &str,
    pricing_scope: crate::plugins::PricingScope,
) -> Result<(), ApiError> {
    let lock = model_reconciliation_lock(provider_id);
    let _guard = lock.lock().await;
    let effective_pricing_scope = if pricing_scope == crate::plugins::PricingScope::DirectApi {
        let provider = db::get_provider(&state.pool, provider_id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("provider not found"))?;
        match direct_api_manifest_trust(
            state,
            &provider.name,
            &provider.base_url,
            Some(source_plugin_id),
            Some(source_integration_id),
        )
        .await
        {
            Ok(DirectApiManifestTrust::Trusted) => crate::plugins::PricingScope::DirectApi,
            Ok(DirectApiManifestTrust::PluginUnavailable) => {
                crate::plugins::PricingScope::Integration
            }
            Err(problem) => {
                tracing::warn!(
                    provider = %provider.id,
                    plugin = %source_plugin_id,
                    integration = %source_integration_id,
                    %problem,
                    "direct_api integration trust no longer matches provider identity; demoting pricing scope"
                );
                crate::plugins::PricingScope::Integration
            }
        }
    } else {
        crate::plugins::PricingScope::Integration
    };
    db::update_provider_credential_semantics_with_scope(
        &state.pool,
        provider_id,
        credential_mode.as_str(),
        Some(source_plugin_id),
        Some(source_integration_id),
        Some(effective_pricing_scope.as_str()),
    )
    .await
    .map_err(ApiError::internal)?;
    reconcile_provider_account_mode(state, provider_id, credential_mode).await
}

pub(crate) async fn auto_provision_plugin_providers(state: &AppState, id: &str) {
    let Some(manager) = state.plugin_manager().cloned() else {
        return;
    };
    let row = match manager.get(id).await {
        Ok(Some(row)) if row.enabled != 0 => row,
        _ => return,
    };
    let Some(manifest) = row.manifest() else {
        return;
    };
    for integration in &manifest.integrations {
        let Some(template) = &integration.provider else {
            continue;
        };
        if validate_outbound_url(state, &template.base_url).is_err() {
            continue;
        }
        let wire_plugin = integration
            .provider_adapter
            .as_deref()
            .map(|name| format!("plugin:{id}/{name}"))
            .unwrap_or_default();
        let credential_plugin = integration
            .credential_strategy
            .as_deref()
            .map(|name| format!("plugin:{id}/{name}"))
            .unwrap_or_default();
        let model_source_plugin = integration
            .model_source
            .as_deref()
            .map(|name| format!("plugin:{id}/{name}"))
            .unwrap_or_default();
        let credential_mode = integration.effective_credential_mode(&manifest.permissions);

        let Some(wire) = WireFormat::parse(&template.wire_format) else {
            continue;
        };
        let Some(auth) = AuthScheme::parse(&template.auth_scheme) else {
            continue;
        };

        let Ok(providers) = db::list_providers(&state.pool).await else {
            continue;
        };
        let existing = providers.into_iter().find(|provider| {
            provider.base_url == template.base_url
                && provider.wire_plugin == wire_plugin
                && provider.credential_plugin == credential_plugin
                && provider.model_source_plugin == model_source_plugin
        });
        if let Some(provider) = existing {
            if let Err(error) = reconcile_provider_integration_semantics(
                state,
                &provider.id,
                credential_mode,
                id,
                &integration.id,
                template.pricing_scope,
            )
            .await
            {
                tracing::warn!(
                    provider = %provider.id,
                    plugin = %id,
                    integration = %integration.id,
                    error = %error.1,
                    "failed to upgrade plugin provider credential semantics"
                );
            } else {
                let _ = state.registry.reload(&state.pool).await;
            }
            continue;
        }

        let credential_hosts = template.credential_hosts.join(",");
        let extra_headers = serde_json::to_value(&template.extra_headers).unwrap_or(json!({}));
        let insert_res = db::insert_provider(
            &state.pool,
            &db::NewProvider {
                name: &integration.name,
                base_url: &template.base_url,
                wire_format: wire,
                auth_scheme: auth,
                custom_header_name: template.custom_header_name.as_deref(),
                custom_param_name: template.custom_param_name.as_deref(),
                extra_headers,
                timeout_ms: template.timeout_ms as i64,
                capability_mode: &template.capability_mode,
                models_path: template.models_path.as_deref(),
                rate_limit_rules: json!({}),
                follow_redirects: template.follow_redirects,
                credential_hosts: &credential_hosts,
                allow_insecure_tls: false,
                wire_plugin: &wire_plugin,
                credential_plugin: &credential_plugin,
                model_source_plugin: &model_source_plugin,
                credential_mode: credential_mode.as_str(),
                source_plugin_id: Some(id),
                source_integration_id: Some(&integration.id),
            },
        )
        .await;

        if let Ok(id_created) = insert_res {
            let _ = db::insert_audit(
                &state.pool,
                "admin",
                "plugin_integration_provider_created",
                "provider",
                &id_created,
                &integration.name,
                &format!(
                    "Created provider from plugin {} integration {}.",
                    id, integration.id
                ),
            )
            .await;

            if let Err(error) = reconcile_provider_integration_semantics(
                state,
                &id_created,
                credential_mode,
                id,
                &integration.id,
                template.pricing_scope,
            )
            .await
            {
                tracing::warn!(
                    provider = %id_created,
                    plugin = %id,
                    integration = %integration.id,
                    error = %error.1,
                    "failed to reconcile plugin provider credential semantics"
                );
            }
            let _ = state.registry.reload(&state.pool).await;
        }
    }
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct PluginCatalogQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub capability: Option<String>,
    #[serde(default)]
    pub refresh: bool,
}

/// `GET /admin/api/plugins/catalog` — official discovery metadata.
///
/// Supports remote sync, local disk caching, and query filtering (`q`, `capability`, `refresh`).
/// Annotates each entry with `installed`, `installed_version`, and `update_available`.
pub async fn plugin_catalog(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Query(query): Query<PluginCatalogQuery>,
) -> ApiResult {
    let cache_file = state.config.paths.plugin_catalog_cache_file();
    let catalog =
        crate::plugins::catalog::load_catalog(Some(&state.http), Some(&cache_file), query.refresh)
            .await
            .map_err(ApiError::internal)?;

    let trust = crate::plugins::catalog::embedded_trust_store().map_err(ApiError::internal)?;

    let installed_plugins = if let Ok(manager) = plugin_manager(&state) {
        manager.list().await.unwrap_or_default()
    } else {
        Vec::new()
    };

    let filtered = crate::plugins::catalog::filter_catalog(
        &catalog.plugins,
        query.q.as_deref(),
        query.capability.as_deref(),
    );

    let mut plugins = Vec::with_capacity(filtered.len());
    for plugin in filtered {
        let ready =
            crate::plugins::catalog::install_ready(plugin, &trust).map_err(ApiError::internal)?;
        let mut value = serde_json::to_value(plugin).map_err(ApiError::internal)?;
        value["install_ready"] = json!(ready);
        value["trust_status"] = json!(if ready {
            "trusted"
        } else if plugin.installable {
            "unavailable"
        } else {
            "discovery_only"
        });

        let installed = installed_plugins.iter().find(|p| p.id == plugin.id);
        if let Some(inst) = installed {
            value["installed"] = json!(true);
            value["installed_version"] = json!(inst.version);
            value["update_available"] = json!(crate::plugins::catalog::is_update_available(
                &inst.version,
                &plugin.latest_version
            ));
        } else {
            value["installed"] = json!(false);
            value["installed_version"] = json!(null);
            value["update_available"] = json!(false);
        }
        plugins.push(value);
    }

    Ok(Json(json!({
        "schema_version": catalog.schema_version,
        "plugins": plugins,
    })))
}

/// `POST /admin/api/plugins/catalog/refresh` — force remote synchronization of the marketplace catalog.
pub async fn refresh_plugin_catalog(State(state): State<AppState>, _auth: AdminAuth) -> ApiResult {
    let cache_file = state.config.paths.plugin_catalog_cache_file();
    let catalog = crate::plugins::catalog::load_catalog(Some(&state.http), Some(&cache_file), true)
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(json!({
        "schema_version": catalog.schema_version,
        "count": catalog.plugins.len(),
        "refreshed": true,
    })))
}

async fn download_catalog_package(
    state: &AppState,
    distribution: &crate::plugins::catalog::CatalogDistribution,
) -> Result<Vec<u8>, ApiError> {
    let mut url = url::Url::parse(&distribution.url)
        .map_err(|e| ApiError::bad(format!("invalid catalog artifact URL: {e}")))?;

    for redirect_count in 0..=5 {
        crate::plugins::catalog::validate_download_url(distribution, &url).map_err(plugin_bad)?;

        let mut response = state
            .http
            .get(url.clone())
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| ApiError::bad(format!("catalog artifact download failed: {e}")))?;

        if response.status().is_redirection() {
            if redirect_count == 5 {
                return Err(ApiError::bad("catalog artifact exceeded redirect limit"));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| ApiError::bad("catalog artifact redirect has no valid Location"))?;
            url = url
                .join(location)
                .map_err(|e| ApiError::bad(format!("invalid catalog artifact redirect: {e}")))?;
            continue;
        }

        if !response.status().is_success() {
            return Err(ApiError::bad(format!(
                "catalog artifact returned HTTP {}",
                response.status()
            )));
        }

        if response
            .content_length()
            .is_some_and(|length| length > crate::plugins::package::MAX_PACKAGE_BYTES)
        {
            return Err(ApiError::bad("catalog artifact exceeds package size limit"));
        }

        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ApiError::bad(format!("reading catalog artifact failed: {e}")))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > crate::plugins::package::MAX_PACKAGE_BYTES
            {
                return Err(ApiError::bad("catalog artifact exceeds package size limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(bytes);
    }

    Err(ApiError::bad("catalog artifact download failed"))
}

struct VerifiedCatalogPackage {
    plugin: crate::plugins::catalog::CatalogPlugin,
    bytes: Vec<u8>,
    keys: Vec<[u8; 32]>,
    validated: crate::plugins::ValidatedManifest,
}

async fn verify_catalog_package(
    state: &AppState,
    manager: &crate::plugins::PluginManager,
    id: &str,
) -> Result<VerifiedCatalogPackage, ApiError> {
    let cache_file = state.config.paths.plugin_catalog_cache_file();
    let catalog =
        crate::plugins::catalog::load_catalog(Some(&state.http), Some(&cache_file), false)
            .await
            .map_err(ApiError::internal)?;

    let plugin = crate::plugins::catalog::find_plugin_in_catalog(&catalog, id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("catalog plugin '{id}' not found")))?;
    let distribution = plugin
        .distribution
        .as_ref()
        .ok_or_else(|| ApiError::bad("catalog plugin has no installable distribution"))?;
    let trust = crate::plugins::catalog::embedded_trust_store().map_err(ApiError::internal)?;
    if !crate::plugins::catalog::install_ready(&plugin, &trust).map_err(plugin_bad)? {
        return Err(ApiError::bad(
            "catalog plugin is not install-ready: signed artifact metadata or publisher trust is unavailable",
        ));
    }
    let keys = crate::plugins::catalog::trusted_keys(&trust, &plugin).map_err(plugin_bad)?;
    if keys.is_empty() {
        return Err(ApiError::bad("catalog publisher key is not trusted"));
    }

    let bytes = download_catalog_package(state, distribution).await?;
    let pkg = crate::plugins::package::read_package(&bytes).map_err(plugin_bad)?;
    if !distribution
        .sha256
        .eq_ignore_ascii_case(&pkg.package_sha256)
    {
        return Err(ApiError::bad(format!(
            "catalog package hash mismatch: expected {}, computed {}",
            distribution.sha256, pkg.package_sha256
        )));
    }
    let validated =
        crate::plugins::package::validate_manifest(&pkg, manager.policy()).map_err(plugin_bad)?;
    if validated.manifest.id != plugin.id {
        return Err(ApiError::bad(format!(
            "catalog artifact id mismatch: expected '{}', package declares '{}'",
            plugin.id, validated.manifest.id
        )));
    }
    if validated.manifest.version != plugin.latest_version {
        return Err(ApiError::bad(format!(
            "catalog artifact version mismatch: expected '{}', package declares '{}'",
            plugin.latest_version, validated.manifest.version
        )));
    }
    let signature = crate::plugins::package::verify_signature(&pkg, &keys).map_err(plugin_bad)?;
    if signature != crate::plugins::package::SignatureStatus::Verified {
        return Err(ApiError::bad(
            "catalog package is not signed by its trusted publisher key",
        ));
    }

    Ok(VerifiedCatalogPackage {
        plugin,
        bytes,
        keys,
        validated,
    })
}

/// `GET /admin/api/plugins/catalog/{id}/preview` — verify a catalog package
/// and report its authority delta without mutating plugin state.
pub async fn preview_catalog_plugin(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let verified = verify_catalog_package(&state, manager, &id).await?;

    let current = manager.get(&id).await.map_err(ApiError::internal)?;
    let (current_version, current_permissions) = match current {
        Some(row) => {
            let manifest = row
                .manifest()
                .ok_or_else(|| ApiError::bad("installed plugin manifest is unreadable"))?;
            (Some(row.version), manifest.permissions)
        }
        None => (None, crate::plugins::Permissions::default()),
    };

    let target_permissions = verified.validated.manifest.permissions.clone();
    let permission_diff =
        crate::plugins::manager::permission_diff(&current_permissions, &target_permissions);

    Ok(Json(json!({
        "id": verified.plugin.id,
        "name": verified.plugin.name,
        "current_version": current_version,
        "target_version": verified.plugin.latest_version,
        "sha256": verified
            .plugin
            .distribution
            .as_ref()
            .map(|distribution| distribution.sha256.clone())
            .unwrap_or_default(),
        "signature": "verified",
        "permissions": target_permissions,
        "permission_diff": permission_diff,
        "provides": verified.validated.manifest.provides.provided(),
        "source": format!(
            "catalog:{}@{}",
            verified.plugin.id, verified.plugin.latest_version
        ),
    })))
}

/// `POST /admin/api/plugins/catalog/{id}/install` — install a trusted catalog package.
pub async fn install_catalog_plugin(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let verified = verify_catalog_package(&state, manager, &id).await?;
    let distribution = verified
        .plugin
        .distribution
        .as_ref()
        .ok_or_else(|| ApiError::bad("catalog plugin has no installable distribution"))?;
    let source = format!(
        "catalog:{}@{}",
        verified.plugin.id, verified.plugin.latest_version
    );
    let outcome = manager
        .install_from_source(
            &verified.bytes,
            Some(&distribution.sha256),
            &verified.keys,
            false,
            &source,
        )
        .await
        .map_err(plugin_bad)?;

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_catalog_installed",
        "plugin",
        &outcome.id,
        &outcome.id,
        &format!(
            "Installed trusted catalog plugin {} v{} (SHA-256 {}). Installed disabled pending permission review.",
            outcome.id, outcome.version, outcome.package_sha256
        ),
    )
    .await;

    Ok(Json(json!({
        "id": outcome.id,
        "version": outcome.version,
        "sha256": outcome.package_sha256,
        "signature": outcome.signature.as_str(),
        "provides": outcome.provides,
        "enabled": false,
        "source": source,
        "note": "trusted catalog package installed disabled; review permissions before enabling",
    })))
}

/// `GET /admin/api/plugins` — list installed plugins.
pub async fn list_plugins(State(state): State<AppState>, _auth: AdminAuth) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let rows = manager.list().await.map_err(ApiError::internal)?;
    let plugins: Vec<Value> = rows
        .iter()
        .map(crate::plugins::manager::manifest_summary)
        .collect();
    Ok(Json(json!({ "plugins": plugins })))
}

/// `GET /admin/api/plugins/{id}` — plugin detail (§21).
pub async fn get_plugin(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let row = manager
        .get(&id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("plugin not found"))?;
    let perms = crate::plugins::store::permissions(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let runtime = crate::plugins::store::runtime_state(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let packages = crate::plugins::store::list_packages(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let mut summary = crate::plugins::manager::manifest_summary(&row);
    summary["permissions_approved"] = json!(perms);
    summary["runtime"] = json!(runtime);
    summary["packages"] = json!(packages);
    Ok(Json(summary))
}

/// `GET /admin/api/plugins/{id}/settings` — read host-owned plugin settings.
pub async fn plugin_settings(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let settings = manager.ui_settings(&id).await.map_err(plugin_bad)?;
    Ok(Json(settings))
}

#[derive(Deserialize)]
pub struct PluginSettingsBody {
    #[serde(default)]
    pub values: serde_json::Map<String, Value>,
}

/// `PUT /admin/api/plugins/{id}/settings` — partially update validated settings.
pub async fn update_plugin_settings(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<PluginSettingsBody>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let settings = manager
        .update_ui_settings(&id, &body.values)
        .await
        .map_err(plugin_bad)?;

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_settings_updated",
        "plugin",
        &id,
        &id,
        &format!(
            "Updated {} plugin setting(s). Values are not written to audit logs.",
            body.values.len()
        ),
    )
    .await;

    Ok(Json(settings))
}

/// `POST /admin/api/plugins/install` — install (or upgrade) a package.
pub async fn install_plugin(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Json(body): Json<PluginInstallBody>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let (bytes, source) = if let Some(b64) = &body.package_base64 {
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| ApiError::bad(format!("invalid package_base64: {e}")))?;
        (b, "upload".to_string())
    } else if let Some(url_str) = &body.url {
        let b = crate::plugins::catalog::download_package_from_url(&state.http, url_str)
            .await
            .map_err(|e| ApiError::bad(e.to_string()))?;
        (b, format!("url:{}", url_str.trim()))
    } else if let Some(path) = &body.path {
        let b =
            std::fs::read(path).map_err(|e| ApiError::bad(format!("cannot read {path}: {e}")))?;
        (b, format!("file:{path}"))
    } else {
        return Err(ApiError::bad("provide package_base64, url, or path"));
    };

    let trusted: Vec<[u8; 32]> = body
        .trusted_keys
        .iter()
        .filter_map(|k| decode_key(k))
        .collect();

    let outcome = manager
        .install_from_source(
            &bytes,
            body.sha256.as_deref(),
            &trusted,
            body.allow_untrusted_signature,
            &source,
        )
        .await
        .map_err(plugin_bad)?;

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_installed",
        "plugin",
        &outcome.id,
        &outcome.id,
        &format!(
            "Installed plugin {} v{} (signature: {}, provides {} capabilities). Installed disabled.",
            outcome.id,
            outcome.version,
            outcome.signature.as_str(),
            outcome.provides.len()
        ),
    )
    .await;

    Ok(Json(json!({
        "id": outcome.id,
        "version": outcome.version,
        "sha256": outcome.package_sha256,
        "signature": outcome.signature.as_str(),
        "provides": outcome.provides,
        "enabled": false,
        "note": "installed-disabled; enable is a separate operation",
    })))
}

/// `POST /admin/api/plugins/{id}/integrations/{integration}/provider` —
/// create (or return) the host-owned provider described by an Integration.
pub async fn setup_plugin_integration_provider(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path((id, integration_id)): Path<(String, String)>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let row = manager
        .get(&id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("plugin not found"))?;
    if row.enabled == 0 {
        return Err(ApiError::bad(
            "plugin must be enabled before its integration can create a provider",
        ));
    }
    let manifest = row
        .manifest()
        .ok_or_else(|| ApiError::bad("plugin manifest is unreadable"))?;
    let integration = manifest
        .integrations
        .iter()
        .find(|integration| integration.id == integration_id)
        .ok_or_else(|| ApiError::not_found("plugin integration not found"))?;
    let template = integration
        .provider
        .as_ref()
        .ok_or_else(|| ApiError::bad("integration does not declare provider defaults"))?;

    validate_outbound_url(&state, &template.base_url)?;

    let wire_plugin = integration
        .provider_adapter
        .as_deref()
        .map(|name| format!("plugin:{id}/{name}"))
        .unwrap_or_default();
    let credential_plugin = integration
        .credential_strategy
        .as_deref()
        .map(|name| format!("plugin:{id}/{name}"))
        .unwrap_or_default();
    let model_source_plugin = integration
        .model_source
        .as_deref()
        .map(|name| format!("plugin:{id}/{name}"))
        .unwrap_or_default();
    let credential_mode = integration.effective_credential_mode(&manifest.permissions);

    for (reference, capability) in [
        (&wire_plugin, crate::plugins::Capability::ProviderAdapter),
        (
            &credential_plugin,
            crate::plugins::Capability::CredentialStrategy,
        ),
    ] {
        if !reference.is_empty()
            && manager
                .resolve_binding(reference, capability)
                .await
                .is_none()
        {
            return Err(ApiError::bad(format!(
                "integration capability binding '{reference}' is not enabled and approved"
            )));
        }
    }
    if !model_source_plugin.is_empty()
        && manager
            .resolve_binding(
                &model_source_plugin,
                crate::plugins::Capability::AccountModelSource,
            )
            .await
            .is_none()
        && manager
            .resolve_binding(
                &model_source_plugin,
                crate::plugins::Capability::ModelSource,
            )
            .await
            .is_none()
    {
        return Err(ApiError::bad(format!(
            "integration model source binding '{model_source_plugin}' is not enabled and approved"
        )));
    }

    let wire = WireFormat::parse(&template.wire_format)
        .ok_or_else(|| ApiError::bad("integration provider has invalid wire_format"))?;
    if wire == WireFormat::Plugin && wire_plugin.is_empty() {
        return Err(ApiError::bad(
            "integration provider uses plugin wire format without a provider adapter",
        ));
    }
    let auth = AuthScheme::parse(&template.auth_scheme)
        .ok_or_else(|| ApiError::bad("integration provider has invalid auth_scheme"))?;

    let existing = db::list_providers(&state.pool)
        .await
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|provider| {
            provider.base_url == template.base_url
                && provider.wire_plugin == wire_plugin
                && provider.credential_plugin == credential_plugin
                && provider.model_source_plugin == model_source_plugin
        });
    if let Some(provider) = existing {
        reconcile_provider_integration_semantics(
            &state,
            &provider.id,
            credential_mode,
            &id,
            &integration.id,
            template.pricing_scope,
        )
        .await?;
        state
            .registry
            .reload(&state.pool)
            .await
            .map_err(ApiError::internal)?;
        return Ok(Json(json!({
            "id": provider.id,
            "name": provider.name,
            "created": false,
        })));
    }

    let credential_hosts = template.credential_hosts.join(",");
    let id_created = db::insert_provider(
        &state.pool,
        &db::NewProvider {
            name: &integration.name,
            base_url: &template.base_url,
            wire_format: wire,
            auth_scheme: auth,
            custom_header_name: template.custom_header_name.as_deref(),
            custom_param_name: template.custom_param_name.as_deref(),
            extra_headers: serde_json::to_value(&template.extra_headers)
                .map_err(ApiError::internal)?,
            timeout_ms: template.timeout_ms as i64,
            capability_mode: &template.capability_mode,
            models_path: template.models_path.as_deref(),
            rate_limit_rules: json!({}),
            follow_redirects: template.follow_redirects,
            credential_hosts: &credential_hosts,
            allow_insecure_tls: false,
            wire_plugin: &wire_plugin,
            credential_plugin: &credential_plugin,
            model_source_plugin: &model_source_plugin,
            credential_mode: credential_mode.as_str(),
            source_plugin_id: Some(&id),
            source_integration_id: Some(&integration.id),
        },
    )
    .await
    .map_err(ApiError::internal)?;

    reconcile_provider_integration_semantics(
        &state,
        &id_created,
        credential_mode,
        &id,
        &integration.id,
        template.pricing_scope,
    )
    .await?;

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_integration_provider_created",
        "provider",
        &id_created,
        &integration.name,
        &format!(
            "Created provider from plugin {} integration {}.",
            id, integration.id
        ),
    )
    .await;
    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(json!({
        "id": id_created,
        "name": integration.name,
        "created": true,
    })))
}

pub async fn start_provider_credential_enrollment(
    State(state): State<AppState>,
    auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let provider = db::get_provider(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;

    match provider.credential_mode.as_str() {
        "auth_flow" => {}
        "none" => {
            return Err(ApiError::bad("provider does not require user credentials"));
        }
        _ => {
            return Err(ApiError::bad("provider uses manual credential enrollment"));
        }
    }

    let plugin_id = provider
        .source_plugin_id
        .clone()
        .ok_or_else(|| ApiError::bad("provider auth integration provenance is unavailable"))?;
    let integration_id = provider
        .source_integration_id
        .clone()
        .ok_or_else(|| ApiError::bad("provider auth integration provenance is unavailable"))?;

    let manager = plugin_manager(&state)?;
    let row = manager
        .get(&plugin_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::bad("provider authentication plugin is not installed"))?;
    if row.enabled == 0 {
        return Err(ApiError::bad("provider authentication plugin is disabled"));
    }
    let manifest = row
        .manifest()
        .ok_or_else(|| ApiError::bad("plugin manifest is unreadable"))?;
    let integration = manifest
        .integrations
        .iter()
        .find(|integration| integration.id == integration_id)
        .ok_or_else(|| ApiError::bad("provider authentication integration is unavailable"))?;
    let flow_name = integration
        .auth_flow
        .clone()
        .ok_or_else(|| ApiError::bad("provider integration has no authentication flow"))?;

    start_plugin_auth(
        State(state),
        auth,
        Json(PluginAuthStartBody {
            plugin_id,
            flow_name,
            provider_id: id,
        }),
    )
    .await
}

#[derive(Deserialize)]
pub struct PluginAuthStartBody {
    pub plugin_id: String,
    pub flow_name: String,
    pub provider_id: String,
}

fn loopback_bind_port(bind: &str) -> Result<u16, ApiError> {
    let base = url::Url::parse(&format!("http://{bind}"))
        .map_err(|_| ApiError::bad("KINETIX_BIND is not a valid host:port"))?;
    let host = base
        .host_str()
        .ok_or_else(|| ApiError::bad("KINETIX_BIND has no host"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if !matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1" | "::") {
        return Err(ApiError::bad(
            "loopback OAuth requires KINETIX_BIND to be loopback or unspecified",
        ));
    }
    base.port()
        .ok_or_else(|| ApiError::bad("KINETIX_BIND must include a port"))
}

fn claude_code_loopback_redirect(bind: &str) -> Result<String, ApiError> {
    let port = loopback_bind_port(bind)?;
    Ok(format!("http://localhost:{port}/callback"))
}

fn antigravity_loopback_redirect(bind: &str) -> Result<String, ApiError> {
    let base = url::Url::parse(&format!("http://{bind}"))
        .map_err(|_| ApiError::bad("KINETIX_BIND is not a valid host:port"))?;
    let host = base
        .host_str()
        .ok_or_else(|| ApiError::bad("KINETIX_BIND has no host"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = loopback_bind_port(bind)?;

    let callback_host = match host {
        "127.0.0.1" | "0.0.0.0" => "127.0.0.1",
        "localhost" => "localhost",
        "::1" | "::" => "[::1]",
        _ => unreachable!("loopback_bind_port already validated the host"),
    };

    Ok(format!("http://{callback_host}:{port}/callback"))
}

#[cfg(test)]
mod credential_enrollment_tests {
    use super::{
        manual_account_enrollment_error, resolve_auth_integration, validate_auth_flow_binding_edit,
        validate_plugin_auth_enrollment,
    };

    fn provider(mode: &str) -> crate::db::ProviderRow {
        crate::db::ProviderRow {
            id: "provider".into(),
            name: "Provider".into(),
            base_url: "https://api.example.com".into(),
            wire_format: "openai".into(),
            auth_scheme: "bearer".into(),
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: "{}".into(),
            timeout_ms: 1_000,
            capability_mode: "permissive".into(),
            models_path: None,
            rate_limit_rules: "{}".into(),
            enabled: 1,
            follow_redirects: 0,
            credential_hosts: String::new(),
            allow_insecure_tls: 0,
            created_at: "2026-01-01T00:00:00Z".into(),
            wire_plugin: String::new(),
            credential_plugin: "plugin:plugin.test/strategy".into(),
            model_source_plugin: String::new(),
            credential_mode: mode.into(),
            source_plugin_id: Some("plugin.test".into()),
            source_integration_id: Some("oauth".into()),
            pricing_scope: "integration".into(),
        }
    }

    #[test]
    fn manual_account_creation_is_mode_gated() {
        assert_eq!(manual_account_enrollment_error("manual"), None);
        assert_eq!(
            manual_account_enrollment_error("auth_flow"),
            Some("provider uses an authentication flow; connect an account instead")
        );
        assert_eq!(
            manual_account_enrollment_error("none"),
            Some("provider does not require user credentials")
        );
    }

    #[test]
    fn plugin_auth_start_requires_auth_flow_mode_and_matching_provenance() {
        let binding = "plugin:plugin.test/strategy";

        let manual = provider("manual");
        assert!(validate_plugin_auth_enrollment(&manual, "plugin.test", "oauth", binding).is_err());

        let auth_flow = provider("auth_flow");
        assert!(
            validate_plugin_auth_enrollment(&auth_flow, "plugin.test", "oauth", binding).is_ok()
        );
        assert!(
            validate_plugin_auth_enrollment(&auth_flow, "other.plugin", "oauth", binding).is_err()
        );
        assert!(
            validate_plugin_auth_enrollment(&auth_flow, "plugin.test", "other", binding).is_err()
        );
        assert!(validate_plugin_auth_enrollment(
            &auth_flow,
            "plugin.test",
            "oauth",
            "plugin:plugin.test/other"
        )
        .is_err());
    }

    #[test]
    fn shared_auth_flow_resolves_exact_source_integration() {
        let integrations = vec![
            crate::plugins::Integration {
                id: "first".into(),
                name: "First".into(),
                description: String::new(),
                credential_mode: Some(crate::plugins::CredentialMode::AuthFlow),
                provider_adapter: None,
                credential_strategy: Some("first-strategy".into()),
                auth_flow: Some("shared-login".into()),
                model_source: None,
                provider: None,
            },
            crate::plugins::Integration {
                id: "second".into(),
                name: "Second".into(),
                description: String::new(),
                credential_mode: Some(crate::plugins::CredentialMode::AuthFlow),
                provider_adapter: None,
                credential_strategy: Some("second-strategy".into()),
                auth_flow: Some("shared-login".into()),
                model_source: None,
                provider: None,
            },
        ];

        let resolved = resolve_auth_integration(&integrations, "second", "shared-login").unwrap();
        assert_eq!(resolved.id, "second");
        assert_eq!(
            resolved.credential_strategy.as_deref(),
            Some("second-strategy")
        );
    }

    #[test]
    fn auth_flow_provider_edit_rejects_conflicting_credential_binding() {
        let provider = provider("auth_flow");
        let expected = "plugin:plugin.test/strategy";

        assert!(
            validate_auth_flow_binding_edit(&provider, expected, "plugin:plugin.test/other")
                .is_err()
        );
        assert!(validate_auth_flow_binding_edit(&provider, expected, expected).is_ok());
        assert!(
            validate_auth_flow_binding_edit(&provider, expected, &provider.credential_plugin)
                .is_ok()
        );
    }
}

#[cfg(test)]
mod model_body_tests {
    use super::{validate_thinking_map, ModelBody};

    #[test]
    fn rejects_legacy_dashboard_thinking_shape() {
        let body = serde_json::json!({
            "upstream_id": "reasoning-model",
            "thinking_map": {
                "scale": "medium",
                "mappedField": "thinkingConfig"
            }
        });
        assert!(serde_json::from_value::<ModelBody>(body).is_err());
    }

    #[test]
    fn accepts_canonical_thinking_shape() {
        let body = serde_json::json!({
            "upstream_id": "reasoning-model",
            "thinking_map": {
                "levels": {
                    "low": {"reasoning_effort": "low"},
                    "medium": {"reasoning_effort": "medium"},
                    "high": {"reasoning_effort": "high"}
                },
                "budget_field": null
            }
        });
        let parsed = serde_json::from_value::<ModelBody>(body).unwrap();
        assert_eq!(parsed.thinking_map.levels.len(), 3);
    }

    #[test]
    fn rejects_non_executable_thinking_mappings() {
        for thinking_map in [
            serde_json::json!({"levels": {"high": null}}),
            serde_json::json!({"levels": {"high": 4096}}),
        ] {
            let body = serde_json::json!({
                "upstream_id": "reasoning-model",
                "thinking_map": thinking_map
            });
            let parsed = serde_json::from_value::<ModelBody>(body).unwrap();
            assert!(validate_thinking_map(&parsed.thinking_map).is_err());
        }
    }

    #[test]
    fn accepts_scalar_thinking_mapping_with_budget_field() {
        let body = serde_json::json!({
            "upstream_id": "reasoning-model",
            "thinking_map": {
                "levels": {"high": 4096},
                "budget_field": "thinking.budget_tokens"
            }
        });
        let parsed = serde_json::from_value::<ModelBody>(body).unwrap();
        assert!(validate_thinking_map(&parsed.thinking_map).is_ok());
    }
}

#[cfg(test)]
mod plugin_oauth_redirect_tests {
    use super::{antigravity_loopback_redirect, claude_code_loopback_redirect};

    #[test]
    fn claude_code_redirect_uses_bind_port_not_public_origin() {
        assert_eq!(
            claude_code_loopback_redirect("127.0.0.1:8080")
                .ok()
                .unwrap(),
            "http://localhost:8080/callback"
        );
        assert_eq!(
            claude_code_loopback_redirect("0.0.0.0:9090").ok().unwrap(),
            "http://localhost:9090/callback"
        );
    }

    #[test]
    fn antigravity_redirect_uses_ipv4_loopback_bind_port() {
        assert_eq!(
            antigravity_loopback_redirect("127.0.0.1:8080")
                .ok()
                .unwrap(),
            "http://127.0.0.1:8080/callback"
        );
    }

    #[test]
    fn antigravity_redirect_maps_unspecified_ipv4_to_loopback() {
        assert_eq!(
            antigravity_loopback_redirect("0.0.0.0:8080").ok().unwrap(),
            "http://127.0.0.1:8080/callback"
        );
    }

    #[test]
    fn antigravity_redirect_uses_ipv6_loopback_for_ipv6_bind() {
        assert_eq!(
            antigravity_loopback_redirect("[::1]:8080").ok().unwrap(),
            "http://[::1]:8080/callback"
        );
        assert_eq!(
            antigravity_loopback_redirect("[::]:8080").ok().unwrap(),
            "http://[::1]:8080/callback"
        );
    }

    #[test]
    fn loopback_oauth_rejects_non_loopback_specific_bind() {
        assert!(claude_code_loopback_redirect("192.0.2.10:8080").is_err());
        assert!(antigravity_loopback_redirect("192.0.2.10:8080").is_err());
    }
}

fn resolve_auth_integration<'a>(
    integrations: &'a [crate::plugins::Integration],
    integration_id: &str,
    flow_name: &str,
) -> Result<&'a crate::plugins::Integration, ApiError> {
    let integration = integrations
        .iter()
        .find(|integration| integration.id == integration_id)
        .ok_or_else(|| ApiError::bad("provider authentication integration is unavailable"))?;
    if integration.auth_flow.as_deref() != Some(flow_name) {
        return Err(ApiError::bad(
            "requested auth flow does not match the provider source integration",
        ));
    }
    Ok(integration)
}

/// Validate that a provider may enroll through this exact plugin integration.
fn validate_plugin_auth_enrollment(
    provider: &db::ProviderRow,
    plugin_id: &str,
    integration_id: &str,
    credential_binding: &str,
) -> Result<(), ApiError> {
    if provider.credential_mode != "auth_flow" {
        return Err(ApiError::bad(
            "provider does not use authentication-flow credential enrollment",
        ));
    }
    if provider.source_plugin_id.as_deref() != Some(plugin_id)
        || provider.source_integration_id.as_deref() != Some(integration_id)
    {
        return Err(ApiError::bad(
            "provider authentication integration provenance does not match the requested flow",
        ));
    }
    if provider.credential_plugin != credential_binding {
        return Err(ApiError::bad(format!(
            "provider '{}' is not bound to integration credential strategy '{}'",
            provider.id, credential_binding
        )));
    }
    Ok(())
}

/// Start a one-time browser authorization session for a plugin integration.
pub async fn start_plugin_auth(
    State(state): State<AppState>,
    auth: AdminAuth,
    Json(body): Json<PluginAuthStartBody>,
) -> ApiResult {
    let provider = db::get_provider(&state.pool, &body.provider_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    let integration_id = provider
        .source_integration_id
        .as_deref()
        .ok_or_else(|| ApiError::bad("provider auth integration provenance is unavailable"))?;

    let manager = plugin_manager(&state)?;
    let row = manager
        .get(&body.plugin_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("plugin not found"))?;
    let manifest = row
        .manifest()
        .ok_or_else(|| ApiError::bad("plugin manifest is unreadable"))?;

    let integration =
        resolve_auth_integration(&manifest.integrations, integration_id, &body.flow_name)?;
    let credential_strategy = integration
        .credential_strategy
        .as_deref()
        .ok_or_else(|| ApiError::bad("integration has no credential strategy"))?;

    let expected_binding = format!("plugin:{}/{}", body.plugin_id, credential_strategy);
    validate_plugin_auth_enrollment(
        &provider,
        &body.plugin_id,
        &integration.id,
        &expected_binding,
    )?;

    let redirect_uri = if body.plugin_id == "dev.kinetix.claude-code-oauth" {
        claude_code_loopback_redirect(&state.config.bind)?
    } else if body.plugin_id == "dev.kinetix.antigravity-oauth" {
        antigravity_loopback_redirect(&state.config.bind)?
    } else {
        let public_base_url = effective_public_base_url(&state).await?;
        format!("{public_base_url}/admin/api/plugins/auth/callback")
    };
    let pending = state.plugin_auth_sessions.create(
        &body.plugin_id,
        &body.flow_name,
        &integration.id,
        &body.provider_id,
        &expected_binding,
        &redirect_uri,
        &auth.token,
    );

    let authorize_url = match manager
        .auth_begin(
            &body.plugin_id,
            &body.flow_name,
            &redirect_uri,
            &pending.state,
            Some(&pending.pkce_challenge),
        )
        .await
    {
        Ok(url) => url,
        Err(error) => {
            state.plugin_auth_sessions.revoke(&pending.state);
            return Err(ApiError::bad(error.to_string()));
        }
    };

    let parsed = url::Url::parse(&authorize_url)
        .map_err(|_| ApiError::bad("plugin returned an invalid authorization URL"))?;
    if parsed.scheme() != "https" {
        state.plugin_auth_sessions.revoke(&pending.state);
        return Err(ApiError::bad("plugin authorization URL must use https"));
    }
    let auth_host = parsed
        .host_str()
        .ok_or_else(|| ApiError::bad("plugin authorization URL has no host"))?;
    if !manifest
        .permissions
        .network_hosts
        .iter()
        .any(|pattern| crate::plugins::manifest::host_matches(pattern, auth_host))
    {
        state.plugin_auth_sessions.revoke(&pending.state);
        return Err(ApiError::bad(format!(
            "authorization host '{auth_host}' is not declared in plugin network_hosts"
        )));
    }

    // The plugin must not be able to redirect the authorization code anywhere
    // other than the host's own callback, must not silently downgrade PKCE, and
    // must carry the host-generated state through the IdP (so the callback can
    // verify it was the plugin's own round-trip). Bind the returned authorize
    // URL to the host-generated values.
    let mut redirect_matches = false;
    let mut pkce_present = false;
    let mut state_matches = false;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "redirect_uri" => redirect_matches = value == redirect_uri,
            "code_challenge" => pkce_present = !value.is_empty(),
            "state" => state_matches = value == pending.state,
            _ => {}
        }
    }
    if !redirect_matches {
        state.plugin_auth_sessions.revoke(&pending.state);
        return Err(ApiError::bad(
            "plugin authorization URL does not use the host-generated redirect_uri",
        ));
    }
    if !pkce_present {
        state.plugin_auth_sessions.revoke(&pending.state);
        return Err(ApiError::bad(
            "plugin authorization URL is missing the PKCE code_challenge",
        ));
    }
    if !state_matches {
        state.plugin_auth_sessions.revoke(&pending.state);
        return Err(ApiError::bad(
            "plugin authorization URL does not carry the host-generated state",
        ));
    }

    Ok(Json(json!({
        "authorize_url": authorize_url,
        "redirect_uri": redirect_uri,
        "state": pending.state,
        "expires_in_secs": 600,
        "manual_callback_supported": matches!(
            body.plugin_id.as_str(),
            "dev.kinetix.claude-code-oauth" | "dev.kinetix.antigravity-oauth"
        ),
    })))
}

#[derive(Deserialize)]
pub struct PluginAuthCallbackQuery {
    pub state: String,
    pub code: Option<String>,
    pub error: Option<String>,
}

/// Browser callback. The one-time high-entropy state is the callback
/// credential and is consumed before code exchange, so replay fails closed.
#[derive(serde::Serialize)]
struct PluginAuthCompletion {
    result: &'static str,
    provider_id: Option<String>,
}

async fn finalize_plugin_auth_account(
    state: &AppState,
    provider: &db::ProviderRow,
    account_id: &str,
    label: &str,
    plugin_id: &str,
    flow_name: &str,
) -> Result<&'static str, ApiError> {
    // Validate a newly authorized account immediately. Transient failures do
    // not invalidate the completed enrollment, but terminal invalid credentials
    // mean the account needs reauthorization and must not be reported as a
    // clean success.
    let account = db::get_account(&state.pool, account_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("completed OAuth account is missing"))?;
    match state.credential_for(provider, &account).await {
        Ok(_) => {}
        Err(error) if error.invalid_credential() => {
            if let Err(disable_error) = state
                .disable_invalid_credential(
                    &account,
                    &error,
                    "OAuth completion credential resolution",
                )
                .await
            {
                tracing::error!(
                    provider = %provider.id,
                    account = %account.id,
                    credential_error = %error,
                    %disable_error,
                    "failed to disable newly authorized account after credential resolution confirmed invalid"
                );
                return Err(ApiError::internal(disable_error));
            }
            let _ = db::insert_audit(
                &state.pool,
                "admin",
                "plugin_auth_credential_invalid",
                "account",
                account_id,
                label,
                "The newly authorized credential was rejected as invalid; the account was disabled and must be reauthorized.",
            )
            .await;
            return Ok("reauthorization_required");
        }
        Err(error) => {
            tracing::debug!(
                provider = %provider.id,
                account = %account.id,
                %error,
                "new OAuth account credential lease could not be scheduled yet"
            );
        }
    }

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_account_authorized",
        "account",
        account_id,
        label,
        &format!(
            "Authorized account through plugin {} flow {}.",
            plugin_id, flow_name
        ),
    )
    .await;
    Ok("success")
}

async fn complete_plugin_auth(
    state: &AppState,
    session: auth::PluginAuthSession,
    query: &PluginAuthCallbackQuery,
) -> Result<PluginAuthCompletion, ApiError> {
    if query.error.is_some() {
        let _ = db::insert_audit(
            &state.pool,
            "admin",
            "plugin_auth_cancelled",
            "plugin",
            &session.plugin_id,
            &session.flow_name,
            "Provider authorization was cancelled or rejected.",
        )
        .await;
        return Ok(PluginAuthCompletion {
            result: "cancelled",
            provider_id: None,
        });
    }

    let code = query
        .code
        .as_deref()
        .filter(|code| !code.trim().is_empty())
        .ok_or_else(|| ApiError::bad("authorization callback is missing code"))?;

    let manager = plugin_manager(state)?;
    let exchange_code = if session.plugin_id == "dev.kinetix.claude-code-oauth" {
        format!("{code}#{}", query.state)
    } else {
        code.to_string()
    };
    let result = match manager
        .auth_exchange(
            &session.plugin_id,
            &session.flow_name,
            &exchange_code,
            &session.redirect_uri,
            Some(&session.pkce_verifier),
        )
        .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(
                plugin = %session.plugin_id,
                flow = %session.flow_name,
                error = %error,
                "plugin account authorization exchange failed"
            );
            let _ = db::insert_audit(
                &state.pool,
                "admin",
                "plugin_auth_failed",
                "plugin",
                &session.plugin_id,
                &session.flow_name,
                "Provider authorization code exchange failed.",
            )
            .await;
            return Ok(PluginAuthCompletion {
                result: "error",
                provider_id: None,
            });
        }
    };

    if result.secret_json.len() > 256 * 1024 {
        return Err(ApiError::bad("plugin auth credential exceeds 256 KiB"));
    }
    let secret_value: Value = serde_json::from_str(&result.secret_json)
        .map_err(|_| ApiError::bad("plugin auth credential is not valid JSON"))?;
    if !secret_value.is_object() {
        return Err(ApiError::bad(
            "plugin auth credential must be a JSON object",
        ));
    }

    let row = manager
        .get(&session.plugin_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::bad("provider authentication plugin is not installed"))?;
    let manifest = row
        .manifest()
        .ok_or_else(|| ApiError::bad("plugin manifest is unreadable"))?;
    let integration = resolve_auth_integration(
        &manifest.integrations,
        &session.integration_id,
        &session.flow_name,
    )?;
    let credential_strategy = integration
        .credential_strategy
        .as_deref()
        .ok_or_else(|| ApiError::bad("integration has no credential strategy"))?;
    let expected_binding = format!("plugin:{}/{}", session.plugin_id, credential_strategy);

    let provider = db::get_provider(&state.pool, &session.provider_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    if expected_binding != session.credential_binding
        || validate_plugin_auth_enrollment(
            &provider,
            &session.plugin_id,
            &integration.id,
            &session.credential_binding,
        )
        .is_err()
    {
        let _ = db::insert_audit(
            &state.pool,
            "admin",
            "plugin_auth_binding_changed",
            "provider",
            &provider.id,
            &provider.name,
            "Provider credential enrollment mode, provenance, or binding changed during browser authorization; enrollment refused.",
        )
        .await;
        return Ok(PluginAuthCompletion {
            result: "binding_changed",
            provider_id: Some(provider.id),
        });
    }

    let encrypted = state
        .crypto
        .encrypt(&result.secret_json)
        .map_err(ApiError::internal)?;
    let base_label = result
        .account_label
        .as_deref()
        .map(sanitize_account_label)
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| provider.name.clone());
    let existing_accounts = db::accounts_for_provider(&state.pool, &provider.id)
        .await
        .map_err(ApiError::internal)?;
    let label = if existing_accounts
        .iter()
        .any(|account| account.label == base_label)
    {
        let mut suffix = 2usize;
        loop {
            let candidate = format!("{base_label} (#{suffix})");
            if !existing_accounts
                .iter()
                .any(|account| account.label == candidate)
            {
                break candidate;
            }
            suffix += 1;
        }
    } else {
        base_label
    };
    let priority = existing_accounts
        .iter()
        .map(|account| account.priority)
        .max()
        .unwrap_or(0)
        + 1;
    let account_id = db::insert_account(
        &state.pool,
        &provider.id,
        &label,
        &encrypted,
        "oauth:****",
        priority,
        1,
        None,
        "none",
    )
    .await
    .map_err(ApiError::internal)?;

    state
        .registry
        .reload(&state.pool)
        .await
        .map_err(ApiError::internal)?;

    // Seed proactive refresh immediately for a newly authorized account rather
    // than waiting for its first inference request or a process restart.
    let result = finalize_plugin_auth_account(
        state,
        &provider,
        &account_id,
        &label,
        &session.plugin_id,
        &session.flow_name,
    )
    .await?;

    Ok(PluginAuthCompletion {
        result,
        provider_id: Some(provider.id),
    })
}

/// Browser callback. The one-time high-entropy state is the callback
/// credential and is consumed before code exchange, so replay fails closed.
pub async fn plugin_auth_callback(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<PluginAuthCallbackQuery>,
) -> Result<Redirect, ApiError> {
    if !db_healthy(&state).await {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "plugin account authorization unavailable: control plane degraded".into(),
        ));
    }

    let initiator = jar
        .get(SESSION_COOKIE)
        .map(|cookie| cookie.value().to_string());
    let session = match initiator {
        Some(initiator) => state.plugin_auth_sessions.take(&query.state, &initiator),
        None => state
            .plugin_auth_sessions
            .take_loopback_callback(&query.state),
    }
    .ok_or_else(|| ApiError::bad("invalid or expired plugin auth state"))?;

    let completion = complete_plugin_auth(&state, session.clone(), &query).await?;
    state.plugin_auth_sessions.record_completion(
        &query.state,
        &session.initiator,
        completion.result,
        completion.provider_id.clone(),
    );
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("plugin_auth", completion.result);
    if let Some(provider_id) = completion.provider_id.as_deref() {
        serializer.append_pair("plugin_auth_provider", provider_id);
    }
    let query = serializer.finish();
    Ok(Redirect::to(&format!("/admin/plugins?{query}")))
}

#[derive(Deserialize)]
pub struct PluginAuthStatusQuery {
    pub state: String,
}

pub async fn plugin_auth_status(
    State(state): State<AppState>,
    auth: AdminAuth,
    Query(query): Query<PluginAuthStatusQuery>,
) -> Result<Json<Value>, ApiError> {
    let status = state
        .plugin_auth_sessions
        .status(&query.state, &auth.token)
        .ok_or_else(|| ApiError::bad("invalid or expired plugin auth state"))?;
    Ok(Json(json!({
        "result": status.result,
        "provider_id": status.provider_id,
    })))
}

#[derive(Deserialize)]
pub struct PluginAuthManualCallbackBody {
    pub callback_url: String,
}

/// Complete a native/desktop OAuth flow from a callback URL pasted into the
/// authenticated dashboard. This is the remote/VPS counterpart to the normal
/// loopback browser callback: the provider still sees the exact native
/// redirect_uri, while Kinetix receives the short-lived code through the
/// existing authenticated admin session.
pub async fn complete_plugin_auth_manual(
    State(state): State<AppState>,
    auth: AdminAuth,
    Json(body): Json<PluginAuthManualCallbackBody>,
) -> Result<Json<Value>, ApiError> {
    if !db_healthy(&state).await {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "plugin account authorization unavailable: control plane degraded".into(),
        ));
    }

    let callback = url::Url::parse(body.callback_url.trim())
        .map_err(|_| ApiError::bad("callback URL is not a valid URL"))?;
    let state_value = callback
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .ok_or_else(|| ApiError::bad("callback URL is missing state"))?;
    let session = state
        .plugin_auth_sessions
        .peek(&state_value, &auth.token)
        .ok_or_else(|| ApiError::bad("invalid or expired plugin auth state"))?;

    if !matches!(
        session.plugin_id.as_str(),
        "dev.kinetix.claude-code-oauth" | "dev.kinetix.antigravity-oauth"
    ) {
        return Err(ApiError::bad(
            "manual callback completion is only available for reviewed loopback OAuth integrations",
        ));
    }

    let expected = url::Url::parse(&session.redirect_uri)
        .map_err(|_| ApiError::bad("stored plugin redirect URI is invalid"))?;
    let callback_origin_matches = callback.scheme() == expected.scheme()
        && callback.host_str() == expected.host_str()
        && callback.port_or_known_default() == expected.port_or_known_default()
        && callback.path() == expected.path();
    if !callback_origin_matches {
        return Err(ApiError::bad(
            "pasted callback URL does not match the OAuth redirect URI for this session",
        ));
    }

    let session = state
        .plugin_auth_sessions
        .take(&state_value, &auth.token)
        .ok_or_else(|| ApiError::bad("invalid or expired plugin auth state"))?;

    let query = PluginAuthCallbackQuery {
        state: state_value.clone(),
        code: callback
            .query_pairs()
            .find_map(|(key, value)| (key == "code").then(|| value.into_owned())),
        error: callback
            .query_pairs()
            .find_map(|(key, value)| (key == "error").then(|| value.into_owned())),
    };
    let completion = complete_plugin_auth(&state, session, &query).await?;
    state.plugin_auth_sessions.record_completion(
        &state_value,
        &auth.token,
        completion.result,
        completion.provider_id.clone(),
    );
    Ok(Json(json!({
        "ok": completion.result == "success",
        "result": completion.result,
        "provider_id": completion.provider_id,
    })))
}

/// `GET /admin/api/plugins/{id}/packages/{sha256}/preview` — inspect a retained rollback target.
pub async fn preview_plugin_rollback(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path((id, sha256)): Path<(String, String)>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let preview = manager
        .rollback_preview(&id, sha256.trim())
        .await
        .map_err(plugin_bad)?;
    let value = serde_json::to_value(preview).map_err(ApiError::internal)?;
    Ok(Json(value))
}

/// `POST /admin/api/plugins/{id}/packages/{sha256}/reinstall` — reinstall a
/// retained package after the plugin was removed. The bytes are re-hashed and
/// re-validated; the plugin is installed disabled and permissions must be
/// re-approved before it can be enabled.
pub async fn reinstall_plugin_package(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path((id, sha256)): Path<(String, String)>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let outcome = manager
        .install_retained(&id, sha256.trim())
        .await
        .map_err(plugin_bad)?;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_reinstalled",
        "plugin",
        &outcome.id,
        &outcome.id,
        &format!(
            "Reinstalled retained plugin package v{} (SHA-256 {}). Plugin is disabled and permissions must be re-approved.",
            outcome.version, outcome.package_sha256
        ),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "id": outcome.id,
        "version": outcome.version,
        "sha256": outcome.package_sha256,
        "signature": outcome.signature.as_str(),
        "provides": outcome.provides,
        "enabled": false,
    })))
}

#[derive(Deserialize)]
pub struct PluginRollbackBody {
    pub sha256: String,
}

/// Optional subset approval for `POST /plugins/{id}/permissions/approve`. With
/// no fields the full declared set is approved (back-compat); with fields the
/// granted scope is exactly what is requested (a subset of the manifest).
#[derive(Debug, Default, serde::Deserialize)]
pub struct PluginPermissionApprovalBody {
    #[serde(default)]
    pub network_hosts: Option<Vec<String>>,
    #[serde(default)]
    pub credential_scopes: Option<Vec<String>>,
    #[serde(default)]
    pub credential_read: Option<bool>,
}

/// `POST /admin/api/plugins/{id}/rollback` — reactivate a retained package.
pub async fn rollback_plugin(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<PluginRollbackBody>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let outcome = manager
        .rollback(&id, body.sha256.trim())
        .await
        .map_err(plugin_bad)?;
    // The rollback activated a different package disabled: drop the previous
    // package's registered capabilities so nothing stale keeps serving.
    state.unregister_plugin_capabilities(&id);

    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_rolled_back",
        "plugin",
        &outcome.id,
        &outcome.id,
        &format!(
            "Reactivated retained plugin package v{} (SHA-256 {}). Plugin is disabled and permissions must be re-approved.",
            outcome.version, outcome.package_sha256
        ),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "id": outcome.id,
        "version": outcome.version,
        "sha256": outcome.package_sha256,
        "signature": outcome.signature,
        "provides": outcome.provides,
        "enabled": false,
        "note": "rollback activated package disabled; review permissions before enabling",
    })))
}

/// `POST /admin/api/plugins/{id}/enable`.
pub async fn enable_plugin(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    manager.enable(&id).await.map_err(plugin_bad)?;
    register_enabled_plugin_capabilities(&state, &id).await;
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_enabled",
        "plugin",
        &id,
        &id,
        "Enabled plugin.",
    )
    .await;
    Ok(Json(json!({ "ok": true, "id": id, "enabled": true })))
}

/// `POST /admin/api/plugins/{id}/disable`.
pub async fn disable_plugin(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    manager.disable(&id).await.map_err(ApiError::internal)?;
    state.unregister_plugin_capabilities(&id);
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_disabled",
        "plugin",
        &id,
        &id,
        "Disabled plugin.",
    )
    .await;
    Ok(Json(json!({ "ok": true, "id": id, "enabled": false })))
}

/// `DELETE /admin/api/plugins/{id}` — remove a plugin.
pub async fn remove_plugin(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    manager.remove(&id).await.map_err(ApiError::internal)?;
    state.unregister_plugin_capabilities(&id);
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_removed",
        "plugin",
        &id,
        &id,
        "Removed plugin and its stored state.",
    )
    .await;
    Ok(Json(json!({ "ok": true, "id": id })))
}

/// `POST /admin/api/plugins/{id}/validate` — re-instantiate and self-check.
pub async fn validate_plugin(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let provides = manager.validate(&id).await.map_err(plugin_bad)?;
    Ok(Json(json!({ "ok": true, "id": id, "provides": provides })))
}

/// `GET /admin/api/plugins/{id}/permissions` — approved grants (§20).
pub async fn plugin_permissions(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let row = crate::plugins::store::get_plugin(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("plugin not found"))?;
    let approved = crate::plugins::store::permissions(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let requested = row.manifest().map(|m| m.permissions).unwrap_or_default();
    Ok(Json(json!({
        "id": id,
        "requested": requested,
        "approved": approved,
    })))
}

/// `POST /admin/api/plugins/{id}/permissions/approve` — re-approve the declared
/// set (all-or-nothing, §20).
pub async fn approve_plugin_permissions(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    body: Option<Json<PluginPermissionApprovalBody>>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let scoped = body.map(|Json(b)| b).unwrap_or_default();
    let grants = if scoped.network_hosts.is_none()
        && scoped.credential_scopes.is_none()
        && scoped.credential_read.is_none()
    {
        manager.approve_permissions(&id).await.map_err(plugin_bad)?
    } else {
        manager
            .approve_permissions_scoped(
                &id,
                scoped.network_hosts,
                scoped.credential_scopes,
                scoped.credential_read,
            )
            .await
            .map_err(plugin_bad)?
    };
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_permissions_approved",
        "plugin",
        &id,
        &id,
        &format!("Approved {} permission grant(s).", grants.len()),
    )
    .await;
    Ok(Json(json!({ "ok": true, "id": id, "approved": grants })))
}

/// `POST /admin/api/plugins/{id}/permissions/revoke` — revoke a grant (§20).
/// Revocation is all-or-nothing: the plugin is disabled if the requested set is
/// no longer fully granted. KV state is retained.
pub async fn revoke_plugin_permissions(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult {
    let permission = body["permission"].as_str().unwrap_or("");
    if permission.is_empty() {
        return Err(ApiError::bad("provide a permission to revoke"));
    }
    let manager = plugin_manager(&state)?;
    manager
        .revoke_permission(&id, permission)
        .await
        .map_err(plugin_bad)?;
    state.unregister_plugin_capabilities(&id);
    let _ = db::insert_audit(
        &state.pool,
        "admin",
        "plugin_permission_revoked",
        "plugin",
        &id,
        &id,
        &format!("Revoked permission '{permission}'. Plugin KV state is retained."),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "id": id,
        "revoked": permission,
        "enabled": false
    })))
}

/// `GET /admin/api/plugins/{id}/audit` — audit entries mentioning this plugin.
pub async fn plugin_audit(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let rows = db::recent_audit(&state.pool, 500)
        .await
        .map_err(ApiError::internal)?;
    let filtered: Vec<&db::AuditLogRow> = rows
        .iter()
        .filter(|r| r.target_type == "plugin" && r.target_id == id)
        .collect();
    Ok(Json(json!({ "id": id, "entries": filtered })))
}

/// `GET /admin/api/plugins/{id}/metrics` — counters + runtime state (§18).
pub async fn plugin_metrics(
    State(state): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult {
    let manager = plugin_manager(&state)?;
    let metrics = manager.metrics_for_plugin(&id);
    let runtime = crate::plugins::store::runtime_state(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    let bytes = crate::plugins::store::kv_bytes(&state.pool, &id)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "id": id,
        "host_invocations_total": metrics.totals.invocations,
        "host_successes_total": metrics.totals.successes,
        "host_faults_total": metrics.totals.faults,
        "host_timeouts_total": metrics.totals.timeouts,
        "host_cancellations_total": metrics.totals.cancellations,
        "host_http_requests_total": metrics.totals.http_requests,
        "host_duration_micros_total": metrics.totals.duration_micros,
        "by_capability": metrics.by_capability,
        "storage_bytes": bytes,
        "runtime": runtime,
    })))
}

/// Decode a base64 (or hex) Ed25519 public key into 32 bytes.
fn decode_key(k: &str) -> Option<[u8; 32]> {
    use base64::Engine;
    if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(k.trim()) {
        if let Ok(arr) = <[u8; 32]>::try_from(b.as_slice()) {
            return Some(arr);
        }
    }
    if let Ok(b) = hex::decode(k.trim()) {
        if let Ok(arr) = <[u8; 32]>::try_from(b.as_slice()) {
            return Some(arr);
        }
    }
    None
}

#[cfg(test)]
mod reasoning_discovery_control_plane_tests {
    use super::*;

    fn model(id: &str) -> crate::adapters::DiscoveredModel {
        crate::adapters::DiscoveredModel {
            id: id.to_string(),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
        }
    }

    fn provider_catalog(
        model_id: &str,
        context_window: Option<i64>,
        max_output_tokens: Option<i64>,
        capabilities_json: Value,
        modalities: Option<Value>,
        prices: Prices,
    ) -> crate::model_catalog::CatalogResolution {
        let mut catalog = crate::model_catalog::CatalogResolution::unresolved(model_id);
        catalog.provider = Some(crate::model_catalog::ProviderModelMatch {
            source: crate::model_catalog::CatalogSource::ModelsDev,
            provider_id: "example".to_string(),
            host: "api.example.com".to_string(),
            model_id: model_id.to_string(),
            context_window,
            max_input_tokens: None,
            max_output_tokens,
            capabilities_json,
            modalities,
            prices,
            model_type: None,
            metadata: Value::Null,
            source_url: Some("https://models.dev/catalog.json?type=all".to_string()),
        });
        catalog
    }

    #[test]
    fn provider_metadata_precedes_plugin_fallback_metadata() {
        let observation = discovered_observation(
            model("reasoner"),
            Some(json!({"supportedThinkingEfforts": ["low", "medium"]})),
            Some(json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": true,
                    "mode": "level",
                    "levels": ["high"]
                }
            })),
            WireFormat::Openai,
        );

        let reasoning = observation.reasoning.unwrap();
        assert_eq!(
            reasoning.levels,
            vec!["low".to_string(), "medium".to_string()]
        );
        assert_eq!(
            reasoning.upstream_format,
            "provider_supported_thinking_efforts"
        );
        assert_eq!(
            observation.thinking_map.and_then(|map| map.level_field),
            Some("reasoning_effort".to_string())
        );
    }

    #[test]
    fn discovery_transport_preserves_provider_precedence_and_plugin_fallback() {
        let provider = discovered_observation(
            model("transport-model"),
            Some(json!({"transport":{"format":"openai-responses"}})),
            Some(json!({
                "schema_version": 1,
                "transport": {"format": "anthropic"}
            })),
            WireFormat::Plugin,
        );
        assert_eq!(provider.transport.as_deref(), Some("openai-responses"));
        assert_eq!(
            provider.transport_source.as_deref(),
            Some("provider_metadata")
        );

        let plugin = discovered_observation(
            model("transport-model"),
            Some(json!({"id": "transport-model"})),
            Some(json!({
                "schema_version": 1,
                "transport": {"format": "anthropic"}
            })),
            WireFormat::Plugin,
        );
        assert_eq!(plugin.transport.as_deref(), Some("anthropic"));
        assert_eq!(
            plugin.transport_source.as_deref(),
            Some("plugin_capabilities_json")
        );
    }

    #[test]
    fn generic_effort_metadata_is_not_executable_on_anthropic_transport() {
        let observation = discovered_observation(
            model("reasoner"),
            Some(json!({"supportedThinkingEfforts": ["low", "high"]})),
            None,
            WireFormat::Anthropic,
        );

        let reasoning = observation.reasoning.unwrap();
        assert_eq!(
            reasoning.upstream_format,
            "provider_supported_thinking_efforts"
        );
        assert!(observation.thinking_map.is_none());
    }

    #[test]
    fn unsupported_provider_reasoning_does_not_fall_back_to_plugin_metadata() {
        for provider_metadata in [
            json!({"supportedThinkingEfforts": ["vendor_ultra"]}),
            json!({"supportedThinkingEfforts": []}),
        ] {
            let observation = discovered_observation(
                model("reasoner"),
                Some(provider_metadata),
                Some(json!({
                    "schema_version": 1,
                    "reasoning": {
                        "supported": true,
                        "mode": "level",
                        "levels": ["low", "high"]
                    }
                })),
                WireFormat::Openai,
            );

            assert!(observation.reasoning.is_none());
            assert!(observation.thinking_map.is_none());
        }
    }

    #[test]
    fn plugin_capabilities_are_used_when_raw_metadata_has_no_reasoning_shape() {
        let observation = discovered_observation(
            model("reasoner"),
            Some(json!({"id": "reasoner", "owned_by": "example"})),
            Some(json!({
                "schema_version": 1,
                "transport": {"format": "claude"},
                "reasoning": {
                    "supported": true,
                    "mode": "level",
                    "levels": ["low", "high", "max"],
                    "default": "high",
                    "can_disable": false
                }
            })),
            WireFormat::Anthropic,
        );

        let reasoning = observation.reasoning.unwrap();
        assert_eq!(
            reasoning.mode,
            Some(crate::adapters::ReasoningCapabilityMode::Level)
        );
        assert_eq!(reasoning.default.as_deref(), Some("high"));
        assert_eq!(reasoning.upstream_format, "provider_declared");
        assert!(observation.thinking_map.is_none());
    }

    #[test]
    fn plugin_descriptive_reasoning_survives_discovery() {
        for (reasoning, expected_mode) in [
            (json!({"supported": true}), None),
            (
                json!({"supported": true, "mode": "toggle", "can_disable": true}),
                Some(crate::adapters::ReasoningCapabilityMode::Toggle),
            ),
        ] {
            let observation = discovered_observation(
                model("reasoner"),
                Some(json!({"id": "reasoner", "owned_by": "example"})),
                Some(json!({
                    "schema_version": 1,
                    "reasoning": reasoning
                })),
                WireFormat::Plugin,
            );

            let reasoning = observation.reasoning.unwrap();
            assert_eq!(reasoning.mode, expected_mode);
            assert!(reasoning.levels.is_empty());
            assert!(observation.thinking_map.is_none());
        }
    }

    #[test]
    fn reasoning_support_preserves_unknown_and_explicit_unsupported() {
        let unknown = discovered_observation(
            model("unknown"),
            Some(json!({"id": "unknown"})),
            Some(json!({"schema_version": 1})),
            WireFormat::Plugin,
        );
        assert_eq!(unknown.reasoning_support, None);
        assert!(unknown.reasoning.is_none());

        let unsupported = discovered_observation(
            model("unsupported"),
            Some(json!({"id": "unsupported"})),
            Some(json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": false
                }
            })),
            WireFormat::Plugin,
        );
        assert_eq!(unsupported.reasoning_support, Some(false));
        assert!(unsupported.reasoning.is_none());

        let supported = discovered_observation(
            model("supported"),
            Some(json!({"id": "supported"})),
            Some(json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": true
                }
            })),
            WireFormat::Plugin,
        );
        assert_eq!(supported.reasoning_support, Some(true));
        assert!(supported.reasoning.is_some());

        let unknown_capabilities = discovered_capabilities(&unknown);
        let unsupported_capabilities = discovered_capabilities(&unsupported);
        let supported_capabilities = discovered_capabilities(&supported);
        assert!(unknown_capabilities["reasoning"].is_null());
        assert_eq!(unsupported_capabilities["reasoning"], false);
        assert_eq!(supported_capabilities["reasoning"], true);
    }

    #[test]
    fn invalid_plugin_v1_contract_is_ignored() {
        for metadata in [
            json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": true,
                    "mode": "level",
                    "levels": ["low"],
                    "default": "max"
                }
            }),
            json!({
                "schema_version": 1,
                "unknown": true
            }),
            json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": true,
                    "unknown": true
                }
            }),
        ] {
            let observation = discovered_observation(
                model("reasoner"),
                Some(json!({"id": "reasoner", "owned_by": "example"})),
                Some(metadata),
                WireFormat::Plugin,
            );

            assert!(observation.reasoning.is_none());
            assert!(observation.thinking_map.is_none());
        }
    }

    #[test]
    fn valid_plugin_reasoning_default_survives_discovery() {
        let observation = discovered_observation(
            model("reasoner"),
            Some(json!({"id": "reasoner", "owned_by": "example"})),
            Some(json!({
                "schema_version": 1,
                "transport": {"format": "openai"},
                "reasoning": {
                    "supported": true,
                    "mode": "level",
                    "levels": ["low", "max"],
                    "default": "max",
                    "can_disable": false
                }
            })),
            WireFormat::Openai,
        );

        let reasoning = observation.reasoning.unwrap();
        assert_eq!(reasoning.default.as_deref(), Some("max"));
        assert_eq!(
            serde_json::to_value(&reasoning).unwrap()["default"],
            json!("max")
        );
        assert_eq!(
            observation.thinking_map.and_then(|map| map.level_field),
            Some("reasoning_effort".to_string())
        );
    }

    #[test]
    fn responses_transport_uses_responses_reasoning_mapping() {
        let observation = discovered_observation(
            model("reasoner"),
            Some(json!({"id": "reasoner", "owned_by": "example"})),
            Some(json!({
                "schema_version": 1,
                "transport": {"format": "openai-responses"},
                "reasoning": {
                    "supported": true,
                    "mode": "level",
                    "levels": ["low", "high"],
                    "default": "high",
                    "can_disable": false
                }
            })),
            WireFormat::Openai,
        );

        let reasoning = observation.reasoning.unwrap();
        assert_eq!(reasoning.upstream_format, "responses_effort");
        assert_eq!(reasoning.default.as_deref(), Some("high"));
        assert_eq!(
            observation.thinking_map.and_then(|map| map.level_field),
            Some("reasoning.effort".to_string())
        );
    }

    #[test]
    fn unsupported_plugin_capability_schema_is_ignored() {
        for metadata in [
            json!({
                "schema_version": 3,
                "reasoning": {
                    "supported": true,
                    "mode": "toggle",
                    "can_disable": true
                }
            }),
            json!({
                "schema_version": "1",
                "reasoning": {
                    "supported": true
                }
            }),
            json!({
                "reasoning": {
                    "supported": true
                }
            }),
        ] {
            let observation = discovered_observation(
                model("reasoner"),
                Some(json!({"id": "reasoner", "owned_by": "example"})),
                Some(metadata),
                WireFormat::Plugin,
            );

            assert!(observation.reasoning.is_none());
            assert!(observation.thinking_map.is_none());
        }
    }

    #[test]
    fn pricing_precedence_is_per_field_with_provenance() {
        let catalog = provider_catalog(
            "priced-model",
            None,
            None,
            json!({"schema_version": 1}),
            None,
            Prices {
                input_per_1m: Some(0.75),
                output_per_1m: Some(3.75),
                cached_per_1m: Some(0.075),
                cache_write_per_1m: Some(0.1),
                thinking_per_1m: None,
            },
        );
        let observation = discovered_observation_with_catalog(
            model("priced-model"),
            Some(json!({
                "id": "priced-model",
                "prices": {
                    "input_per_1m": 1.5,
                    "cached_per_1m": 0.05
                }
            })),
            Some(json!({
                "schema_version": 1,
                "prices": {
                    "input_per_1m": 1.0,
                    "output_per_1m": null,
                    "cache_write_per_1m": 0.2
                }
            })),
            WireFormat::Openai,
            Some(catalog),
        );

        assert_eq!(observation.prices.input_per_1m, Some(1.5));
        assert_eq!(observation.prices.output_per_1m, Some(3.75));
        assert_eq!(observation.prices.cached_per_1m, Some(0.05));
        assert_eq!(observation.prices.cache_write_per_1m, Some(0.2));
        assert_eq!(observation.prices.thinking_per_1m, None);
        assert_eq!(
            observation.price_sources["input_per_1m"],
            json!("provider_metadata")
        );
        assert_eq!(
            observation.price_sources["output_per_1m"],
            json!("models.dev:provider")
        );
        assert_eq!(
            observation.price_sources["cached_per_1m"],
            json!("provider_metadata")
        );
        assert_eq!(
            observation.price_sources["cache_write_per_1m"],
            json!("plugin_capabilities_json")
        );
        assert!(observation.price_sources["thinking_per_1m"].is_null());
    }

    #[test]
    fn malformed_plugin_prices_are_ignored_without_erasing_catalog_values() {
        let catalog = provider_catalog(
            "priced-model",
            None,
            None,
            json!({"schema_version": 1}),
            None,
            Prices {
                input_per_1m: Some(0.75),
                output_per_1m: Some(3.75),
                ..Prices::default()
            },
        );
        let observation = discovered_observation_with_catalog(
            model("priced-model"),
            None,
            Some(json!({
                "schema_version": 1,
                "prices": {
                    "input_per_1m": -1,
                    "output_per_1m": "free",
                    "cached_per_1m": null
                }
            })),
            WireFormat::Plugin,
            Some(catalog),
        );

        assert_eq!(observation.prices.input_per_1m, Some(0.75));
        assert_eq!(observation.prices.output_per_1m, Some(3.75));
        assert_eq!(observation.prices.cached_per_1m, None);
        assert_eq!(
            observation.price_sources["input_per_1m"],
            json!("models.dev:provider")
        );
        assert_eq!(
            observation.price_sources["output_per_1m"],
            json!("models.dev:provider")
        );
    }

    #[tokio::test]
    async fn admin_rediscovery_preserves_import_provenance() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let home = std::env::temp_dir().join(format!(
            "kinetix-admin-rediscovery-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let db_path = home.join("kinetix.db");
        let database_url = format!("sqlite://{}", db_path.display());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = r#"{"data":[{"id":"reasoner","supportedThinkingEfforts":["low","high"],"capabilities":{"tool_calling":true}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let config = Arc::new(
            crate::config::Config::build(crate::config::CliOverrides {
                home: Some(home.clone()),
                database_url: Some(database_url.clone()),
                master_key: Some(hex::encode([7u8; 32])),
                admin_token: Some("test-admin-password".into()),
                allow_private_upstreams: Some(true),
                allow_insecure_tls: Some(true),
                ..Default::default()
            })
            .unwrap(),
        );
        let pool = db::connect(&database_url).await.unwrap();
        db::migrate(&pool).await.unwrap();

        let crypto = Arc::new(crate::crypto::Crypto::new(&config.master_key));
        let base_url = format!("http://{address}");
        let provider_id = db::insert_provider(
            &pool,
            &db::NewProvider {
                name: "test",
                base_url: &base_url,
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1000,
                capability_mode: "permissive",
                models_path: Some("/models"),
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

        let encrypted = crypto.encrypt("test-api-key").unwrap();
        db::insert_account(
            &pool,
            &provider_id,
            "default",
            &encrypted,
            "test:****",
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
                upstream_id: "reasoner",
                display_name: "Reasoner",
                enabled: true,
                context_window: Some(4096),
                max_output_tokens: Some(1024),
                capabilities: json!({
                    "text": false,
                    "reasoning": false,
                    "tool_calling": false
                }),
                prices: json!({
                    "input_per_1m": 9.99,
                    "output_per_1m": 19.99
                }),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({
                    "imported_from_discovery": true,
                    "import_source": "dashboard"
                }),
            },
        )
        .await
        .unwrap();

        let registry = Arc::new(crate::registry::Registry::new());
        registry.reload(&pool).await.unwrap();
        let state = AppState::new(
            config,
            pool.clone(),
            registry,
            crypto,
            reqwest::Client::new(),
            crate::logqueue::UsageLogQueue::new(pool.clone(), 16),
            0,
        );

        let response = discover_models(
            State(state.clone()),
            AdminAuth {
                actor: "admin".into(),
                token: "test".into(),
            },
            Path(provider_id.clone()),
        )
        .await
        .unwrap();
        assert_eq!(response.0["models"][0]["id"], "reasoner");
        assert_eq!(response.0["models"][0]["already_imported"], true);
        assert_eq!(
            response.0["models"][0]["raw_metadata"],
            json!({
                "id": "reasoner",
                "supportedThinkingEfforts": ["low", "high"],
                "capabilities": {
                    "tool_calling": true
                }
            })
        );
        assert_eq!(
            response.0["models"][0]["raw_metadata_truncated"],
            json!(false)
        );

        server.await.unwrap();

        let rediscovered = db::get_model(&pool, &model_id).await.unwrap().unwrap();
        let discovery: Value = serde_json::from_str(&rediscovered.discovery).unwrap();
        assert_eq!(discovery["imported_from_discovery"], true);
        assert_eq!(discovery["import_source"], "dashboard");
        assert_eq!(discovery["disappeared"], false);
        assert!(discovery.get("last_seen").is_some());
        assert_eq!(
            discovery["latest_observation"]["raw_metadata"],
            json!({
                "id": "reasoner",
                "supportedThinkingEfforts": ["low", "high"],
                "capabilities": {
                    "tool_calling": true
                }
            })
        );
        assert_eq!(
            discovery["latest_observation"]["raw_metadata_truncated"],
            false
        );
        assert!(
            discovery.get("reasoning_capability").is_none(),
            "fresh reasoning metadata must remain observational until accepted"
        );

        assert_eq!(rediscovered.context_window, Some(4096));
        assert_eq!(rediscovered.max_output_tokens, Some(1024));
        assert_eq!(
            serde_json::from_str::<Value>(&rediscovered.capabilities).unwrap(),
            json!({
                "text": false,
                "reasoning": false,
                "tool_calling": false
            })
        );
        assert_eq!(
            serde_json::from_str::<Value>(&rediscovered.prices).unwrap(),
            json!({
                "input_per_1m": 9.99,
                "output_per_1m": 19.99
            })
        );

        // Fresh reconciliation metadata is observational only. Even after the
        // registry reloads from DB, runtime reasoning stays at the configured
        // value until the operator explicitly accepts the drift.
        state.registry.reload(&pool).await.unwrap();
        let snapshot = state.registry.snapshot();
        let runtime_provider = snapshot.providers.get(&provider_id).unwrap();
        let runtime_model = snapshot.models.get(&model_id).unwrap();
        let before_accept =
            crate::adapters::resolve_execution_profile(runtime_provider, runtime_model).unwrap();
        assert_eq!(before_accept.capabilities.reasoning, Some(false));
        assert_eq!(before_accept.capabilities.tool_calling, Some(false));
        assert!(before_accept.thinking_map.levels.is_empty());

        update_model_reconciliation(
            State(state.clone()),
            AdminAuth {
                actor: "admin".into(),
                token: "test".into(),
            },
            Path(model_id.clone()),
            Json(ReconciliationActionBody {
                action: "accept".into(),
                fields: vec![
                    "capabilities.reasoning".into(),
                    "capabilities.tool_calling".into(),
                    "reasoning_capability".into(),
                    "thinking_map".into(),
                ],
            }),
        )
        .await
        .unwrap();

        let snapshot = state.registry.snapshot();
        let runtime_provider = snapshot.providers.get(&provider_id).unwrap();
        let runtime_model = snapshot.models.get(&model_id).unwrap();
        let after_accept =
            crate::adapters::resolve_execution_profile(runtime_provider, runtime_model).unwrap();
        assert_eq!(after_accept.capabilities.reasoning, Some(true));
        assert_eq!(after_accept.capabilities.tool_calling, Some(true));
        assert!(after_accept.thinking_map.level_is_executable("low"));
        assert!(after_accept.thinking_map.level_is_executable("high"));

        drop(state);
        pool.close().await;
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn canonical_enrichment_does_not_invent_provider_controls_or_pricing() {
        let catalog = crate::model_catalog::resolve(
            "https://unknown-gateway.example/v1",
            "DeepSeek-V4.1-Flash",
            None,
        );
        let observation = discovered_observation_with_catalog(
            model("DeepSeek-V4.1-Flash"),
            Some(json!({"id": "DeepSeek-V4.1-Flash"})),
            None,
            WireFormat::Openai,
            Some(catalog),
        );

        assert_eq!(
            observation.canonical_model_id.as_deref(),
            Some("deepseek/deepseek-v4.1-flash")
        );
        assert_eq!(
            observation.canonical_match.as_deref(),
            Some("case_insensitive_model_id")
        );
        assert_eq!(observation.model.context_window, Some(1_000_000));
        assert_eq!(observation.model.max_output_tokens, Some(384_000));
        assert_eq!(observation.reasoning_support, Some(true));
        assert!(observation
            .reasoning
            .as_ref()
            .is_some_and(|reasoning| reasoning.levels.is_empty()));
        assert!(observation.thinking_map.is_none());
        assert!(observation.prices.input_per_1m.is_none());
        assert!(observation.prices.output_per_1m.is_none());
        assert!(observation.prices.cached_per_1m.is_none());
        assert!(observation.prices.cache_write_per_1m.is_none());
        assert!(observation.prices.thinking_per_1m.is_none());
        assert_eq!(
            observation.capability_sources["reasoning"],
            json!("bundled_catalog")
        );
        assert!(observation.price_sources["input_per_1m"].is_null());
        assert!(observation.catalog.as_ref().unwrap()["provider"].is_null());
    }

    #[test]
    fn sparse_bai_model_is_enriched_from_catalog() {
        let catalog =
            crate::model_catalog::resolve("https://api.b.ai/v1/", "DeepSeek-V4.1-Flash", None);
        let observation = discovered_observation_with_catalog(
            model("DeepSeek-V4.1-Flash"),
            Some(json!({"id": "DeepSeek-V4.1-Flash", "object": "model"})),
            None,
            WireFormat::Openai,
            Some(catalog),
        );

        assert_eq!(observation.model.context_window, Some(1_000_000));
        assert_eq!(observation.model.max_output_tokens, Some(384_000));
        assert_eq!(observation.capabilities.vision, Some(true));
        assert_eq!(observation.capabilities.tool_calling, Some(true));
        assert_eq!(observation.reasoning_support, Some(true));

        let reasoning = observation.reasoning.as_ref().unwrap();
        assert_eq!(
            reasoning.levels,
            vec!["low".to_string(), "high".to_string(), "max".to_string()]
        );
        assert_eq!(reasoning.default.as_deref(), Some("high"));
        assert_eq!(reasoning.upstream_format, "provider_declared");
        assert!(observation.thinking_map.is_none());
        assert_eq!(
            observation.capability_sources["reasoning"],
            json!("bundled_catalog")
        );
    }

    #[test]
    fn ai_studio_thinking_hint_is_enriched_with_models_dev_levels() {
        let catalog = provider_catalog(
            "gemini-3.8-flash",
            Some(1_048_576),
            Some(65_536),
            json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": true,
                    "mode": "level",
                    "levels": ["low", "medium", "high"],
                    "can_disable": false
                },
                "text": {"supported": true},
                "tools": {"supported": true},
                "vision": {"input": true},
                "structured_output": {"supported": true}
            }),
            Some(json!({
                "input": ["text", "image"],
                "output": ["text"]
            })),
            Prices {
                input_per_1m: Some(0.75),
                output_per_1m: Some(3.75),
                cached_per_1m: Some(0.075),
                cache_write_per_1m: None,
                thinking_per_1m: None,
            },
        );
        let mut discovered = model("gemini-3.8-flash");
        discovered.context_window = Some(1_048_576);
        discovered.max_output_tokens = Some(65_536);
        let observation = discovered_observation_with_catalog(
            discovered,
            Some(json!({
                "name": "models/gemini-3.8-flash",
                "inputTokenLimit": 1048576,
                "outputTokenLimit": 65536,
                "supportedGenerationMethods": ["generateContent", "countTokens"],
                "thinking": true
            })),
            None,
            WireFormat::Gemini,
            Some(catalog),
        );

        assert_eq!(observation.model.context_window, Some(1_048_576));
        assert_eq!(observation.model.max_output_tokens, Some(65_536));
        assert_eq!(observation.reasoning_support, Some(true));
        assert_eq!(observation.capabilities.text, Some(true));
        assert_eq!(observation.capabilities.vision, Some(true));
        assert_eq!(observation.capabilities.tool_calling, Some(true));
        assert_eq!(observation.prices.input_per_1m, Some(0.75));
        assert_eq!(observation.prices.output_per_1m, Some(3.75));
        assert_eq!(observation.prices.cached_per_1m, Some(0.075));
        assert_eq!(observation.prices.cache_write_per_1m, None);
        assert_eq!(observation.prices.thinking_per_1m, None);
        assert_eq!(
            observation.price_sources["input_per_1m"],
            json!("models.dev:provider")
        );
        assert_eq!(observation.price_sources["thinking_per_1m"], Value::Null);
        assert_eq!(
            observation.modalities.as_ref().unwrap()["input"],
            json!(["text", "image"])
        );

        let reasoning = observation.reasoning.as_ref().unwrap();
        assert_eq!(
            reasoning.levels,
            vec!["low".to_string(), "medium".to_string(), "high".to_string()]
        );
        assert_eq!(reasoning.default, None);
        assert_eq!(reasoning.upstream_format, "gemini_thinking_level");
        assert_eq!(
            observation
                .thinking_map
                .as_ref()
                .and_then(|map| map.level_field.as_deref()),
            Some("thinkingConfig.thinkingLevel")
        );
        assert_eq!(
            observation.capability_sources["reasoning"],
            json!("provider_metadata+models.dev:provider")
        );
    }

    #[test]
    fn ai_studio_thinking_false_overrides_catalog() {
        let catalog = provider_catalog(
            "gemini-3.8-flash",
            Some(1_048_576),
            Some(65_536),
            json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": true,
                    "mode": "level",
                    "levels": ["low", "medium", "high"],
                    "can_disable": false
                }
            }),
            None,
            Prices::default(),
        );
        let observation = discovered_observation_with_catalog(
            model("gemini-3.8-flash"),
            Some(json!({
                "name": "models/gemini-3.8-flash",
                "thinking": false
            })),
            None,
            WireFormat::Gemini,
            Some(catalog),
        );

        assert_eq!(observation.reasoning_support, Some(false));
        assert!(observation.reasoning.is_none());
        assert!(observation.thinking_map.is_none());
        assert_eq!(
            observation.capability_sources["reasoning"],
            json!("provider_metadata")
        );
    }

    #[test]
    fn provider_reasoning_metadata_overrides_catalog() {
        let catalog =
            crate::model_catalog::resolve("https://api.b.ai/v1", "DeepSeek-V4.1-Flash", None);
        let observation = discovered_observation_with_catalog(
            model("DeepSeek-V4.1-Flash"),
            Some(json!({
                "id": "DeepSeek-V4.1-Flash",
                "supportedThinkingEfforts": ["low", "high"]
            })),
            None,
            WireFormat::Openai,
            Some(catalog),
        );

        let reasoning = observation.reasoning.unwrap();
        assert_eq!(
            reasoning.levels,
            vec!["low".to_string(), "high".to_string()]
        );
        assert_eq!(
            observation.capability_sources["reasoning"],
            json!("provider_metadata")
        );
    }

    #[test]
    fn plugin_field_override_does_not_hide_catalog_reasoning() {
        let catalog =
            crate::model_catalog::resolve("https://api.b.ai/v1", "DeepSeek-V4.1-Flash", None);
        let observation = discovered_observation_with_catalog(
            model("DeepSeek-V4.1-Flash"),
            Some(json!({"id": "DeepSeek-V4.1-Flash"})),
            Some(json!({
                "schema_version": 1,
                "vision": {"input": false}
            })),
            WireFormat::Openai,
            Some(catalog),
        );

        assert_eq!(observation.capabilities.vision, Some(false));
        assert_eq!(observation.reasoning_support, Some(true));
        assert_eq!(
            observation.capability_sources["vision"],
            json!("plugin_capabilities_json")
        );
        assert_eq!(
            observation.capability_sources["reasoning"],
            json!("bundled_catalog")
        );
    }

    #[tokio::test]
    async fn stale_and_specialized_discovery_imports_are_rejected_server_side() {
        use std::sync::Arc;

        let home = std::env::temp_dir().join(format!(
            "kinetix-specialized-import-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let db_path = home.join("kinetix.db");
        let database_url = format!("sqlite://{}", db_path.display());
        let config = Arc::new(
            crate::config::Config::build(crate::config::CliOverrides {
                home: Some(home.clone()),
                database_url: Some(database_url.clone()),
                master_key: Some(hex::encode([9u8; 32])),
                admin_token: Some("test-admin-password".into()),
                ..Default::default()
            })
            .unwrap(),
        );
        let pool = db::connect(&database_url).await.unwrap();
        db::migrate(&pool).await.unwrap();

        let provider_id = db::insert_provider(
            &pool,
            &db::NewProvider {
                name: "specialized",
                base_url: "https://example.invalid/v1",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1000,
                capability_mode: "strict",
                models_path: Some("/models"),
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: false,
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

        let registry = Arc::new(crate::registry::Registry::new());
        registry.reload(&pool).await.unwrap();
        let state = AppState::new(
            config,
            pool.clone(),
            registry,
            Arc::new(crate::crypto::Crypto::new(&[9u8; 32])),
            reqwest::Client::new(),
            crate::logqueue::UsageLogQueue::new(pool.clone(), 16),
            0,
        );

        let stale_error = create_model(
            State(state.clone()),
            AdminAuth {
                actor: "admin".into(),
                token: "test".into(),
            },
            Path(provider_id.clone()),
            Json(ModelBody {
                upstream_id: "jev-latest".into(),
                display_name: Some("JEV".into()),
                enabled: true,
                context_window: Some(64_000),
                max_output_tokens: Some(0),
                capabilities: json!({"text": true}),
                prices: json!({}),
                parameters: json!({}),
                thinking_map: ThinkingMap::default(),
                extra_request: json!({}),
                discovery: json!({
                    "imported_from_discovery": true
                }),
                transport_override: None,
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(stale_error.0, StatusCode::BAD_REQUEST);
        assert!(stale_error.1.contains("execution_supported: true"));

        let specialized_error = create_model(
            State(state),
            AdminAuth {
                actor: "admin".into(),
                token: "test".into(),
            },
            Path(provider_id),
            Json(ModelBody {
                upstream_id: "jev-latest".into(),
                display_name: Some("JEV".into()),
                enabled: true,
                context_window: Some(64_000),
                max_output_tokens: Some(0),
                capabilities: json!({"text": true}),
                prices: json!({}),
                parameters: json!({}),
                thinking_map: ThinkingMap::default(),
                extra_request: json!({}),
                discovery: json!({
                    "imported_from_discovery": true,
                    "model_type": "decision",
                    "execution_supported": true
                }),
                transport_override: None,
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(specialized_error.0, StatusCode::BAD_REQUEST);
        assert!(specialized_error.1.contains("decision"));

        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn sparse_discovery_import_preserves_unknowns_through_runtime() {
        use std::sync::Arc;

        let home = std::env::temp_dir().join(format!(
            "kinetix-sparse-import-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let db_path = home.join("kinetix.db");
        let database_url = format!("sqlite://{}", db_path.display());
        let config = Arc::new(
            crate::config::Config::build(crate::config::CliOverrides {
                home: Some(home.clone()),
                database_url: Some(database_url.clone()),
                master_key: Some(hex::encode([9u8; 32])),
                admin_token: Some("test-admin-password".into()),
                ..Default::default()
            })
            .unwrap(),
        );
        let pool = db::connect(&database_url).await.unwrap();
        db::migrate(&pool).await.unwrap();

        let provider_id = db::insert_provider(
            &pool,
            &db::NewProvider {
                name: "sparse",
                base_url: "https://example.invalid/v1",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1000,
                capability_mode: "strict",
                models_path: Some("/models"),
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: false,
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

        let observation = discovered_observation(
            model("sparse-model"),
            Some(json!({"id": "sparse-model"})),
            Some(json!({
                "schema_version": 1,
                "structured_output": {"supported": true}
            })),
            WireFormat::Openai,
        );
        let discovery_caps = discovered_capabilities(&observation);
        assert!(discovery_caps["vision"].is_null());
        assert!(discovery_caps["tool_calling"].is_null());
        assert_eq!(discovery_caps["structured_output"], true);

        let registry = Arc::new(crate::registry::Registry::new());
        registry.reload(&pool).await.unwrap();
        let state = AppState::new(
            config,
            pool.clone(),
            registry.clone(),
            Arc::new(crate::crypto::Crypto::new(&[9u8; 32])),
            reqwest::Client::new(),
            crate::logqueue::UsageLogQueue::new(pool.clone(), 16),
            0,
        );

        let created = create_model(
            State(state),
            AdminAuth {
                actor: "admin".into(),
                token: "test".into(),
            },
            Path(provider_id),
            Json(ModelBody {
                upstream_id: "sparse-model".into(),
                display_name: Some("Sparse Model".into()),
                enabled: true,
                context_window: observation.model.context_window,
                max_output_tokens: observation.model.max_output_tokens,
                capabilities: discovery_caps,
                prices: json!({}),
                parameters: json!({}),
                thinking_map: ThinkingMap::default(),
                extra_request: json!({}),
                discovery: json!({
                    "imported_from_discovery": true,
                    "execution_supported": true
                }),
                transport_override: None,
            }),
        )
        .await
        .unwrap();
        let model_id = created.0["id"].as_str().unwrap();

        let row = db::get_model(&pool, model_id).await.unwrap().unwrap();
        assert_eq!(row.context_window, None);
        assert_eq!(row.max_output_tokens, None);
        assert_eq!(
            serde_json::from_str::<Value>(&row.capabilities).unwrap(),
            json!({"structured_output": true})
        );
        assert!(row.caps().structured_output);
        assert!(!row.caps().vision);
        assert!(!row.caps().tool_calling);

        let body = crate::frontends::models::models_body(
            FrontendFormat::OpenAi,
            registry.as_ref(),
            &["*".to_string()],
        );
        let listed = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == "sparse-model")
            .unwrap();
        assert!(listed.get("context_window").is_none());
        assert!(listed.get("max_output_tokens").is_none());
        assert_eq!(listed["capabilities"], json!({"structured_output": true}));

        let raw_capabilities = serde_json::from_str::<Value>(&row.capabilities).unwrap();
        let caps = row.caps();
        let target = crate::predicate::TargetFacts {
            model_id: &row.id,
            model_display: &row.display_name,
            provider_id: &row.provider_id,
            provider_name: "sparse",
            capabilities: &caps,
            capabilities_raw: &raw_capabilities,
            context_window: row.context_window,
            max_output_tokens: row.max_output_tokens,
        };
        let request = crate::predicate::RequestFacts {
            frontend: "openai",
            requested_model: "sparse-model",
            requested_route: None,
            key_tag: None,
            has_tools: false,
            has_images: false,
            has_reasoning: false,
            input_tokens: 1,
        };

        let vision = crate::predicate::TargetPredicate {
            expr: Some(
                serde_json::from_value(json!({
                    "fact": {"name": "target_capability", "arg": "vision"},
                    "op": "eq",
                    "value": true
                }))
                .unwrap(),
            ),
            when_unknown: crate::predicate::WhenUnknown::Skip,
        };
        let vision_result = crate::predicate::eligibility(&vision, &request, &target);
        assert_eq!(vision_result.result, crate::predicate::Tri::Unknown);
        assert!(!vision_result.eligible);

        let structured = crate::predicate::TargetPredicate {
            expr: Some(
                serde_json::from_value(json!({
                    "fact": {"name": "target_capability", "arg": "structured_output"},
                    "op": "eq",
                    "value": true
                }))
                .unwrap(),
            ),
            when_unknown: crate::predicate::WhenUnknown::Skip,
        };
        let structured_result = crate::predicate::eligibility(&structured, &request, &target);
        assert_eq!(structured_result.result, crate::predicate::Tri::True);
        assert!(structured_result.eligible);

        pool.close().await;
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn raw_metadata_is_bounded_for_admin_inspection() {
        let small = json!({"id": "small", "provider_field": "visible"});
        let (raw, truncated) = bounded_raw_metadata(Some(&small));
        assert_eq!(raw, Some(small));
        assert!(!truncated);

        let oversized = json!({
            "id": "oversized",
            "payload": "x".repeat(MAX_RAW_DISCOVERY_METADATA_BYTES)
        });
        let (raw, truncated) = bounded_raw_metadata(Some(&oversized));
        assert!(raw.is_none());
        assert!(truncated);
    }

    #[test]
    fn plugin_v2_discovery_keeps_variant_opaque_state_and_plugin_managed_reasoning() {
        let observation = discovered_observation_with_catalog(
            crate::adapters::DiscoveredModel {
                id: "gemini-3.8-flash-high".into(),
                display_name: Some("Gemini 3.8 Flash High".into()),
                context_window: None,
                max_output_tokens: None,
            },
            None,
            Some(json!({
                "schema_version": 2,
                "reasoning": {
                    "supported": true,
                    "mode": "level",
                    "levels": ["high"],
                    "default": "high",
                    "can_disable": false
                },
                "identity": {
                    "canonical_model_id": "google/gemini-3.8-flash",
                    "variant": {
                        "kind": "reasoning_tier",
                        "id": "high",
                        "reasoning_level": "high",
                        "fixed": true
                    }
                },
                "opaque_state": {
                    "kind": "gemini_thought_signature",
                    "family": "gemini",
                    "encoding_version": 1,
                    "placeholder_strategy": "gemini3_skip_validator"
                }
            })),
            WireFormat::Plugin,
            None,
        );

        assert_eq!(observation.reasoning_support, Some(true));
        assert_eq!(
            observation.reasoning.as_ref().unwrap().levels,
            vec!["high".to_string()]
        );
        assert!(observation.thinking_map.is_none());
        assert_eq!(
            observation.provider_variant.as_ref().unwrap()["id"],
            json!("high")
        );
        assert_eq!(
            observation.opaque_state.as_ref().unwrap()["family"],
            json!("gemini")
        );
        assert_eq!(
            observation.capability_sources["reasoning"],
            json!("plugin_capabilities_json")
        );
    }

    #[test]
    fn raw_metadata_matches_openai_and_gemini_model_ids() {
        let openai = json!({
            "data": [
                {"id": "gpt-test", "supportedThinkingEfforts": ["low"]}
            ]
        });
        assert_eq!(
            raw_discovery_metadata(&openai, "gpt-test")
                .and_then(|value| value.get("id"))
                .and_then(Value::as_str),
            Some("gpt-test")
        );

        let gemini = json!({
            "models": [
                {"name": "models/gemini-test", "thinking": {"levels": ["high"]}}
            ]
        });
        assert_eq!(
            raw_discovery_metadata(&gemini, "gemini-test")
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str),
            Some("models/gemini-test")
        );
    }
}

#[cfg(test)]
mod credential_enrollment_regression_tests {
    use super::*;
    use std::sync::Arc;

    struct ExpiredCredential;

    #[async_trait::async_trait]
    impl crate::credentials::CredentialStrategy for ExpiredCredential {
        fn name(&self) -> &'static str {
            "test_expired_auth_credential"
        }

        async fn resolve(
            &self,
            _account: &db::AccountRow,
        ) -> std::result::Result<
            crate::credentials::ResolvedCredential,
            crate::credentials::CredentialRotationError,
        > {
            Err(crate::credentials::CredentialRotationError::new(
                "credential_expired",
                "new OAuth credential was rejected",
                false,
                None,
            ))
        }
    }

    async fn test_state(tag: &str) -> (AppState, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "kinetix-credential-enrollment-{tag}-{}",
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

    async fn test_state_with_plugins(tag: &str) -> (AppState, std::path::PathBuf) {
        let (state, root) = test_state(tag).await;
        let manager = crate::plugins::PluginManager::new(
            state.pool.clone(),
            state.crypto.clone(),
            crate::plugins::HostPolicy {
                allow_private_network: true,
                ..Default::default()
            },
            state.config.paths.plugin_packages_dir(),
        )
        .unwrap();
        (state.with_plugins(Arc::new(manager)), root)
    }

    async fn install_direct_api_test_plugin(state: &AppState, base_url: &str) {
        let manifest = json!({
            "manifest_version": crate::plugins::MANIFEST_VERSION,
            "id": "plugin.test",
            "name": "Test Plugin",
            "version": "0.1.0",
            "plugin_api": format!("{}.0.0", crate::plugins::PLUGIN_API_MAJOR),
            "integrations": [{
                "id": "direct",
                "name": "Direct Provider",
                "credential_mode": "manual",
                "provider": {
                    "base_url": base_url,
                    "wire_format": "openai",
                    "auth_scheme": "bearer",
                    "pricing_scope": "direct_api"
                }
            }]
        });
        let now = db::now_iso();
        sqlx::query(
            "INSERT OR REPLACE INTO plugins
             (id, version, plugin_api_major, package_sha256, enabled, signature,
              manifest_json, component, installed_at, updated_at)
             VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?, ?)",
        )
        .bind("plugin.test")
        .bind("0.1.0")
        .bind(crate::plugins::PLUGIN_API_MAJOR as i64)
        .bind("test-package")
        .bind("test")
        .bind(manifest.to_string())
        .bind(Vec::<u8>::new())
        .bind(&now)
        .bind(&now)
        .execute(&state.pool)
        .await
        .unwrap();
    }

    fn auth() -> AdminAuth {
        AdminAuth {
            actor: "test".into(),
            token: "test-admin".into(),
        }
    }

    #[tokio::test]
    async fn reasoning_disable_api_validates_value_before_model_lookup() {
        let (state, root) = test_state("reasoning-disable-probe").await;

        let invalid = probe_model_capability(
            State(state.clone()),
            auth(),
            Path("missing-model".into()),
            Json(CapabilityProbeBody {
                account_id: None,
                capability: "reasoning_disable".into(),
                value: Some(json!("low")),
                max_cost_usd: None,
                transport: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(invalid.0, StatusCode::BAD_REQUEST);

        let omitted = probe_model_capability(
            State(state.clone()),
            auth(),
            Path("missing-model".into()),
            Json(CapabilityProbeBody {
                account_id: None,
                capability: "reasoning_disable".into(),
                value: None,
                max_cost_usd: None,
                transport: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(omitted.0, StatusCode::NOT_FOUND);

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    async fn insert_provider(
        state: &AppState,
        name: &str,
        mode: crate::plugins::CredentialMode,
        source_plugin_id: Option<&str>,
        source_integration_id: Option<&str>,
    ) -> String {
        let credential_plugin = match (mode, source_plugin_id) {
            (crate::plugins::CredentialMode::AuthFlow, Some(plugin_id)) => {
                format!("plugin:{plugin_id}/strategy")
            }
            _ => String::new(),
        };
        db::insert_provider(
            &state.pool,
            &db::NewProvider {
                name,
                base_url: "http://127.0.0.1:12345",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: true,
                wire_plugin: "",
                credential_plugin: &credential_plugin,
                model_source_plugin: "",
                credential_mode: mode.as_str(),
                source_plugin_id,
                source_integration_id,
            },
        )
        .await
        .unwrap()
    }

    fn provider_body(name: &str, api_key: Option<&str>) -> ProviderBody {
        ProviderBody {
            name: name.into(),
            base_url: "http://127.0.0.1:12345".into(),
            wire_format: "openai".into(),
            auth_scheme: "bearer".into(),
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: serde_json::Map::new(),
            timeout_ms: 1_000,
            capability_mode: "permissive".into(),
            models_path: None,
            rate_limit_rules: Some(json!({})),
            follow_redirects: false,
            credential_hosts: String::new(),
            allow_insecure_tls: true,
            wire_plugin: String::new(),
            credential_plugin: String::new(),
            model_source_plugin: String::new(),
            pricing_scope: None,
            api_key: api_key.map(str::to_string),
            account_label: Some("manual-key".into()),
        }
    }

    #[tokio::test]
    async fn model_update_keeps_effective_price_when_version_persistence_fails() {
        let (state, root) = test_state("model-price-rollback").await;
        let provider_id = insert_provider(
            &state,
            "pricing-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let old_prices = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            ..Prices::default()
        };
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "priced-model",
                display_name: "Priced Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: serde_json::to_value(&old_prices).unwrap(),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({}),
            },
        )
        .await
        .unwrap();
        let old_version = db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &old_prices,
            "operator",
            &json!({"configured_by": "test"}),
        )
        .await
        .unwrap()
        .unwrap();

        sqlx::query(
            "CREATE TRIGGER reject_model_price_version_insert
             BEFORE INSERT ON price_versions
             BEGIN
                 SELECT RAISE(ABORT, 'injected price version persistence failure');
             END",
        )
        .execute(&state.pool)
        .await
        .unwrap();

        let result = update_model(
            State(state.clone()),
            auth(),
            Path(model_id.clone()),
            Json(ModelBody {
                upstream_id: "priced-model".into(),
                display_name: Some("Changed Model Name".into()),
                enabled: true,
                context_window: Some(200_000),
                max_output_tokens: Some(16_384),
                capabilities: json!({}),
                prices: json!({
                    "input_per_1m": 9.0,
                    "output_per_1m": 18.0
                }),
                parameters: json!({}),
                thinking_map: ThinkingMap::default(),
                extra_request: json!({}),
                discovery: json!({}),
                transport_override: None,
            }),
        )
        .await;

        assert!(result.is_err());
        let row = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.display_name, "Priced Model");
        assert_eq!(row.context_window, None);
        assert_eq!(row.max_output_tokens, None);
        assert_eq!(row.prices().input_per_1m, Some(1.0));
        assert_eq!(row.prices().output_per_1m, Some(2.0));
        let discovery: Value = serde_json::from_str(&row.discovery).unwrap();
        assert_eq!(
            discovery
                .pointer("/effective_pricing/price_version_id")
                .and_then(Value::as_str),
            Some(old_version.as_str())
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn model_creation_rolls_back_when_price_version_persistence_fails() {
        let (state, root) = test_state("model-create-price-rollback").await;
        let provider_id = insert_provider(
            &state,
            "create-pricing-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;

        sqlx::query(
            "CREATE TRIGGER reject_created_model_price_version
             BEFORE INSERT ON price_versions
             BEGIN
                 SELECT RAISE(ABORT, 'injected create price version persistence failure');
             END",
        )
        .execute(&state.pool)
        .await
        .unwrap();

        let result = create_model(
            State(state.clone()),
            auth(),
            Path(provider_id.clone()),
            Json(ModelBody {
                upstream_id: "failed-priced-model".into(),
                display_name: Some("Failed Priced Model".into()),
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: json!({
                    "input_per_1m": 1.0,
                    "output_per_1m": 2.0
                }),
                parameters: json!({}),
                thinking_map: ThinkingMap::default(),
                extra_request: json!({}),
                discovery: json!({}),
                transport_override: None,
            }),
        )
        .await;

        assert!(result.is_err());
        assert!(
            db::find_model_by_upstream(&state.pool, &provider_id, "failed-priced-model")
                .await
                .unwrap()
                .is_none()
        );
        state.registry.reload(&state.pool).await.unwrap();
        assert!(state.registry.resolve("failed-priced-model").is_none());

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn imported_price_provenance_reuses_version_on_unchanged_sync() {
        let (state, root) = test_state("import-price-version-reuse").await;
        let provider_id = insert_provider(
            &state,
            "import-pricing-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let observed_prices = json!({
            "input_per_1m": 1.0,
            "output_per_1m": 5.0
        });
        let catalog_source_state = json!({
            "source": "models.dev",
            "retrieved_at": "2026-09-28T00:00:00Z",
            "freshness": "fresh"
        });

        let Json(created) = create_model(
            State(state.clone()),
            auth(),
            Path(provider_id),
            Json(ModelBody {
                upstream_id: "imported-priced-model".into(),
                display_name: Some("Imported Priced Model".into()),
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: observed_prices.clone(),
                parameters: json!({}),
                thinking_map: ThinkingMap::default(),
                extra_request: json!({}),
                discovery: json!({
                    "prices": observed_prices,
                    "price_sources": {
                        "input_per_1m": "models.dev:provider",
                        "output_per_1m": "models.dev:provider"
                    },
                    "catalog": {
                        "source_state": catalog_source_state
                    },
                    "execution_supported": true,
                    "imported_from_discovery": true
                }),
                transport_override: None,
            }),
        )
        .await
        .unwrap();
        let model_id = created["id"].as_str().unwrap().to_string();

        let imported = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let discovery = discovery_object(&imported);
        let original_version = discovery
            .pointer("/effective_pricing/price_version_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let observation = latest_reconciliation_observation(&discovery);
        let observed: Prices = serde_json::from_value(observation["prices"].clone()).unwrap();
        let (effective, fields, preserved_manual) = merge_automatic_price_observation(
            &imported.prices(),
            &observed,
            observation,
            &discovery,
        );
        assert!(!preserved_manual);
        let source = effective_price_source(&fields, &effective);
        let metadata = json!({
            "fields": fields,
            "catalog_source_state": observation
                .pointer("/catalog/source_state")
                .cloned()
                .unwrap_or(Value::Null),
        });
        let reused_version = db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &effective,
            &source,
            &metadata,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(reused_version, original_version);
        let version_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id = ?")
                .bind(&model_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(version_count, 1);

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn config_import_materializes_operator_model_ownership_and_survives_pricing_sync() {
        let (state, root) = test_state("config-import-model-ownership").await;
        let provider_id = db::insert_provider(
            &state.pool,
            &db::NewProvider {
                name: "config-import-provider",
                base_url: "https://generativelanguage.googleapis.com/v1beta",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: false,
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
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "gemini-3.8-flash",
                display_name: "Gemini 3.8 Flash",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({"tool_calling": false}),
                prices: json!({}),
                parameters: json!({
                    "temperature": {"supported": false}
                }),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({}),
            },
        )
        .await
        .unwrap();
        let automatic_prices = Prices {
            input_per_1m: Some(1.0),
            ..Prices::default()
        };
        db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &automatic_prices,
            "models.dev:provider",
            &json!({
                "fields": {
                    "input_per_1m": {
                        "source": "models.dev:provider",
                        "metadata": {}
                    }
                }
            }),
        )
        .await
        .unwrap();

        import_config(
            State(state.clone()),
            auth(),
            Json(ImportBody {
                config: json!({
                    "models": [{
                        "provider": "config-import-provider",
                        "upstream_id": "gemini-3.8-flash",
                        "display_name": "Gemini 3.8 Flash",
                        "enabled": true,
                        "context_window": 200000,
                        "max_output_tokens": 32000,
                        "capabilities": {
                            "tool_calling": true
                        },
                        "prices": {
                            "input_per_1m": 9.99
                        },
                        "parameters": {
                            "temperature": {"supported": true}
                        },
                        "thinking_map": {
                            "levels": {"high": "high"},
                            "mode": "level",
                            "level_field": "reasoning_effort"
                        },
                        "extra_request": {},
                        "transport_override": null
                    }]
                }),
                apply: true,
            }),
        )
        .await
        .unwrap();

        let imported = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(imported.prices().input_per_1m, Some(9.99));
        let discovery = discovery_object(&imported);
        assert_eq!(
            discovery
                .pointer("/effective_pricing/source")
                .and_then(Value::as_str),
            Some("operator")
        );
        assert_eq!(
            discovery
                .pointer("/effective_pricing/fields/input_per_1m/source")
                .and_then(Value::as_str),
            Some("operator")
        );
        assert_eq!(
            discovery
                .pointer("/effective_pricing/fields/input_per_1m/metadata/configured_by")
                .and_then(Value::as_str),
            Some("config_import")
        );
        assert_eq!(
            discovery
                .pointer("/operator_capability_overrides/tool_calling")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            discovery
                .pointer("/operator_parameter_overrides/temperature")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            discovery
                .pointer("/operator_thinking_overrides/thinking_map/mode")
                .and_then(Value::as_str),
            Some("level")
        );

        let version_id = discovery
            .pointer("/effective_pricing/price_version_id")
            .and_then(Value::as_str)
            .expect("imported operator price must be version-backed");
        let (version_source, version_input): (String, Option<f64>) =
            sqlx::query_as("SELECT source, input_per_1m FROM price_versions WHERE id=?")
                .bind(version_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(version_source, "operator");
        assert_eq!(version_input, Some(9.99));

        let catalog = crate::model_catalog::ModelsDevCatalog::from_parts(
            json!({}),
            json!({
                "google": {
                    "id": "google",
                    "api": "https://generativelanguage.googleapis.com/v1beta",
                    "models": {
                        "gemini-3.8-flash": {
                            "id": "gemini-3.8-flash",
                            "cost": {
                                "input": 0.75,
                                "output": 3.75
                            }
                        }
                    }
                }
            }),
        )
        .unwrap();
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        apply_provider_pricing_sync(&state, &provider, &catalog)
            .await
            .unwrap();

        let after_sync = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_sync.prices().input_per_1m, Some(9.99));
        let after_discovery = discovery_object(&after_sync);
        assert_eq!(
            after_discovery
                .pointer("/effective_pricing/fields/input_per_1m/source")
                .and_then(Value::as_str),
            Some("operator")
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn integration_scope_filters_bundled_catalog_prices() {
        let prices = Prices {
            input_per_1m: Some(1.0),
            ..Prices::default()
        };
        let filtered = automatic_prices_for_provider_scope(
            &prices,
            &json!({
                "price_sources": {
                    "input_per_1m": "bundled_catalog:provider"
                }
            }),
            "integration",
        );
        assert_eq!(filtered.input_per_1m, None);
    }

    #[tokio::test]
    async fn pricing_sync_does_not_materialize_bundled_catalog_prices_for_integration_scope() {
        let (state, root) = test_state("bundled-catalog-sync-scope").await;
        let provider_id = insert_provider(
            &state,
            "bundled-sync-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        db::update_provider_pricing_scope(&state.pool, &provider_id, "integration")
            .await
            .unwrap();
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "bundled-sync-model",
                display_name: "Bundled Sync Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: json!({}),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({
                    "prices": {
                        "input_per_1m": 1.5
                    },
                    "price_sources": {
                        "input_per_1m": "bundled_catalog:provider"
                    }
                }),
            },
        )
        .await
        .unwrap();

        let catalog =
            crate::model_catalog::ModelsDevCatalog::from_parts(json!({}), json!({})).unwrap();
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        apply_provider_pricing_sync(&state, &provider, &catalog)
            .await
            .unwrap();

        let model = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.prices().input_per_1m, None);
        assert!(discovery_object(&model)
            .get("effective_pricing")
            .is_none_or(Value::is_null));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn integration_scope_revokes_existing_bundled_catalog_prices() {
        let (state, root) = test_state("bundled-catalog-price-scope").await;
        let provider_id = insert_provider(
            &state,
            "bundled-catalog-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "bundled-priced-model",
                display_name: "Bundled Priced Model",
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
        let prices = Prices {
            input_per_1m: Some(1.5),
            ..Prices::default()
        };
        db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &prices,
            "bundled_catalog",
            &json!({
                "fields": {
                    "input_per_1m": {
                        "source": "bundled_catalog",
                        "metadata": {}
                    }
                }
            }),
        )
        .await
        .unwrap();

        db::update_provider_pricing_scope(&state.pool, &provider_id, "integration")
            .await
            .unwrap();

        let model = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.prices().input_per_1m, None);
        assert!(discovery_object(&model)
            .get("effective_pricing")
            .is_none_or(Value::is_null));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn discovery_import_keeps_models_dev_prices_observed_for_integration_scope() {
        let (state, root) = test_state("integration-import-price-scope").await;
        let provider_id = insert_provider(
            &state,
            "integration-import-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        db::update_provider_pricing_scope(&state.pool, &provider_id, "integration")
            .await
            .unwrap();

        let observed_prices = json!({
            "input_per_1m": 1.25,
            "output_per_1m": 6.5
        });
        let Json(created) = create_model(
            State(state.clone()),
            auth(),
            Path(provider_id.clone()),
            Json(ModelBody {
                upstream_id: "catalog-only-model".into(),
                display_name: Some("Catalog Only Model".into()),
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: observed_prices.clone(),
                parameters: json!({}),
                thinking_map: ThinkingMap::default(),
                extra_request: json!({}),
                discovery: json!({
                    "prices": observed_prices,
                    "price_sources": {
                        "input_per_1m": "models.dev:provider",
                        "output_per_1m": "models.dev:provider"
                    },
                    "catalog": {
                        "source_state": {
                            "source": "models.dev",
                            "retrieved_at": "2026-09-28T00:00:00Z",
                            "freshness": "fresh"
                        },
                        "provider": {
                            "reference": "models.dev:provider:test/catalog-only-model",
                            "provider_id": "test",
                            "model_id": "catalog-only-model"
                        }
                    },
                    "execution_supported": true,
                    "imported_from_discovery": true
                }),
                transport_override: None,
            }),
        )
        .await
        .unwrap();

        let model_id = created["id"].as_str().unwrap();
        let model = db::get_model(&state.pool, model_id).await.unwrap().unwrap();
        assert_eq!(model.prices().input_per_1m, None);
        assert_eq!(model.prices().output_per_1m, None);
        let discovery = discovery_object(&model);
        assert_eq!(
            discovery
                .pointer("/prices/input_per_1m")
                .and_then(Value::as_f64),
            Some(1.25)
        );
        assert_eq!(
            discovery
                .pointer("/prices/output_per_1m")
                .and_then(Value::as_f64),
            Some(6.5)
        );
        assert!(discovery
            .get("effective_pricing")
            .is_none_or(Value::is_null));
        let version_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id=?")
                .bind(model_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(version_count, 0);

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn direct_provider_base_url_change_revokes_old_catalog_prices() {
        let (state, root) = test_state("provider-base-url-price-revoke").await;
        let provider_id = insert_provider(
            &state,
            "endpoint-price-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "priced-model",
                display_name: "Priced Model",
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
        let prices = Prices {
            input_per_1m: Some(1.0),
            ..Prices::default()
        };
        db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &prices,
            "models.dev:provider",
            &json!({
                "fields": {
                    "input_per_1m": {
                        "source": "models.dev:provider",
                        "metadata": {
                            "catalog_provider": {
                                "reference": "models.dev:provider:old/priced-model",
                                "provider_id": "old",
                                "model_id": "priced-model"
                            }
                        }
                    }
                }
            }),
        )
        .await
        .unwrap();

        let mut body = provider_body("endpoint-price-provider", None);
        body.base_url = "http://127.0.0.1:23456".into();
        update_provider(
            State(state.clone()),
            auth(),
            Path(provider_id.clone()),
            Json(body),
        )
        .await
        .unwrap();

        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(provider.pricing_scope, "direct_api");
        assert_eq!(provider.base_url, "http://127.0.0.1:23456");
        let model = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.prices().input_per_1m, None);
        assert!(discovery_object(&model)
            .get("effective_pricing")
            .is_none_or(Value::is_null));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn scope_transition_waits_for_provider_pricing_lifecycle_lock() {
        let (state, root) = test_state("provider-pricing-scope-race").await;
        let provider_id = insert_provider(
            &state,
            "pricing-race-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "race-model",
                display_name: "Race Model",
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

        let lock = model_reconciliation_lock(&provider_id);
        let guard = lock.lock().await;
        let mut body = provider_body("pricing-race-provider", None);
        body.pricing_scope = Some("integration".into());
        let update_state = state.clone();
        let update_provider_id = provider_id.clone();
        let update = tokio::spawn(async move {
            update_provider(
                State(update_state),
                auth(),
                Path(update_provider_id),
                Json(body),
            )
            .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            db::provider_pricing_scope(&state.pool, &provider_id)
                .await
                .unwrap(),
            "direct_api"
        );

        let old_scope_prices = Prices {
            input_per_1m: Some(2.0),
            ..Prices::default()
        };
        db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &old_scope_prices,
            "models.dev:provider",
            &json!({
                "fields": {
                    "input_per_1m": {
                        "source": "models.dev:provider",
                        "metadata": {}
                    }
                }
            }),
        )
        .await
        .unwrap();

        drop(guard);
        update.await.unwrap().unwrap();

        assert_eq!(
            db::provider_pricing_scope(&state.pool, &provider_id)
                .await
                .unwrap(),
            "integration"
        );
        assert_eq!(
            db::get_model(&state.pool, &model_id)
                .await
                .unwrap()
                .unwrap()
                .prices()
                .input_per_1m,
            None
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn catalog_provider_identity_participates_in_price_snapshot_identity() {
        let (state, root) = test_state("catalog-price-provenance-identity").await;
        let provider_id = insert_provider(
            &state,
            "catalog-provenance-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "shared-model",
                display_name: "Shared Model",
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
        let prices = Prices {
            input_per_1m: Some(0.75),
            output_per_1m: Some(3.75),
            ..Prices::default()
        };
        let observation_a = json!({
            "price_sources": {
                "input_per_1m": "models.dev:provider",
                "output_per_1m": "models.dev:provider"
            },
            "catalog": {
                "source_state": {
                    "source": "models.dev",
                    "retrieved_at": "2026-09-28T00:00:00Z",
                    "freshness": "fresh",
                    "etag": "a"
                },
                "provider": {
                    "reference": "models.dev:provider:provider-a/shared-model",
                    "provider_id": "provider-a",
                    "model_id": "shared-model"
                }
            }
        });
        let (source_a, metadata_a) = automatic_price_provenance(&prices, &observation_a);
        assert_eq!(
            metadata_a
                .pointer("/fields/input_per_1m/metadata/catalog_provider/provider_id")
                .and_then(Value::as_str),
            Some("provider-a")
        );
        let version_a = db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &prices,
            &source_a,
            &metadata_a,
        )
        .await
        .unwrap()
        .unwrap();

        let mut observation_same = observation_a.clone();
        observation_same["catalog"]["source_state"]["retrieved_at"] = json!("2026-09-28T01:00:00Z");
        observation_same["catalog"]["source_state"]["freshness"] = json!("stale");
        observation_same["catalog"]["source_state"]["etag"] = json!("b");
        let (source_same, metadata_same) = automatic_price_provenance(&prices, &observation_same);
        let version_same = db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &prices,
            &source_same,
            &metadata_same,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(version_same, version_a);

        let mut observation_b = observation_a;
        observation_b["catalog"]["provider"] = json!({
            "reference": "models.dev:provider:provider-b/shared-model",
            "provider_id": "provider-b",
            "model_id": "shared-model"
        });
        let (source_b, metadata_b) = automatic_price_provenance(&prices, &observation_b);
        let version_b = db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &prices,
            &source_b,
            &metadata_b,
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(version_b, version_a);

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn provider_plugin_edit_rederives_pricing_scope_conservatively() {
        let (state, root) = test_state("provider-plugin-pricing-scope").await;
        let provider_id = insert_provider(
            &state,
            "scope-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let existing = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(existing.pricing_scope, "direct_api");

        let provider = db::NewProvider {
            name: &existing.name,
            base_url: &existing.base_url,
            wire_format: existing.wire(),
            auth_scheme: existing.auth(),
            custom_header_name: existing.custom_header_name.as_deref(),
            custom_param_name: existing.custom_param_name.as_deref(),
            extra_headers: serde_json::to_value(existing.extra_headers_map()).unwrap(),
            timeout_ms: existing.timeout_ms,
            capability_mode: &existing.capability_mode,
            models_path: existing.models_path.as_deref(),
            rate_limit_rules: serde_json::from_str(&existing.rate_limit_rules).unwrap(),
            follow_redirects: existing.follow_redirects != 0,
            credential_hosts: &existing.credential_hosts,
            allow_insecure_tls: existing.allow_insecure_tls != 0,
            wire_plugin: "",
            credential_plugin: "",
            model_source_plugin: "plugin:plugin.test/models",
            credential_mode: &existing.credential_mode,
            source_plugin_id: existing.source_plugin_id.as_deref(),
            source_integration_id: existing.source_integration_id.as_deref(),
        };
        db::update_provider(&state.pool, &provider_id, &provider, None)
            .await
            .unwrap();

        assert_eq!(
            db::get_provider(&state.pool, &provider_id)
                .await
                .unwrap()
                .unwrap()
                .pricing_scope,
            "integration"
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn explicit_integration_manifest_direct_api_scope_is_preserved() {
        let (state, root) = test_state_with_plugins("manifest-direct-pricing-scope").await;
        let base_url = "https://provider-a.example/v1";
        install_direct_api_test_plugin(&state, base_url).await;
        let provider_id = db::insert_provider(
            &state.pool,
            &db::NewProvider {
                name: "manifest-direct-provider",
                base_url,
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: false,
                wire_plugin: "",
                credential_plugin: "",
                model_source_plugin: "",
                credential_mode: "manual",
                source_plugin_id: Some("plugin.test"),
                source_integration_id: Some("direct"),
            },
        )
        .await
        .unwrap();

        reconcile_provider_integration_semantics(
            &state,
            &provider_id,
            crate::plugins::CredentialMode::Manual,
            "plugin.test",
            "direct",
            crate::plugins::PricingScope::DirectApi,
        )
        .await
        .unwrap();

        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(provider.pricing_scope, "direct_api");

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn scope_transition_revokes_models_dev_effective_pricing_without_sync() {
        let (state, root) = test_state("scope-transition-price-revoke").await;
        let provider_id = db::insert_provider(
            &state.pool,
            &db::NewProvider {
                name: "google-scope-transition",
                base_url: "https://generativelanguage.googleapis.com/v1beta",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: false,
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
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "shared-model",
                display_name: "Shared Model",
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
        let prices = Prices {
            input_per_1m: Some(0.75),
            output_per_1m: Some(9.0),
            ..Prices::default()
        };
        let metadata = json!({
            "fields": {
                "input_per_1m": {
                    "source": "models.dev:provider",
                    "metadata": {}
                },
                "output_per_1m": {
                    "source": "operator",
                    "metadata": {"configured_by": "admin"}
                }
            },
            "catalog_source_state": {"source": "models.dev"}
        });
        let original_version =
            db::commit_effective_model_pricing(&state.pool, &model_id, &prices, "mixed", &metadata)
                .await
                .unwrap()
                .unwrap();

        db::update_provider_pricing_scope(&state.pool, &provider_id, "integration")
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();

        let model = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.prices().input_per_1m, None);
        assert_eq!(model.prices().output_per_1m, Some(9.0));
        let discovery = discovery_object(&model);
        let replacement_version = discovery
            .pointer("/effective_pricing/price_version_id")
            .and_then(Value::as_str)
            .expect("surviving operator price must remain version-backed");
        assert_ne!(replacement_version, original_version);
        assert_eq!(
            discovery
                .pointer("/effective_pricing/source")
                .and_then(Value::as_str),
            Some("operator")
        );
        assert_eq!(
            discovery
                .pointer("/effective_pricing/fields/output_per_1m/source")
                .and_then(Value::as_str),
            Some("operator")
        );
        assert!(discovery
            .pointer("/effective_pricing/fields/input_per_1m")
            .is_none());
        assert!(discovery
            .pointer("/effective_pricing/metadata/catalog_source_state")
            .is_none());
        let version_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id=?")
                .bind(&model_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(version_count, 2);
        assert_eq!(
            state
                .registry
                .snapshot()
                .models
                .get(&model_id)
                .unwrap()
                .prices()
                .input_per_1m,
            None
        );
        assert_eq!(
            state
                .registry
                .snapshot()
                .models
                .get(&model_id)
                .unwrap()
                .prices()
                .output_per_1m,
            Some(9.0)
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn migration_scope_enforcement_revokes_existing_plugin_models_dev_pricing() {
        let (state, root) = test_state("migration-scope-price-revoke").await;
        let provider_id = insert_provider(
            &state,
            "migration-plugin-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "legacy-priced-model",
                display_name: "Legacy Priced Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: json!({"input_per_1m": 1.0}),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({}),
            },
        )
        .await
        .unwrap();
        let prices = Prices {
            input_per_1m: Some(1.0),
            ..Prices::default()
        };
        let metadata = json!({
            "fields": {
                "input_per_1m": {
                    "source": "models.dev:provider",
                    "metadata": {}
                }
            },
            "catalog_source_state": {"source": "models.dev"}
        });
        db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &prices,
            "models.dev:provider",
            &metadata,
        )
        .await
        .unwrap();

        sqlx::query(
            "UPDATE providers
             SET model_source_plugin='plugin:plugin.test/models', pricing_scope='integration'
             WHERE id=?",
        )
        .bind(&provider_id)
        .execute(&state.pool)
        .await
        .unwrap();
        db::enforce_provider_pricing_scopes(&state.pool)
            .await
            .unwrap();

        assert_eq!(
            db::get_model(&state.pool, &model_id)
                .await
                .unwrap()
                .unwrap()
                .prices()
                .input_per_1m,
            None
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn models_dev_direct_pricing_respects_provider_pricing_scope() {
        let (state, root) = test_state("pricing-scope-isolation").await;
        let catalog = crate::model_catalog::ModelsDevCatalog::from_parts(
            json!({}),
            json!({
                "google": {
                    "id": "google",
                    "models": {
                        "shared-model": {
                            "id": "shared-model",
                            "cost": {"input": 0.75, "output": 3.75}
                        }
                    }
                }
            }),
        )
        .unwrap();

        async fn insert_google_provider(
            state: &AppState,
            name: &str,
            credential_mode: &str,
            source_plugin_id: Option<&str>,
            source_integration_id: Option<&str>,
            credential_plugin: &str,
        ) -> (String, db::ProviderRow, String) {
            let provider_id = db::insert_provider(
                &state.pool,
                &db::NewProvider {
                    name,
                    base_url: "https://generativelanguage.googleapis.com/v1beta",
                    wire_format: WireFormat::Openai,
                    auth_scheme: AuthScheme::Bearer,
                    custom_header_name: None,
                    custom_param_name: None,
                    extra_headers: json!({}),
                    timeout_ms: 1_000,
                    capability_mode: "permissive",
                    models_path: None,
                    rate_limit_rules: json!({}),
                    follow_redirects: false,
                    credential_hosts: "",
                    allow_insecure_tls: false,
                    wire_plugin: "",
                    credential_plugin,
                    model_source_plugin: "",
                    credential_mode,
                    source_plugin_id,
                    source_integration_id,
                },
            )
            .await
            .unwrap();
            let model_id = db::insert_model(
                &state.pool,
                &db::NewModel {
                    provider_id: &provider_id,
                    upstream_id: "shared-model",
                    display_name: "Shared Model",
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
            let provider = db::get_provider(&state.pool, &provider_id)
                .await
                .unwrap()
                .unwrap();
            (provider_id, provider, model_id)
        }

        let (direct_provider_id, direct_provider, direct_model_id) =
            insert_google_provider(&state, "google-direct", "manual", None, None, "").await;
        assert_eq!(
            db::provider_pricing_scope(&state.pool, &direct_provider_id)
                .await
                .unwrap(),
            "direct_api"
        );
        apply_provider_pricing_sync(&state, &direct_provider, &catalog)
            .await
            .unwrap();
        let direct_model = db::get_model(&state.pool, &direct_model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(direct_model.prices().input_per_1m, Some(0.75));

        let (plugin_provider_id, plugin_provider, plugin_model_id) = insert_google_provider(
            &state,
            "google-oauth-plugin",
            "auth_flow",
            Some("plugin.google-oauth"),
            Some("oauth"),
            "plugin:plugin.google-oauth/strategy",
        )
        .await;
        assert_eq!(
            db::provider_pricing_scope(&state.pool, &plugin_provider_id)
                .await
                .unwrap(),
            "integration"
        );
        apply_provider_pricing_sync(&state, &plugin_provider, &catalog)
            .await
            .unwrap();
        let plugin_model = db::get_model(&state.pool, &plugin_model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(plugin_model.prices().input_per_1m, None);
        assert_eq!(plugin_model.prices().output_per_1m, None);
        let plugin_versions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id=?")
                .bind(&plugin_model_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(plugin_versions, 0);

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn operator_pricing_sync_reuses_version_when_catalog_does_not_contribute() {
        let (state, root) = test_state("operator-sync-version-reuse").await;
        let provider_id = db::insert_provider(
            &state.pool,
            &db::NewProvider {
                name: "google-operator-pricing",
                base_url: "https://generativelanguage.googleapis.com/v1beta",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: false,
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
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();

        let Json(created) = create_model(
            State(state.clone()),
            auth(),
            Path(provider_id),
            Json(ModelBody {
                upstream_id: "shared-model".into(),
                display_name: Some("Operator Priced Model".into()),
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: json!({
                    "input_per_1m": 1.0,
                    "output_per_1m": 2.0
                }),
                parameters: json!({}),
                thinking_map: ThinkingMap::default(),
                extra_request: json!({}),
                discovery: json!({}),
                transport_override: None,
            }),
        )
        .await
        .unwrap();
        let model_id = created["id"].as_str().unwrap().to_string();
        let before = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let before_discovery = discovery_object(&before);
        let version_id = before_discovery
            .pointer("/effective_pricing/price_version_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        let catalog = crate::model_catalog::ModelsDevCatalog::from_parts(
            json!({}),
            json!({
                "google": {
                    "id": "google",
                    "models": {
                        "shared-model": {
                            "id": "shared-model",
                            "cost": {"input": 0.75, "output": 3.75}
                        }
                    }
                }
            }),
        )
        .unwrap();
        apply_provider_pricing_sync(&state, &provider, &catalog)
            .await
            .unwrap();

        let after = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.prices().input_per_1m, Some(1.0));
        assert_eq!(after.prices().output_per_1m, Some(2.0));
        let after_discovery = discovery_object(&after);
        assert_eq!(
            after_discovery
                .pointer("/effective_pricing/price_version_id")
                .and_then(Value::as_str),
            Some(version_id.as_str())
        );
        let version_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id=?")
                .bind(&model_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(version_count, 1);

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn provider_pricing_batch_rolls_back_all_models_on_version_failure() {
        let (state, root) = test_state("provider-pricing-batch-rollback").await;
        let provider_id = db::insert_provider(
            &state.pool,
            &db::NewProvider {
                name: "google-pricing-batch",
                base_url: "https://generativelanguage.googleapis.com/v1beta",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: false,
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
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();

        async fn insert_priced_model(
            state: &AppState,
            provider_id: &str,
            upstream_id: &str,
            price: f64,
        ) -> (String, String) {
            let prices = Prices {
                input_per_1m: Some(price),
                ..Prices::default()
            };
            let model_id = db::insert_model(
                &state.pool,
                &db::NewModel {
                    provider_id,
                    upstream_id,
                    display_name: upstream_id,
                    enabled: true,
                    context_window: None,
                    max_output_tokens: None,
                    capabilities: json!({}),
                    prices: serde_json::to_value(&prices).unwrap(),
                    parameters: json!({}),
                    thinking_map: json!({}),
                    extra_request: json!({}),
                    discovery: json!({
                        "latest_observation": {
                            "marker": "before",
                            "prices": {"input_per_1m": price},
                            "price_sources": {"input_per_1m": "models.dev:provider"}
                        }
                    }),
                },
            )
            .await
            .unwrap();
            let metadata = json!({
                "fields": {
                    "input_per_1m": {
                        "source": "models.dev:provider",
                        "metadata": {}
                    }
                }
            });
            let version_id = db::commit_effective_model_pricing(
                &state.pool,
                &model_id,
                &prices,
                "models.dev:provider",
                &metadata,
            )
            .await
            .unwrap()
            .unwrap();
            (model_id, version_id)
        }

        let (model_a, version_a) = insert_priced_model(&state, &provider_id, "model-a", 1.0).await;
        let (model_b, version_b) = insert_priced_model(&state, &provider_id, "model-b", 2.0).await;
        state.registry.reload(&state.pool).await.unwrap();

        sqlx::query(&format!(
            "CREATE TRIGGER reject_model_b_pricing_batch
             BEFORE INSERT ON price_versions
             WHEN NEW.model_id = '{}'
             BEGIN
                 SELECT RAISE(ABORT, 'injected provider pricing batch failure');
             END",
            model_b
        ))
        .execute(&state.pool)
        .await
        .unwrap();

        let catalog = crate::model_catalog::ModelsDevCatalog::from_parts(
            json!({}),
            json!({
                "google": {
                    "id": "google",
                    "models": {
                        "model-a": {
                            "id": "model-a",
                            "cost": {"input": 1.5}
                        },
                        "model-b": {
                            "id": "model-b",
                            "cost": {"input": 2.5}
                        }
                    }
                }
            }),
        )
        .unwrap();

        let result = apply_provider_pricing_sync(&state, &provider, &catalog).await;
        assert!(result.is_err());

        state.registry.reload(&state.pool).await.unwrap();
        for (model_id, expected_price, expected_version) in
            [(&model_a, 1.0, &version_a), (&model_b, 2.0, &version_b)]
        {
            let row = db::get_model(&state.pool, model_id).await.unwrap().unwrap();
            assert_eq!(row.prices().input_per_1m, Some(expected_price));
            let discovery = discovery_object(&row);
            assert_eq!(
                discovery
                    .pointer("/latest_observation/marker")
                    .and_then(Value::as_str),
                Some("before")
            );
            assert_eq!(
                discovery
                    .pointer("/effective_pricing/price_version_id")
                    .and_then(Value::as_str),
                Some(expected_version.as_str())
            );

            let snapshot = state.registry.snapshot();
            let runtime = snapshot.models.get(model_id).unwrap();
            assert_eq!(runtime.prices().input_per_1m, Some(expected_price));

            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id = ?")
                    .bind(model_id)
                    .fetch_one(&state.pool)
                    .await
                    .unwrap();
            assert_eq!(count, 1);
        }

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn manual_creation_reuses_price_version_on_unrelated_edit() {
        let (state, root) = test_state("manual-create-price-version-reuse").await;
        let provider_id = insert_provider(
            &state,
            "manual-price-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;

        let Json(created) = create_model(
            State(state.clone()),
            auth(),
            Path(provider_id),
            Json(ModelBody {
                upstream_id: "manual-priced-model".into(),
                display_name: Some("Manual Priced Model".into()),
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: json!({
                    "input_per_1m": 1.0,
                    "output_per_1m": 2.0
                }),
                parameters: json!({}),
                thinking_map: ThinkingMap::default(),
                extra_request: json!({}),
                discovery: json!({}),
                transport_override: None,
            }),
        )
        .await
        .unwrap();
        let model_id = created["id"].as_str().unwrap().to_string();
        let created_row = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let created_discovery = discovery_object(&created_row);
        let original_version = created_discovery
            .pointer("/effective_pricing/price_version_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        update_model(
            State(state.clone()),
            auth(),
            Path(model_id.clone()),
            Json(ModelBody {
                upstream_id: created_row.upstream_id.clone(),
                display_name: Some("Renamed Manual Priced Model".into()),
                enabled: created_row.enabled != 0,
                context_window: created_row.context_window,
                max_output_tokens: created_row.max_output_tokens,
                capabilities: serde_json::from_str(&created_row.capabilities).unwrap(),
                prices: serde_json::from_str(&created_row.prices).unwrap(),
                parameters: serde_json::from_str(&created_row.parameters).unwrap(),
                thinking_map: created_row.thinking(),
                extra_request: created_row.extra_request_value(),
                discovery: created_discovery,
                transport_override: None,
            }),
        )
        .await
        .unwrap();

        let updated = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let updated_discovery = discovery_object(&updated);
        assert_eq!(
            updated_discovery
                .pointer("/effective_pricing/price_version_id")
                .and_then(Value::as_str),
            Some(original_version.as_str())
        );
        let version_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id = ?")
                .bind(&model_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(version_count, 1);

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn imported_thinking_map_is_unowned_until_operator_changes_it() {
        let (state, root) = test_state("imported-thinking-ownership").await;
        let provider_id = insert_provider(
            &state,
            "imported-thinking-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let discovered_thinking = ThinkingMap {
            levels: [
                ("low".to_string(), json!("low")),
                ("high".to_string(), json!("high")),
                ("max".to_string(), json!("max")),
            ]
            .into_iter()
            .collect(),
            mode: Some(crate::types::ThinkingMode::Level),
            budget_field: None,
            level_field: Some("reasoning_effort".into()),
        };

        let Json(created) = create_model(
            State(state.clone()),
            auth(),
            Path(provider_id.clone()),
            Json(ModelBody {
                upstream_id: "imported-thinking-model".into(),
                display_name: Some("Imported Thinking Model".into()),
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({"reasoning": true}),
                prices: json!({}),
                parameters: json!({}),
                thinking_map: discovered_thinking.clone(),
                extra_request: json!({}),
                discovery: json!({
                    "thinking_map": serde_json::to_value(&discovered_thinking).unwrap(),
                    "reasoning_capability": {
                        "mode": "level",
                        "levels": ["low", "high", "max"],
                        "can_disable": false,
                        "upstream_format": "openai_effort"
                    },
                    "execution_supported": true,
                    "imported_from_discovery": true
                }),
                transport_override: None,
            }),
        )
        .await
        .unwrap();
        let model_id = created["id"].as_str().unwrap().to_string();

        let imported = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let imported_discovery = discovery_object(&imported);
        assert_eq!(
            imported_discovery
                .get("operator_thinking_overrides")
                .and_then(Value::as_object)
                .map(|value| value.len()),
            Some(0)
        );

        let mut configured_thinking = discovered_thinking.clone();
        configured_thinking
            .levels
            .insert("max".into(), json!("vendor-max"));
        update_model(
            State(state.clone()),
            auth(),
            Path(model_id.clone()),
            Json(ModelBody {
                upstream_id: imported.upstream_id.clone(),
                display_name: Some(imported.display_name.clone()),
                enabled: imported.enabled != 0,
                context_window: imported.context_window,
                max_output_tokens: imported.max_output_tokens,
                capabilities: serde_json::from_str(&imported.capabilities).unwrap(),
                prices: serde_json::from_str(&imported.prices).unwrap(),
                parameters: serde_json::from_str(&imported.parameters).unwrap(),
                thinking_map: configured_thinking,
                extra_request: imported.extra_request_value(),
                discovery: imported_discovery,
                transport_override: None,
            }),
        )
        .await
        .unwrap();

        let configured = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let configured_discovery = discovery_object(&configured);
        assert_eq!(
            configured_discovery.pointer("/operator_thinking_overrides/thinking_map/levels/max"),
            Some(&json!("vendor-max"))
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pin_action_promotes_capability_and_price_ownership() {
        let (state, root) = test_state("pin-ownership").await;
        let provider_id = insert_provider(
            &state,
            "pin-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let prices = Prices {
            output_per_1m: Some(5.0),
            ..Prices::default()
        };
        let reconciliation = json!({
            "status": "changed",
            "diff": [
                {
                    "field": "capabilities.tool_calling",
                    "configured": false,
                    "observed": true
                },
                {
                    "field": "prices.output_per_1m",
                    "configured": 5.0,
                    "observed": 6.0
                }
            ]
        });
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "pinned-model",
                display_name: "Pinned Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({"tool_calling": false}),
                prices: serde_json::to_value(&prices).unwrap(),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({
                    "operator_capability_overrides": {},
                    "latest_observation": {
                        "capabilities": {"tool_calling": true},
                        "prices": {"output_per_1m": 6.0},
                        "price_sources": {"output_per_1m": "models.dev:provider"}
                    },
                    "reconciliation": reconciliation,
                    "effective_pricing": {
                        "source": "models.dev:provider",
                        "fields": {
                            "output_per_1m": {
                                "source": "models.dev:provider",
                                "metadata": {}
                            }
                        }
                    }
                }),
            },
        )
        .await
        .unwrap();

        let _ = update_model_reconciliation(
            State(state.clone()),
            auth(),
            Path(model_id.clone()),
            Json(ReconciliationActionBody {
                action: "pin".into(),
                fields: vec![
                    "capabilities.tool_calling".into(),
                    "prices.output_per_1m".into(),
                ],
            }),
        )
        .await
        .unwrap();

        let pinned = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let pinned_discovery = discovery_object(&pinned);
        assert_eq!(
            pinned_discovery
                .pointer("/operator_capability_overrides/tool_calling")
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            pinned_discovery
                .pointer("/effective_pricing/fields/output_per_1m/source")
                .and_then(Value::as_str),
            Some("operator_pin")
        );

        db::merge_model_discovery(
            &state.pool,
            &model_id,
            &json!({
                "probe_evidence": {
                    "tool_calling": {
                        "status": "supported",
                        "fresh_until": "2999-01-01T00:00:00Z",
                        "scope": {
                            "provider_id": provider_id.clone(),
                            "account_id": "account-a",
                            "model_id": model_id.clone(),
                            "transport": "openai"
                        }
                    }
                }
            }),
        )
        .await
        .unwrap();
        let pinned = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        let profile = crate::adapters::resolve_execution_profile_for_target(
            &provider,
            &pinned,
            Some("account-a"),
        )
        .unwrap();
        assert_eq!(profile.capabilities.tool_calling, Some(false));

        let pinned_discovery = discovery_object(&pinned);
        let observation = latest_reconciliation_observation(&pinned_discovery);
        let observed = Prices {
            output_per_1m: Some(6.0),
            ..Prices::default()
        };
        let (effective, fields, preserved_manual) = merge_automatic_price_observation(
            &pinned.prices(),
            &observed,
            observation,
            &pinned_discovery,
        );
        assert_eq!(effective.output_per_1m, Some(5.0));
        assert!(preserved_manual);
        assert_eq!(fields["output_per_1m"]["source"], "operator_pin");

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pin_action_materializes_unknown_capability_ownership() {
        let (state, root) = test_state("pin-unknown-ownership").await;
        let provider_id = insert_provider(
            &state,
            "pin-unknown-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "pin-unknown-model",
                display_name: "Pin Unknown Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: json!({}),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({
                    "operator_capability_overrides": {},
                    "latest_observation": {
                        "capabilities": {"tool_calling": true}
                    },
                    "reconciliation": {
                        "status": "changed",
                        "diff": [{
                            "field": "capabilities.tool_calling",
                            "configured": null,
                            "observed": true
                        }]
                    }
                }),
            },
        )
        .await
        .unwrap();

        let _ = update_model_reconciliation(
            State(state.clone()),
            auth(),
            Path(model_id.clone()),
            Json(ReconciliationActionBody {
                action: "pin".into(),
                fields: vec!["capabilities.tool_calling".into()],
            }),
        )
        .await
        .unwrap();

        let pinned = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let pinned_discovery = discovery_object(&pinned);
        assert!(pinned_discovery
            .pointer("/operator_capability_overrides/tool_calling")
            .is_some_and(Value::is_null));

        db::merge_model_discovery(
            &state.pool,
            &model_id,
            &json!({
                "probe_evidence": {
                    "tool_calling": {
                        "status": "supported",
                        "fresh_until": "2999-01-01T00:00:00Z",
                        "scope": {
                            "provider_id": provider_id.clone(),
                            "account_id": "account-a",
                            "model_id": model_id.clone(),
                            "transport": "openai"
                        }
                    }
                }
            }),
        )
        .await
        .unwrap();

        let pinned = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        let profile = crate::adapters::resolve_execution_profile_for_target(
            &provider,
            &pinned,
            Some("account-a"),
        )
        .unwrap();
        assert_eq!(profile.capabilities.tool_calling, None);

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn accept_reasoning_capability_owns_runtime_against_disable_probe() {
        let (state, root) = test_state("accept-reasoning-ownership").await;
        let provider_id = insert_provider(
            &state,
            "accept-reasoning-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let observed_reasoning = json!({
            "mode": "level",
            "levels": ["low", "high"],
            "can_disable": true,
            "upstream_format": "openai_effort"
        });
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "accept-reasoning-model",
                display_name: "Accept Reasoning Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({"reasoning": true}),
                prices: json!({}),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({
                    "latest_observation": {
                        "reasoning_capability": observed_reasoning.clone()
                    },
                    "reconciliation": {
                        "status": "changed",
                        "diff": [{
                            "field": "reasoning_capability",
                            "configured": null,
                            "observed": observed_reasoning.clone()
                        }]
                    }
                }),
            },
        )
        .await
        .unwrap();

        update_model_reconciliation(
            State(state.clone()),
            auth(),
            Path(model_id.clone()),
            Json(ReconciliationActionBody {
                action: "accept".into(),
                fields: vec!["reasoning_capability".into()],
            }),
        )
        .await
        .unwrap();

        let accepted = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let accepted_discovery = discovery_object(&accepted);
        assert_eq!(
            accepted_discovery
                .pointer("/operator_reasoning_overrides/reasoning_capability/can_disable")
                .and_then(Value::as_bool),
            Some(true)
        );

        db::merge_model_discovery(
            &state.pool,
            &model_id,
            &json!({
                "probe_evidence": {
                    "reasoning_disable": {
                        "status": "unsupported",
                        "fresh_until": "2999-01-01T00:00:00Z",
                        "scope": {
                            "provider_id": provider_id.clone(),
                            "account_id": "account-a",
                            "model_id": model_id.clone(),
                            "transport": "openai"
                        }
                    }
                }
            }),
        )
        .await
        .unwrap();

        let accepted = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        let profile = crate::adapters::resolve_execution_profile_for_target(
            &provider,
            &accepted,
            Some("account-a"),
        )
        .unwrap();
        assert!(profile.reasoning.as_ref().unwrap().can_disable);
        assert!(profile.thinking_map.level_is_executable("off"));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pin_reasoning_capability_owns_runtime_against_effort_and_disable_probes() {
        let (state, root) = test_state("pin-reasoning-ownership").await;
        let provider_id = insert_provider(
            &state,
            "pin-reasoning-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let configured_reasoning = json!({
            "mode": "level",
            "levels": ["low", "high"],
            "can_disable": true,
            "upstream_format": "openai_effort"
        });
        let observed_reasoning = json!({
            "mode": "level",
            "levels": ["low"],
            "can_disable": false,
            "upstream_format": "openai_effort"
        });
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "pin-reasoning-model",
                display_name: "Pin Reasoning Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({"reasoning": true}),
                prices: json!({}),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({
                    "reasoning_capability": configured_reasoning.clone(),
                    "latest_observation": {
                        "reasoning_capability": observed_reasoning.clone()
                    },
                    "reconciliation": {
                        "status": "changed",
                        "diff": [{
                            "field": "reasoning_capability",
                            "configured": configured_reasoning.clone(),
                            "observed": observed_reasoning
                        }]
                    }
                }),
            },
        )
        .await
        .unwrap();

        update_model_reconciliation(
            State(state.clone()),
            auth(),
            Path(model_id.clone()),
            Json(ReconciliationActionBody {
                action: "pin".into(),
                fields: vec!["reasoning_capability".into()],
            }),
        )
        .await
        .unwrap();

        let pinned = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let pinned_discovery = discovery_object(&pinned);
        assert_eq!(
            pinned_discovery
                .pointer("/operator_reasoning_overrides/reasoning_capability/can_disable")
                .and_then(Value::as_bool),
            Some(true)
        );

        db::merge_model_discovery(
            &state.pool,
            &model_id,
            &json!({
                "probe_evidence": {
                    "reasoning_disable": {
                        "status": "unsupported",
                        "fresh_until": "2999-01-01T00:00:00Z",
                        "scope": {
                            "provider_id": provider_id.clone(),
                            "account_id": "account-a",
                            "model_id": model_id.clone(),
                            "transport": "openai"
                        }
                    },
                    "reasoning_effort_high": {
                        "status": "unsupported",
                        "fresh_until": "2999-01-01T00:00:00Z",
                        "scope": {
                            "provider_id": provider_id.clone(),
                            "account_id": "account-a",
                            "model_id": model_id.clone(),
                            "transport": "openai"
                        }
                    }
                }
            }),
        )
        .await
        .unwrap();

        let pinned = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        let profile = crate::adapters::resolve_execution_profile_for_target(
            &provider,
            &pinned,
            Some("account-a"),
        )
        .unwrap();
        let reasoning = profile.reasoning.as_ref().unwrap();
        assert!(reasoning.can_disable);
        assert!(reasoning.levels.iter().any(|level| level == "high"));
        assert!(profile.thinking_map.level_is_executable("off"));
        assert!(profile.thinking_map.level_is_executable("high"));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reconciliation_accept_rolls_back_context_price_and_decision_on_price_failure() {
        let (state, root) = test_state("accept-rollback").await;
        let provider_id = insert_provider(
            &state,
            "accept-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let old_prices = Prices {
            output_per_1m: Some(5.0),
            ..Prices::default()
        };
        let original_reconciliation = json!({
            "status": "changed",
            "diff": [
                {
                    "field": "context_window",
                    "configured": 100000,
                    "observed": 200000
                },
                {
                    "field": "prices.output_per_1m",
                    "configured": 5.0,
                    "observed": 6.0
                }
            ]
        });
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "accept-model",
                display_name: "Accept Model",
                enabled: true,
                context_window: Some(100_000),
                max_output_tokens: None,
                capabilities: json!({}),
                prices: serde_json::to_value(&old_prices).unwrap(),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({
                    "latest_observation": {
                        "context_window": 200000,
                        "prices": {"output_per_1m": 6.0},
                        "price_sources": {"output_per_1m": "models.dev:provider"}
                    },
                    "reconciliation": original_reconciliation.clone()
                }),
            },
        )
        .await
        .unwrap();
        let old_version = db::commit_effective_model_pricing(
            &state.pool,
            &model_id,
            &old_prices,
            "operator",
            &json!({
                "fields": {
                    "output_per_1m": {
                        "source": "operator",
                        "metadata": {}
                    }
                }
            }),
        )
        .await
        .unwrap()
        .unwrap();

        sqlx::query(
            "CREATE TRIGGER reject_accept_price_version_insert
             BEFORE INSERT ON price_versions
             BEGIN
                 SELECT RAISE(ABORT, 'injected accept price version failure');
             END",
        )
        .execute(&state.pool)
        .await
        .unwrap();

        let result = update_model_reconciliation(
            State(state.clone()),
            auth(),
            Path(model_id.clone()),
            Json(ReconciliationActionBody {
                action: "accept".into(),
                fields: vec!["context_window".into(), "prices.output_per_1m".into()],
            }),
        )
        .await;
        assert!(result.is_err());

        let row = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.context_window, Some(100_000));
        assert_eq!(row.prices().output_per_1m, Some(5.0));
        let discovery = discovery_object(&row);
        assert_eq!(
            discovery.get("reconciliation"),
            Some(&original_reconciliation)
        );
        assert_eq!(
            discovery
                .pointer("/effective_pricing/price_version_id")
                .and_then(Value::as_str),
            Some(old_version.as_str())
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn candidate_transport_probe_model_is_ephemeral() {
        let (state, root) = test_state("candidate-transport").await;
        let provider_id = insert_provider(
            &state,
            "transport-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "transport-model",
                display_name: "Transport Model",
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
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        let stored = db::get_model(&state.pool, &model_id)
            .await
            .unwrap()
            .unwrap();

        let candidate =
            probe_execution_model(&provider, &stored, Some("openai-responses")).unwrap();
        assert!(discovery_object(&stored)
            .get("configured_transport")
            .is_none());
        assert_eq!(
            discovery_object(&candidate)
                .get("configured_transport")
                .and_then(Value::as_str),
            Some("openai-responses")
        );
        assert_eq!(
            crate::adapters::resolve_execution_profile(&provider, &candidate)
                .unwrap()
                .transport,
            crate::adapters::TargetTransport::OpenAiResponses
        );
        assert!(discovery_object(
            &db::get_model(&state.pool, &model_id)
                .await
                .unwrap()
                .unwrap()
        )
        .get("configured_transport")
        .is_none());

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn completed_oauth_with_expired_credential_disables_account_and_requires_reauthorization()
    {
        let (state, root) = test_state("expired-completion").await;
        let provider_id = insert_provider(
            &state,
            "expired-oauth",
            crate::plugins::CredentialMode::AuthFlow,
            Some("plugin.test"),
            Some("oauth"),
        )
        .await;
        state.register_plugin_credential_strategy("plugin.test", Arc::new(ExpiredCredential));
        let encrypted = state.crypto.encrypt("{}").unwrap();
        let account_id = db::insert_account(
            &state.pool,
            &provider_id,
            "connected-account",
            &encrypted,
            "oauth:****",
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();

        let result = finalize_plugin_auth_account(
            &state,
            &provider,
            &account_id,
            "connected-account",
            "plugin.test",
            "oauth",
        )
        .await;

        assert_eq!(result.unwrap(), "reauthorization_required");
        assert_eq!(
            db::get_account(&state.pool, &account_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "disabled"
        );
        let audit = db::recent_audit(&state.pool, 10).await.unwrap();
        assert!(audit.iter().any(|entry| {
            entry.action == "plugin_auth_credential_invalid" && entry.target_id == account_id
        }));
        assert!(!audit.iter().any(|entry| {
            entry.action == "plugin_account_authorized" && entry.target_id == account_id
        }));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oauth_completion_propagates_credential_disable_persistence_failure() {
        let (state, root) = test_state("disable-write-failure").await;
        let provider_id = insert_provider(
            &state,
            "disable-write-failure",
            crate::plugins::CredentialMode::AuthFlow,
            Some("plugin.test"),
            Some("oauth"),
        )
        .await;
        let encrypted = state.crypto.encrypt("{}").unwrap();
        let account_id = db::insert_account(
            &state.pool,
            &provider_id,
            "account",
            &encrypted,
            "oauth:****",
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        state.register_plugin_credential_strategy("plugin.test", Arc::new(ExpiredCredential));
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_account_status_update BEFORE UPDATE OF status ON accounts \
             BEGIN SELECT RAISE(ABORT, 'injected status persistence failure'); END",
        )
        .execute(&state.pool)
        .await
        .unwrap();

        let result = finalize_plugin_auth_account(
            &state,
            &provider,
            &account_id,
            "account",
            "plugin.test",
            "oauth",
        )
        .await;

        assert!(
            result.is_err(),
            "OAuth completion must not report reauthorization without disabling"
        );
        assert_eq!(
            db::get_account(&state.pool, &account_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "healthy"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn provider_update_rejects_manual_key_for_non_manual_enrollment() {
        let (state, root) = test_state("provider-update").await;

        for (name, mode) in [
            ("oauth", crate::plugins::CredentialMode::AuthFlow),
            ("public", crate::plugins::CredentialMode::None),
        ] {
            let id = insert_provider(&state, name, mode, Some("plugin.test"), Some(name)).await;
            let error = update_provider(
                State(state.clone()),
                auth(),
                Path(id.clone()),
                Json(provider_body(name, Some("manual-secret"))),
            )
            .await
            .unwrap_err();

            assert_eq!(error.0, StatusCode::BAD_REQUEST);
            assert!(db::accounts_for_provider(&state.pool, &id)
                .await
                .unwrap()
                .is_empty());
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reconciliation_removes_accounts_invalid_for_the_new_mode() {
        let (state, root) = test_state("transitions").await;
        let provider_id = insert_provider(
            &state,
            "transition-provider",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;

        let encrypted = state.crypto.encrypt("manual-secret").unwrap();
        db::insert_account(
            &state.pool,
            &provider_id,
            "manual",
            &encrypted,
            &crate::crypto::mask_secret("manual-secret"),
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();

        reconcile_provider_credential_semantics(
            &state,
            &provider_id,
            crate::plugins::CredentialMode::None,
            "plugin.test",
            "public",
        )
        .await
        .unwrap();
        state.registry.reload(&state.pool).await.unwrap();

        let accounts = db::accounts_for_provider(&state.pool, &provider_id)
            .await
            .unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].label, "__kinetix_noauth__");
        assert_eq!(state.crypto.decrypt(&accounts[0].secret_enc).unwrap(), "");
        assert_eq!(
            state
                .registry
                .snapshot()
                .accounts
                .values()
                .filter(|account| account.provider_id == provider_id)
                .count(),
            1
        );

        // Runtime selection must fail closed even if stale real credentials are
        // inserted outside the reconciliation path.
        let stale = state.crypto.encrypt("should-not-route").unwrap();
        let stale_id = db::insert_account(
            &state.pool,
            &provider_id,
            "stale-real",
            &stale,
            &crate::crypto::mask_secret("should-not-route"),
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        let runtime_accounts: Vec<_> = state
            .registry
            .snapshot()
            .accounts
            .values()
            .filter(|account| account.provider_id == provider_id)
            .map(|account| account.label.clone())
            .collect();
        assert_eq!(runtime_accounts, vec!["__kinetix_noauth__".to_string()]);
        db::delete_account(&state.pool, &stale_id).await.unwrap();

        reconcile_provider_credential_semantics(
            &state,
            &provider_id,
            crate::plugins::CredentialMode::Manual,
            "plugin.test",
            "manual",
        )
        .await
        .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        assert!(db::accounts_for_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .is_empty());
        assert!(state
            .registry
            .snapshot()
            .accounts
            .values()
            .all(|account| account.provider_id != provider_id));

        let encrypted = state.crypto.encrypt("stale-secret").unwrap();
        db::insert_account(
            &state.pool,
            &provider_id,
            "stale",
            &encrypted,
            &crate::crypto::mask_secret("stale-secret"),
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        reconcile_provider_credential_semantics(
            &state,
            &provider_id,
            crate::plugins::CredentialMode::None,
            "plugin.test",
            "public",
        )
        .await
        .unwrap();
        reconcile_provider_credential_semantics(
            &state,
            &provider_id,
            crate::plugins::CredentialMode::AuthFlow,
            "plugin.test",
            "oauth",
        )
        .await
        .unwrap();
        state.registry.reload(&state.pool).await.unwrap();

        assert!(db::accounts_for_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .is_empty());
        assert!(state
            .registry
            .snapshot()
            .accounts
            .values()
            .all(|account| account.provider_id != provider_id));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn config_import_rejects_malformed_credential_semantics_before_apply() {
        let (state, root) = test_state("invalid-import").await;

        let base_provider = json!({
            "name": "invalid-provider",
            "base_url": "http://127.0.0.1:12345",
            "wire_format": "openai",
            "auth_scheme": "bearer",
            "extra_headers": {},
            "rate_limit_rules": {},
            "credential_plugin": "",
            "wire_plugin": "",
            "model_source_plugin": ""
        });

        let mut none_provider = base_provider.clone();
        none_provider["credential_mode"] = json!("none");
        none_provider["credential_plugin"] = json!("plugin:plugin.test/strategy");
        let none_error = import_config(
            State(state.clone()),
            auth(),
            Json(ImportBody {
                config: json!({"providers": [none_provider]}),
                apply: true,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(none_error.0, StatusCode::BAD_REQUEST);
        assert!(none_error
            .1
            .contains("credential_mode 'none' may not declare credential_plugin"));
        assert!(db::list_providers(&state.pool).await.unwrap().is_empty());

        let mut auth_flow_provider = base_provider.clone();
        auth_flow_provider["credential_mode"] = json!("auth_flow");
        auth_flow_provider["credential_plugin"] = json!("plugin:wrong.plugin/strategy");
        auth_flow_provider["source_plugin_id"] = json!("plugin.test");
        auth_flow_provider["source_integration_id"] = json!("oauth");
        let auth_flow_error = import_config(
            State(state.clone()),
            auth(),
            Json(ImportBody {
                config: json!({"providers": [auth_flow_provider]}),
                apply: true,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(auth_flow_error.0, StatusCode::BAD_REQUEST);
        assert!(auth_flow_error
            .1
            .contains("credential_plugin does not match source_plugin_id"));
        assert!(db::list_providers(&state.pool).await.unwrap().is_empty());

        let mut missing_provenance = base_provider;
        missing_provenance["credential_mode"] = json!("auth_flow");
        missing_provenance["credential_plugin"] = json!("plugin:plugin.test/strategy");
        let missing_error = import_config(
            State(state.clone()),
            auth(),
            Json(ImportBody {
                config: json!({"providers": [missing_provenance]}),
                apply: true,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(missing_error.0, StatusCode::BAD_REQUEST);
        assert!(missing_error.1.contains("requires source_plugin_id"));
        assert!(db::list_providers(&state.pool).await.unwrap().is_empty());

        let existing_id = insert_provider(
            &state,
            "existing-auth",
            crate::plugins::CredentialMode::AuthFlow,
            Some("plugin.test"),
            Some("oauth"),
        )
        .await;
        let existing_update = json!({
            "name": "existing-auth",
            "base_url": "http://127.0.0.1:12345",
            "wire_format": "openai",
            "auth_scheme": "bearer",
            "extra_headers": {},
            "rate_limit_rules": {},
            "credential_plugin": "plugin:wrong.plugin/strategy",
            "wire_plugin": "",
            "model_source_plugin": ""
        });
        let update_error = import_config(
            State(state.clone()),
            auth(),
            Json(ImportBody {
                config: json!({"providers": [existing_update]}),
                apply: true,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(update_error.0, StatusCode::BAD_REQUEST);
        assert!(update_error
            .1
            .contains("credential_plugin does not match source_plugin_id"));
        let existing = db::get_provider(&state.pool, &existing_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(existing.credential_mode, "auth_flow");
        assert_eq!(existing.credential_plugin, "plugin:plugin.test/strategy");

        let existing_manual_plugin = insert_provider(
            &state,
            "manual-plugin-bound",
            crate::plugins::CredentialMode::Manual,
            None,
            None,
        )
        .await;
        let plugin_bound_manual = json!({
            "name": "manual-plugin-bound",
            "base_url": "http://127.0.0.1:12345",
            "wire_format": "openai",
            "auth_scheme": "bearer",
            "extra_headers": {},
            "rate_limit_rules": {},
            "credential_mode": "manual",
            "credential_plugin": "",
            "wire_plugin": "",
            "model_source_plugin": "plugin:plugin.test/models"
        });
        import_config(
            State(state.clone()),
            auth(),
            Json(ImportBody {
                config: json!({"providers": [plugin_bound_manual]}),
                apply: true,
            }),
        )
        .await
        .unwrap();
        let imported = db::list_providers(&state.pool)
            .await
            .unwrap()
            .into_iter()
            .find(|provider| provider.name == "manual-plugin-bound")
            .unwrap();
        assert_eq!(imported.id, existing_manual_plugin);
        assert_eq!(imported.pricing_scope, "integration");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn provider_update_rejects_direct_api_endpoint_drift_from_manifest() {
        let (state, root) = test_state_with_plugins("direct-api-endpoint-drift").await;
        let base_url = "https://provider-a.example/v1";
        install_direct_api_test_plugin(&state, base_url).await;
        let provider_id = db::insert_provider(
            &state.pool,
            &db::NewProvider {
                name: "trusted-direct-provider",
                base_url,
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: false,
                wire_plugin: "",
                credential_plugin: "",
                model_source_plugin: "",
                credential_mode: "manual",
                source_plugin_id: Some("plugin.test"),
                source_integration_id: Some("direct"),
            },
        )
        .await
        .unwrap();
        db::update_provider_pricing_scope(&state.pool, &provider_id, "direct_api")
            .await
            .unwrap();

        let mut body = provider_body("trusted-direct-provider", None);
        body.base_url = "https://provider-b.example/v1".into();
        body.allow_insecure_tls = false;
        body.pricing_scope = Some("direct_api".into());
        let error = update_provider(
            State(state.clone()),
            auth(),
            Path(provider_id.clone()),
            Json(body),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("pricing_scope 'direct_api' is bound"));
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(provider.base_url, base_url);
        assert_eq!(provider.pricing_scope, "direct_api");

        let mut omitted_scope = provider_body("trusted-direct-provider", None);
        omitted_scope.base_url = "https://provider-b.example/v1".into();
        omitted_scope.allow_insecure_tls = false;
        omitted_scope.pricing_scope = None;
        let error = update_provider(
            State(state.clone()),
            auth(),
            Path(provider_id.clone()),
            Json(omitted_scope),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("pricing_scope 'direct_api' is bound"));
        let provider = db::get_provider(&state.pool, &provider_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(provider.base_url, base_url);
        assert_eq!(provider.pricing_scope, "direct_api");

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn config_import_rejects_direct_api_endpoint_not_matching_installed_manifest() {
        let (state, root) = test_state_with_plugins("direct-api-import-endpoint").await;
        install_direct_api_test_plugin(&state, "https://provider-a.example/v1").await;

        let error = import_config(
            State(state.clone()),
            auth(),
            Json(ImportBody {
                config: json!({
                    "providers": [{
                        "name": "mismatched-direct-provider",
                        "base_url": "https://provider-b.example/v1",
                        "wire_format": "openai",
                        "auth_scheme": "bearer",
                        "extra_headers": {},
                        "rate_limit_rules": {},
                        "credential_mode": "manual",
                        "credential_plugin": "",
                        "wire_plugin": "",
                        "model_source_plugin": "",
                        "source_plugin_id": "plugin.test",
                        "source_integration_id": "direct",
                        "pricing_scope": "direct_api"
                    }]
                }),
                apply: true,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("pricing_scope 'direct_api' is bound"));
        assert!(db::list_providers(&state.pool).await.unwrap().is_empty());

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn config_import_restores_missing_plugin_direct_api_conservatively_then_promotes() {
        let (state, root) = test_state_with_plugins("portable-direct-api-import").await;
        let base_url = "https://provider-a.example/v1";
        let config = json!({
            "providers": [{
                "name": "portable-direct-provider",
                "base_url": base_url,
                "wire_format": "openai",
                "auth_scheme": "bearer",
                "extra_headers": {},
                "rate_limit_rules": {},
                "credential_mode": "manual",
                "credential_plugin": "",
                "wire_plugin": "",
                "model_source_plugin": "",
                "source_plugin_id": "plugin.test",
                "source_integration_id": "direct",
                "pricing_scope": "direct_api"
            }],
            "models": [{
                "provider": "portable-direct-provider",
                "upstream_id": "portable-priced-model",
                "display_name": "Portable Priced Model",
                "enabled": true,
                "capabilities": {},
                "prices": {
                    "input_per_1m": 1.25
                },
                "parameters": {},
                "thinking_map": {},
                "extra_request": {},
                "ownership": {
                    "operator_capability_overrides": {},
                    "operator_parameter_overrides": {},
                    "operator_reasoning_overrides": {},
                    "operator_thinking_overrides": {},
                    "effective_pricing": {
                        "source": "models.dev:provider",
                        "metadata": {
                            "fields": {
                                "input_per_1m": {
                                    "source": "models.dev:provider",
                                    "metadata": {
                                        "catalog_provider": {
                                            "provider_id": "provider-a",
                                            "model_id": "portable-priced-model"
                                        }
                                    }
                                }
                            },
                            "catalog_source_state": {
                                "source": "models.dev"
                            }
                        }
                    }
                }
            }]
        });

        let dry_run = import_config(
            State(state.clone()),
            auth(),
            Json(ImportBody {
                config: config.clone(),
                apply: false,
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(dry_run["valid"], true);
        let provider_plan = dry_run["plan"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["kind"] == "provider")
            .unwrap();
        assert_eq!(provider_plan["requested_pricing_scope"], "direct_api");
        assert_eq!(provider_plan["effective_pricing_scope"], "integration");
        assert!(dry_run["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|warning| warning.contains("will be restored as 'integration'"))));
        let model_plan = dry_run["plan"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["kind"] == "model")
            .unwrap();
        assert_eq!(
            model_plan["suppressed_external_price_fields"],
            json!(["input_per_1m"])
        );

        import_config(
            State(state.clone()),
            auth(),
            Json(ImportBody {
                config,
                apply: true,
            }),
        )
        .await
        .unwrap();

        let provider = db::list_providers(&state.pool)
            .await
            .unwrap()
            .into_iter()
            .find(|provider| provider.name == "portable-direct-provider")
            .unwrap();
        assert_eq!(provider.pricing_scope, "integration");
        let model =
            db::find_model_by_upstream(&state.pool, &provider.id, "portable-priced-model")
                .await
                .unwrap()
                .unwrap();
        assert_eq!(model.prices().input_per_1m, None);
        let discovery = discovery_object(&model);
        assert!(discovery
            .get("effective_pricing")
            .is_none_or(Value::is_null));
        let version_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM price_versions WHERE model_id = ?")
                .bind(&model.id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(version_count, 0);

        install_direct_api_test_plugin(&state, base_url).await;
        auto_provision_plugin_providers(&state, "plugin.test").await;
        let provider = db::get_provider(&state.pool, &provider.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(provider.pricing_scope, "direct_api");

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn config_import_cannot_inject_external_catalog_pricing_into_integration_provider() {
        let (state, root) = test_state("integration-import-catalog-price").await;
        let config = json!({
            "providers": [{
                "name": "integration-provider",
                "base_url": "https://integration.example/v1",
                "wire_format": "openai",
                "auth_scheme": "bearer",
                "extra_headers": {},
                "rate_limit_rules": {},
                "credential_mode": "manual",
                "credential_plugin": "",
                "wire_plugin": "",
                "model_source_plugin": "",
                "pricing_scope": "integration"
            }],
            "models": [{
                "provider": "integration-provider",
                "upstream_id": "crafted-model",
                "display_name": "Crafted Model",
                "enabled": true,
                "capabilities": {},
                "prices": {
                    "input_per_1m": 9.99,
                    "output_per_1m": 3.0
                },
                "parameters": {},
                "thinking_map": {},
                "extra_request": {},
                "ownership": {
                    "operator_capability_overrides": {},
                    "operator_parameter_overrides": {},
                    "operator_reasoning_overrides": {},
                    "operator_thinking_overrides": {},
                    "effective_pricing": {
                        "source": "mixed",
                        "metadata": {
                            "fields": {
                                "input_per_1m": {
                                    "source": "models.dev:provider",
                                    "metadata": {}
                                },
                                "output_per_1m": {
                                    "source": "operator",
                                    "metadata": {
                                        "configured_by": "config_import"
                                    }
                                }
                            }
                        }
                    }
                }
            }]
        });

        import_config(
            State(state.clone()),
            auth(),
            Json(ImportBody {
                config,
                apply: true,
            }),
        )
        .await
        .unwrap();

        let provider = db::list_providers(&state.pool)
            .await
            .unwrap()
            .into_iter()
            .find(|provider| provider.name == "integration-provider")
            .unwrap();
        assert_eq!(provider.pricing_scope, "integration");
        let model = db::find_model_by_upstream(&state.pool, &provider.id, "crafted-model")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.prices().input_per_1m, None);
        assert_eq!(model.prices().output_per_1m, Some(3.0));
        let discovery = discovery_object(&model);
        assert_eq!(
            discovery
                .pointer("/effective_pricing/fields/output_per_1m/source")
                .and_then(Value::as_str),
            Some("operator")
        );
        assert!(discovery
            .pointer("/effective_pricing/fields/input_per_1m")
            .is_none());
        assert_eq!(
            discovery
                .pointer("/effective_pricing/source")
                .and_then(Value::as_str),
            Some("operator")
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn config_export_import_preserves_automatic_model_ownership() {
        let (source, source_root) = test_state("portable-model-ownership-source").await;
        let provider_id = db::insert_provider(
            &source.pool,
            &db::NewProvider {
                name: "portable-google",
                base_url: "https://generativelanguage.googleapis.com/v1beta",
                wire_format: WireFormat::Openai,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: false,
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
        let model_id = db::insert_model(
            &source.pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: "portable-model",
                display_name: "Portable Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({"tool_calling": false}),
                prices: json!({}),
                parameters: json!({"temperature": {"supported": true}}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({
                    "operator_capability_overrides": {},
                    "operator_parameter_overrides": {},
                    "operator_reasoning_overrides": {},
                    "operator_thinking_overrides": {}
                }),
            },
        )
        .await
        .unwrap();
        let source_prices = Prices {
            input_per_1m: Some(1.0),
            ..Prices::default()
        };
        db::commit_effective_model_pricing(
            &source.pool,
            &model_id,
            &source_prices,
            "models.dev:provider",
            &json!({
                "fields": {
                    "input_per_1m": {
                        "source": "models.dev:provider",
                        "metadata": {
                            "observed_at": "2026-09-28T01:00:00Z",
                            "catalog_provider": {
                                "provider_id": "google",
                                "model_id": "portable-model"
                            }
                        }
                    }
                },
                "catalog_source_state": {
                    "source": "models.dev",
                    "retrieved_at": "2026-09-28T01:00:00Z"
                }
            }),
        )
        .await
        .unwrap();

        let exported = export_config(
            State(source.clone()),
            auth(),
            Query(ExportQuery {
                include_secrets: false,
            }),
        )
        .await
        .unwrap()
        .0;
        let exported_model = &exported["models"][0];
        assert_eq!(
            exported_model["ownership"]["effective_pricing"]["source"],
            "models.dev:provider"
        );
        assert!(exported_model["ownership"]["effective_pricing"]
            .get("price_version_id")
            .is_none());
        assert!(exported_model["ownership"]["effective_pricing"]["metadata"]
            .pointer("/fields/input_per_1m/metadata/observed_at")
            .is_none());

        let (target, target_root) = test_state("portable-model-ownership-target").await;
        import_config(
            State(target.clone()),
            auth(),
            Json(ImportBody {
                config: exported,
                apply: true,
            }),
        )
        .await
        .unwrap();

        let target_provider = db::list_providers(&target.pool)
            .await
            .unwrap()
            .into_iter()
            .find(|provider| provider.name == "portable-google")
            .unwrap();
        let imported =
            db::find_model_by_upstream(&target.pool, &target_provider.id, "portable-model")
                .await
                .unwrap()
                .unwrap();
        let discovery = discovery_object(&imported);
        assert_eq!(
            discovery
                .pointer("/effective_pricing/source")
                .and_then(Value::as_str),
            Some("models.dev:provider")
        );
        assert_eq!(discovery["operator_capability_overrides"], json!({}));
        assert_eq!(discovery["operator_parameter_overrides"], json!({}));
        assert_eq!(discovery["operator_thinking_overrides"], json!({}));

        let catalog = crate::model_catalog::ModelsDevCatalog::from_parts(
            json!({}),
            json!({
                "google": {
                    "id": "google",
                    "models": {
                        "portable-model": {
                            "id": "portable-model",
                            "cost": {"input": 2.0}
                        }
                    }
                }
            }),
        )
        .unwrap();
        apply_provider_pricing_sync(&target, &target_provider, &catalog)
            .await
            .unwrap();
        let imported = db::get_model(&target.pool, &imported.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(imported.prices().input_per_1m, Some(2.0));

        db::merge_model_discovery(
            &target.pool,
            &imported.id,
            &json!({"capabilities": {"tool_calling": true}}),
        )
        .await
        .unwrap();
        target.registry.reload(&target.pool).await.unwrap();
        let snapshot = target.registry.snapshot();
        let runtime_provider = snapshot.providers.get(&target_provider.id).unwrap();
        let runtime_model = snapshot.models.get(&imported.id).unwrap();
        let profile =
            crate::adapters::resolve_execution_profile(runtime_provider, runtime_model).unwrap();
        assert_eq!(profile.capabilities.tool_calling, Some(true));

        drop(source);
        drop(target);
        let _ = std::fs::remove_dir_all(source_root);
        let _ = std::fs::remove_dir_all(target_root);
    }

    #[tokio::test]
    async fn config_export_import_round_trips_credential_semantics() {
        let (source, source_root) = test_state("export-source").await;
        let auth_provider = insert_provider(
            &source,
            "oauth-provider",
            crate::plugins::CredentialMode::AuthFlow,
            Some("plugin.oauth"),
            Some("oauth"),
        )
        .await;
        let noauth_provider = insert_provider(
            &source,
            "public-provider",
            crate::plugins::CredentialMode::None,
            Some("plugin.public"),
            Some("public"),
        )
        .await;
        let source_model_id = db::insert_model(
            &source.pool,
            &db::NewModel {
                provider_id: &noauth_provider,
                upstream_id: "mixed-transport-model",
                display_name: "Mixed Transport Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                prices: json!({}),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({"transport":{"format":"anthropic"}}),
            },
        )
        .await
        .unwrap();
        db::set_model_transport_override(&source.pool, &source_model_id, Some("openai-responses"))
            .await
            .unwrap();
        let source_model = db::get_model(&source.pool, &source_model_id)
            .await
            .unwrap()
            .unwrap();
        persist_model_discovery_update(
            &source.pool,
            &source_model,
            json!({"transport":{"format":"gemini"}}),
        )
        .await
        .unwrap();

        let encrypted = source.crypto.encrypt("oauth-secret").unwrap();
        db::insert_account(
            &source.pool,
            &auth_provider,
            "connected",
            &encrypted,
            &crate::crypto::mask_secret("oauth-secret"),
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        reconcile_provider_credential_semantics(
            &source,
            &noauth_provider,
            crate::plugins::CredentialMode::None,
            "plugin.public",
            "public",
        )
        .await
        .unwrap();

        let exported = export_config(
            State(source.clone()),
            auth(),
            Query(ExportQuery {
                include_secrets: true,
            }),
        )
        .await
        .unwrap()
        .0;

        let oauth = exported["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["name"] == "oauth-provider")
            .unwrap();
        assert_eq!(oauth["credential_mode"], "auth_flow");
        assert_eq!(oauth["source_plugin_id"], "plugin.oauth");
        assert_eq!(oauth["source_integration_id"], "oauth");
        assert_eq!(oauth["pricing_scope"], "integration");

        let public = exported["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["name"] == "public-provider")
            .unwrap();
        assert_eq!(public["credential_mode"], "none");
        assert_eq!(public["source_plugin_id"], "plugin.public");
        assert_eq!(public["source_integration_id"], "public");
        assert!(exported["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|account| account["label"] != "__kinetix_noauth__"));
        assert_eq!(
            exported["models"][0]["transport_override"],
            "openai-responses"
        );

        let (target, target_root) = test_state("export-target").await;
        let _ = import_config(
            State(target.clone()),
            auth(),
            Json(ImportBody {
                config: exported,
                apply: true,
            }),
        )
        .await
        .unwrap();

        let providers = db::list_providers(&target.pool).await.unwrap();
        let oauth = providers
            .iter()
            .find(|provider| provider.name == "oauth-provider")
            .unwrap();
        assert_eq!(oauth.credential_mode, "auth_flow");
        assert_eq!(oauth.source_plugin_id.as_deref(), Some("plugin.oauth"));
        assert_eq!(oauth.source_integration_id.as_deref(), Some("oauth"));
        assert_eq!(oauth.pricing_scope, "integration");

        let oauth_accounts = db::accounts_for_provider(&target.pool, &oauth.id)
            .await
            .unwrap();
        assert_eq!(oauth_accounts.len(), 1);
        assert_eq!(
            target
                .crypto
                .decrypt(&oauth_accounts[0].secret_enc)
                .unwrap(),
            "oauth-secret"
        );

        let public = providers
            .iter()
            .find(|provider| provider.name == "public-provider")
            .unwrap();
        assert_eq!(public.credential_mode, "none");
        assert_eq!(public.source_plugin_id.as_deref(), Some("plugin.public"));
        assert_eq!(public.source_integration_id.as_deref(), Some("public"));
        let imported_model =
            db::find_model_by_upstream(&target.pool, &public.id, "mixed-transport-model")
                .await
                .unwrap()
                .unwrap();
        let imported_discovery: Value = serde_json::from_str(&imported_model.discovery).unwrap();
        assert_eq!(
            imported_discovery["configured_transport"],
            "openai-responses"
        );

        let public_accounts = db::accounts_for_provider(&target.pool, &public.id)
            .await
            .unwrap();
        assert_eq!(public_accounts.len(), 1);
        assert_eq!(public_accounts[0].label, "__kinetix_noauth__");
        assert_eq!(
            target
                .crypto
                .decrypt(&public_accounts[0].secret_enc)
                .unwrap(),
            ""
        );

        let _ = std::fs::remove_dir_all(source_root);
        let _ = std::fs::remove_dir_all(target_root);
    }

    #[tokio::test]
    async fn runtime_health_separates_diagnostic_and_routing_quota_evidence() {
        let (state, root) = test_state("runtime-quota-evidence").await;
        state.quota.observe_account_global(
            "provider-test",
            "account-test",
            Some(0.75),
            None,
            "plugin_health_probe",
            std::time::Duration::from_secs(60),
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-remaining-requests", "0".parse().unwrap());
        state
            .quota
            .observe_headers("provider-test", "account-test", &headers)
            .unwrap();

        let response = runtime_health(
            State(state.clone()),
            auth(),
            Query(RuntimeHealthQuery {
                window: Some("1h".into()),
            }),
        )
        .await
        .unwrap()
        .0;
        let quota = &response["quota"][0];
        assert_eq!(quota["remaining_fraction"], 0.0);
        assert_eq!(
            quota["source"],
            "response_header:x-ratelimit-remaining-requests"
        );
        assert_eq!(quota["routing"]["scope"], "account-global");
        assert_eq!(quota["routing"]["remaining_fraction"], 0.75);
        assert_eq!(quota["routing"]["source"], "plugin_health_probe");
        assert_eq!(quota["routing"]["routing_eligible"], true);

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }
}
