//! Request pipeline: auth -> limits -> route resolution (predicates) -> account
//! selection (health, circuit breaker, sticky affinity) -> adapter call ->
//! pre-commit failover -> streaming encode -> async usage logging + Route Trace.
//!
//! Commit semantics (FR-4.5, NFR-2.9): retry/fallback happens **only before**
//! the commit point (the first client response bytes). A failure after commit
//! terminates the stream with a format-correct error and is never spliced.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::adapters::{Adapter, UpstreamContext};
use crate::app::AppState;
use crate::cost;
use crate::db::{self, UsageLogRow};
use crate::frontends::{self, Encoder, EncoderCtx, FrontendFormat};
use crate::opaque_state::{
    OpaqueClientScope, OpaqueLookupResult, OpaqueStateStore, OpaqueStateTarget,
};
use crate::passthrough;
use crate::pool;
use crate::predicate::{self, PluginFacts, RequestFacts, TargetFacts, TargetPredicate};
use crate::registry::{Resolved, ResolvedTarget};
use crate::stream_outcome::{CommitState, StreamOutcome, StreamTermination};
use crate::trace::RouteTrace;
use crate::types::{
    FailureKind, InternalRequest, ProxyError, StreamEvent, TokenUsage, UpstreamFailure,
};

const STICKY_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_PRE_COMMIT_DEADLINE: Duration = Duration::from_secs(30);
const CIRCUIT_THRESHOLD: i64 = 4;
/// SSE keepalive cadence. Cloudflare drops proxied connections that are idle
/// for ~100s (HTTP 524), so a silent thinking phase must be kept alive well
/// within that window (NFR-1 / FR-9.4). Exposed for tests.
pub const KEEPALIVE_INTERVAL_SECS: u64 = 15;
const CIRCUIT_OPEN_SECS: i64 = 30;
/// Bounded exponential backoff between pre-commit retry attempts (FR-4.4):
/// 100ms, 200ms, 400ms, capped at 1s, so a failing pool cannot be hot-looped.
const BACKOFF_BASE_MS: u64 = 100;
const BACKOFF_CAP_MS: u64 = 1000;

fn provider_phase_timeout(provider: &db::ProviderRow) -> Duration {
    Duration::from_millis(provider.timeout_ms.max(1) as u64)
}

fn timeout_failure(message: &'static str) -> UpstreamFailure {
    UpstreamFailure {
        kind: FailureKind::Timeout,
        status: None,
        retry_after_secs: None,
        message: message.into(),
        quota_reset_at: None,
    }
}

fn phase_budget(deadline: Instant, provider_timeout: Duration) -> Option<(Instant, Duration)> {
    let now = Instant::now();
    let remaining = deadline.saturating_duration_since(now);
    if remaining.is_zero() {
        return None;
    }
    let budget = remaining.min(provider_timeout);
    Some((now + budget, budget))
}

fn cache_status_from_usage(usage: &TokenUsage) -> &'static str {
    if usage.cached.unwrap_or(0) > 0 {
        "hit"
    } else if usage.cache_write.unwrap_or(0) > 0 {
        "miss"
    } else {
        "bypass"
    }
}

struct PartialAttemptUsage {
    usage: TokenUsage,
    cost: Option<f64>,
    row: db::UsageAttemptRow,
}

struct LastPrecommitFailure {
    target: ResolvedTarget,
    upstream_request_id: Option<String>,
    failure: UpstreamFailure,
    termination: StreamTermination,
    attempts_done: usize,
    client_error: ProxyError,
}

struct ActivePrecommitAttempt {
    target: ResolvedTarget,
    attempt_number: usize,
    opaque_route_id: String,
    upstream_request_id: Option<String>,
}

/// Request-scoped metadata carried into the usage log and Route Trace.
pub struct RequestMeta {
    pub request_id: String,
    pub key_id: Option<String>,
    pub key_name: Option<String>,
    pub key_tag: Option<String>,
    pub client_format: &'static str,
    pub requested_model: String,
    pub route_id: Option<String>,
    pub route_name: Option<String>,
    pub fallback_hops: i64,
    pub retry_count: i64,
    pub fallback_path: Vec<String>,
    fallback_enabled: bool,
    pub cache_status: &'static str,
    pub commit_state: &'static str,
    pub session: Option<String>,
    /// Atomic RPM/TPM/budget reservation owned by this request. A dispatched
    /// failure or disconnect reconciles it as incomplete instead of canceling it.
    pub admission: Option<crate::admission::AdmissionReservation>,
    /// Usage and costs from failed precommit provider attempts. Persisted as
    /// per-attempt rows and included in admission reconciliation.
    partial_attempts: Vec<PartialAttemptUsage>,
    active_precommit_attempt: Option<ActivePrecommitAttempt>,
    log_queue: Option<crate::logqueue::UsageLogQueue>,
    accounting_enqueued: AtomicBool,
    /// Bounded global, key, and Route in-flight admission; streaming transfers it to the response body.
    pub concurrency: Option<crate::admission::ConcurrencyReservation>,
    /// Set immediately before the inference request is handed to the HTTP client.
    upstream_dispatched: AtomicBool,
    client_disconnect: Option<crate::client_disconnect::ClientDisconnect>,
    /// Set by a watchdog when the client disconnects, so an in-flight upstream
    /// response can be aborted even if the write channel still looks open.
    pub disconnected: Arc<std::sync::atomic::AtomicBool>,
    /// When the client was first observed to have disconnected (for NFR-1.10
    /// cancellation-latency measurement, distinct from request start).
    pub disconnect_at: Arc<parking_lot::Mutex<Option<Instant>>>,
}

impl RequestMeta {
    pub fn new(
        request_id: String,
        format: FrontendFormat,
        requested_model: String,
        fallback_enabled: bool,
    ) -> Self {
        RequestMeta {
            request_id,
            key_id: None,
            key_name: None,
            key_tag: None,
            client_format: format.as_str(),
            requested_model,
            route_id: None,
            route_name: None,
            fallback_hops: 0,
            retry_count: 0,
            fallback_path: Vec::new(),
            fallback_enabled,
            cache_status: "bypass",
            commit_state: "",
            session: None,
            admission: None,
            concurrency: None,
            partial_attempts: Vec::new(),
            active_precommit_attempt: None,
            log_queue: None,
            accounting_enqueued: AtomicBool::new(false),
            upstream_dispatched: AtomicBool::new(false),
            client_disconnect: None,
            disconnected: Arc::new(AtomicBool::new(false)),
            disconnect_at: Arc::new(parking_lot::Mutex::new(None)),
        }
    }
}

impl Drop for RequestMeta {
    fn drop(&mut self) {
        let disconnected_after_dispatch = self.upstream_dispatched.load(Ordering::Acquire)
            && self
                .client_disconnect
                .as_ref()
                .is_some_and(crate::client_disconnect::ClientDisconnect::is_cancelled);
        if disconnected_after_dispatch {
            self.record_cancelled_accounting();
            if let Some(admission) = self.admission.take() {
                // The active dispatch may have incurred unreported usage. Earlier
                // attempts cannot make this request's total exact.
                admission.reconcile_incomplete();
            }
        }
    }
}

impl RequestMeta {
    fn record_cancelled_accounting(&mut self) {
        if self.accounting_enqueued.load(Ordering::Acquire) {
            return;
        }
        if let Some(active) = self.active_precommit_attempt.take() {
            let termination = StreamTermination::new(
                StreamOutcome::ClientCancelled,
                CommitState::PreCommit,
                None,
                None,
            );
            let mut row = usage_attempt_row(
                self,
                &active.target,
                None,
                active.upstream_request_id.as_deref(),
                active.attempt_number,
                &TokenUsage::default(),
                None,
                None,
                termination,
                Some("client disconnected before response commit"),
                &active.opaque_route_id,
            );
            row.key_id = self.key_id.clone();
            row.key_name = self.key_name.clone();
            self.partial_attempts.push(PartialAttemptUsage {
                usage: TokenUsage::default(),
                cost: None,
                row,
            });
        }
        let Some(log_queue) = self.log_queue.clone() else {
            return;
        };
        let Some(last_attempt) = self.partial_attempts.last() else {
            return;
        };
        let (usage, cost) = aggregate_request_accounting(self, None, None);
        let request_row = UsageLogRow {
            id: format!("usage_{}", uuid::Uuid::new_v4().simple()),
            request_id: self.request_id.clone(),
            ts: db::now_iso(),
            key_id: self.key_id.clone(),
            key_name: self.key_name.clone(),
            client_format: self.client_format.to_string(),
            requested_model: self.requested_model.clone(),
            effective_model: last_attempt.row.effective_model.clone(),
            route_id: self.route_id.clone(),
            route_name: self.route_name.clone(),
            fallback_hops: self.partial_attempts.len().saturating_sub(1) as i64,
            fallback_path: serde_json::to_string(&self.fallback_path)
                .unwrap_or_else(|_| "[]".into()),
            status: "client_disconnect".into(),
            status_code: 499,
            latency_ms: None,
            ttft_ms: None,
            input_tokens: usage.input.map(|value| value as i64),
            output_tokens: usage.output.map(|value| value as i64),
            cached_tokens: usage.cached.map(|value| value as i64),
            cache_write_tokens: usage.cache_write.map(|value| value as i64),
            thinking_tokens: usage.thinking.map(|value| value as i64),
            cost_usd: cost,
            cost_known: cost.is_some() as i64,
            price_version_id: None,
            cache_status: self.cache_status.to_string(),
            serving_account_id: last_attempt.row.serving_account_id.clone(),
            serving_account: last_attempt.row.serving_account.clone(),
            serving_provider: last_attempt.row.serving_provider.clone(),
            upstream_request_id: last_attempt.row.upstream_request_id.clone(),
            flagged: 0,
            error_message: Some("client disconnected before response commit".into()),
            usage_confidence: if usage_has_reported_tokens(&usage) {
                "provider_reported".into()
            } else {
                "unknown".into()
            },
            commit_state: CommitState::PreCommit.as_usage_str().to_string(),
            retry_count: self.partial_attempts.len().saturating_sub(1) as i64,
            route_trace_id: last_attempt.row.opaque_route_id.clone(),
            opaque_route_id: last_attempt.row.opaque_route_id.clone(),
            admission_cost_usd: self
                .admission
                .as_ref()
                .and_then(|admission| admission.conservative_cost_estimate()),
        };
        log_queue.enqueue_bundle(db::UsageAccountingBundle {
            request: request_row,
            attempts: self
                .partial_attempts
                .iter()
                .map(|attempt| attempt.row.clone())
                .collect(),
        });
        self.accounting_enqueued.store(true, Ordering::Release);
    }
}

/// The result of a successful upstream connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCountResult {
    pub input_tokens: u64,
    pub exact: bool,
}

fn estimated_input_tokens(req: &InternalRequest) -> u64 {
    let mut tokens = req.approx_input_tokens();
    let tool_chars: u64 = req
        .tools
        .iter()
        .map(|tool| {
            tool.name.len() as u64
                + tool.description.as_deref().map(str::len).unwrap_or(0) as u64
                + tool.parameters.to_string().len() as u64
                + 32
        })
        .sum();
    tokens = tokens.saturating_add(tool_chars.div_ceil(4));
    tokens.max(1)
}

fn token_count_exact_target(
    state: &AppState,
    key: &db::VirtualKeyRow,
    req: &InternalRequest,
    input_protocol: &str,
) -> Result<Option<ResolvedTarget>, ProxyError> {
    let snap = state.registry.snapshot();
    let resolved =
        crate::registry::Registry::resolve_in(&snap, &req.requested_model).ok_or_else(|| {
            ProxyError::not_found(format!(
                "model '{}' is not configured. Use GET /v1/models to list available models.",
                req.requested_model
            ))
        })?;
    let needs = req.capability_needs();
    let allowed_providers = key.allowed_providers();

    let eligible = |target: &ResolvedTarget| {
        let capabilities_match = provider_accepts_input_protocol(&target.provider, input_protocol)
            && integration_feature_ceiling_satisfies_needs(&target.provider, &needs)
            && (!target.provider.strict()
                || crate::adapters::resolve_execution_profile_for_target(
                    &target.provider,
                    &target.model,
                    Some(target.account.id.as_str()),
                )
                .map(|profile| {
                    profile_satisfies_needs(&profile.capabilities, &needs, target.provider.strict())
                })
                .unwrap_or(false));
        (allowed_providers.is_empty() || allowed_providers.contains(&target.provider.id))
            && capabilities_match
    };

    match resolved {
        Resolved::Single {
            provider_id,
            model_id,
        } => {
            let model = snap
                .models
                .get(&model_id)
                .cloned()
                .ok_or_else(|| ProxyError::not_found("model not found"))?;
            let provider = snap
                .providers
                .get(&provider_id)
                .cloned()
                .ok_or_else(|| ProxyError::not_found("provider not found"))?;
            if !provider_accepts_input_protocol(&provider, input_protocol) {
                return Err(ProxyError::unsupported(format!(
                    "provider integration does not allow input protocol '{input_protocol}'"
                )));
            }
            if !allowed_providers.is_empty() && !allowed_providers.contains(&provider.id) {
                return Err(ProxyError::new(
                    crate::types::ErrorKind::Forbidden,
                    "this key is not allowed to use the resolved provider",
                ));
            }
            let account = select_accounts(&snap, &provider_id, None)?
                .into_iter()
                .find(|account| {
                    matches!(
                        pool::effective_status(account),
                        pool::AccountStatus::Healthy
                    )
                });
            let Some(account) = account else {
                return Ok(None);
            };
            let profile = crate::adapters::resolve_execution_profile_for_target(
                &provider,
                &model,
                Some(account.id.as_str()),
            )?;
            if !provider_satisfies_needs(&provider, &profile.capabilities, &needs) {
                return Err(ProxyError::unsupported(
                    "resolved model cannot satisfy this token-count request",
                ));
            }

            Ok(Some(ResolvedTarget {
                account,
                model,
                provider,
                route_target_id: None,
                priority: 1,
                weight: 1,
                predicate: TargetPredicate::default(),
                param_overrides: Value::Null,
            }))
        }
        Resolved::Route { targets, .. } => {
            let mut targets: Vec<_> = targets.into_iter().filter(eligible).collect();
            if targets.is_empty() {
                return Err(ProxyError::unsupported(
                    "no configured target can satisfy this token-count request",
                ));
            }

            // Counting must not advance round-robin/weighted route state. Exact
            // upstream counting is therefore safe only when every eligible
            // route candidate shares the same tokenizer/model. Heterogeneous
            // routes use the documented local estimate.
            let first_provider = targets[0].provider.id.clone();
            let first_model = targets[0].model.id.clone();
            if targets.iter().any(|target| {
                target.provider.id != first_provider || target.model.id != first_model
            }) {
                return Ok(None);
            }

            targets.sort_by_key(|target| target.account.priority);
            Ok(targets.into_iter().find(|target| {
                matches!(
                    pool::effective_status(&target.account),
                    pool::AccountStatus::Healthy
                )
            }))
        }
    }
}

pub async fn count_tokens(
    state: &AppState,
    key: &db::VirtualKeyRow,
    req: &InternalRequest,
    request_id: &str,
    protocol_headers: &[(String, String)],
) -> Result<TokenCountResult, ProxyError> {
    let estimate = || TokenCountResult {
        input_tokens: estimated_input_tokens(req),
        exact: false,
    };

    let Some(target) =
        token_count_exact_target(state, key, req, FrontendFormat::Anthropic.protocol())?
    else {
        return Ok(estimate());
    };
    let profile = crate::adapters::resolve_execution_profile_for_target(
        &target.provider,
        &target.model,
        Some(target.account.id.as_str()),
    )?;
    let adapter = state.adapters.for_transport(&profile.transport)?;
    if !adapter.supports_count_tokens() {
        return Ok(estimate());
    }
    let credential = match state
        .credential_for(&target.provider, &target.account)
        .await
    {
        Ok(credential) => credential.secret,
        Err(error) => {
            if let Err(disable_error) = state
                .disable_invalid_credential(
                    &target.account,
                    &error,
                    "token-count credential resolution",
                )
                .await
            {
                tracing::error!(
                    account = %target.account.id,
                    credential_error = %error,
                    %disable_error,
                    "failed to disable account after token-count credential resolution confirmed invalid"
                );
            }
            return Err(ProxyError::internal("credential unavailable"));
        }
    };
    let ctx = UpstreamContext {
        provider: &target.provider,
        model: &target.model,
        account_id: Some(target.account.id.as_str()),
        session_context: None,
        credential,
    };
    let Some(url) = adapter.count_tokens_url(&ctx)? else {
        return Ok(estimate());
    };
    let url = url::Url::parse(&url)
        .map_err(|error| ProxyError::internal(format!("invalid token-count URL: {error}")))?;

    let raw = req
        .raw_body
        .as_deref()
        .ok_or_else(|| ProxyError::internal("token-count request body unavailable"))?;
    let mut body: Value = serde_json::from_str(raw)
        .map_err(|error| ProxyError::bad_request(format!("invalid JSON body: {error}")))?;
    let object = body
        .as_object_mut()
        .ok_or_else(|| ProxyError::bad_request("request body must be a JSON object"))?;
    object.insert("model".into(), serde_json::json!(target.model.upstream_id));

    let response = crate::outbound::send_provider_request(
        &state.outbound_clients,
        state.config.allow_private_upstreams,
        state.config.allow_insecure_tls,
        &adapter,
        &ctx,
        crate::outbound::ProviderRequest {
            method: reqwest::Method::POST,
            url,
            json_body: Some(body),
            accept_event_stream: false,
            request_id: Some(request_id.to_string()),
            headers: if adapter.wire_format() == "anthropic" {
                protocol_headers.to_vec()
            } else {
                Vec::new()
            },
            total_timeout: Some(provider_phase_timeout(&target.provider)),
        },
    )
    .await
    .map_err(|error| {
        if let Some(failure) = error.adapter_failure {
            failure_to_error(&failure, &target)
        } else if error.timeout {
            ProxyError::upstream("upstream token-count request timed out")
        } else {
            ProxyError::upstream(error.message)
        }
    })?;

    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let text = response
        .text()
        .await
        .map_err(|error| ProxyError::upstream(classify_reqwest(&error)))?;
    if !(200..300).contains(&status) {
        let native = adapter.classify_error(status, &text, &headers);
        let failure = apply_provider_failure_rules(&target.provider, status, &text, native);
        return Err(preserve_anthropic_error(
            failure_to_error(&failure, &target),
            FrontendFormat::Anthropic,
            adapter.as_ref(),
            &failure,
            status,
            &headers,
            Some(&text),
        ));
    }

    let parsed: Value = serde_json::from_str(&text)
        .map_err(|error| ProxyError::upstream(format!("invalid token-count response: {error}")))?;
    let input_tokens = parsed
        .get("input_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| ProxyError::upstream("token-count response missing input_tokens"))?;

    Ok(TokenCountResult {
        input_tokens,
        exact: true,
    })
}

struct Attempt {
    target: ResolvedTarget,
    upstream_request_id: Option<String>,
    stream: Option<reqwest::Response>,
    /// Raw SSE transport chunks consumed during pre-commit validation. Drivers
    /// replay them before reading the remaining response body.
    prefetched: Vec<Bytes>,
    /// Parsed complete events for a successful non-SSE JSON response.
    full_events: Option<Vec<StreamEvent>>,
    /// Cache usage observed before downstream commit. Anthropic normally sends
    /// this in message_start, allowing X-Kinetix-Cache to reflect real usage.
    precommit_usage: TokenUsage,
    /// Maximum silence between upstream transport chunks after pre-commit
    /// validation. This is a per-gap timer, never a total stream lifetime.
    idle_timeout: Duration,
    adapter: Arc<dyn Adapter>,
    is_sse: bool,
    /// Same-format passthrough is only possible when the upstream is actually
    /// SSE. A JSON response is normalized through parse_full_response().
    passthrough: bool,
    /// Adaptive target-local permit held for the full upstream lifecycle.
    traffic_permit: Option<crate::upstream_traffic::TrafficPermit>,
    /// Provider-wide breaker attempt. Half-open recovery/failure is committed
    /// only when the stream reaches a terminal outcome.
    provider_attempt: crate::provider_circuit::ProviderAttempt,
    /// First-event validation is recorded separately from terminal recovery.
    provider_circuit_transition: Option<crate::provider_circuit::ProviderCircuitTransition>,
    /// Account recovery is committed only after a completed terminal outcome.
    account_probe: bool,
    /// Target-attempt-local start, excluding routing and credential work.
    attempt_started: Instant,
}

fn mark_provider_probe_validated(
    provider_attempt: &crate::provider_circuit::ProviderAttempt,
) -> Option<crate::provider_circuit::ProviderCircuitTransition> {
    provider_attempt
        .is_half_open_probe()
        .then(|| provider_attempt.mark_validated_success())
}

/// Run the full pipeline and produce a client response.
///
/// A single immutable snapshot is taken at entry and used for the whole
/// request, so a concurrent configuration change never alters an in-flight
/// request (NFR-2.10).
///
/// Cancellation note (FR-2.9): the public API handler cancels this future when
/// its client connection closes, including during pre-commit upstream work. Once
/// a response is produced, the response body's drop-guard handles post-commit
/// cancellation. Active streams have no total wall-clock timeout and only
/// enforce per-gap idle time.
pub async fn run(
    state: &AppState,
    format: FrontendFormat,
    key: Option<db::VirtualKeyRow>,
    req: InternalRequest,
    request_id: String,
    allow_fallback: bool,
    session: Option<String>,
    protocol_headers: Vec<(String, String)>,
) -> Result<Response, ProxyError> {
    run_with_disconnect(
        state,
        format,
        key,
        req,
        request_id,
        allow_fallback,
        session,
        protocol_headers,
        None,
        None,
    )
    .await
}

pub(crate) async fn run_with_disconnect(
    state: &AppState,
    format: FrontendFormat,
    key: Option<db::VirtualKeyRow>,
    req: InternalRequest,
    request_id: String,
    allow_fallback: bool,
    session: Option<String>,
    protocol_headers: Vec<(String, String)>,
    client_disconnect: Option<crate::client_disconnect::ClientDisconnect>,
    cancellation_trace: Option<Arc<parking_lot::Mutex<Option<RouteTrace>>>>,
) -> Result<Response, ProxyError> {
    let started = Instant::now();
    let snap = state.registry.snapshot();
    let resolved_route = match crate::registry::Registry::resolve_in(&snap, &req.requested_model) {
        Some(Resolved::Route { route, .. }) => Some(route),
        _ => None,
    };
    let concurrency = state.admission.reserve_concurrency(
        key.as_ref()
            .map(|key| (key.id.as_str(), key.max_concurrent_requests)),
        resolved_route
            .as_ref()
            .map(|route| (route.id.as_str(), route.max_concurrent_requests)),
    )?;
    let admission = match &key {
        Some(key) => {
            Some(crate::limits::reserve(&state.admission, &state.pool, &snap, key, &req).await?)
        }
        None => None,
    };
    let mut trace = cancellation_trace
        .as_ref()
        .and_then(|snapshot| snapshot.lock().clone())
        .unwrap_or_else(|| RouteTrace::new(request_id.clone(), req.requested_model.clone()));
    if let Some(snapshot) = cancellation_trace {
        trace.attach_cancellation_snapshot(snapshot);
    }
    let mut meta = RequestMeta::new(
        request_id.clone(),
        format,
        req.requested_model.clone(),
        allow_fallback,
    );
    meta.session = session.clone();
    meta.client_disconnect = client_disconnect;
    meta.log_queue = Some(state.log_queue.clone());
    meta.admission = admission;
    meta.concurrency = Some(concurrency);
    if let Some(k) = &key {
        meta.key_id = Some(k.id.clone());
        meta.key_name = Some(k.name.clone());
        if !k.tag.is_empty() {
            meta.key_tag = Some(k.tag.clone());
        }
    }
    state.total_requests.fetch_add(1, Ordering::Relaxed);
    state.live.start(
        &request_id,
        &format!("{format:?}").to_lowercase(),
        &req.requested_model,
        meta.key_name.clone(),
        None,
    );
    state
        .flight
        .record(&request_id, 0, "request_accepted", format!("{format:?}"));
    state.flight.record(
        &request_id,
        started.elapsed().as_millis() as u64,
        "auth_complete",
        meta.key_name.clone().unwrap_or_else(|| "anonymous".into()),
    );

    // 1. Resolve the route against the request's snapshot.
    let resolved =
        crate::registry::Registry::resolve_in(&snap, &req.requested_model).ok_or_else(|| {
            ProxyError::not_found(format!(
                "model '{}' is not configured. Use GET /v1/models to list available models.",
                req.requested_model
            ))
        })?;
    trace.step(
        "resolve",
        None,
        format!("resolved '{}'", req.requested_model),
    );

    // 2. Build the ordered attempt list, evaluating target predicates.
    let needs = req.capability_needs();
    let request_facts = RequestFacts {
        frontend: format.as_str(),
        requested_model: &req.requested_model,
        requested_route: None,
        key_tag: meta.key_tag.as_deref(),
        has_tools: needs.tools,
        has_images: needs.vision,
        has_reasoning: needs.reasoning,
        input_tokens: req.approx_input_tokens(),
    };

    // 2b. Read-only request hook (§6.6): observe the normalized request before
    // routing. Fire-and-forget on the bounded async queue; it can never block,
    // fail, or slow a client request.
    if let Some(manager) = state.plugin_manager().cloned() {
        let json = serde_json::json!({
            "request_id": meta.request_id,
            "frontend": format.as_str(),
            "requested_model": req.requested_model,
            "has_tools": needs.tools,
            "has_images": needs.vision,
            "has_reasoning": needs.reasoning,
            "input_tokens": req.approx_input_tokens(),
        })
        .to_string();
        state.spawn_hook(move || async move {
            let mut hooks = tokio::task::JoinSet::new();
            for id in manager.plugins_with_hook("on_request_normalized").await {
                let manager = manager.clone();
                let json = json.clone();
                hooks.spawn(async move {
                    let _ = manager.hook_request_normalized(&id, &json).await;
                });
            }
            while hooks.join_next().await.is_some() {}
        });
    }

    let mut execution_profiles = std::collections::HashMap::new();
    let (mut targets, route, route_target_origins) = match resolved {
        Resolved::Single {
            provider_id,
            model_id,
        } => {
            let model = snap
                .models
                .get(&model_id)
                .cloned()
                .ok_or_else(|| ProxyError::not_found("model not found"))?;
            let provider = snap
                .providers
                .get(&provider_id)
                .cloned()
                .ok_or_else(|| ProxyError::not_found("provider not found"))?;
            let accounts = select_accounts(&snap, &provider_id, None)?;
            let targets: Vec<ResolvedTarget> = accounts
                .into_iter()
                .map(|account| ResolvedTarget {
                    account,
                    model: model.clone(),
                    provider: provider.clone(),
                    route_target_id: None,
                    priority: 1,
                    weight: 1,
                    predicate: TargetPredicate::default(),
                    param_overrides: Value::Null,
                })
                .collect();
            (targets, None, Vec::new())
        }
        Resolved::Route { route, targets } => {
            meta.route_id = Some(route.id.clone());
            meta.route_name = Some(route.name.clone());
            trace.route_id = Some(route.id.clone());
            trace.route_name = Some(route.name.clone());
            trace.publish_cancellation_snapshot();
            // Retain route-wide producer provenance before request-specific
            // predicates and other eligibility filters remove candidates.
            let route_target_origins = targets
                .iter()
                .map(|target| (target_key(&route, target), continuation_origin(target)))
                .collect::<Vec<_>>();
            // Read-only target hook (§6.6): observe each candidate target before
            // eligibility filtering. Fire-and-forget; never blocks routing.
            if let Some(manager) = state.plugin_manager().cloned() {
                let req_id = meta.request_id.clone();
                for t in &targets {
                    let json = serde_json::json!({
                        "request_id": req_id,
                        "route": route.name,
                        "model_id": t.model.id,
                        "provider_id": t.provider.id,
                        "provider": t.provider.name,
                        "account": t.account.label,
                        "priority": t.priority,
                    })
                    .to_string();
                    let manager = manager.clone();
                    state.spawn_hook(move || async move {
                        let mut hooks = tokio::task::JoinSet::new();
                        for id in manager.plugins_with_hook("on_target_candidate").await {
                            let manager = manager.clone();
                            let json = json.clone();
                            hooks.spawn(async move {
                                let _ = manager.hook_target_candidate(&id, &json).await;
                            });
                        }
                        while hooks.join_next().await.is_some() {}
                    });
                }
            }
            (targets, Some(route), route_target_origins)
        }
    };

    if let Some(route) = &route {
        meta.fallback_path.push(format!("route:{}", route.name));
        state.live.set_fallback_hops(&meta.request_id, 0, 0);
    }

    // Runtime, Route Trace, and dry-run use the same request-level eligibility
    // decision before any Route strategy orders candidates.
    let request_facts = RequestFacts {
        requested_route: route.as_ref().map(|route| route.name.as_str()),
        ..request_facts
    };
    let plugin_facts = if route.is_some() {
        gather_plugin_facts(state, &request_facts, Some(&meta.disconnected)).await
    } else {
        PluginFacts::default()
    };
    for (name, value, source) in plugin_facts.iter_trace() {
        trace.plugin_fact(name, value, source);
    }
    for (plugin_id, reason) in &plugin_facts.failures {
        trace.plugin_fact_failure(plugin_id, reason);
    }
    let allowed_providers = key
        .as_ref()
        .map(|key| key.allowed_providers())
        .unwrap_or_default();
    let mut eligible_targets = Vec::with_capacity(targets.len());
    for target in targets.drain(..) {
        let decision = evaluate_request_eligibility(
            &target,
            &request_facts,
            &plugin_facts,
            &needs,
            &allowed_providers,
        );
        let protocol_ok = provider_accepts_input_protocol(&target.provider, format.protocol());
        if route.is_some() {
            trace.candidate(
                format!("{} @ {}", target.model.display_name, target.account.label),
                decision.eligible() && protocol_ok,
                Some(decision.predicate.result.as_str().to_string()),
                decision.explanation(),
            );
        }
        if !decision.predicate.eligible {
            continue;
        }
        if let Some(error) = decision.execution_profile_error.as_deref() {
            trace.step(
                "skip",
                Some(target.model.display_name.clone()),
                format!("invalid execution profile: {error}"),
            );
            continue;
        }
        if !decision.capability.eligible {
            trace.step(
                "skip",
                Some(target.model.display_name.clone()),
                decision.capability.reasons.join("; "),
            );
            continue;
        }
        if !decision.integration_features_allowed {
            trace.step(
                "skip",
                Some(target.model.display_name.clone()),
                "integration_feature_ceiling".to_string(),
            );
            continue;
        }
        if !decision.provider_permitted {
            trace.step(
                "skip",
                Some(target.account.label.clone()),
                format!("model={} provider_not_permitted", target.model.display_name),
            );
            continue;
        }
        if !protocol_ok {
            trace.step(
                "skip",
                Some(target.model.display_name.clone()),
                format!(
                    "model={} input protocol '{}' is not allowed by the integration",
                    target.model.display_name,
                    format.protocol()
                ),
            );
            continue;
        }
        if !decision.context_eligible {
            let limit = target.model.context_window.unwrap_or_default();
            trace.step(
                "skip",
                Some(target.model.display_name.clone()),
                format!(
                    "model={} context-window mismatch: input exceeds context window ({limit})",
                    target.model.display_name
                ),
            );
            continue;
        }
        let profile = decision
            .execution_profile
            .expect("eligible request decision has a resolved profile");
        execution_profiles.insert(execution_profile_key(&target), profile);
        eligible_targets.push(target);
    }
    targets = eligible_targets;

    // Every Route strategy runs only after predicates, access, capabilities,
    // execution profiles, and context limits have reduced the candidate set.
    if let Some(route) = &route {
        let ordering = order_route_targets(state, route, targets).await;
        targets = ordering.targets;
        if route.strategy == "adaptive" {
            for target in &targets {
                trace_adaptive_quota_evidence(&mut trace, target, &ordering.quota_evidence);
            }
        }
    }

    // Circuit deferral is applied by the shared dispatch planner after Route
    // strategy ordering and before affinity promotion.

    if targets.is_empty() {
        trace.finish("no_eligible_target");
        state
            .live
            .finish(&meta.request_id, "no_eligible_target", 0, None, None);
        let _ = db::insert_route_trace(&state.pool, &trace).await;
        return Err(ProxyError::unsupported(
            "no configured target can satisfy this request (predicates, protocols, capabilities, or limits mismatch)",
        ));
    }

    let max_attempts = route
        .as_ref()
        .and_then(|c| c.max_attempts)
        .map(|m| m as usize)
        .unwrap_or(targets.len())
        .clamp(1, 5);

    // Session target provenance is retained even when sticky/cache affinity is
    // disabled. It lets the portability layer identify a first-attempt provider
    // boundary for opaque continuation state.
    let session_origin_key = session
        .as_deref()
        .and_then(|session| state.sticky_lookup(session, STICKY_TTL));
    let session_origin = session_origin_key
        .as_ref()
        .and_then(|key| resolve_session_origin(key, &route_target_origins));
    let possible_history_origins = if route.is_some() {
        route_target_origins
            .iter()
            .map(|(_, origin)| origin.clone())
            .collect::<Vec<_>>()
    } else {
        targets.iter().map(continuation_origin).collect::<Vec<_>>()
    };
    let initial_origin = targets.first().map(continuation_origin);
    let session_origin_unknown = session_origin_key.is_some() && session_origin.is_none();
    // Anthropic continuation state was produced by successful conversation
    // history, not by any fallback candidate dispatched during this request.
    // Without a session, a multi-target Route has no known producer; only
    // preserve reasoning when every possible producer is explicitly compatible.
    let history_origin = session_origin.as_ref().or_else(|| {
        (!session_origin_unknown && possible_history_origins.len() == 1)
            .then(|| &possible_history_origins[0])
    });

    // The shared planner owns affinity promotion as well as the ordered
    // pre-dispatch frontier. Runtime supplies its realized strategy order and
    // executes the resulting candidate plan.
    let affinity_candidate_id = match (route.as_ref(), session_origin_key.as_ref()) {
        (Some(route), Some(sticky_key))
            if route.cache_affinity != 0 || route.sticky_routing != 0 =>
        {
            targets
                .iter()
                .find(|target| target_key(route, target) == *sticky_key)
                .map(adaptive_candidate_key)
        }
        _ => None,
    };
    let quota_fallback_allowed =
        allow_fallback && route_allows_fallback(route.as_ref(), FailureKind::QuotaExhausted);
    let mut dispatch_candidates = Vec::with_capacity(targets.len());
    for (rank, target) in targets.iter().enumerate() {
        let status = pool::effective_status(&target.account);
        let account_eligible =
            matches!(status, pool::AccountStatus::Healthy) || pool::should_probe(&target.account);
        let account_quota_reached = if account_eligible {
            Some(
                pool::soft_quota_reached(&state.pool, &target.account)
                    .await
                    .unwrap_or(false),
            )
        } else {
            None
        };
        let provider_circuit_available = if account_eligible && account_quota_reached != Some(true)
        {
            Some(
                state
                    .provider_circuits
                    .availability(&target.provider.id)
                    .available,
            )
        } else {
            None
        };
        let facts = crate::pre_dispatch::PreDispatchFacts {
            request_eligible: true,
            account_eligible,
            account_quota_reached,
            quota_fallback_allowed,
            provider_circuit_available,
            route_capacity_available: true,
            quota_override_available: true,
            adaptive_capacity_available: true,
        };
        let id = adaptive_candidate_key(target);
        let defer_for_circuit = matches!(status, pool::AccountStatus::CircuitOpen)
            && !pool::should_probe(&target.account);
        dispatch_candidates.push(crate::pre_dispatch::DispatchCandidate {
            id,
            rank: Some(rank),
            group_id: target.route_target_id.clone(),
            account_priority: target.account.priority,
            weight: target.weight,
            defer_for_circuit,
            facts,
        });
    }
    let dispatch_plan = crate::pre_dispatch::plan_dispatch(
        &dispatch_candidates,
        route.is_some(),
        if route
            .as_ref()
            .is_some_and(|route| route.strategy == "weighted")
        {
            crate::pre_dispatch::DispatchStrategy::Weighted
        } else {
            crate::pre_dispatch::DispatchStrategy::Ordered
        },
        affinity_candidate_id.as_deref(),
        false,
    );
    let dispatch_records_by_id: std::collections::HashMap<_, _> = dispatch_plan
        .candidates
        .iter()
        .map(|record| (record.candidate_id.clone(), record.clone()))
        .collect();
    let dispatch_facts_by_id: std::collections::HashMap<_, _> = dispatch_candidates
        .iter()
        .map(|candidate| (candidate.id.clone(), candidate.facts))
        .collect();
    let mut targets_by_id: std::collections::HashMap<_, _> = targets
        .into_iter()
        .map(|target| (adaptive_candidate_key(&target), target))
        .collect();
    let mut pending_targets: std::collections::VecDeque<_> = dispatch_plan
        .ordered_candidate_ids
        .iter()
        .filter_map(|candidate_id| targets_by_id.remove(candidate_id))
        .collect();
    if let Some(affinity_id) = affinity_candidate_id.as_deref() {
        if pending_targets
            .front()
            .is_some_and(|target| adaptive_candidate_key(target) == affinity_id)
        {
            let target = pending_targets.front().expect("affinity target exists");
            trace.step(
                "candidate",
                Some(target.account.label.clone()),
                if route
                    .as_ref()
                    .is_some_and(|route| route.sticky_routing != 0)
                {
                    "sticky-routing: previous session target promoted (FR-7.5)"
                } else {
                    "cache-affinity: session target promoted (FR-7.3)"
                },
            );
        }
    }
    for record in &dispatch_plan.candidates {
        let Some(target) = targets_by_id.get(&record.candidate_id).or_else(|| {
            pending_targets
                .iter()
                .find(|target| adaptive_candidate_key(target) == record.candidate_id)
        }) else {
            continue;
        };
        trace.step(
            "candidate",
            Some(target.account.label.clone()),
            format!(
                "dispatch plan rank={:?} decision={:?} reason={}",
                record.strategy_rank, record.decision, record.decision_reason
            ),
        );
    }
    trace.step(
        "candidate",
        None,
        format!("dispatch frontier outcome={:?}", dispatch_plan.outcome),
    );

    // 3. Attempt loop: all fallback happens before any client bytes.
    let mut last_error: Option<ProxyError> = None;
    let mut all_accounts: Vec<db::AccountRow> = Vec::new();
    let mut attempts_done = 0usize;
    let mut previous_origin: Option<ContinuationOrigin> = None;
    let mut skip_logical_target: Option<String> = None;
    let deadline = started + MAX_PRE_COMMIT_DEADLINE;
    let mut auth_retried_accounts = std::collections::HashSet::new();
    let mut last_precommit_failure: Option<LastPrecommitFailure> = None;

    while let Some(target_owned) = pending_targets.pop_front() {
        let target = &target_owned;
        if let (Some(skip), Some(target_id)) = (
            skip_logical_target.as_deref(),
            target.route_target_id.as_deref(),
        ) {
            if skip == target_id {
                trace.step(
                    "skip",
                    Some(target.account.label.clone()),
                    "same logical target skipped after target-local failure",
                );
                continue;
            }
        }
        if attempts_done >= max_attempts || Instant::now() >= deadline {
            if Instant::now() >= deadline {
                trace.step("skip", None, "pre-commit deadline exceeded");
            }
            break;
        }
        all_accounts.push(target.account.clone());

        let candidate_id = adaptive_candidate_key(target);
        let dispatch_record = dispatch_records_by_id
            .get(&candidate_id)
            .expect("runtime candidate has a dispatch-plan record");
        let status = pool::effective_status(&target.account);
        if dispatch_record.decision == crate::pre_dispatch::PreDispatchDecision::AccountUnavailable
        {
            let why = account_skip_detail(target, status);
            meta.fallback_path
                .push(format!("{}:{why}", target.account.label));
            trace.step("skip", Some(target.account.label.clone()), why);
            state.record_skip();
            continue;
        }
        let probing = !matches!(status, pool::AccountStatus::Healthy);
        if probing {
            // Half-open recovery probe (FR-4.7): an account whose circuit has
            // opened may be retried once per bounded interval so it can recover
            // without an operator action. Success clears the circuit; failure
            // re-arms it. The probe is a normal attempt (so it refreshes account
            // state) rather than a synthetic one.
            trace.step(
                "candidate",
                Some(target.account.label.clone()),
                format!("half-open recovery probe ({})", status.as_str()),
            );
        }

        // Keep the dispatch plan's ordering and explanations, but refresh
        // mutable account quota at the execution gate before transport checks.
        // The canonical planner still evaluates quota before provider circuit.
        let planning_facts = *dispatch_facts_by_id
            .get(&candidate_id)
            .expect("runtime candidate has dispatch facts");
        let live_decision =
            plan_runtime_candidate(&state.pool, &target.account, planning_facts).await;
        if live_decision != dispatch_record.decision {
            trace.step(
                "candidate",
                Some(target.account.label.clone()),
                format!(
                    "dispatch plan refreshed decision={live_decision:?} (planned={:?})",
                    dispatch_record.decision
                ),
            );
        }
        match live_decision {
            crate::pre_dispatch::PreDispatchDecision::SkipAfterQuota
            | crate::pre_dispatch::PreDispatchDecision::RateLimited => {
                let reset_at = (chrono::Utc::now()
                    + chrono::Duration::seconds(default_quota_window(&target.account)))
                .to_rfc3339();
                let _ = db::set_account_status_if_version(
                    &state.pool,
                    &target.account.id,
                    target.account.account_state_version,
                    "exhausted",
                    "account_quota_exhausted",
                    None,
                    Some(&reset_at),
                    Some("soft quota reached"),
                )
                .await;
                meta.fallback_path
                    .push(format!("{}:soft_quota", target.account.label));
                trace.step(
                    "skip",
                    Some(target.account.label.clone()),
                    format!("model={} soft quota reached", target.model.display_name),
                );
                state.record_skip();
                if live_decision == crate::pre_dispatch::PreDispatchDecision::RateLimited {
                    trace.finish("failed");
                    state.live.finish(
                        &meta.request_id,
                        "failed",
                        started.elapsed().as_millis() as u64,
                        None,
                        None,
                    );
                    let _ = db::insert_route_trace(&state.pool, &trace).await;
                    return Err(ProxyError::rate_limited("account soft quota reached", None));
                }
                continue;
            }
            crate::pre_dispatch::PreDispatchDecision::CheckProviderCircuit
            | crate::pre_dispatch::PreDispatchDecision::ProviderCircuitUnavailable
            | crate::pre_dispatch::PreDispatchDecision::Dispatchable => {}
            other => unreachable!("unexpected dispatch-plan result: {other:?}"),
        }

        // Resolve target transport and its execution metadata before any
        // target-specific credential lookup or network dispatch.
        let profile = execution_profiles
            .get(&execution_profile_key(target))
            .cloned()
            .expect("eligible target has a resolved execution profile");
        trace.resolved_transport(
            target.model.display_name.clone(),
            profile.transport.as_str(),
        );
        let adapter = match state.adapters.for_transport(&profile.transport) {
            Ok(adapter) => adapter,
            Err(error) => {
                trace.step(
                    "skip",
                    Some(target.model.display_name.clone()),
                    format!("resolved adapter unavailable: {}", error.message),
                );
                last_error = Some(error);
                continue;
            }
        };
        if !provider_satisfies_needs(&target.provider, &profile.capabilities, &needs) {
            trace.step(
                "skip",
                Some(target.model.display_name.clone()),
                "resolved target capabilities do not satisfy the request",
            );
            last_error = Some(ProxyError::unsupported(
                "resolved target capabilities do not satisfy the request",
            ));
            continue;
        }

        // Check provider eligibility before credential work, but do not reserve
        // the exclusive HALF_OPEN network probe until immediately before dispatch.
        // This is an execution-time availability check, not another selection
        // pass. The canonical plan already fixed candidate order and fallback
        // policy; this check only catches circuit changes before credential work.
        if let Err(reject) = state.provider_circuits.check_available(&target.provider.id) {
            last_error = Some(record_provider_circuit_reject(
                state, target, &mut meta, &mut trace, reject,
            ));
            continue;
        }
        let correlation_policy = if target.provider.credential_mode == "none" {
            crate::provider_circuit::ProviderCorrelationPolicy::AccountlessTargets
        } else {
            crate::provider_circuit::ProviderCorrelationPolicy::DistinctAccounts
        };

        // Credential. A provider bound to a plugin credential strategy
        // (§6.0) resolves through the plugin; otherwise the built-in static
        // strategy is used. A disabled/unavailable plugin fails closed.
        let credential = match state
            .credential_for(&target.provider, &target.account)
            .await
        {
            Ok(c) => {
                crate::alerts::record_credential_success();
                c.secret
            }
            Err(error) => {
                tracing::error!(
                    account = %target.account.id,
                    code = %error.code,
                    error = %error,
                    "credential resolution failed"
                );
                crate::alerts::record_credential_failure();
                last_error = Some(ProxyError::internal("credential unavailable"));
                match state
                    .disable_invalid_credential(
                        &target.account,
                        &error,
                        "request credential resolution",
                    )
                    .await
                {
                    Ok(true) => {
                        let detail =
                            format!("{}:credential_invalid(disabled)", target.account.label);
                        meta.fallback_path.push(detail.clone());
                        trace.step("skip", Some(target.account.label.clone()), detail);
                    }
                    Ok(false) => trace.step(
                        "skip",
                        Some(target.account.label.clone()),
                        "credential unavailable",
                    ),
                    Err(disable_error) => {
                        tracing::error!(
                            account = %target.account.id,
                            credential_error = %error,
                            %disable_error,
                            "failed to disable account after request credential resolution confirmed invalid"
                        );
                        trace.step(
                            "skip",
                            Some(target.account.label.clone()),
                            "credential invalid; account disable failed",
                        );
                    }
                }
                continue;
            }
        };

        // Every target gets an isolated request view. Target overrides and
        // continuity transforms must never leak into a later fallback target.
        let mut target_req = req.clone();

        // Portability applies on cross-format translation, on fallback across
        // providers, and on the first attempt when session provenance shows the
        // previous turn came from a different provider.
        let attempt_origin = previous_origin
            .as_ref()
            .or(if attempts_done == 0 {
                session_origin.as_ref()
            } else {
                None
            })
            .or(initial_origin.as_ref());
        let cross_provider =
            attempt_origin.is_some_and(|origin| origin.provider_id != target.provider.id);
        let cross_format = !passthrough::is_transport_passthrough(format, &profile.transport);

        // Anthropic continuation is safe for the same provider/model, or when
        // both models explicitly share an operator-declared continuation family.
        // Wire-format and upstream model-name equality alone are insufficient.
        let history_is_compatible = !session_origin_unknown
            && history_origin.map_or_else(
                || {
                    possible_history_origins
                        .iter()
                        .all(|origin| anthropic_continuation_is_compatible(Some(origin), target))
                },
                |origin| anthropic_continuation_is_compatible(Some(origin), target),
            );
        let preserves_anthropic_thinking = format == FrontendFormat::Anthropic
            && adapter.wire_format() == "anthropic"
            && history_is_compatible;
        // Evaluate inline opaque state before hydration so a signature about to
        // be restored for a compatible target is not mistaken for client state.
        let inline_opaque = request_has_nonportable_inline_state(
            &target_req,
            preserves_anthropic_thinking,
            cross_provider || cross_format,
        );

        // Resolve host-owned stored continuation state for this candidate
        // target. Compatible signatures are restored only after portability is
        // settled (below); incompatible stored state feeds the portability
        // decision exactly like inline state.
        let opaque_report = resolve_opaque_state(
            state,
            &target_req,
            key.as_ref(),
            session.as_deref(),
            target,
            adapter.as_ref(),
        )
        .await?;

        // `opaque_report.nonportable()` must independently enter portability
        // handling: stored state can be incompatible with a target even when
        // neither cross_provider nor cross_format is set (e.g. an OpenAI
        // client whose earlier Gemini turn stored a signature, now routed to
        // an OpenAI target with no session provenance). Without this, the
        // Route's reject/strip_with_warning policy would be bypassed and the
        // state silently dropped.
        if cross_provider || cross_format || inline_opaque || opaque_report.nonportable() {
            // A target that cannot carry the real stored state may still have a
            // documented placeholder for it (e.g. Gemini's cross-model
            // sentinel). The adapter owns that wire detail; the pipeline only
            // decides whether the Route policy allows proceeding.
            let placeholder = adapter.opaque_state_placeholder(&target.model);
            if let Some(route) = &route {
                if let Err(error) = apply_portability(
                    &mut target_req,
                    route,
                    target,
                    inline_opaque,
                    &opaque_report,
                    placeholder,
                    preserves_anthropic_thinking,
                    &mut trace,
                ) {
                    return Err(finish_policy_rejection(
                        state,
                        &meta,
                        &mut trace,
                        Some(target.account.label.clone()),
                        error.message,
                        started,
                    )
                    .await);
                }
            } else if let Some(placeholder) =
                placeholder.filter(|_| opaque_report.nonportable() && !inline_opaque)
            {
                // A direct target has no Route policy to authorize dropping the
                // state, but the target adapter documents a protocol-valid
                // substitute for the stored state it cannot carry (e.g. Gemini's
                // cross-model function-call placeholder). Translate with it
                // rather than failing the request; never strip silently.
                let placed = strip_nonportable_state(
                    &mut target_req,
                    &opaque_report,
                    Some(placeholder),
                    preserves_anthropic_thinking,
                );
                let warning = portability_warning(target, placed);
                trace.warn(warning.clone());
                tracing::warn!("{}", warning);
            } else if inline_opaque || opaque_report.nonportable() {
                // A direct target with no Route policy must not silently drop
                // known non-portable continuation state (§24).
                return Err(
                    finish_policy_rejection(
                        state,
                        &meta,
                        &mut trace,
                        Some(target.account.label.clone()),
                        "non-portable provider continuation state cannot be sent to a direct cross-format target",
                        started,
                    )
                    .await,
                );
            }
        }

        hydrate_opaque_state(&mut target_req, &opaque_report);
        if opaque_report.known_state() {
            // Coarse, secret-free diagnostics (§35/§36): counts and kinds only,
            // never a signature, tool id, or session value.
            state.flight.record(
                &meta.request_id,
                started.elapsed().as_millis() as u64,
                "opaque_state_lookup",
                format!(
                    "restored={} incompatible={} unavailable={}",
                    opaque_report.restored, opaque_report.incompatible, opaque_report.unavailable
                ),
            );
            if opaque_report.restored > 0 {
                trace.step(
                    "candidate",
                    Some(target.model.display_name.clone()),
                    format!(
                        "restored {} opaque provider continuation signature(s)",
                        opaque_report.restored
                    ),
                );
            }
        }

        apply_target_overrides(&mut target_req, &target.param_overrides)?;
        if target
            .param_overrides
            .as_object()
            .is_some_and(|obj| !obj.is_empty())
        {
            trace.step(
                "candidate",
                Some(target.account.label.clone()),
                "applied target parameter overrides",
            );
        }

        let mut execution_model = target.model.clone();
        execution_model.thinking_map = serde_json::to_string(&profile.thinking_map)
            .expect("ThinkingMap serialization is infallible");
        let session_context = meta.session.clone();
        let ctx = UpstreamContext {
            provider: &target.provider,
            model: &execution_model,
            account_id: Some(target.account.id.as_str()),
            session_context: session_context.as_deref(),
            credential,
        };

        // Parameter policy reject (FR-10.6): a request-level failure, never retried.
        if let Err(error) = check_param_policy(target, &profile, &target_req) {
            return Err(finish_policy_rejection(
                state,
                &meta,
                &mut trace,
                Some(target.account.label.clone()),
                error.message,
                started,
            )
            .await);
        }

        // Same-format passthrough (FR-2.7).
        let use_passthrough = target_req.raw_body.is_some()
            && passthrough::is_transport_passthrough(format, &profile.transport);

        // Never silently drop behaviorally significant client fields on a
        // translating path (FR-2.8). A request-level failure; never retried.
        if !use_passthrough {
            if let Some(msg) = crate::frontends::translation_unsupported(&target_req.extra) {
                return Err(finish_policy_rejection(
                    state,
                    &meta,
                    &mut trace,
                    Some(target.account.label.clone()),
                    format!("request uses a feature that cannot be translated: {msg}"),
                    started,
                )
                .await);
            }
            if let Err(error) =
                check_resolved_thinking_translation(adapter.as_ref(), target, &profile, &target_req)
            {
                return Err(finish_policy_rejection(
                    state,
                    &meta,
                    &mut trace,
                    Some(target.account.label.clone()),
                    error.message,
                    started,
                )
                .await);
            }
        }

        // Bounded exponential backoff between attempts (FR-4.4). Never applied
        // before the first attempt, and capped so a healthy pool is not slowed.
        if attempts_done > 0 {
            let exp = BACKOFF_BASE_MS.saturating_mul(1u64 << (attempts_done - 1).min(4));
            tokio::time::sleep(Duration::from_millis(exp.min(BACKOFF_CAP_MS))).await;
        }

        // Adaptive upstream concurrency is opt-in with the adaptive route
        // strategy, so existing route strategies keep their exact semantics.
        // An affine target gets a short bounded wait before spillover to protect
        // prompt-cache/session locality from transient saturation.
        let traffic_permit = if route
            .as_ref()
            .is_some_and(|route| route.strategy == "adaptive")
        {
            let key = traffic_key(target);
            let affinity = route.as_ref().is_some_and(|route| {
                (route.cache_affinity != 0 || route.sticky_routing != 0)
                    && session_origin_key
                        .as_ref()
                        .is_some_and(|sticky| target_key(route, target) == *sticky)
            });
            let wait = if affinity {
                Duration::from_millis(300)
            } else {
                Duration::ZERO
            };
            match state.upstream_traffic.acquire(key, wait).await {
                Ok(permit) => Some(permit),
                Err(snapshot) => {
                    let detail = format!(
                        "adaptive-concurrency: saturated (inflight={}, limit={})",
                        snapshot.inflight, snapshot.estimated_limit
                    );
                    trace.step("skip", Some(target.account.label.clone()), detail.clone());
                    meta.fallback_path
                        .push(format!("{}:adaptive_saturated", target.account.label));
                    state.record_skip();
                    state.target_telemetry.record(
                        crate::target_telemetry::TelemetryEvent::synthetic(
                            traffic_key(target),
                            crate::target_telemetry::TelemetryOutcome::AdaptiveSaturation,
                        ),
                    );
                    last_error = Some(ProxyError::all_unavailable(detail, None));
                    continue;
                }
            }
        } else {
            None
        };

        // Kinetix-attributable dispatch overhead (auth, limits, routing,
        // predicates, body build) — recorded for the p95 added-latency alert
        // (Monitoring). Excludes upstream network time.
        crate::alerts::record_added_latency(started.elapsed().as_millis() as u64);

        let provider_timeout = provider_phase_timeout(&target.provider);
        let Some((phase_deadline, send_budget)) = phase_budget(deadline, provider_timeout) else {
            trace.step("skip", None, "pre-commit deadline exceeded");
            break;
        };

        // Reserve HALF_OPEN exclusively only after all local validation,
        // credential resolution, adaptive admission, and deadline checks pass.
        let provider_attempt = match state.provider_circuits.begin_attempt_with_policy(
            &target.provider.id,
            &target.account.id,
            provider_correlation_target_id(target),
            correlation_policy,
        ) {
            Ok(attempt) => attempt,
            Err(reject) => {
                last_error = Some(record_provider_circuit_reject(
                    state, target, &mut meta, &mut trace, reject,
                ));
                continue;
            }
        };

        if probing {
            match db::claim_half_open_probe_at(
                &state.pool,
                &target.account.id,
                target.account.account_state_version,
                chrono::Utc::now(),
                pool::HALF_OPEN_PROBE_MIN_GAP_SECS,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => {
                    provider_attempt.finish_neutral();
                    let detail =
                        "half-open recovery probe already claimed or account state changed";
                    trace.step("skip", Some(target.account.label.clone()), detail);
                    meta.fallback_path
                        .push(format!("{}:probe_claimed", target.account.label));
                    state.record_skip();
                    last_error = Some(ProxyError::all_unavailable(detail, None));
                    continue;
                }
                Err(error) => {
                    provider_attempt.finish_neutral();
                    tracing::warn!(
                        account_id = %target.account.id,
                        error = %error,
                        "failed to claim half-open account probe"
                    );
                    let detail = "half-open recovery probe could not be claimed";
                    trace.step("skip", Some(target.account.label.clone()), detail);
                    state.record_skip();
                    last_error = Some(ProxyError::all_unavailable(detail, None));
                    continue;
                }
            }
        }

        let attempt_started = Instant::now();
        attempts_done += 1;
        previous_origin = Some(continuation_origin(target));
        state.live.set_fallback_hops(
            &meta.request_id,
            (attempts_done - 1) as u32,
            (attempts_done - 1) as u32,
        );
        state.flight.record(
            &meta.request_id,
            started.elapsed().as_millis() as u64,
            "upstream_connect",
            format!(
                "{} via {}{}",
                target.model.upstream_id,
                profile.transport.as_str(),
                if use_passthrough {
                    " (passthrough)"
                } else {
                    ""
                }
            ),
        );

        meta.active_precommit_attempt = Some(ActivePrecommitAttempt {
            target: target.clone(),
            attempt_number: meta.partial_attempts.len() + 1,
            opaque_route_id: trace.opaque_route_id.clone(),
            upstream_request_id: None,
        });
        let send_result = match tokio::time::timeout(
            send_budget,
            send_upstream(
                state,
                &adapter,
                &ctx,
                &target_req,
                use_passthrough,
                &meta.request_id,
                &protocol_headers,
                &meta.upstream_dispatched,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(timeout_failure(
                "upstream timed out before response headers",
            )),
        };

        match send_result {
            Ok(resp) => {
                let quota_observation = state.quota.observe_headers(
                    &target.provider.id,
                    &target.account.id,
                    resp.headers(),
                );
                if resp.status().is_success() {
                    let upstream_request_id = extract_upstream_request_id(&resp);
                    if let Some(active) = meta.active_precommit_attempt.as_mut() {
                        active.upstream_request_id = upstream_request_id.clone();
                    }
                    let responses = if format == FrontendFormat::OpenAiResponses {
                        match frontends::responses::response_fields_from_request(&target_req) {
                            Ok(fields) => fields,
                            Err(error) => {
                                return Ok(crate::api::error_response(
                                    format,
                                    &meta.request_id,
                                    error,
                                ));
                            }
                        }
                    } else {
                        frontends::ResponsesResponseFields::default()
                    };
                    let encoder_ctx = EncoderCtx {
                        model_name: target_req.requested_model.clone(),
                        request_id: meta.request_id.clone(),
                        created: chrono::Utc::now().timestamp(),
                        responses,
                    };
                    let first_event_remaining =
                        phase_deadline.saturating_duration_since(Instant::now());
                    let is_sse = resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .split(';')
                        .next()
                        .map(str::trim)
                        == Some("text/event-stream");
                    let aggregate_sse = !req.stream && is_sse;
                    let prepared_result = if first_event_remaining.is_zero() {
                        Err(PreparedResponseFailure::from(timeout_failure(
                            "upstream timed out before first valid event",
                        )))
                    } else if aggregate_sse {
                        prepare_success_response(
                            resp,
                            &adapter,
                            true,
                            format,
                            use_passthrough,
                            &encoder_ctx,
                            first_event_remaining,
                            provider_timeout,
                        )
                        .await
                    } else if is_sse {
                        prepare_success_response(
                            resp,
                            &adapter,
                            false,
                            format,
                            use_passthrough,
                            &encoder_ctx,
                            first_event_remaining,
                            provider_timeout,
                        )
                        .await
                    } else {
                        match tokio::time::timeout(
                            first_event_remaining,
                            prepare_success_response(
                                resp,
                                &adapter,
                                false,
                                format,
                                use_passthrough,
                                &encoder_ctx,
                                first_event_remaining,
                                provider_timeout,
                            ),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(PreparedResponseFailure::from(timeout_failure(
                                "upstream timed out before first valid event",
                            ))),
                        }
                    };
                    let prepared = match prepared_result {
                        Ok(prepared) => prepared,
                        Err(prepared_failure) => {
                            let PreparedResponseFailure {
                                failure,
                                outcome,
                                precommit_usage,
                            } = prepared_failure;
                            let termination = StreamTermination::new(
                                outcome,
                                CommitState::PreCommit,
                                Some(failure.kind),
                                failure.status,
                            );
                            record_precommit_attempt_usage(
                                state,
                                &mut meta,
                                target,
                                key.as_ref(),
                                &trace,
                                upstream_request_id.as_deref(),
                                precommit_usage,
                                &failure,
                                termination,
                            )
                            .await;
                            if let Some(permit) = traffic_permit.as_ref() {
                                permit.finish(traffic_outcome_for_failure(failure.kind));
                            }
                            state.flight.record(
                                &meta.request_id,
                                started.elapsed().as_millis() as u64,
                                "upstream_precommit_invalid",
                                failure.message.clone(),
                            );
                            handle_key_failure(state, target, &failure, &mut meta, &mut trace)
                                .await;
                            let can_fallback = termination.fallback_allowed(|kind| {
                                allow_fallback && route_allows_fallback(route.as_ref(), kind)
                            });
                            trace.stream_termination(termination, Some(can_fallback));
                            if failure.kind == FailureKind::QuotaExhausted {
                                state.quota.observe_exhausted(
                                    &target.provider.id,
                                    &target.account.id,
                                    failure.quota_reset_at,
                                    "upstream_error",
                                );
                            }
                            let circuit_transition =
                                provider_attempt.finish_failure(failure.kind, failure.status);
                            record_target_telemetry(
                                state,
                                target,
                                telemetry_outcome_for_failure(failure.kind),
                                attempt_started,
                                None,
                                attempts_done > 1,
                                can_fallback,
                                circuit_transition,
                                &mut trace,
                            );
                            if !can_fallback {
                                trace.step(
                                    "attempt",
                                    Some(target.account.label.clone()),
                                    format!(
                                        "HTTP 2xx invalid before commit; fallback disabled for {:?}",
                                        failure.kind
                                    ),
                                );
                                trace.finish("failed");
                                state.live.finish(
                                    &meta.request_id,
                                    "failed",
                                    started.elapsed().as_millis() as u64,
                                    None,
                                    None,
                                );
                                let client_error = failure_to_error(&failure, target);
                                record_precommit_request_log(
                                    state,
                                    &meta,
                                    &req,
                                    target,
                                    key.as_ref(),
                                    &trace,
                                    upstream_request_id.as_deref(),
                                    &failure,
                                    &client_error,
                                    termination,
                                    started,
                                    attempts_done,
                                );
                                let _ = db::insert_route_trace(&state.pool, &trace).await;
                                enqueue_precommit_failure(
                                    state,
                                    &req,
                                    &mut meta,
                                    started,
                                    attempts_done,
                                    &client_error,
                                );
                                return Err(client_error);
                            }
                            let client_error = failure_to_error(&failure, target);
                            last_precommit_failure = Some(LastPrecommitFailure {
                                target: (*target).clone(),
                                upstream_request_id: upstream_request_id.clone(),
                                failure: failure.clone(),
                                termination,
                                attempts_done,
                                client_error: client_error.clone(),
                            });
                            if failure.kind == FailureKind::TargetError {
                                skip_logical_target = target.route_target_id.clone();
                            }
                            last_error = Some(client_error);
                            continue;
                        }
                    };

                    // A full JSON upstream response has no separable first
                    // event; using its total body latency would make generation
                    // length look like congestion. Only streaming responses train
                    // the TTFT controller.
                    if prepared.is_sse {
                        if let Some(permit) = traffic_permit.as_ref() {
                            permit.mark_first_event(attempt_started.elapsed());
                        }
                    }

                    meta.fallback_hops = (attempts_done - 1) as i64;
                    meta.retry_count = (attempts_done - 1) as i64;
                    if attempts_done > 1 {
                        state.record_fallback();
                    }
                    meta.fallback_path
                        .push(format!("{}:validated", target.account.label));
                    trace.step(
                        "attempt",
                        Some(target.account.label.clone()),
                        format!("HTTP 2xx validated (attempt {attempts_done})"),
                    );
                    trace.final_target = Some(format!(
                        "{} @ {}",
                        target.model.display_name, target.account.label
                    ));
                    state.flight.record(
                        &meta.request_id,
                        started.elapsed().as_millis() as u64,
                        "upstream_validated",
                        if prepared.is_sse { "sse" } else { "json" },
                    );
                    let provider_circuit_transition =
                        mark_provider_probe_validated(&provider_attempt);
                    let attempt = Attempt {
                        target: target.clone(),
                        upstream_request_id,
                        stream: prepared.stream,
                        prefetched: prepared.prefetched,
                        full_events: prepared.full_events,
                        precommit_usage: prepared.precommit_usage,
                        idle_timeout: provider_timeout,
                        adapter: adapter.clone(),
                        is_sse: prepared.is_sse,
                        passthrough: use_passthrough && prepared.is_sse,
                        traffic_permit,
                        provider_attempt,
                        provider_circuit_transition,
                        account_probe: probing,
                        attempt_started,
                    };
                    return Ok(stream_response(
                        state,
                        snap,
                        format,
                        meta,
                        target_req,
                        attempt,
                        encoder_ctx,
                        started,
                        key,
                        trace,
                    )
                    .await);
                }

                // Classify and maybe fail over. Reading an error body is still
                // part of the pre-commit phase and cannot outlive its budget.
                let upstream_request_id = extract_upstream_request_id(&resp);
                if let Some(active) = meta.active_precommit_attempt.as_mut() {
                    active.upstream_request_id = upstream_request_id.clone();
                }
                let status = resp.status().as_u16();
                let headers = resp.headers().clone();
                let error_body_remaining = phase_deadline.saturating_duration_since(Instant::now());
                let mut upstream_error_body: Option<String> = None;
                let failure = if error_body_remaining.is_zero() {
                    timeout_failure("upstream timed out while reading error response")
                } else {
                    match tokio::time::timeout(error_body_remaining, resp.text()).await {
                        Ok(Ok(body)) => {
                            let native = adapter.classify_error(status, &body, &headers);
                            let failure = apply_provider_failure_rules(
                                &target.provider,
                                status,
                                &body,
                                native,
                            );
                            upstream_error_body = Some(body);
                            failure
                        }
                        Ok(Err(error)) => UpstreamFailure {
                            kind: if error.is_timeout() {
                                FailureKind::Timeout
                            } else {
                                FailureKind::ConnectionError
                            },
                            status: Some(status),
                            retry_after_secs: None,
                            message: classify_reqwest(&error),
                            quota_reset_at: None,
                        },
                        Err(_) => {
                            timeout_failure("upstream timed out while reading error response")
                        }
                    }
                };
                let failure = apply_header_reset_to_rate_limit(failure, quota_observation);
                let termination = StreamTermination::new(
                    stream_outcome_for_failure(failure.kind),
                    CommitState::PreCommit,
                    Some(failure.kind),
                    failure.status,
                );
                let client_error = preserve_anthropic_error(
                    failure_to_error(&failure, target),
                    format,
                    adapter.as_ref(),
                    &failure,
                    status,
                    &headers,
                    upstream_error_body.as_deref(),
                );
                record_precommit_attempt_usage(
                    state,
                    &mut meta,
                    target,
                    key.as_ref(),
                    &trace,
                    upstream_request_id.as_deref(),
                    TokenUsage::default(),
                    &failure,
                    termination,
                )
                .await;
                last_precommit_failure = Some(LastPrecommitFailure {
                    target: (*target).clone(),
                    upstream_request_id: upstream_request_id.clone(),
                    failure: failure.clone(),
                    termination,
                    attempts_done,
                    client_error: client_error.clone(),
                });
                if let Some(permit) = traffic_permit.as_ref() {
                    permit.finish(traffic_outcome_for_failure(failure.kind));
                }
                if failure.kind == FailureKind::QuotaExhausted {
                    state.quota.observe_exhausted(
                        &target.provider.id,
                        &target.account.id,
                        failure.quota_reset_at,
                        "upstream_error",
                    );
                }
                let circuit_transition =
                    provider_attempt.finish_failure(failure.kind, failure.status);
                state.flight.record(
                    &meta.request_id,
                    started.elapsed().as_millis() as u64,
                    "upstream_error",
                    format!("HTTP {status} {:?}", failure.kind),
                );

                if failure.kind == FailureKind::AuthError
                    && !auth_retried_accounts.contains(&target.account.id)
                {
                    match state
                        .rotate_credential_after_auth_error(
                            &target.provider,
                            &target.account,
                            &ctx.credential,
                        )
                        .await
                    {
                        Ok(true) => {
                            record_target_telemetry(
                                state,
                                target,
                                telemetry_outcome_for_failure(failure.kind),
                                attempt_started,
                                None,
                                attempts_done > 1,
                                false,
                                circuit_transition,
                                &mut trace,
                            );
                            auth_retried_accounts.insert(target.account.id.clone());
                            // Credential renewal is part of this logical target
                            // attempt, not a route fallback hop.
                            attempts_done = attempts_done.saturating_sub(1);
                            pending_targets.push_front(target_owned.clone());
                            trace.step(
                                "attempt",
                                Some(target.account.label.clone()),
                                "auth failed; refreshed plugin credential and retrying same target",
                            );
                            state.flight.record(
                                &meta.request_id,
                                started.elapsed().as_millis() as u64,
                                "credential_rotated_retry",
                                target.account.label.clone(),
                            );
                            continue;
                        }
                        Ok(false) => {}
                        Err(error) if error.invalid_credential() => {
                            crate::alerts::record_credential_failure();
                            tracing::warn!(
                                account = %target.account.id,
                                code = %error.code,
                                "credential rotation confirmed a non-retryable invalid credential"
                            );
                            // Fall through to the original AuthError handling,
                            // which disables the account and tries another one.
                        }
                        Err(error) => {
                            crate::alerts::record_credential_failure();
                            let cooldown = error
                                .retry_after_secs
                                .unwrap_or(if error.retryable { 5 } else { 30 })
                                .min(3600);
                            let message = format!("credential refresh failed: {}", error.message);
                            let cooldown_until = (chrono::Utc::now()
                                + chrono::Duration::seconds(cooldown as i64))
                            .to_rfc3339();
                            let cooldown_applied = match db::set_account_status_if_version(
                                &state.pool,
                                &target.account.id,
                                target.account.account_state_version,
                                "cooldown",
                                "rate_limited",
                                Some(&cooldown_until),
                                None,
                                Some(&crate::crypto::redact(&message)),
                            )
                            .await
                            {
                                Ok(applied) => applied,
                                Err(persist_error) => {
                                    tracing::error!(
                                        account = %target.account.id,
                                        error = %persist_error,
                                        "failed to persist credential refresh cooldown"
                                    );
                                    false
                                }
                            };
                            if cooldown_applied {
                                let _ = state.registry.reload(&state.pool).await;
                            }
                            let detail = if cooldown_applied {
                                format!(
                                    "{}:credential_refresh(cooldown {}s)",
                                    target.account.label, cooldown
                                )
                            } else {
                                format!(
                                    "{}:credential_refresh(stale lifecycle update ignored)",
                                    target.account.label
                                )
                            };
                            meta.fallback_path.push(detail.clone());
                            trace.step("attempt", Some(target.account.label.clone()), detail);
                            tracing::warn!(
                                account = %target.account.id,
                                code = %error.code,
                                retryable = error.retryable,
                                cooldown,
                                "credential rotation failed; cooling down account"
                            );

                            let refresh_error = ProxyError::all_unavailable(
                                "credential refresh temporarily unavailable",
                                Some(cooldown),
                            );
                            if let Some(last_failure) = last_precommit_failure.as_mut() {
                                last_failure.client_error = refresh_error.clone();
                            }
                            let can_fallback = allow_fallback
                                && route_allows_fallback(route.as_ref(), FailureKind::AuthError);
                            trace.stream_termination(termination, Some(can_fallback));
                            record_target_telemetry(
                                state,
                                target,
                                telemetry_outcome_for_failure(failure.kind),
                                attempt_started,
                                None,
                                attempts_done > 1,
                                can_fallback,
                                circuit_transition,
                                &mut trace,
                            );
                            if !can_fallback {
                                trace.finish("failed");
                                state.live.finish(
                                    &meta.request_id,
                                    "failed",
                                    started.elapsed().as_millis() as u64,
                                    None,
                                    None,
                                );
                                record_precommit_request_log(
                                    state,
                                    &meta,
                                    &req,
                                    target,
                                    key.as_ref(),
                                    &trace,
                                    upstream_request_id.as_deref(),
                                    &failure,
                                    &refresh_error,
                                    termination,
                                    started,
                                    attempts_done,
                                );
                                let _ = db::insert_route_trace(&state.pool, &trace).await;
                                enqueue_precommit_failure(
                                    state,
                                    &req,
                                    &mut meta,
                                    started,
                                    attempts_done,
                                    &refresh_error,
                                );
                                return Err(refresh_error);
                            }
                            last_error = Some(refresh_error);
                            continue;
                        }
                    }
                }

                handle_key_failure(state, target, &failure, &mut meta, &mut trace).await;
                let can_fallback =
                    allow_fallback && route_allows_fallback(route.as_ref(), failure.kind);
                trace.stream_termination(termination, Some(can_fallback));
                record_target_telemetry(
                    state,
                    target,
                    telemetry_outcome_for_failure(failure.kind),
                    attempt_started,
                    None,
                    attempts_done > 1,
                    can_fallback,
                    circuit_transition,
                    &mut trace,
                );
                if !can_fallback {
                    trace.step(
                        "attempt",
                        Some(target.account.label.clone()),
                        format!("HTTP {status}; fallback disabled for {:?}", failure.kind),
                    );
                    trace.finish("failed");
                    state.live.finish(
                        &meta.request_id,
                        "failed",
                        started.elapsed().as_millis() as u64,
                        None,
                        None,
                    );
                    record_precommit_request_log(
                        state,
                        &meta,
                        &req,
                        target,
                        key.as_ref(),
                        &trace,
                        upstream_request_id.as_deref(),
                        &failure,
                        &client_error,
                        termination,
                        started,
                        attempts_done,
                    );
                    let _ = db::insert_route_trace(&state.pool, &trace).await;
                    enqueue_precommit_failure(
                        state,
                        &req,
                        &mut meta,
                        started,
                        attempts_done,
                        &client_error,
                    );
                    return Err(client_error);
                }
                if failure.kind == FailureKind::TargetError {
                    skip_logical_target = target.route_target_id.clone();
                }
                last_error = Some(client_error);
                continue;
            }
            Err(failure) => {
                let termination = StreamTermination::new(
                    stream_outcome_for_failure(failure.kind),
                    CommitState::PreCommit,
                    Some(failure.kind),
                    failure.status,
                );
                let client_error = failure_to_error(&failure, target);
                record_precommit_attempt_usage(
                    state,
                    &mut meta,
                    target,
                    key.as_ref(),
                    &trace,
                    None,
                    TokenUsage::default(),
                    &failure,
                    termination,
                )
                .await;
                last_precommit_failure = Some(LastPrecommitFailure {
                    target: (*target).clone(),
                    upstream_request_id: None,
                    failure: failure.clone(),
                    termination,
                    attempts_done,
                    client_error: client_error.clone(),
                });
                if let Some(permit) = traffic_permit.as_ref() {
                    permit.finish(traffic_outcome_for_failure(failure.kind));
                }
                if failure.kind == FailureKind::QuotaExhausted {
                    state.quota.observe_exhausted(
                        &target.provider.id,
                        &target.account.id,
                        failure.quota_reset_at,
                        "upstream_error",
                    );
                }
                let circuit_transition =
                    provider_attempt.finish_failure(failure.kind, failure.status);
                state.flight.record(
                    &meta.request_id,
                    started.elapsed().as_millis() as u64,
                    "upstream_connect_failed",
                    format!("{:?}", failure.kind),
                );
                handle_key_failure(state, target, &failure, &mut meta, &mut trace).await;
                let can_fallback =
                    allow_fallback && route_allows_fallback(route.as_ref(), failure.kind);
                trace.stream_termination(termination, Some(can_fallback));
                record_target_telemetry(
                    state,
                    target,
                    telemetry_outcome_for_failure(failure.kind),
                    attempt_started,
                    None,
                    attempts_done > 1,
                    can_fallback,
                    circuit_transition,
                    &mut trace,
                );
                if !can_fallback {
                    trace.step(
                        "attempt",
                        Some(target.account.label.clone()),
                        format!("fallback disabled for {:?}", failure.kind),
                    );
                    trace.finish("failed");
                    state.live.finish(
                        &meta.request_id,
                        "failed",
                        started.elapsed().as_millis() as u64,
                        None,
                        None,
                    );
                    record_precommit_request_log(
                        state,
                        &meta,
                        &req,
                        target,
                        key.as_ref(),
                        &trace,
                        None,
                        &failure,
                        &client_error,
                        termination,
                        started,
                        attempts_done,
                    );
                    let _ = db::insert_route_trace(&state.pool, &trace).await;
                    enqueue_precommit_failure(
                        state,
                        &req,
                        &mut meta,
                        started,
                        attempts_done,
                        &client_error,
                    );
                    return Err(client_error);
                }
                last_error = Some(client_error);
                continue;
            }
        }
    }

    // 4. Every target unavailable (FR-12.12).
    state.failures_pre_commit.fetch_add(1, Ordering::Relaxed);
    let retry_after = pool::soonest_recovery(&all_accounts)
        .map(|t| ((t - chrono::Utc::now()).num_seconds().max(1)) as u64);
    let name = route
        .as_ref()
        .map(|c| format!("route '{}'", c.name))
        .unwrap_or_else(|| req.requested_model.clone());
    let msg = last_precommit_failure
        .as_ref()
        .map(|failure| failure.client_error.message.clone())
        .or_else(|| last_error.as_ref().map(|error| error.message.clone()))
        .unwrap_or_else(|| format!("all targets of {name} are currently unavailable"));
    trace.finish("all_targets_unavailable");
    if let Some(failure) = last_precommit_failure.as_ref() {
        record_precommit_request_log(
            state,
            &meta,
            &req,
            &failure.target,
            key.as_ref(),
            &trace,
            failure.upstream_request_id.as_deref(),
            &failure.failure,
            &failure.client_error,
            failure.termination,
            started,
            failure.attempts_done,
        );
        enqueue_precommit_failure(
            state,
            &req,
            &mut meta,
            started,
            failure.attempts_done,
            &failure.client_error,
        );
    }
    state.live.finish(
        &meta.request_id,
        "all_targets_unavailable",
        started.elapsed().as_millis() as u64,
        None,
        None,
    );
    let _ = db::insert_route_trace(&state.pool, &trace).await;
    // Preserve the actual dispatched failure and its client error together.
    // Routing exhaustion must not turn a useful 429/401/5xx into a generic 503.
    if let Some(failure) = last_precommit_failure {
        return Err(failure.client_error);
    }
    if let Some(error) = last_error {
        if attempts_done > 0 {
            enqueue_precommit_failure(state, &req, &mut meta, started, attempts_done, &error);
        }
        return Err(error);
    }

    // No attempt was possible. If every candidate is cooling down from a prior
    // rate limit, preserve rate-limit semantics for subsequent requests too.
    if !all_accounts.is_empty()
        && all_accounts
            .iter()
            .all(|a| matches!(pool::effective_status(a), pool::AccountStatus::Cooldown))
    {
        return Err(ProxyError::rate_limited(
            format!("{name}: all targets are rate limited"),
            retry_after,
        ));
    }

    Err(ProxyError::all_unavailable(
        format!("{name}: {msg}"),
        retry_after,
    ))
}

fn enqueue_precommit_failure(
    state: &AppState,
    req: &InternalRequest,
    meta: &mut RequestMeta,
    started: Instant,
    attempts_done: usize,
    error: &ProxyError,
) {
    let retries = attempts_done.saturating_sub(1) as i64;
    let upstream_dispatched = meta.upstream_dispatched.load(Ordering::Acquire);
    let admission_cost_usd = upstream_dispatched
        .then(|| {
            meta.admission
                .as_ref()
                .and_then(|admission| admission.conservative_cost_estimate())
        })
        .flatten();
    if !meta.accounting_enqueued.swap(true, Ordering::AcqRel) {
        state.log_queue.enqueue(db::UsageLogRow {
            id: format!("usage_{}", uuid::Uuid::new_v4().simple()),
            request_id: meta.request_id.clone(),
            ts: db::now_iso(),
            key_id: meta.key_id.clone(),
            key_name: meta.key_name.clone(),
            client_format: meta.client_format.to_string(),
            requested_model: req.requested_model.clone(),
            effective_model: None,
            route_id: None,
            route_name: None,
            fallback_hops: retries,
            fallback_path: "[]".into(),
            status: "upstream_error".into(),
            status_code: i64::from(error.http_status()),
            latency_ms: Some(started.elapsed().as_millis() as i64),
            ttft_ms: None,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
            thinking_tokens: None,
            cost_usd: None,
            cost_known: 0,
            price_version_id: None,
            cache_status: "bypass".into(),
            serving_account_id: None,
            serving_account: None,
            serving_provider: None,
            upstream_request_id: None,
            flagged: 0,
            error_message: None,
            usage_confidence: "unknown".into(),
            commit_state: "pre_commit".into(),
            retry_count: retries,
            route_trace_id: None,
            opaque_route_id: None,
            admission_cost_usd,
        });
    }
    if upstream_dispatched {
        if let Some(admission) = meta.admission.take() {
            let (usage, cost) = aggregate_admission_accounting(meta, None, None);
            admission.reconcile(&usage, cost);
        }
    }
}

fn target_key(route: &db::RouteRow, t: &ResolvedTarget) -> String {
    format!("{}|{}|{}", route.id, t.account.id, t.model.id)
}

fn execution_profile_key(target: &ResolvedTarget) -> (String, String, String) {
    (
        target.provider.id.clone(),
        target.account.id.clone(),
        target.model.id.clone(),
    )
}

fn target_route_model_identity(key: &str) -> Option<(&str, &str)> {
    let (route_id, remainder) = key.split_once('|')?;
    let (_, model_id) = remainder.split_once('|')?;
    Some((route_id, model_id))
}

fn resolve_session_origin(
    sticky_key: &str,
    route_target_origins: &[(String, ContinuationOrigin)],
) -> Option<ContinuationOrigin> {
    if let Some((_, origin)) = route_target_origins
        .iter()
        .find(|(candidate_key, _)| candidate_key == sticky_key)
    {
        return Some(origin.clone());
    }

    // Account identity is used for affinity, but an account rotation does not
    // change who produced continuation state when the Route/model still resolves
    // unambiguously to the same provider, upstream model, and family metadata.
    let identity = target_route_model_identity(sticky_key)?;
    let mut candidates = route_target_origins
        .iter()
        .filter(|(candidate_key, _)| target_route_model_identity(candidate_key) == Some(identity));
    let (_, origin) = candidates.next()?;
    candidates
        .all(|(_, candidate)| candidate == origin)
        .then(|| origin.clone())
}

fn traffic_key(t: &ResolvedTarget) -> crate::upstream_traffic::TargetKey {
    crate::upstream_traffic::TargetKey::new(
        t.provider.id.clone(),
        t.account.id.clone(),
        t.model.id.clone(),
    )
}

/// Route target IDs are configuration row identities; model IDs identify the serving target.
fn provider_correlation_target_id(target: &ResolvedTarget) -> &str {
    target.model.id.as_str()
}

fn trace_adaptive_quota_evidence(
    trace: &mut RouteTrace,
    target: &ResolvedTarget,
    evidence: &std::collections::HashMap<String, Option<crate::quota::QuotaSnapshot>>,
) {
    let detail = match evidence.get(&adaptive_candidate_key(target)) {
        Some(Some(quota)) => {
            let remaining = quota
                .remaining_fraction
                .map(|value| format!("{:.1}%", value * 100.0))
                .unwrap_or_else(|| "unknown".into());
            format!(
                "quota-evidence: remaining={remaining} reset={} source={} observed={} freshness=fresh scope=account-global",
                quota
                    .reset_at
                    .map(|value| value.to_rfc3339())
                    .unwrap_or_else(|| "unknown".into()),
                quota.source,
                quota.observed_at.to_rfc3339(),
            )
        }
        Some(None) => {
            "quota-evidence: unknown (no fresh account-global observation at ordering time)".into()
        }
        None => {
            "quota-evidence: not used (target was not dispatchable during adaptive ordering)".into()
        }
    };
    trace.step("candidate", Some(target.account.label.clone()), detail);
}

fn adaptive_candidate_key(t: &ResolvedTarget) -> String {
    format!(
        "{}|{}|{}|{}",
        t.route_target_id.as_deref().unwrap_or("direct"),
        t.provider.id,
        t.account.id,
        t.model.id
    )
}

fn snapshot_traffic_targets(
    state: &AppState,
    targets: &[ResolvedTarget],
) -> std::collections::HashMap<
    crate::upstream_traffic::TargetKey,
    crate::upstream_traffic::TrafficSnapshot,
> {
    let mut snapshots = std::collections::HashMap::new();
    for target in targets {
        let key = traffic_key(target);
        snapshots
            .entry(key.clone())
            .or_insert_with(|| state.upstream_traffic.snapshot(&key));
    }
    snapshots
}

#[derive(Debug, Clone)]
struct OrderedRouteTargets {
    targets: Vec<ResolvedTarget>,
    /// Frozen account-global snapshots and unknowns used to score dispatchable candidates.
    quota_evidence: std::collections::HashMap<String, Option<crate::quota::QuotaSnapshot>>,
}

#[derive(Debug, Clone)]
struct AdaptiveSortScore {
    has_capacity: bool,
    overload_ewma: f64,
    error_ewma: f64,
    quota_preference: f64,
    ttft_ms: f64,
    priority: i64,
    route_target_id: String,
    account_id: String,
    model_id: String,
}

fn median_observed_ttft(
    targets: &[ResolvedTarget],
    snapshots: &std::collections::HashMap<
        crate::upstream_traffic::TargetKey,
        crate::upstream_traffic::TrafficSnapshot,
    >,
) -> Option<f64> {
    let mut values: Vec<f64> = targets
        .iter()
        .filter_map(|target| snapshots.get(&traffic_key(target)))
        .filter(|snapshot| snapshot.ttft_samples > 0)
        .filter_map(|snapshot| snapshot.fast_ttft_ms)
        .filter(|ttft| ttft.is_finite())
        .collect();
    if values.is_empty() {
        return None;
    }

    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some(values[mid - 1] + (values[mid] - values[mid - 1]) / 2.0)
    } else {
        Some(values[mid])
    }
}

#[cfg(test)]
fn build_adaptive_scores(
    targets: &[ResolvedTarget],
    snapshots: &std::collections::HashMap<
        crate::upstream_traffic::TargetKey,
        crate::upstream_traffic::TrafficSnapshot,
    >,
) -> std::collections::HashMap<String, AdaptiveSortScore> {
    build_adaptive_scores_with_quota(targets, snapshots, &std::collections::HashMap::new())
}

fn build_adaptive_scores_with_quota(
    targets: &[ResolvedTarget],
    snapshots: &std::collections::HashMap<
        crate::upstream_traffic::TargetKey,
        crate::upstream_traffic::TrafficSnapshot,
    >,
    quota: &std::collections::HashMap<String, crate::quota::QuotaSnapshot>,
) -> std::collections::HashMap<String, AdaptiveSortScore> {
    let neutral_ttft = median_observed_ttft(targets, snapshots).unwrap_or(0.0);
    let now = chrono::Utc::now();
    targets
        .iter()
        .map(|target| {
            let key = traffic_key(target);
            let snapshot = snapshots
                .get(&key)
                .copied()
                .expect("adaptive target snapshot");
            let ttft_ms = if snapshot.ttft_samples > 0 {
                snapshot
                    .fast_ttft_ms
                    .filter(|ttft| ttft.is_finite())
                    .unwrap_or(neutral_ttft)
            } else {
                neutral_ttft
            };
            let score = AdaptiveSortScore {
                has_capacity: snapshot.has_capacity,
                overload_ewma: snapshot.overload_ewma,
                error_ewma: snapshot.error_ewma,
                quota_preference: quota
                    .get(&adaptive_candidate_key(target))
                    .map(|snapshot| snapshot.preference(now))
                    .unwrap_or(0.0),
                ttft_ms,
                priority: target.priority,
                route_target_id: target.route_target_id.clone().unwrap_or_default(),
                account_id: target.account.id.clone(),
                model_id: target.model.id.clone(),
            };
            (adaptive_candidate_key(target), score)
        })
        .collect()
}

fn compare_adaptive_targets(
    a: &ResolvedTarget,
    b: &ResolvedTarget,
    scores: &std::collections::HashMap<String, AdaptiveSortScore>,
) -> std::cmp::Ordering {
    let a_score = scores
        .get(&adaptive_candidate_key(a))
        .expect("adaptive candidate score");
    let b_score = scores
        .get(&adaptive_candidate_key(b))
        .expect("adaptive candidate score");

    b_score
        .has_capacity
        .cmp(&a_score.has_capacity)
        .then_with(|| {
            b_score
                .quota_preference
                .total_cmp(&a_score.quota_preference)
        })
        .then_with(|| a_score.overload_ewma.total_cmp(&b_score.overload_ewma))
        .then_with(|| a_score.error_ewma.total_cmp(&b_score.error_ewma))
        .then_with(|| a_score.ttft_ms.total_cmp(&b_score.ttft_ms))
        .then_with(|| a_score.priority.cmp(&b_score.priority))
        .then_with(|| a_score.route_target_id.cmp(&b_score.route_target_id))
        .then_with(|| a_score.account_id.cmp(&b_score.account_id))
        .then_with(|| a_score.model_id.cmp(&b_score.model_id))
}

fn traffic_outcome_for_failure(kind: FailureKind) -> crate::upstream_traffic::TrafficOutcome {
    use crate::upstream_traffic::TrafficOutcome;
    match kind {
        FailureKind::RateLimit => TrafficOutcome::Overload,
        FailureKind::Timeout => TrafficOutcome::Timeout,
        FailureKind::ServerError
        | FailureKind::ConnectionError
        | FailureKind::MalformedUpstream => TrafficOutcome::Error,
        FailureKind::PluginFailure => TrafficOutcome::Neutral,
        FailureKind::ClientCancelled => TrafficOutcome::Cancelled,
        FailureKind::QuotaExhausted
        | FailureKind::AuthError
        | FailureKind::TargetError
        | FailureKind::BadRequest
        | FailureKind::PolicyRejected => TrafficOutcome::Neutral,
    }
}

fn telemetry_outcome_for_failure(kind: FailureKind) -> crate::target_telemetry::TelemetryOutcome {
    use crate::target_telemetry::TelemetryOutcome;
    match kind {
        FailureKind::RateLimit => TelemetryOutcome::RateLimit,
        FailureKind::QuotaExhausted => TelemetryOutcome::QuotaExhausted,
        FailureKind::ServerError | FailureKind::MalformedUpstream => TelemetryOutcome::ServerError,
        FailureKind::PluginFailure => TelemetryOutcome::TargetError,
        FailureKind::ConnectionError => TelemetryOutcome::ConnectionError,
        FailureKind::Timeout => TelemetryOutcome::Timeout,
        FailureKind::AuthError => TelemetryOutcome::AuthError,
        FailureKind::TargetError | FailureKind::PolicyRejected => TelemetryOutcome::TargetError,
        FailureKind::BadRequest => TelemetryOutcome::BadRequest,
        FailureKind::ClientCancelled => TelemetryOutcome::Cancelled,
    }
}

fn trace_provider_circuit_transition(
    trace: &mut RouteTrace,
    provider_name: &str,
    transition: crate::provider_circuit::ProviderCircuitTransition,
) {
    if transition.opened {
        let detail = if transition.validated_first_event {
            "provider circuit opened after a terminal failure following first-event validation"
        } else {
            "provider circuit opened after a qualifying failure"
        };
        trace.step("provider_circuit", Some(provider_name.to_string()), detail);
    }
}

fn record_target_telemetry(
    state: &AppState,
    target: &ResolvedTarget,
    outcome: crate::target_telemetry::TelemetryOutcome,
    attempt_started: Instant,
    ttft_ms: Option<u64>,
    fallback_attempt: bool,
    caused_fallback: bool,
    circuit: crate::provider_circuit::ProviderCircuitTransition,
    trace: &mut RouteTrace,
) {
    trace_provider_circuit_transition(trace, &target.provider.name, circuit);
    let mut event = crate::target_telemetry::TelemetryEvent::attempt(
        traffic_key(target),
        outcome,
        ttft_ms,
        attempt_started.elapsed().as_millis() as u64,
        fallback_attempt,
        caused_fallback,
    );
    event.provider_circuit_open = circuit.opened;
    event.provider_circuit_recovery = circuit.recovered;
    event.half_open_probe = circuit.half_open_probe;
    state.target_telemetry.record(event);
}

async fn plan_runtime_candidate(
    pool: &db::Pool,
    account: &db::AccountRow,
    mut facts: crate::pre_dispatch::PreDispatchFacts,
) -> crate::pre_dispatch::PreDispatchDecision {
    facts.account_quota_reached = Some(
        pool::soft_quota_reached(pool, account)
            .await
            .unwrap_or(false),
    );
    crate::pre_dispatch::plan_candidate(facts)
}

fn route_allows_fallback(route: Option<&db::RouteRow>, kind: FailureKind) -> bool {
    if !kind.is_retryable() {
        return false;
    }
    let Some(route) = route else {
        // A target-local model/project failure cannot be repaired by trying a
        // different credential for the same direct provider/model.
        return kind != FailureKind::TargetError;
    };
    let triggers: Value =
        serde_json::from_str(&route.fallback_triggers).unwrap_or_else(|_| serde_json::json!({}));
    let enabled = |name: &str| triggers.get(name).and_then(Value::as_bool).unwrap_or(true);
    match kind {
        FailureKind::RateLimit => enabled("on429"),
        FailureKind::QuotaExhausted => enabled("onQuota"),
        FailureKind::ServerError
        | FailureKind::ConnectionError
        | FailureKind::MalformedUpstream
        | FailureKind::PluginFailure => enabled("on5xx"),
        FailureKind::Timeout => enabled("onTimeout"),
        // Credential-global failures should try another account. Target-local
        // failures should try another logical route target.
        FailureKind::AuthError | FailureKind::TargetError => true,
        FailureKind::BadRequest | FailureKind::PolicyRejected | FailureKind::ClientCancelled => {
            false
        }
    }
}

fn failure_rule_matches(rule: &Value, status: u16, code: &str, message: &str) -> bool {
    let Some(obj) = rule.as_object() else {
        return false;
    };
    let mut constrained = false;

    if let Some(statuses) = obj.get("statuses").and_then(Value::as_array) {
        constrained = true;
        if !statuses.iter().any(|v| v.as_u64() == Some(status as u64)) {
            return false;
        }
    }
    if let Some(single) = obj.get("status").and_then(Value::as_u64) {
        constrained = true;
        if single != status as u64 {
            return false;
        }
    }
    if let Some(codes) = obj.get("codes").and_then(Value::as_array) {
        constrained = true;
        let lower = code.to_ascii_lowercase();
        if !codes
            .iter()
            .filter_map(Value::as_str)
            .any(|candidate| candidate.eq_ignore_ascii_case(&lower))
        {
            return false;
        }
    }
    if let Some(needles) = obj.get("message_contains").and_then(Value::as_array) {
        constrained = true;
        let lower = message.to_ascii_lowercase();
        if !needles
            .iter()
            .filter_map(Value::as_str)
            .any(|needle| lower.contains(&needle.to_ascii_lowercase()))
        {
            return false;
        }
    }

    constrained
}

/// Apply admin-configured provider classification rules after the native
/// adapter has extracted the provider's normal status/code/message semantics.
///
/// Supported keys: `auth`, `quota`, `rate_limit`, `target`,
/// `bad_request`. Each rule may contain `status`/`statuses`, `codes`,
/// and/or `message_contains`.
pub(crate) fn apply_provider_failure_rules(
    provider: &db::ProviderRow,
    status: u16,
    body: &str,
    mut failure: UpstreamFailure,
) -> UpstreamFailure {
    let rules: Value =
        serde_json::from_str(&provider.rate_limit_rules).unwrap_or_else(|_| serde_json::json!({}));
    let Some(obj) = rules.as_object() else {
        return failure;
    };
    if obj.is_empty() {
        return failure;
    }

    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = parsed
        .pointer("/error/code")
        .or_else(|| parsed.pointer("/error/status"))
        .or_else(|| parsed.pointer("/error/type"))
        .or_else(|| parsed.get("code"))
        .or_else(|| parsed.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = parsed
        .pointer("/error/message")
        .or_else(|| parsed.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(&failure.message);

    let ordered = [
        ("auth", FailureKind::AuthError),
        ("quota", FailureKind::QuotaExhausted),
        ("rate_limit", FailureKind::RateLimit),
        ("target", FailureKind::TargetError),
        ("bad_request", FailureKind::BadRequest),
    ];
    for (name, kind) in ordered {
        if obj
            .get(name)
            .map(|rule| failure_rule_matches(rule, status, code, message))
            .unwrap_or(false)
        {
            failure.kind = kind;
            break;
        }
    }
    failure
}

fn apply_target_overrides(req: &mut InternalRequest, overrides: &Value) -> Result<(), ProxyError> {
    let Some(obj) = overrides.as_object() else {
        if overrides.is_null() {
            return Ok(());
        }
        return Err(ProxyError::internal(
            "route target param_overrides must be a JSON object",
        ));
    };
    if obj.is_empty() {
        return Ok(());
    }

    for (key, value) in obj {
        match key.as_str() {
            "temperature" => req.params.temperature = value.as_f64(),
            "top_p" => req.params.top_p = value.as_f64(),
            "top_k" => req.params.top_k = value.as_f64(),
            "max_tokens" | "max_completion_tokens" | "max_output_tokens" => {
                req.params.max_tokens = value.as_u64().and_then(|v| u32::try_from(v).ok())
            }
            "seed" => req.params.seed = value.as_i64(),
            "presence_penalty" => req.params.presence_penalty = value.as_f64(),
            "frequency_penalty" => req.params.frequency_penalty = value.as_f64(),
            "stop" => {
                req.params.stop = match value {
                    Value::String(v) => vec![v.clone()],
                    Value::Array(values) => values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    Value::Null => Vec::new(),
                    _ => {
                        return Err(ProxyError::bad_request(
                            "route target override 'stop' must be a string or array",
                        ))
                    }
                }
            }
            _ => {
                req.extra.insert(key.clone(), value.clone());
            }
        }
    }

    // Same-format passthrough must observe the exact same target overrides as
    // translated requests.
    if let Some(raw) = req.raw_body.as_mut() {
        let mut parsed: Value = serde_json::from_str(raw)
            .map_err(|_| ProxyError::bad_request("client request body is not valid JSON"))?;
        let Some(raw_obj) = parsed.as_object_mut() else {
            return Err(ProxyError::bad_request(
                "client request body must be a JSON object",
            ));
        };
        for (key, value) in obj {
            raw_obj.insert(key.clone(), value.clone());
        }
        *raw = parsed.to_string();
    }

    Ok(())
}

fn default_quota_window(account: &db::AccountRow) -> i64 {
    match account.quota_type.as_str() {
        "daily" => 86_400,
        "monthly" => 30 * 86_400,
        "rolling" => account.quota_window_s.unwrap_or(86_400),
        _ => 86_400,
    }
}

fn build_upstream_body(
    adapter: &dyn Adapter,
    ctx: &UpstreamContext<'_>,
    req: &InternalRequest,
    use_passthrough: bool,
) -> Result<Value, UpstreamFailure> {
    if !use_passthrough {
        return adapter.build_body(ctx, req);
    }

    let raw = req.raw_body.as_deref().unwrap_or("{}");
    let rewritten = if adapter.wire_format() == "openai-responses" {
        passthrough::rewrite_responses_model(raw, &ctx.model.upstream_id, !req.stream)
    } else {
        passthrough::rewrite_model(
            raw,
            &ctx.model.upstream_id,
            !req.stream,
            true,
            ctx.provider.wire(),
        )
    };
    let Some(rewritten) = rewritten else {
        return adapter.build_body(ctx, req);
    };
    let mut body = serde_json::from_str(&rewritten).unwrap_or(Value::Null);
    adapter.normalize_passthrough_body(ctx, req, &mut body)?;
    Ok(body)
}

/// Send the upstream request. Returns the raw response or a classified failure.
///
/// The shared outbound transport resolves and pins the exact destination used
/// for connection and explicitly revalidates every redirect hop.
async fn send_upstream(
    state: &AppState,
    adapter: &Arc<dyn Adapter>,
    ctx: &UpstreamContext<'_>,
    req: &InternalRequest,
    use_passthrough: bool,
    request_id: &str,
    protocol_headers: &[(String, String)],
    dispatched: &AtomicBool,
) -> Result<reqwest::Response, UpstreamFailure> {
    let url = adapter.build_url(ctx).map_err(|e| UpstreamFailure {
        kind: FailureKind::BadRequest,
        status: None,
        retry_after_secs: None,
        message: e.message,
        quota_reset_at: None,
    })?;
    let url = url::Url::parse(&url).map_err(|e| UpstreamFailure {
        kind: FailureKind::BadRequest,
        status: None,
        retry_after_secs: None,
        message: format!("invalid upstream URL: {e}"),
        quota_reset_at: None,
    })?;

    // Passthrough preserves the client's raw body except for mandatory model/
    // accounting rewrites and adapter-owned model compatibility normalization.
    let body = build_upstream_body(adapter.as_ref(), ctx, req, use_passthrough)?;

    crate::outbound::send_provider_request_tracked(
        &state.outbound_clients,
        state.config.allow_private_upstreams,
        state.config.allow_insecure_tls,
        adapter,
        ctx,
        crate::outbound::ProviderRequest {
            method: reqwest::Method::POST,
            url,
            json_body: Some(body),
            accept_event_stream: true,
            request_id: Some(request_id.to_string()),
            headers: if adapter.wire_format() == "anthropic" {
                protocol_headers.to_vec()
            } else {
                Vec::new()
            },
            total_timeout: None,
        },
        dispatched,
    )
    .await
    .map_err(|e| {
        if let Some(failure) = e.adapter_failure {
            failure
        } else {
            UpstreamFailure {
                kind: if e.timeout {
                    FailureKind::Timeout
                } else {
                    FailureKind::ConnectionError
                },
                status: None,
                retry_after_secs: None,
                message: e.message,
                quota_reset_at: None,
            }
        }
    })
}

const MAX_FULL_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

struct PreparedUpstream {
    stream: Option<reqwest::Response>,
    prefetched: Vec<Bytes>,
    full_events: Option<Vec<StreamEvent>>,
    precommit_usage: TokenUsage,
    is_sse: bool,
}

struct PreparedResponseFailure {
    failure: UpstreamFailure,
    outcome: StreamOutcome,
    precommit_usage: TokenUsage,
}

impl PreparedResponseFailure {
    fn new(failure: UpstreamFailure, outcome: StreamOutcome) -> Self {
        Self {
            failure,
            outcome,
            precommit_usage: TokenUsage::default(),
        }
    }

    fn with_precommit_usage(mut self, usage: &TokenUsage) -> Self {
        self.precommit_usage = usage.clone();
        self
    }
}

impl From<UpstreamFailure> for PreparedResponseFailure {
    fn from(failure: UpstreamFailure) -> Self {
        let outcome = match failure.kind {
            FailureKind::Timeout => StreamOutcome::Timeout,
            FailureKind::MalformedUpstream => StreamOutcome::ProtocolViolation,
            FailureKind::ClientCancelled => StreamOutcome::ClientCancelled,
            _ => StreamOutcome::UpstreamError,
        };
        Self::new(failure, outcome)
    }
}

fn is_semantic_event(event: &StreamEvent) -> bool {
    !matches!(event, StreamEvent::Start { .. } | StreamEvent::Usage(_))
}

fn payload_is_terminal(payload: &str, events: &[StreamEvent]) -> bool {
    if payload.trim() == "[DONE]" {
        return true;
    }
    if events
        .iter()
        .any(|event| matches!(event, StreamEvent::Finish(_)))
    {
        return true;
    }
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .as_deref()
        == Some("message_stop")
}

fn payload_error_failure(adapter: &Arc<dyn Adapter>, payload: &str) -> Option<UpstreamFailure> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let looks_error =
        value.get("error").is_some() || value.get("type").and_then(Value::as_str) == Some("error");
    if !looks_error {
        return None;
    }
    Some(adapter.classify_error(502, payload, &reqwest::header::HeaderMap::new()))
}

/// Validate a successful HTTP response before the client response is committed.
/// SSE stays retryable until the destination encoder produces client-visible
/// bytes (or passthrough observes a semantic event). Normal JSON is parsed
/// completely through the adapter's full-response path.
async fn prepare_success_response(
    mut response: reqwest::Response,
    adapter: &Arc<dyn Adapter>,
    aggregate_sse: bool,
    format: FrontendFormat,
    passthrough: bool,
    encoder_ctx: &EncoderCtx,
    first_event_timeout: Duration,
    idle_timeout: Duration,
) -> Result<PreparedUpstream, PreparedResponseFailure> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_sse = content_type.split(';').next().map(str::trim) == Some("text/event-stream");

    if is_sse && aggregate_sse {
        return prepare_aggregated_sse(response, adapter, first_event_timeout, idle_timeout).await;
    }

    if !is_sse {
        if response
            .content_length()
            .map(|len| len > MAX_FULL_RESPONSE_BYTES as u64)
            .unwrap_or(false)
        {
            return Err(UpstreamFailure {
                kind: FailureKind::MalformedUpstream,
                status: Some(502),
                retry_after_secs: None,
                message: "upstream JSON response exceeds size limit".into(),
                quota_reset_at: None,
            }
            .into());
        }
        let body = response.bytes().await.map_err(|error| UpstreamFailure {
            kind: if error.is_timeout() {
                FailureKind::Timeout
            } else {
                FailureKind::ConnectionError
            },
            status: None,
            retry_after_secs: None,
            message: classify_reqwest(&error),
            quota_reset_at: None,
        })?;
        if body.len() > MAX_FULL_RESPONSE_BYTES {
            return Err(UpstreamFailure {
                kind: FailureKind::MalformedUpstream,
                status: Some(502),
                retry_after_secs: None,
                message: "upstream JSON response exceeds size limit".into(),
                quota_reset_at: None,
            }
            .into());
        }
        let body_text = String::from_utf8_lossy(&body);
        if let Some(failure) = payload_error_failure(adapter, &body_text) {
            return Err(failure.into());
        }
        let value: Value = serde_json::from_slice(&body).map_err(|error| UpstreamFailure {
            kind: FailureKind::MalformedUpstream,
            status: Some(502),
            retry_after_secs: None,
            message: format!("invalid upstream JSON response: {error}"),
            quota_reset_at: None,
        })?;
        let events = adapter.parse_full_response(&value)?;
        if !events.iter().any(is_semantic_event) {
            return Err(UpstreamFailure {
                kind: FailureKind::MalformedUpstream,
                status: Some(502),
                retry_after_secs: None,
                message: "upstream JSON response contained no model result".into(),
                quota_reset_at: None,
            }
            .into());
        }
        let mut precommit_usage = TokenUsage::default();
        for event in &events {
            if let StreamEvent::Usage(value) = event {
                precommit_usage.merge(value);
            }
        }
        return Ok(PreparedUpstream {
            stream: None,
            prefetched: Vec::new(),
            full_events: Some(events),
            precommit_usage,
            is_sse: false,
        });
    }

    let mut framer = crate::sse::SseFramer::new();
    let mut prefetched = Vec::new();
    let mut precommit_usage = TokenUsage::default();
    let mut visibility_encoder = (!passthrough).then(|| Encoder::new(format, encoder_ctx.clone()));
    let first_event_deadline = tokio::time::Instant::now() + first_event_timeout;
    loop {
        let remaining = first_event_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, response.chunk()).await {
            Ok(Ok(Some(bytes))) => {
                prefetched.push(bytes.clone());
                let frames = framer.push(&bytes).map_err(|error| {
                    PreparedResponseFailure::from(UpstreamFailure {
                        kind: FailureKind::MalformedUpstream,
                        status: Some(502),
                        retry_after_secs: None,
                        message: error.to_string(),
                        quota_reset_at: None,
                    })
                    .with_precommit_usage(&precommit_usage)
                })?;
                for frame in frames {
                    let Some(payload) = crate::sse::extract_data(&frame) else {
                        continue;
                    };
                    if let Some(failure) = payload_error_failure(adapter, &payload) {
                        return Err(PreparedResponseFailure::from(failure)
                            .with_precommit_usage(&precommit_usage));
                    }
                    if payload.trim() == "[DONE]" {
                        return Err(PreparedResponseFailure::from(UpstreamFailure {
                            kind: FailureKind::MalformedUpstream,
                            status: Some(502),
                            retry_after_secs: None,
                            message: "upstream SSE ended before any model event".into(),
                            quota_reset_at: None,
                        })
                        .with_precommit_usage(&precommit_usage));
                    }
                    let events = adapter.parse_stream_chunk(&payload).map_err(|failure| {
                        PreparedResponseFailure::from(failure)
                            .with_precommit_usage(&precommit_usage)
                    })?;
                    for event in &events {
                        if let StreamEvent::Usage(value) = event {
                            precommit_usage.merge(value);
                        }
                    }
                    let client_visible = if passthrough {
                        events.iter().any(is_semantic_event)
                    } else {
                        events.iter().any(|event| {
                            !visibility_encoder
                                .as_mut()
                                .expect("translated response has a visibility encoder")
                                .encode(event.clone())
                                .is_empty()
                        })
                    };
                    if client_visible {
                        return Ok(PreparedUpstream {
                            stream: Some(response),
                            prefetched,
                            full_events: None,
                            precommit_usage,
                            is_sse: true,
                        });
                    }
                }
            }
            Ok(Ok(None)) => {
                let message = if framer.pending_bytes() > 0 {
                    "upstream SSE ended with an incomplete frame"
                } else {
                    "upstream SSE ended before any model event"
                };
                let failure = UpstreamFailure {
                    kind: FailureKind::MalformedUpstream,
                    status: Some(502),
                    retry_after_secs: None,
                    message: message.into(),
                    quota_reset_at: None,
                };
                let outcome = if framer.pending_bytes() > 0 {
                    StreamOutcome::ProtocolViolation
                } else {
                    StreamOutcome::UpstreamCleanEof
                };
                return Err(PreparedResponseFailure::new(failure, outcome)
                    .with_precommit_usage(&precommit_usage));
            }
            Ok(Err(error)) => {
                return Err(PreparedResponseFailure::from(UpstreamFailure {
                    kind: if error.is_timeout() {
                        FailureKind::Timeout
                    } else {
                        FailureKind::ConnectionError
                    },
                    status: None,
                    retry_after_secs: None,
                    message: classify_reqwest(&error),
                    quota_reset_at: None,
                })
                .with_precommit_usage(&precommit_usage));
            }
            Err(_) => {
                return Err(PreparedResponseFailure::from(timeout_failure(
                    "upstream timed out before first valid event",
                ))
                .with_precommit_usage(&precommit_usage));
            }
        }
    }
}

async fn prepare_aggregated_sse(
    response: reqwest::Response,
    adapter: &Arc<dyn Adapter>,
    first_event_timeout: Duration,
    idle_timeout: Duration,
) -> Result<PreparedUpstream, PreparedResponseFailure> {
    let mut precommit_usage = TokenUsage::default();
    let result = prepare_aggregated_sse_inner(
        response,
        adapter,
        first_event_timeout,
        idle_timeout,
        &mut precommit_usage,
    )
    .await;
    result.map_err(|mut failure| {
        failure.precommit_usage = precommit_usage;
        failure
    })
}

async fn prepare_aggregated_sse_inner(
    mut response: reqwest::Response,
    adapter: &Arc<dyn Adapter>,
    first_event_timeout: Duration,
    idle_timeout: Duration,
    precommit_usage: &mut TokenUsage,
) -> Result<PreparedUpstream, PreparedResponseFailure> {
    let first_event_deadline = Instant::now() + first_event_timeout;
    let mut framer = crate::sse::SseFramer::new();
    let mut events_out = Vec::new();
    let mut semantic_seen = false;
    let mut terminal_seen = false;

    loop {
        let timeout = if semantic_seen {
            idle_timeout
        } else {
            first_event_deadline.saturating_duration_since(Instant::now())
        };
        match tokio::time::timeout(timeout, response.chunk()).await {
            Err(_) => {
                return Err(UpstreamFailure {
                    kind: FailureKind::Timeout,
                    status: None,
                    retry_after_secs: None,
                    message: "timeout waiting for upstream SSE response".into(),
                    quota_reset_at: None,
                }
                .into());
            }
            Ok(Err(error)) => {
                return Err(UpstreamFailure {
                    kind: if error.is_timeout() {
                        FailureKind::Timeout
                    } else {
                        FailureKind::ConnectionError
                    },
                    status: None,
                    retry_after_secs: None,
                    message: classify_reqwest(&error),
                    quota_reset_at: None,
                }
                .into());
            }
            Ok(Ok(Some(bytes))) => {
                let frames = framer.push(&bytes).map_err(|error| UpstreamFailure {
                    kind: FailureKind::MalformedUpstream,
                    status: Some(502),
                    retry_after_secs: None,
                    message: error.to_string(),
                    quota_reset_at: None,
                })?;
                for frame in frames {
                    let Some(payload) = crate::sse::extract_data(&frame) else {
                        continue;
                    };
                    if let Some(failure) = payload_error_failure(adapter, &payload) {
                        return Err(failure.into());
                    }
                    if payload.trim() == "[DONE]" {
                        terminal_seen = true;
                        continue;
                    }
                    let events = adapter.parse_stream_chunk(&payload)?;
                    terminal_seen |= payload_is_terminal(&payload, &events);
                    semantic_seen |= events.iter().any(is_semantic_event);
                    for event in &events {
                        if let StreamEvent::Usage(value) = event {
                            precommit_usage.merge(value);
                        }
                    }
                    events_out.extend(events);
                }
            }
            Ok(Ok(None)) => {
                if framer.pending_bytes() > 0 {
                    return Err(PreparedResponseFailure::new(
                        UpstreamFailure {
                            kind: FailureKind::MalformedUpstream,
                            status: Some(502),
                            retry_after_secs: None,
                            message: "upstream SSE ended with an incomplete frame".into(),
                            quota_reset_at: None,
                        },
                        StreamOutcome::ProtocolViolation,
                    ));
                }
                if !terminal_seen {
                    return Err(PreparedResponseFailure::new(
                        UpstreamFailure {
                            kind: FailureKind::MalformedUpstream,
                            status: Some(502),
                            retry_after_secs: None,
                            message: "upstream SSE ended without a terminal event".into(),
                            quota_reset_at: None,
                        },
                        StreamOutcome::UpstreamCleanEof,
                    ));
                }
                if !semantic_seen {
                    return Err(PreparedResponseFailure::new(
                        UpstreamFailure {
                            kind: FailureKind::MalformedUpstream,
                            status: Some(502),
                            retry_after_secs: None,
                            message: "upstream SSE ended before any model event".into(),
                            quota_reset_at: None,
                        },
                        StreamOutcome::ProtocolViolation,
                    ));
                }
                return Ok(PreparedUpstream {
                    stream: None,
                    prefetched: Vec::new(),
                    full_events: Some(events_out),
                    precommit_usage: precommit_usage.clone(),
                    is_sse: true,
                });
            }
        }
    }
}

fn classify_reqwest(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        "connect error".to_string()
    } else {
        crate::crypto::redact(&e.to_string())
    }
}

fn extract_upstream_request_id(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("x-request-id")
        .or_else(|| resp.headers().get("request-id"))
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

/// Record the state consequence of a classified upstream failure. Target-local
/// and request-local failures are traced but never mutate account health.
async fn handle_key_failure(
    state: &AppState,
    target: &ResolvedTarget,
    failure: &UpstreamFailure,
    meta: &mut RequestMeta,
    trace: &mut RouteTrace,
) {
    let account_id = &target.account.id;
    let label = target.account.label.clone();
    let (status, reason_code, cooldown_until, quota_reset_at) = match failure.kind {
        FailureKind::RateLimit => {
            let cooldown = failure.retry_after_secs.unwrap_or(30).min(3600);
            let until =
                (chrono::Utc::now() + chrono::Duration::seconds(cooldown as i64)).to_rfc3339();
            (Some("cooldown"), "rate_limited", Some(until), None)
        }
        FailureKind::QuotaExhausted => {
            let reset = failure.quota_reset_at.unwrap_or_else(|| {
                chrono::Utc::now()
                    + chrono::Duration::seconds(default_quota_window(&target.account))
            });
            (
                Some("exhausted"),
                "account_quota_exhausted",
                None,
                Some(reset.to_rfc3339()),
            )
        }
        FailureKind::AuthError => (Some("disabled"), failure.kind.reason_code(), None, None),
        _ => (None, failure.kind.reason_code(), None, None),
    };
    let mut persistence_failed = false;
    let failure_count = if let Some(status) = status {
        match db::apply_account_failure(
            &state.pool,
            account_id,
            target.account.account_state_version,
            status,
            reason_code,
            cooldown_until.as_deref(),
            quota_reset_at.as_deref(),
            &failure.message,
            CIRCUIT_THRESHOLD,
            CIRCUIT_OPEN_SECS,
        )
        .await
        {
            Ok(count) => count,
            Err(error) => {
                persistence_failed = true;
                tracing::error!(
                    account = %account_id,
                    error = %error,
                    "failed to persist account-scoped upstream failure"
                );
                None
            }
        }
    } else {
        None
    };
    let transition_applied = failure_count.is_some();
    let n = failure_count.unwrap_or(0);
    let detail = match failure.kind {
        FailureKind::RateLimit => {
            let cooldown = failure.retry_after_secs.unwrap_or(30).min(3600);
            let d = if transition_applied {
                format!("{label}:429(cooldown {cooldown}s)")
            } else if persistence_failed {
                format!("{label}:429(lifecycle update failed)")
            } else {
                format!("{label}:429(stale failure ignored)")
            };
            meta.fallback_path.push(d.clone());
            d
        }
        FailureKind::QuotaExhausted => {
            let d = if transition_applied {
                format!("{label}:quota_exhausted")
            } else if persistence_failed {
                format!("{label}:quota_exhausted(lifecycle update failed)")
            } else {
                format!("{label}:quota_exhausted(stale failure ignored)")
            };
            meta.fallback_path.push(d.clone());
            d
        }
        FailureKind::AuthError => {
            let d = if transition_applied {
                format!("{label}:auth_error(disabled)")
            } else if persistence_failed {
                format!("{label}:auth_error(lifecycle update failed)")
            } else {
                format!("{label}:auth_error(stale failure ignored)")
            };
            meta.fallback_path.push(d.clone());
            d
        }
        FailureKind::ServerError
        | FailureKind::ConnectionError
        | FailureKind::Timeout
        | FailureKind::MalformedUpstream
        | FailureKind::PluginFailure => {
            let d = format!("{label}:{}", failure.kind.reason_code());
            meta.fallback_path.push(d.clone());
            d
        }
        FailureKind::TargetError => {
            let d = format!("{label}:target_error");
            meta.fallback_path.push(d.clone());
            d
        }
        FailureKind::BadRequest | FailureKind::PolicyRejected => {
            format!("{label}:{}", failure.kind.reason_code())
        }
        FailureKind::ClientCancelled => "client_cancelled".into(),
    };

    trace.failure(Some(label), detail, failure.kind, failure.status);
    if transition_applied && n >= CIRCUIT_THRESHOLD {
        trace.step(
            "skip",
            None,
            format!("circuit opened for account after {n} consecutive failures"),
        );
    }
    if transition_applied {
        // Refresh only after this request successfully mutated account state.
        let _ = state.registry.reload(&state.pool).await;
    }
}

fn safe_anthropic_error_body(body: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    if parsed.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let error = parsed.get("error")?.as_object()?;
    let error_type = error.get("type")?.as_str()?;
    let message = error.get("message")?.as_str()?;
    let mut out = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": crate::crypto::redact(message),
        }
    });
    if let Some(request_id) = parsed
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
    {
        out["request_id"] = serde_json::json!(request_id);
    }
    Some(out)
}

fn anthropic_error_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            let allowed = matches!(
                name,
                "retry-after" | "retry-after-ms" | "x-should-retry" | "request-id"
            ) || name.starts_with("anthropic-ratelimit-");
            if !allowed {
                return None;
            }
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), value.to_string()))
        })
        .collect()
}

fn preserve_anthropic_error(
    mut error: ProxyError,
    format: FrontendFormat,
    adapter: &dyn Adapter,
    failure: &UpstreamFailure,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: Option<&str>,
) -> ProxyError {
    if format != FrontendFormat::Anthropic
        || adapter.wire_format() != "anthropic"
        || failure.kind == FailureKind::AuthError
    {
        return error;
    }

    error.http_status_override = Some(status);
    error.headers.extend(anthropic_error_headers(headers));
    if let Some(body) = body.and_then(safe_anthropic_error_body) {
        error.body_override = Some(body);
    }
    error
}

fn apply_header_reset_to_rate_limit(
    mut failure: UpstreamFailure,
    quota: Option<crate::quota::QuotaHeaderObservation>,
) -> UpstreamFailure {
    if failure.kind == FailureKind::RateLimit && failure.retry_after_secs.is_none() {
        failure.retry_after_secs =
            quota.and_then(|observation| observation.retry_after_secs(chrono::Utc::now()));
    }
    failure
}

async fn recover_successful_account(
    state: &AppState,
    account_id: &str,
    observed_state_version: i64,
    is_half_open_probe: bool,
) {
    match pool::recover_after_success(
        &state.pool,
        account_id,
        observed_state_version,
        is_half_open_probe,
    )
    .await
    {
        Ok(true) => {
            if let Err(error) = state.registry.reload(&state.pool).await {
                tracing::warn!(
                    account = %account_id,
                    %error,
                    "account circuit recovered but registry reload failed"
                );
            }
        }
        Ok(false) => {}
        Err(error) => tracing::warn!(
            account = %account_id,
            %error,
            "failed to recover account circuit after successful response"
        ),
    }
}

fn policy_rejection(
    trace: &mut RouteTrace,
    target: Option<String>,
    message: impl Into<String>,
) -> ProxyError {
    let message = message.into();
    trace.failure(target, message.clone(), FailureKind::PolicyRejected, None);
    let mut error = ProxyError::bad_request(message);
    error.http_status_override = FailureKind::PolicyRejected.client_status(None);
    error
}

async fn finish_policy_rejection(
    state: &AppState,
    meta: &RequestMeta,
    trace: &mut RouteTrace,
    target: Option<String>,
    message: impl Into<String>,
    started: Instant,
) -> ProxyError {
    let error = policy_rejection(trace, target, message);
    trace.finish("rejected");
    state.live.finish(
        &meta.request_id,
        "rejected",
        started.elapsed().as_millis() as u64,
        None,
        None,
    );
    let _ = db::insert_route_trace(&state.pool, trace).await;
    error
}

fn failure_to_error(failure: &UpstreamFailure, target: &ResolvedTarget) -> ProxyError {
    match failure.kind {
        FailureKind::RateLimit | FailureKind::QuotaExhausted => ProxyError::rate_limited(
            failure.message.clone(),
            failure.kind.retry_after_secs(failure),
        ),
        FailureKind::BadRequest => ProxyError::bad_request(failure.message.clone()),
        FailureKind::PolicyRejected => {
            let mut error = ProxyError::bad_request(failure.message.clone());
            error.http_status_override = failure.kind.client_status(failure.status);
            error
        }
        FailureKind::AuthError => ProxyError::upstream(format!(
            "upstream authentication failed for provider '{}'",
            target.provider.name
        )),
        FailureKind::TargetError => match failure.status {
            Some(403) => {
                ProxyError::new(crate::types::ErrorKind::Forbidden, failure.message.clone())
            }
            Some(404) => ProxyError::not_found(failure.message.clone()),
            _ => ProxyError::upstream(failure.message.clone()),
        },
        FailureKind::Timeout => {
            let mut error = ProxyError::upstream("upstream request timed out".to_string());
            error.http_status_override = failure.kind.client_status(failure.status);
            error
        }
        FailureKind::ConnectionError
        | FailureKind::ServerError
        | FailureKind::MalformedUpstream
        | FailureKind::PluginFailure => ProxyError::upstream(failure.message.clone()),
        FailureKind::ClientCancelled => ProxyError::new(
            crate::types::ErrorKind::ClientCancelled,
            "client cancelled request",
        ),
    }
}

fn record_provider_circuit_reject(
    state: &AppState,
    target: &ResolvedTarget,
    meta: &mut RequestMeta,
    trace: &mut RouteTrace,
    reject: crate::provider_circuit::ProviderCircuitReject,
) -> ProxyError {
    let detail = format!(
        "provider_circuit_open: retry in {}s",
        reject.retry_after_secs
    );
    trace.step("skip", Some(target.account.label.clone()), detail);
    meta.fallback_path
        .push(format!("{}:provider_circuit_open", target.account.label));
    state.record_skip();
    state
        .target_telemetry
        .record(crate::target_telemetry::TelemetryEvent::synthetic(
            traffic_key(target),
            crate::target_telemetry::TelemetryOutcome::ProviderCircuitReject,
        ));
    ProxyError::all_unavailable(
        format!(
            "provider '{}' temporarily unavailable",
            target.provider.name
        ),
        Some(reject.retry_after_secs),
    )
}

fn account_skip_detail(target: &ResolvedTarget, status: pool::AccountStatus) -> String {
    let lifecycle = pool::lifecycle_at(&target.account, chrono::Utc::now());
    let mut detail = format!(
        "model={} skipped({}); reason_code={}",
        target.model.display_name,
        status.as_admin_str(),
        lifecycle.reason_code
    );
    if let Some(retry_at) = lifecycle.retry_at {
        detail.push_str(&format!("; retry_at={retry_at}"));
    }
    match status {
        pool::AccountStatus::Cooldown => {
            if let Some(until) = target.account.cooldown_until.as_deref() {
                detail.push_str(&format!("; cooldown_until={until}"));
            }
        }
        pool::AccountStatus::Exhausted => {
            if let Some(reset) = target.account.quota_reset_at.as_deref() {
                detail.push_str(&format!("; quota_reset_at={reset}"));
            }
        }
        pool::AccountStatus::CircuitOpen => {
            detail.push_str("; circuit_prevented_execution=true");
            if let Some(until) = target.account.circuit_open_until.as_deref() {
                detail.push_str(&format!("; circuit_open_until={until}"));
            }
        }
        pool::AccountStatus::Disabled => {}
        pool::AccountStatus::Healthy => {}
    }
    detail
}

/// Select healthy accounts for a provider's pool (single-model route).
/// Select candidate accounts for a provider from the request's snapshot.
///
/// Health state is refreshed into the snapshot by the background reload loop,
/// so no database I/O happens on the request path here (NFR-1.7).
fn select_accounts(
    snap: &crate::registry::Snapshot,
    provider_id: &str,
    preferred: Option<&str>,
) -> Result<Vec<db::AccountRow>, ProxyError> {
    let accounts: Vec<db::AccountRow> = snap
        .accounts
        .values()
        .filter(|a| a.provider_id == provider_id)
        .cloned()
        .collect();
    let available: Vec<db::AccountRow> = accounts
        .iter()
        .filter(|a| matches!(pool::effective_status(a), pool::AccountStatus::Healthy))
        .cloned()
        .collect();

    // Keep unavailable rows only when the whole pool is unavailable so the
    // attempt loop can report the actual account state. Half-open probing is
    // circuit-only in pool::should_probe().
    if available.is_empty() {
        return Ok(pool::order_accounts(accounts, preferred));
    }

    Ok(pool::order_accounts(available, preferred))
}

fn select_accounts_for_simulation(
    snap: &crate::registry::Snapshot,
    provider_id: &str,
    seed: u64,
) -> Vec<db::AccountRow> {
    let accounts: Vec<_> = snap
        .accounts
        .values()
        .filter(|account| account.provider_id == provider_id)
        .cloned()
        .collect();
    let available: Vec<_> = accounts
        .iter()
        .filter(|account| {
            matches!(
                pool::effective_status(account),
                pool::AccountStatus::Healthy
            )
        })
        .cloned()
        .collect();
    order_accounts_for_simulation(
        if available.is_empty() {
            accounts
        } else {
            available
        },
        seed,
    )
}

/// Gather plugin routing facts for a request (§6.4).
///
/// Every enabled plugin that declares a `routing_facts` capability is invoked
/// once, within its 25 ms budget. Facts are namespaced `plugin.<id>.<name>` and
/// are treated as `unknown` when absent. A provider that fails or times out
/// contributes no facts and is recorded as a failure so the Route Trace can
/// explain the `unknown`.
///
/// Determinism (§6.4): the manifest is required to be `pure` (no `host-http`
/// import; the host refuses buffered HTTP to routing-fact worlds) or `cached`
/// (the guest returns precomputed values with `observed_at`/`max_age_ms`).
/// Stale cached facts are dropped here and therefore evaluate as `unknown`.
async fn gather_plugin_facts(
    state: &AppState,
    req: &RequestFacts<'_>,
    disconnected: Option<&Arc<std::sync::atomic::AtomicBool>>,
) -> PluginFacts {
    let mut facts = PluginFacts::default();
    let Some(manager) = state.plugin_manager() else {
        return facts;
    };
    let plugins = match manager.list().await {
        Ok(p) => p,
        Err(_) => return facts,
    };
    // Only request/config facts core already knows are exposed to the plugin.
    let request_json = serde_json::json!({
        "frontend": req.frontend,
        "requested_model": req.requested_model,
        "requested_route": req.requested_route,
        "key_tag": req.key_tag,
        "has_tools": req.has_tools,
        "has_images": req.has_images,
        "has_reasoning": req.has_reasoning,
        "input_tokens": req.input_tokens,
    })
    .to_string();

    for row in plugins {
        if !row.status().is_enabled() {
            continue;
        }
        let Some(manifest) = row.manifest() else {
            continue;
        };
        if manifest.provides.routing_facts.is_empty() {
            continue;
        }
        // §6.4 `cached` mode: read the host-stamped snapshot the plugin published
        // on its background schedule. No guest call, no network, no wall time on
        // the request path — the determinism guarantee.
        if manifest.routing_facts_mode == "cached" {
            match manager.cached_facts(&row.id).await {
                Ok(entries) => {
                    for (name, value, observed, max_age) in entries {
                        let full = format!("plugin.{}.{}", row.id, name);
                        if let (Some(observed), Some(max_age)) = (&observed, max_age) {
                            if let Some(ts) = db::parse_dt(observed) {
                                let age = chrono::Utc::now().signed_duration_since(ts);
                                if age.num_milliseconds() > max_age as i64 {
                                    continue; // stale => absent => unknown
                                }
                            }
                        }
                        let source = serde_json::json!({
                            "plugin": row.id,
                            "version": row.version,
                            "capability": "routing-facts",
                            "mode": "cached",
                            "observed_at": observed,
                        });
                        facts.insert(full, value, source);
                    }
                }
                Err(e) => {
                    facts.failures.push((row.id.clone(), e.to_string()));
                }
            }
            continue;
        }
        let call = match disconnected {
            Some(flag) => {
                manager
                    .routing_facts_cancellable(&row.id, &request_json, flag.clone())
                    .await
            }
            None => manager.routing_facts(&row.id, &request_json).await,
        };
        match call {
            Ok(list) => {
                for f in list {
                    // Staleness: a cached fact past its max_age is `unknown`.
                    if let (Some(observed), Some(max_age)) = (&f.observed_at, f.max_age_ms) {
                        if let Some(ts) = db::parse_dt(observed) {
                            let age = chrono::Utc::now().signed_duration_since(ts);
                            if age.num_milliseconds() > max_age as i64 {
                                continue; // stale => absent => unknown
                            }
                        }
                    }
                    let value: Value = serde_json::from_str(&f.value_json)
                        .unwrap_or(Value::String(f.value_json.clone()));
                    let source = serde_json::json!({
                        "plugin": row.id,
                        "version": row.version,
                        "capability": "routing-facts",
                    });
                    facts.insert(f.name, value, source);
                }
            }
            Err(fault) => {
                // A client-driven cancellation is not a fact-provider failure
                // (§7.2); it simply yields no facts, which evaluate as unknown.
                if fault.code() != "cancelled" {
                    facts.failures.push((row.id.clone(), fault.message()));
                }
            }
        }
    }
    facts
}

/// Order sibling accounts for one logical route target. Account priority is
/// primary; weight biases the first choice within equal-priority tiers while
/// retaining every sibling for fallback.
fn order_route_account_candidates(
    targets: Vec<ResolvedTarget>,
    simulation_seed: Option<u64>,
) -> Vec<ResolvedTarget> {
    let accounts: Vec<_> = targets
        .iter()
        .map(|target| target.account.clone())
        .collect();
    let accounts = match simulation_seed {
        Some(seed) => order_accounts_for_simulation(accounts, seed),
        None => pool::order_accounts(accounts, None),
    };
    let mut by_account: std::collections::HashMap<String, ResolvedTarget> = targets
        .into_iter()
        .map(|target| (target.account.id.clone(), target))
        .collect();

    accounts
        .into_iter()
        .filter_map(|account| by_account.remove(&account.id))
        .collect()
}

fn order_accounts_for_simulation(
    mut accounts: Vec<db::AccountRow>,
    seed: u64,
) -> Vec<db::AccountRow> {
    accounts.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut start = 0;
    while start < accounts.len() {
        let priority = accounts[start].priority;
        let mut end = start + 1;
        while end < accounts.len() && accounts[end].priority == priority {
            end += 1;
        }
        for index in start..end {
            let total: u64 = accounts[index..end]
                .iter()
                .map(|account| crate::pre_dispatch::normalized_weight(account.weight))
                .sum();
            let identity = accounts[index..end]
                .iter()
                .map(|account| account.id.as_str())
                .collect::<Vec<_>>()
                .join("|");
            let pick = stable_route_hash(seed ^ index as u64, identity.as_bytes()) % total;
            let mut cumulative = 0;
            let mut selected = index;
            for (offset, account) in accounts[index..end].iter().enumerate() {
                cumulative += crate::pre_dispatch::normalized_weight(account.weight);
                if pick < cumulative {
                    selected = index + offset;
                    break;
                }
            }
            accounts.swap(index, selected);
        }
        start = end;
    }
    accounts
}

fn stable_route_hash(seed: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0xcbf29ce484222325_u64 ^ seed, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

fn adaptive_account_dispatchable(target: &ResolvedTarget) -> bool {
    let status = pool::effective_status(&target.account);
    matches!(status, pool::AccountStatus::Healthy)
        || (matches!(status, pool::AccountStatus::CircuitOpen)
            && pool::should_probe(&target.account))
}

/// Order logical route targets according to the route strategy (FR-12.5), then
/// flatten each target's account pool. A provider with N accounts therefore
/// does not receive N times the configured route weight or round-robin share.
async fn order_route_targets(
    state: &AppState,
    route: &db::RouteRow,
    targets: Vec<ResolvedTarget>,
) -> OrderedRouteTargets {
    order_route_targets_inner(state, route, targets, None).await
}

async fn order_route_targets_for_simulation(
    state: &AppState,
    route: &db::RouteRow,
    targets: Vec<ResolvedTarget>,
    seed: u64,
) -> OrderedRouteTargets {
    order_route_targets_inner(state, route, targets, Some(seed)).await
}

async fn order_route_targets_inner(
    state: &AppState,
    route: &db::RouteRow,
    targets: Vec<ResolvedTarget>,
    simulation_seed: Option<u64>,
) -> OrderedRouteTargets {
    // Freeze adaptive telemetry for this ordering pass. acquire() still
    // performs the authoritative live capacity check immediately before
    // dispatch, but one sort must never observe a moving comparator.
    let adaptive_dispatchable = (route.strategy == "adaptive").then(|| {
        targets
            .iter()
            .filter(|target| {
                adaptive_account_dispatchable(target)
                    && state
                        .provider_circuits
                        .availability(&target.provider.id)
                        .available
            })
            .map(adaptive_candidate_key)
            .collect::<std::collections::HashSet<_>>()
    });
    let quota_evidence = adaptive_dispatchable
        .as_ref()
        .map(|dispatchable_keys| {
            targets
                .iter()
                .filter(|target| dispatchable_keys.contains(&adaptive_candidate_key(target)))
                .map(|target| {
                    (
                        adaptive_candidate_key(target),
                        state
                            .quota
                            .adaptive_snapshot(&target.provider.id, &target.account.id),
                    )
                })
                .collect::<std::collections::HashMap<_, _>>()
        })
        .unwrap_or_default();
    let adaptive_quota = quota_evidence
        .iter()
        .filter_map(|(key, snapshot)| snapshot.clone().map(|snapshot| (key.clone(), snapshot)))
        .collect::<std::collections::HashMap<_, _>>();
    let adaptive_scores = adaptive_dispatchable.as_ref().map(|dispatchable_keys| {
        let dispatchable: Vec<_> = targets
            .iter()
            .filter(|target| dispatchable_keys.contains(&adaptive_candidate_key(target)))
            .cloned()
            .collect();
        let snapshots = snapshot_traffic_targets(state, &dispatchable);
        build_adaptive_scores_with_quota(&dispatchable, &snapshots, &adaptive_quota)
    });

    let mut groups: Vec<Vec<ResolvedTarget>> = Vec::new();
    let mut positions: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for target in targets {
        let key = target
            .route_target_id
            .clone()
            .unwrap_or_else(|| format!("account:{}", target.account.id));
        if let Some(idx) = positions.get(&key).copied() {
            groups[idx].push(target);
        } else {
            positions.insert(key, groups.len());
            groups.push(vec![target]);
        }
    }

    for group in &mut groups {
        let seed = simulation_seed.map(|seed| {
            let key = group
                .first()
                .and_then(|target| target.route_target_id.as_deref())
                .unwrap_or("direct");
            stable_route_hash(seed, key.as_bytes())
        });
        *group = order_route_account_candidates(std::mem::take(group), seed);
    }

    match route.strategy.as_str() {
        "round-robin" => {
            let n = if simulation_seed.is_some() {
                state.rr_counter_snapshot(&route.id)
            } else {
                state.rr_counter(&route.id).fetch_add(1, Ordering::Relaxed)
            } as usize;
            if !groups.is_empty() {
                let offset = n % groups.len();
                groups.rotate_left(offset);
            }
        }
        "weighted" => {
            use rand::Rng;
            let total: u64 = groups
                .iter()
                .filter_map(|group| group.first())
                .map(|target| crate::pre_dispatch::normalized_weight(target.weight))
                .sum();
            if total > 0 {
                let mut pick = simulation_seed
                    .map(|seed| stable_route_hash(seed, route.id.as_bytes()) % total)
                    .unwrap_or_else(|| rand::thread_rng().gen_range(0..total));
                let mut index = 0;
                for (i, group) in groups.iter().enumerate() {
                    let weight = group
                        .first()
                        .map(|target| crate::pre_dispatch::normalized_weight(target.weight))
                        .unwrap_or(1);
                    if pick < weight {
                        index = i;
                        break;
                    }
                    pick -= weight;
                }
                groups.rotate_left(index);
            }
        }
        "adaptive" => {
            let dispatchable_keys = adaptive_dispatchable
                .as_ref()
                .expect("adaptive dispatchable accounts");
            let scores = adaptive_scores.as_ref().expect("adaptive route scores");
            let compare =
                |a: &ResolvedTarget, b: &ResolvedTarget| compare_adaptive_targets(a, b, scores);
            let is_dispatchable = |target: &ResolvedTarget| {
                dispatchable_keys.contains(&adaptive_candidate_key(target))
            };

            // Only currently dispatchable accounts participate in adaptive
            // telemetry. Unavailable siblings remain in the fallback list, but
            // can neither shift the neutral TTFT nor represent their logical
            // route target.
            for group in &mut groups {
                let mut dispatchable = Vec::new();
                let mut unavailable = Vec::new();
                for target in std::mem::take(group) {
                    if is_dispatchable(&target) {
                        dispatchable.push(target);
                    } else {
                        unavailable.push(target);
                    }
                }
                dispatchable.sort_by(&compare);
                dispatchable.extend(unavailable);
                *group = dispatchable;
            }
            groups.sort_by(|a, b| {
                let a_rep = a.iter().find(|target| is_dispatchable(target));
                let b_rep = b.iter().find(|target| is_dispatchable(target));
                match (a_rep, b_rep) {
                    (Some(a), Some(b)) => compare(a, b),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a
                        .first()
                        .map(|target| target.priority)
                        .unwrap_or(i64::MAX)
                        .cmp(&b.first().map(|target| target.priority).unwrap_or(i64::MAX)),
                }
            });
        }
        "least-used" => {
            let (_, by_account) = db::lifetime_totals(&state.pool).await.unwrap_or_default();
            groups.sort_by_key(|group| {
                let requests = group
                    .iter()
                    .map(|t| by_account.get(&t.account.id).map(|(n, _)| *n).unwrap_or(0))
                    .min()
                    .unwrap_or(0);
                let priority = group.first().map(|t| t.priority).unwrap_or(i64::MAX);
                (requests, priority)
            });
        }
        _ => {
            groups.sort_by_key(|group| group.first().map(|t| t.priority).unwrap_or(i64::MAX));
        }
    }

    OrderedRouteTargets {
        targets: groups.into_iter().flatten().collect(),
        quota_evidence,
    }
}

/// Apply a route's continuity/portability policy when falling back across
/// providers (FR-2.11, FR-12.13).
///
/// * `reject` — refuse to fall back to a target that cannot carry the
///   non-portable opaque state; the client gets a clear error.
/// * `strip_with_warning` — remove the non-portable state, record it in the
///   Route Trace, and emit a client-visible warning. Silent stripping is
///   forbidden.
#[derive(Clone, PartialEq, Eq)]
struct ContinuationOrigin {
    provider_id: String,
    upstream_model_id: String,
    families: HashSet<String>,
}

fn continuation_origin(target: &ResolvedTarget) -> ContinuationOrigin {
    let capabilities =
        serde_json::from_str::<Value>(&target.model.capabilities).unwrap_or(Value::Null);
    let families = capabilities
        .get("continuation_families")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|family| !family.is_empty())
        .map(str::to_owned)
        .collect();
    ContinuationOrigin {
        provider_id: target.provider.id.clone(),
        upstream_model_id: target.model.upstream_id.clone(),
        families,
    }
}

fn anthropic_continuation_is_compatible(
    source: Option<&ContinuationOrigin>,
    target: &ResolvedTarget,
) -> bool {
    let Some(source) = source else {
        return false;
    };
    if source.provider_id == target.provider.id
        && source.upstream_model_id == target.model.upstream_id
    {
        return true;
    }

    let target_families = continuation_origin(target).families;
    source
        .families
        .iter()
        .any(|family| target_families.contains(family))
}

fn request_has_nonportable_inline_state(
    req: &InternalRequest,
    preserves_anthropic_thinking: bool,
    include_generic_opaque: bool,
) -> bool {
    req.messages.iter().any(|message| {
        message.parts.iter().any(|part| match part {
            crate::types::Part::Thinking { signature, .. } => {
                !preserves_anthropic_thinking
                    || !signature
                        .as_deref()
                        .is_some_and(|signature| !signature.is_empty())
            }
            crate::types::Part::RedactedThinking { .. } => !preserves_anthropic_thinking,
            crate::types::Part::ToolCall {
                signature: Some(_), ..
            } => include_generic_opaque,
            _ => false,
        })
    })
}

fn strip_opaque_raw_body(req: &mut InternalRequest, preserves_anthropic_thinking: bool) {
    let Some(raw) = req.raw_body.as_deref() else {
        return;
    };
    let Ok(mut value) = serde_json::from_str::<Value>(raw) else {
        req.raw_body = None;
        return;
    };
    let Some(messages) = value.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        if let Some(object) = message.as_object_mut() {
            object.remove("reasoning_content");
            object.remove("reasoning_signature");
            if object.get("reasoning").is_some_and(Value::is_string) {
                object.remove("reasoning");
            }
        }
        let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        parts.retain(|part| match part.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                preserves_anthropic_thinking
                    && part
                        .get("signature")
                        .and_then(Value::as_str)
                        .is_some_and(|signature| !signature.is_empty())
            }
            Some("redacted_thinking") => {
                preserves_anthropic_thinking && part.get("data").and_then(Value::as_str).is_some()
            }
            _ => true,
        });
        for part in parts {
            let block_type = part.get("type").and_then(Value::as_str);
            if !preserves_anthropic_thinking
                || !matches!(block_type, Some("thinking" | "redacted_thinking"))
            {
                if let Some(object) = part.as_object_mut() {
                    object.remove("signature");
                    object.remove("thoughtSignature");
                }
            }
        }
    }
    req.raw_body = serde_json::to_string(&value).ok();
}

/// The result of resolving stored opaque continuation state for one candidate
/// target. Kept internal to the data plane; it never crosses the HTTP boundary.
#[derive(Default)]
struct OpaqueHydrationReport {
    /// Compatible stored signatures that should be restored onto the canonical
    /// request (applied only after portability handling).
    restored: usize,
    /// Stored signatures that exist but this target cannot carry.
    incompatible: usize,
    /// Tool-call ids whose stored state exists but this target cannot carry.
    /// Lets the portability policy translate those specific historical calls
    /// (e.g. with a provider placeholder) instead of stripping them blindly.
    incompatible_ids: Vec<String>,
    /// Stored signatures that could not be decrypted (treated as lost, never
    /// surfaced to the client).
    unavailable: usize,
    /// `(tool_call_id, signature)` restorations to apply. Keyed by the
    /// client-visible tool-call id rather than a positional index so a
    /// portability strip (which removes parts) cannot invalidate the mapping.
    restorations: Vec<(String, String)>,
}

impl OpaqueHydrationReport {
    /// Whether any stored continuation state was found for this conversation,
    /// regardless of whether the target can carry it.
    fn known_state(&self) -> bool {
        self.restored + self.incompatible + self.unavailable > 0
    }

    /// Whether stored continuation state exists that this target cannot carry,
    /// which is what portability policy must act on.
    fn nonportable(&self) -> bool {
        self.incompatible > 0
    }
}

/// Resolve stored opaque continuation state for every historical tool call that
/// lacks an inline signature. Compatible records are collected for restoration;
/// incompatible records feed portability policy. A tool-call id reused with a
/// different tool name is a hard client error and never reaches the upstream
/// (§20).
async fn resolve_opaque_state(
    state: &AppState,
    req: &InternalRequest,
    key: Option<&db::VirtualKeyRow>,
    session: Option<&str>,
    target: &ResolvedTarget,
    adapter: &dyn Adapter,
) -> Result<OpaqueHydrationReport, ProxyError> {
    // Fast path: without any historical tool call lacking a signature there is
    // nothing to recover, so no opaque-state work happens at all (§64).
    let needs_lookup = req.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                crate::types::Part::ToolCall {
                    id: Some(_),
                    signature: None,
                    ..
                }
            )
        })
    });
    if !needs_lookup {
        return Ok(OpaqueHydrationReport::default());
    }

    let capability = adapter.opaque_state_target(&target.model);
    let scope = match key {
        Some(key) => OpaqueClientScope::for_key(&key.id),
        None => OpaqueClientScope::internal(),
    };

    let mut report = OpaqueHydrationReport::default();
    for message in &req.messages {
        for part in &message.parts {
            let crate::types::Part::ToolCall {
                id: Some(id),
                name,
                signature: None,
                ..
            } = part
            else {
                continue;
            };
            match state
                .opaque_state
                .resolve_tool_signature(&scope, capability.as_ref(), session, id, name)
                .await
            {
                OpaqueLookupResult::Compatible(signature) => {
                    report.restored += 1;
                    report.restorations.push((id.clone(), signature));
                }
                OpaqueLookupResult::Incompatible => {
                    report.incompatible += 1;
                    report.incompatible_ids.push(id.clone());
                }
                OpaqueLookupResult::Unavailable => report.unavailable += 1,
                OpaqueLookupResult::ToolNameMismatch => {
                    // Malformed/reused conversation history: fail closed with a
                    // clear client error instead of letting the provider return
                    // a less useful signature error.
                    return Err(ProxyError::bad_request(format!(
                        "tool call id '{id}' was reused with a different tool name"
                    )));
                }
                // A session conflict simply means we must not restore; it is
                // not a portability boundary.
                OpaqueLookupResult::SessionMismatch | OpaqueLookupResult::Missing => {}
            }
        }
    }
    Ok(report)
}

/// Apply the resolved compatible signatures onto the target-local request
/// clone. Never overwrites an explicit client/canonical signature (§21).
fn hydrate_opaque_state(req: &mut InternalRequest, report: &OpaqueHydrationReport) {
    for (tool_call_id, signature) in &report.restorations {
        for message in &mut req.messages {
            for part in &mut message.parts {
                if let crate::types::Part::ToolCall {
                    id: Some(id),
                    signature: slot,
                    ..
                } = part
                {
                    if id == tool_call_id && slot.is_none() {
                        *slot = Some(signature.clone());
                    }
                }
            }
        }
    }
}

/// Apply a route's continuity/portability policy when falling back across
/// providers or translating across formats (FR-2.11, FR-12.13).
///
/// * `reject` — refuse to fall back to a target that cannot carry the
///   non-portable opaque state; the client gets a clear error.
/// * `strip_with_warning` — remove the non-portable state, record it in the
///   Route Trace, and emit a client-visible warning. Silent stripping is
///   forbidden.
///
/// `inline_opaque` reflects state that this target cannot carry; `report` reflects
/// host-owned stored continuation state resolved for this target. Either source
/// can make the conversation non-portable for the candidate target.
///
/// `placeholder` is the target adapter's documented stand-in for a historical
/// call whose real signature cannot be carried (e.g. a different model in the
/// same protocol family). When present, the incompatible call is translated
/// with that placeholder rather than left unsigned — an unsigned historical
/// call is exactly what the provider rejects. Routing policy still wins: a
/// `reject` Route refuses before any placeholder is applied.
fn apply_portability(
    req: &mut InternalRequest,
    route: &db::RouteRow,
    target: &ResolvedTarget,
    inline_opaque: bool,
    report: &OpaqueHydrationReport,
    placeholder: Option<&'static str>,
    preserves_anthropic_thinking: bool,
    trace: &mut RouteTrace,
) -> Result<(), ProxyError> {
    if !inline_opaque && !report.nonportable() {
        return Ok(());
    }

    if route.portability() == "reject" {
        return Err(ProxyError::unsupported(format!(
            "route '{}' forbids fallback because the conversation contains non-portable provider continuation state for target '{}'",
            route.name, target.model.display_name
        )));
    }

    // portability=strip_with_warning
    let placed = strip_nonportable_state(req, report, placeholder, preserves_anthropic_thinking);
    let warning = portability_warning(target, placed);
    trace.warn(warning.clone());
    tracing::warn!(route = %route.name, "{}", warning);
    Ok(())
}

/// Remove non-portable continuation state (thinking parts and tool-call
/// signatures) from the request and, where the target adapter documents a
/// substitute, paint its placeholder onto the historical calls it cannot carry.
/// Explicitly compatible Anthropic thinking blocks remain unchanged.
/// Returns the number of placeholders applied. Shared by Route-based
/// `strip_with_warning` fallback and a direct target that has a protocol-valid
/// same-family translation.
fn strip_nonportable_state(
    req: &mut InternalRequest,
    report: &OpaqueHydrationReport,
    placeholder: Option<&'static str>,
    preserves_anthropic_thinking: bool,
) -> usize {
    let mut placed = 0usize;
    for msg in &mut req.messages {
        msg.parts.retain(|part| match part {
            crate::types::Part::Thinking { signature, .. } => {
                preserves_anthropic_thinking
                    && signature
                        .as_deref()
                        .is_some_and(|signature| !signature.is_empty())
            }
            crate::types::Part::RedactedThinking { .. } => preserves_anthropic_thinking,
            _ => true,
        });
        for part in &mut msg.parts {
            if let crate::types::Part::ToolCall { id, signature, .. } = part {
                *signature = None;
                let was_incompatible = id
                    .as_deref()
                    .is_some_and(|id| report.incompatible_ids.iter().any(|known| known == id));
                if was_incompatible {
                    if let Some(placeholder) = placeholder {
                        *signature = Some(placeholder.to_string());
                        placed += 1;
                    }
                }
            }
        }
    }
    strip_opaque_raw_body(req, preserves_anthropic_thinking);
    placed
}

/// Client-visible wording for a portability decision: a substituted
/// placeholder is a replacement, an unsigned call is an omission.
fn portability_warning(target: &ResolvedTarget, placed: usize) -> String {
    if placed > 0 {
        format!(
            "non-portable provider continuation state was replaced with the provider's documented placeholder for fallback to '{}'",
            target.model.display_name
        )
    } else {
        format!(
            "non-portable provider continuation state was omitted for fallback to '{}'",
            target.model.display_name
        )
    }
}

/// Check the admin's parameter policy; reject when a value is unsupported and
/// the policy is `reject` (FR-10.6).
#[cfg(test)]
fn thinking_level_key(level: crate::types::ThinkingLevel) -> &'static str {
    level.as_key()
}

#[cfg(test)]
fn check_thinking_translation_for_adapter(
    adapter: &dyn Adapter,
    target: &ResolvedTarget,
    req: &InternalRequest,
) -> Result<(), ProxyError> {
    if req.thinking.is_some() && adapter.handles_thinking_translation() {
        return Ok(());
    }
    check_thinking_translation(target, req)
}

fn check_resolved_thinking_translation(
    adapter: &dyn Adapter,
    target: &ResolvedTarget,
    profile: &crate::adapters::ResolvedExecutionProfile,
    req: &InternalRequest,
) -> Result<(), ProxyError> {
    if req.thinking.is_none() || adapter.handles_thinking_translation() {
        return Ok(());
    }
    let level = req.thinking.expect("checked above");
    let key = level.as_key();
    let thinking = &profile.thinking_map;

    if level == crate::types::ThinkingLevel::Default && thinking.is_adaptive() {
        return Ok(());
    }
    if level == crate::types::ThinkingLevel::Off
        && profile.transport == crate::adapters::TargetTransport::Anthropic
        && thinking.is_adaptive()
        && thinking.anthropic_adaptive_off_is_executable()
    {
        return Ok(());
    }
    if thinking.level_is_executable(key) {
        return Ok(());
    }
    Err(ProxyError::unsupported(format!(
        "thinking level '{key}' has no executable mapping for model '{}' on transport '{}'",
        target.model.display_name,
        profile.transport.as_str()
    )))
}

#[cfg(test)]
fn check_thinking_translation(
    target: &ResolvedTarget,
    req: &InternalRequest,
) -> Result<(), ProxyError> {
    let Some(level) = req.thinking else {
        return Ok(());
    };
    let key = thinking_level_key(level);

    // Adaptive requests with omitted effort preserve the target model's native
    // default. Every explicit level, including "off", requires an executable
    // per-model mapping.
    if level == crate::types::ThinkingLevel::Default && target.model.thinking().is_adaptive() {
        return Ok(());
    }
    if level == crate::types::ThinkingLevel::Off
        && target.provider.wire() == crate::types::WireFormat::Anthropic
        && target.model.thinking().is_adaptive()
    {
        if target
            .model
            .thinking()
            .anthropic_adaptive_off_is_executable()
        {
            return Ok(());
        }
    } else if target.model.thinking().level_is_executable(key) {
        return Ok(());
    }
    Err(ProxyError::unsupported(format!(
        "thinking level '{key}' has no executable mapping for model '{}'",
        target.model.display_name
    )))
}

#[derive(Debug, Clone)]
struct CapabilityEligibility {
    eligible: bool,
    details: Value,
    reasons: Vec<String>,
}

#[derive(Debug, Clone)]
struct RequestEligibilityDecision {
    predicate: predicate::Eligibility,
    execution_profile: Option<crate::adapters::ResolvedExecutionProfile>,
    execution_profile_error: Option<String>,
    capability: CapabilityEligibility,
    integration_features_allowed: bool,
    context_eligible: bool,
    provider_permitted: bool,
}

impl RequestEligibilityDecision {
    fn eligible(&self) -> bool {
        self.predicate.eligible
            && self.execution_profile_error.is_none()
            && self.capability.eligible
            && self.integration_features_allowed
            && self.context_eligible
            && self.provider_permitted
    }

    fn explanation(&self) -> String {
        let mut reasons = Vec::new();
        if !self.predicate.eligible {
            reasons.push(format!("predicate: {}", self.predicate.explanation));
        }
        if let Some(error) = &self.execution_profile_error {
            reasons.push(format!("invalid execution profile: {error}"));
        } else if !self.capability.eligible {
            reasons.extend(self.capability.reasons.iter().cloned());
        }
        if !self.integration_features_allowed {
            reasons.push("integration_feature_ceiling".to_string());
        }
        if !self.provider_permitted {
            reasons.push("provider_not_permitted".to_string());
        }
        if !self.context_eligible {
            reasons.push("context_window".to_string());
        }
        if reasons.is_empty() {
            "request eligibility satisfied".to_string()
        } else {
            reasons.join("; ")
        }
    }
}

fn evaluate_request_eligibility(
    target: &ResolvedTarget,
    request_facts: &RequestFacts<'_>,
    plugin_facts: &PluginFacts,
    needs: &crate::types::CapabilityNeeds,
    allowed_providers: &[String],
) -> RequestEligibilityDecision {
    let profile = crate::adapters::resolve_execution_profile_for_target(
        &target.provider,
        &target.model,
        Some(target.account.id.as_str()),
    );
    let execution_profile_error = profile.as_ref().err().map(|error| error.message.clone());
    let (capabilities, capabilities_raw, capability) = match profile.as_ref() {
        Ok(profile) => {
            let (capabilities, raw) = predicate_capabilities(profile, &target.model);
            let capability =
                capability_eligibility(&profile.capabilities, needs, target.provider.strict());
            (capabilities, raw, capability)
        }
        Err(error) => (
            target.model.caps(),
            serde_json::from_str::<Value>(&target.model.capabilities).unwrap_or(Value::Null),
            CapabilityEligibility {
                eligible: false,
                details: serde_json::json!({"execution_profile": "invalid"}),
                reasons: vec![format!("invalid execution profile: {}", error.message)],
            },
        ),
    };
    let target_facts = TargetFacts {
        model_id: &target.model.id,
        model_display: &target.model.display_name,
        provider_id: &target.provider.id,
        provider_name: &target.provider.name,
        capabilities: &capabilities,
        capabilities_raw: &capabilities_raw,
        context_window: target.model.context_window,
        max_output_tokens: target.model.max_output_tokens,
    };
    let predicate = predicate::eligibility_with_facts(
        &target.predicate,
        request_facts,
        &target_facts,
        plugin_facts,
    );
    let context_eligible = target
        .model
        .context_window
        .map(|limit| limit <= 0 || request_facts.input_tokens <= limit as u64)
        .unwrap_or(true);
    let integration_features_allowed =
        integration_feature_ceiling_satisfies_needs(&target.provider, needs);
    let provider_permitted =
        allowed_providers.is_empty() || allowed_providers.contains(&target.provider.id);

    RequestEligibilityDecision {
        predicate,
        execution_profile: profile.ok(),
        execution_profile_error,
        capability,
        integration_features_allowed,
        context_eligible,
        provider_permitted,
    }
}

fn capability_eligibility(
    capabilities: &crate::adapters::ModelCapabilityFlags,
    needs: &crate::types::CapabilityNeeds,
    strict: bool,
) -> CapabilityEligibility {
    let mut eligible = true;
    let mut reasons = Vec::new();
    let mut detail = serde_json::Map::new();
    for (name, needed, value) in [
        ("vision", needs.vision, capabilities.vision),
        ("tool_calling", needs.tools, capabilities.tool_calling),
        (
            "parallel_tools",
            needs.parallel_tools,
            capabilities.parallel_tools,
        ),
        ("reasoning", needs.reasoning, capabilities.reasoning),
        (
            "structured_output",
            needs.structured_output,
            capabilities.structured_output,
        ),
    ] {
        let status = match value {
            Some(true) => "supported",
            Some(false) => "unsupported",
            None => "unknown",
        };
        let supported = !needed || value == Some(true) || !strict;
        if needed && !supported {
            eligible = false;
            reasons.push(format!(
                "required {name} capability is {status}{}",
                if status == "unknown" {
                    " under strict capability mode"
                } else {
                    ""
                }
            ));
        }
        detail.insert(
            name.to_string(),
            serde_json::json!({
                "required": needed,
                "status": status,
                "eligible": supported,
            }),
        );
    }
    CapabilityEligibility {
        eligible,
        details: Value::Object(detail),
        reasons,
    }
}

/// Integration declarations are hard ceilings even for permissive providers;
/// model capability metadata remains advisory in that mode.
fn integration_feature_ceiling_satisfies_needs(
    provider: &db::ProviderRow,
    needs: &crate::types::CapabilityNeeds,
) -> bool {
    let ceiling = match provider.integration_feature_ceiling() {
        Ok(Some(ceiling)) => ceiling,
        Ok(None) => return true,
        Err(_) => return false,
    };
    (!needs.vision || ceiling.vision)
        && (!needs.tools || ceiling.tools)
        && (!needs.parallel_tools || ceiling.parallel_tools)
        && (!needs.streaming || ceiling.streaming)
        && (!needs.reasoning || ceiling.reasoning)
        && (!needs.structured_output || ceiling.structured_output)
}

fn provider_accepts_input_protocol(provider: &db::ProviderRow, protocol: &str) -> bool {
    match provider.allows_input_protocol(protocol) {
        Ok(allowed) => allowed,
        Err(error) => {
            tracing::error!(
                provider = %provider.id,
                %error,
                "invalid persisted integration protocol ceiling"
            );
            false
        }
    }
}

fn dry_run_provider_accepts_frontend(provider: &db::ProviderRow, frontend: &str) -> bool {
    let protocol = match frontend {
        "openai" | "openai-chat" => Some("openai-chat"),
        "openai-responses" => Some("openai-responses"),
        "anthropic" => Some("anthropic"),
        "gemini" => Some("gemini"),
        _ => None,
    };
    if let Some(protocol) = protocol {
        return provider_accepts_input_protocol(provider, protocol);
    }
    matches!(provider.integration_protocol_ceiling(), Ok(None))
}

fn provider_satisfies_needs(
    provider: &db::ProviderRow,
    capabilities: &crate::adapters::ModelCapabilityFlags,
    needs: &crate::types::CapabilityNeeds,
) -> bool {
    integration_feature_ceiling_satisfies_needs(provider, needs)
        && (!provider.strict() || profile_satisfies_needs(capabilities, needs, provider.strict()))
}

fn profile_satisfies_needs(
    capabilities: &crate::adapters::ModelCapabilityFlags,
    needs: &crate::types::CapabilityNeeds,
    strict: bool,
) -> bool {
    capability_eligibility(capabilities, needs, strict).eligible
}

fn predicate_capabilities(
    profile: &crate::adapters::ResolvedExecutionProfile,
    model: &db::ModelRow,
) -> (crate::types::Capabilities, Value) {
    let resolved = &profile.capabilities;
    let mut flags = model.caps();
    flags.text = resolved.text == Some(true);
    flags.vision = resolved.vision == Some(true);
    flags.reasoning = resolved.reasoning == Some(true);
    flags.tool_calling = resolved.tool_calling == Some(true);
    flags.parallel_tools = resolved.parallel_tools == Some(true);
    flags.structured_output = resolved.structured_output == Some(true);

    let mut raw = serde_json::from_str::<Value>(&model.capabilities)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    for (name, value) in [
        ("text", resolved.text),
        ("vision", resolved.vision),
        ("reasoning", resolved.reasoning),
        ("tool_calling", resolved.tool_calling),
        ("parallel_tools", resolved.parallel_tools),
        ("structured_output", resolved.structured_output),
    ] {
        raw.insert(name.to_string(), value.map_or(Value::Null, Value::Bool));
    }
    (flags, Value::Object(raw))
}

fn check_param_policy(
    target: &ResolvedTarget,
    profile: &crate::adapters::ResolvedExecutionProfile,
    req: &InternalRequest,
) -> Result<(), ProxyError> {
    let params = &profile.parameters;
    let checks: [(&str, Option<f64>); 3] = [
        ("temperature", req.params.temperature),
        ("top_p", req.params.top_p),
        ("top_k", req.params.top_k),
    ];
    for (name, value) in checks {
        let Some(v) = value else { continue };
        let Some(spec) = params.get(name) else {
            continue;
        };
        if !spec.supported && spec.policy == crate::types::ParamPolicy::Reject {
            return Err(ProxyError::unsupported(format!(
                "parameter '{name}' is not supported by model '{}'",
                target.model.display_name
            )));
        }
        if spec.policy == crate::types::ParamPolicy::Reject {
            if let Some(min) = spec.min {
                if v < min {
                    return Err(ProxyError::bad_request(format!(
                        "parameter '{name}' below minimum {min}"
                    )));
                }
            }
            if let Some(max) = spec.max {
                if v > max {
                    return Err(ProxyError::bad_request(format!(
                        "parameter '{name}' above maximum {max}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Build the client response: streaming or aggregated non-streaming.
#[allow(clippy::too_many_arguments)]
async fn stream_response(
    state: &AppState,
    snap: Arc<crate::registry::Snapshot>,
    format: FrontendFormat,
    mut meta: RequestMeta,
    req: InternalRequest,
    attempt: Attempt,
    encoder_ctx: EncoderCtx,
    started: Instant,
    key: Option<db::VirtualKeyRow>,
    trace: RouteTrace,
) -> Response {
    let stream = req.stream;
    let state = state.clone();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);

    let model_display = attempt.target.model.display_name.clone();
    let request_id = meta.request_id.clone();

    // Anthropic message_start usage is normally available during pre-commit
    // validation, so expose the best cache status known without delaying the stream.
    meta.cache_status = cache_status_from_usage(&attempt.precommit_usage);

    // Response headers injected by Kinetix (FR-12.15). Serving topology is
    // hidden by default; the opaque route id is resolvable by an admin only.
    let mut builder = Response::builder()
        .header("x-request-id", &request_id)
        .header("x-kinetix-route-id", &trace.opaque_route_id)
        .header("x-kinetix-cache", meta.cache_status);
    if meta.fallback_hops > 0 {
        // r4 specifies the literal value '1' (present only when a fallback
        // occurred). The hop count and hop trace are internal routing detail and
        // are not exposed on the ordinary response; the full trace is
        // admin-resolvable through the opaque route id.
        builder = builder.header("x-kinetix-fallback", "1");
    }
    if !trace.warnings.is_empty() {
        if let Ok(w) = serde_json::to_string(&trace.warnings) {
            builder = builder.header("x-kinetix-warning", w);
        }
    }

    let notify = Arc::new(tokio::sync::Notify::new());

    if stream {
        builder = builder
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .header("x-accel-buffering", "no");

        // Watchdog (FR-2.9, NFR-1.10): the response body is dropped the instant
        // the client goes away, so a drop-guard flips `meta.disconnected` and
        // wakes the driver immediately. Without this a cancellation could wait up
        // to a keepalive interval, because a broken client socket is otherwise
        // only detected on the next write.
        let guard = DisconnectGuard {
            flag: meta.disconnected.clone(),
            notify: notify.clone(),
            at: meta.disconnect_at.clone(),
            _concurrency: meta.concurrency.take(),
        };
        let mut body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let watched = async_stream::stream! {
            let _guard = guard;
            while let Some(v) = body_stream.next().await {
                yield v;
            }
        };

        let passthrough = attempt.passthrough;
        tokio::spawn(async move {
            if passthrough {
                drive_stream_passthrough(
                    state,
                    snap,
                    format,
                    meta,
                    req,
                    attempt,
                    encoder_ctx,
                    started,
                    key,
                    tx,
                    model_display,
                    trace,
                    notify,
                )
                .await;
            } else {
                drive_stream(
                    state,
                    snap,
                    format,
                    meta,
                    req,
                    attempt,
                    encoder_ctx,
                    started,
                    key,
                    tx,
                    model_display,
                    trace,
                    notify,
                )
                .await;
            }
        });

        let body = Body::from_stream(watched);
        builder.body(body).unwrap_or_else(|_| {
            Response::builder()
                .status(500)
                .body(Body::from("internal error"))
                .unwrap()
        })
    } else {
        // Non-streaming stays uncommitted until aggregation completes, allowing
        // a real HTTP error status when the validated upstream later truncates.
        let model_name = req.requested_model.clone();
        let mut req = req;
        req.stream = true;
        let result = drive_aggregate(
            state,
            snap,
            format,
            meta,
            req,
            attempt,
            encoder_ctx,
            started,
            key,
            model_name,
            trace,
        )
        .await;
        builder
            .status(result.status_code)
            .header("content-type", "application/json")
            .body(Body::from(result.body.to_string()))
            .unwrap_or_else(|_| Response::new(Body::empty()))
    }
}

#[derive(Default)]
struct ToolStreamState {
    next_index: u32,
    upstream_indexes: std::collections::HashMap<u32, u32>,
    ids: std::collections::HashMap<String, u32>,
    request_id: String,
    /// An opaque provider signature observed on a signature-only part (e.g.
    /// Gemini's `{ text: "", thoughtSignature: ... }`) that arrived *before*
    /// the `ToolCallStart` it belongs to. Carried forward only until the next
    /// relevant event so it can never attach to an unrelated later tool call.
    pending_signature: Option<String>,
}

impl ToolStreamState {
    fn new(request_id: &str) -> Self {
        Self {
            request_id: request_id.replace(['-', '_'], ""),
            ..Default::default()
        }
    }

    fn normalize(&mut self, events: Vec<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::with_capacity(events.len());
        for event in events {
            let normalized = match event {
                StreamEvent::ThinkingDelta {
                    block_index,
                    text,
                    signature,
                } if text.is_empty() && signature.is_some() => {
                    // Signature-only part: remember it for the tool call that
                    // should follow, but still emit the raw event unchanged so
                    // the client encoder behaves exactly as before.
                    self.pending_signature = signature.clone();
                    StreamEvent::ThinkingDelta {
                        block_index,
                        text,
                        signature,
                    }
                }
                StreamEvent::ThinkingDelta {
                    block_index,
                    text,
                    signature,
                } => {
                    // Real thinking content is not a signature-only marker;
                    // drop any stale pending signature so it cannot leak onto
                    // an unrelated later tool call.
                    self.pending_signature = None;
                    StreamEvent::ThinkingDelta {
                        block_index,
                        text,
                        signature,
                    }
                }
                StreamEvent::ThinkingBlockStart {
                    index,
                    thinking,
                    signature,
                } => {
                    self.pending_signature = None;
                    StreamEvent::ThinkingBlockStart {
                        index,
                        thinking,
                        signature,
                    }
                }
                StreamEvent::ThinkingBlockStop { index } => {
                    self.pending_signature = None;
                    StreamEvent::ThinkingBlockStop { index }
                }
                StreamEvent::RedactedThinking { index, data } => {
                    self.pending_signature = None;
                    StreamEvent::RedactedThinking { index, data }
                }
                StreamEvent::TextDelta(text) => {
                    self.pending_signature = None;
                    StreamEvent::TextDelta(text)
                }
                StreamEvent::RefusalDelta(text) => {
                    self.pending_signature = None;
                    StreamEvent::RefusalDelta(text)
                }
                StreamEvent::ToolCallStart {
                    index,
                    id,
                    name,
                    signature,
                } => {
                    // An explicit signature on the tool call itself always
                    // wins; never overwrite it with a pending one. Either way
                    // pending state is consumed here so it cannot attach to a
                    // second, later tool call: parallel calls must keep their
                    // original signature placement exactly (never copy FC0's
                    // signature onto FC1/FC2).
                    let resolved_signature = match signature {
                        Some(sig) => Some(sig),
                        None => self.pending_signature.take(),
                    };
                    self.pending_signature = None;

                    let canonical = id
                        .as_ref()
                        .and_then(|id| self.ids.get(id).copied())
                        .unwrap_or_else(|| {
                            let next = self.next_index;
                            self.next_index += 1;
                            next
                        });
                    self.upstream_indexes.insert(index, canonical);

                    let id = match id.filter(|id| !id.is_empty()) {
                        Some(id) => {
                            self.ids.entry(id.clone()).or_insert(canonical);
                            Some(id)
                        }
                        None => {
                            let generated = format!("call_{}_{}", self.request_id, canonical);
                            self.ids.insert(generated.clone(), canonical);
                            Some(generated)
                        }
                    };

                    StreamEvent::ToolCallStart {
                        index: canonical,
                        id,
                        name,
                        signature: resolved_signature,
                    }
                }
                StreamEvent::ToolCallArgsDelta { index, args } => {
                    let canonical = self.upstream_indexes.get(&index).copied().unwrap_or(index);
                    StreamEvent::ToolCallArgsDelta {
                        index: canonical,
                        args,
                    }
                }
                StreamEvent::Finish(reason) => {
                    self.pending_signature = None;
                    StreamEvent::Finish(reason)
                }
                other => other,
            };
            out.push(normalized);
        }
        out
    }
}

/// Capture context for opaque provider continuation state (e.g. Gemini
/// `thoughtSignature`). Created once per successfully-connected target so the
/// streaming and non-streaming drivers share one capture path.
struct OpaqueCaptureContext {
    scope: OpaqueClientScope,
    session: Option<String>,
    target: OpaqueStateTarget,
}

impl OpaqueCaptureContext {
    /// Build a capture context only when the target adapter declares an
    /// opaque-state capability. For every other adapter (all non-Gemini
    /// built-ins, and plugins without an explicit contract) this returns
    /// `None`, so no opaque-state work happens on their request path.
    fn from_attempt(
        attempt: &Attempt,
        key: Option<&db::VirtualKeyRow>,
        session: Option<&str>,
    ) -> Option<Self> {
        let target = attempt.adapter.opaque_state_target(&attempt.target.model)?;
        let scope = match key {
            Some(key) => OpaqueClientScope::for_key(&key.id),
            None => OpaqueClientScope::internal(),
        };
        Some(Self {
            scope,
            session: session.map(str::to_string),
            target,
        })
    }
}

/// Record every opaque signature observed on normalized `ToolCallStart`
/// events, keyed by the **client-visible** tool-call id (after
/// `ToolStreamState::normalize`) so a later translated request can recover it
/// by the id the client actually saw. Shared by both drivers. This is
/// synchronous by design: it only writes the RAM cache and enqueues an
/// asynchronous durability job, so a slow/locked database never stalls the
/// streaming response (a storage failure is recorded by the store and never
/// interrupts the response).
fn capture_opaque_state(
    events: &[StreamEvent],
    ctx: &OpaqueCaptureContext,
    store: &OpaqueStateStore,
) {
    for event in events {
        if let StreamEvent::ToolCallStart {
            id: Some(id),
            name,
            signature: Some(signature),
            ..
        } = event
        {
            store.capture_tool_signature(
                &ctx.scope,
                &ctx.target,
                ctx.session.as_deref(),
                id,
                name,
                signature,
            );
        }
    }
}

/// Emit normalized events to a streaming client. Returns false when the client
/// disconnected while writing.
#[allow(clippy::too_many_arguments)]
async fn emit_translated_events(
    events: Vec<StreamEvent>,
    encoder: &mut Encoder,
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    state: &AppState,
    meta: &RequestMeta,
    started: Instant,
    usage: &mut TokenUsage,
    measure_ttft: bool,
    ttft_ms: &mut Option<i64>,
    committed: &mut bool,
    trace: &mut RouteTrace,
    saw_reasoning: &mut bool,
    saw_tool: &mut bool,
) -> bool {
    let mut streaming_marked = false;
    for ev in events {
        if let StreamEvent::Usage(u) = &ev {
            usage.merge(u);
        }
        match &ev {
            StreamEvent::ThinkingDelta { .. } if !*saw_reasoning => {
                *saw_reasoning = true;
                state.flight.record(
                    &meta.request_id,
                    started.elapsed().as_millis() as u64,
                    "reasoning_event",
                    "first thinking delta",
                );
            }
            StreamEvent::ToolCallStart { .. } if !*saw_tool => {
                *saw_tool = true;
                state.flight.record(
                    &meta.request_id,
                    started.elapsed().as_millis() as u64,
                    "tool_call_event",
                    "first tool call",
                );
            }
            _ => {}
        }
        let frames = encoder.encode(ev);
        if !frames.is_empty() {
            if measure_ttft && ttft_ms.is_none() {
                *ttft_ms = Some(started.elapsed().as_millis() as i64);
                state.live.set_ttft(&meta.request_id, ttft_ms.unwrap());
                state.live.mark_streaming(&meta.request_id);
                state.flight.record(
                    &meta.request_id,
                    started.elapsed().as_millis() as u64,
                    "upstream_first_frame",
                    "first upstream model event",
                );
            } else if !measure_ttft && !streaming_marked {
                state.live.mark_streaming(&meta.request_id);
                streaming_marked = true;
            }
        }
        for frame in frames {
            if !*committed {
                *committed = true;
                trace.commit();
                state.live.mark_committed(&meta.request_id);
                state.flight.record(
                    &meta.request_id,
                    started.elapsed().as_millis() as u64,
                    "commit",
                    "first client bytes",
                );
            }
            if tx.send(Ok(frame)).await.is_err() {
                return false;
            }
        }
    }
    true
}

/// The async streaming driver: reads validated upstream SSE (or a complete JSON
/// response), encodes to the client format, and treats incomplete EOF as a
/// post-commit upstream failure.
#[allow(clippy::too_many_arguments)]
async fn drive_stream(
    state: AppState,
    snap: Arc<crate::registry::Snapshot>,
    format: FrontendFormat,
    mut meta: RequestMeta,
    req: InternalRequest,
    mut attempt: Attempt,
    encoder_ctx: EncoderCtx,
    started: Instant,
    key: Option<db::VirtualKeyRow>,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    model_display: String,
    mut trace: RouteTrace,
    notify: Arc<tokio::sync::Notify>,
) {
    let mut encoder = Encoder::new(format, encoder_ctx);
    encoder.set_include_usage(req.include_usage);
    let mut tool_stream = ToolStreamState::new(&meta.request_id);
    let mut usage = TokenUsage::default();
    let mut ttft_ms: Option<i64> = None;
    let mut stream_outcome = StreamOutcome::Completed;
    let mut error_message: Option<String> = None;
    let mut provider_failure: Option<(FailureKind, Option<u16>)> = None;
    let mut committed = false;
    let mut saw_reasoning = false;
    let mut saw_tool = false;
    let adapter = attempt.adapter.clone();
    let opaque_ctx =
        OpaqueCaptureContext::from_attempt(&attempt, key.as_ref(), meta.session.as_deref());

    // A normal JSON response is already complete and validated before commit.
    if let Some(events) = attempt.full_events.take() {
        let events = tool_stream.normalize(events);
        if let Some(ctx) = &opaque_ctx {
            capture_opaque_state(&events, ctx, &state.opaque_state);
        }
        if !emit_translated_events(
            events,
            &mut encoder,
            &tx,
            &state,
            &meta,
            started,
            &mut usage,
            attempt.is_sse,
            &mut ttft_ms,
            &mut committed,
            &mut trace,
            &mut saw_reasoning,
            &mut saw_tool,
        )
        .await
        {
            stream_outcome = output_abort_outcome(&state, &meta, started);
        } else {
            for frame in encoder.finalize() {
                if tx.send(Ok(frame)).await.is_err() {
                    stream_outcome = output_abort_outcome(&state, &meta, started);
                    break;
                }
            }
        }

        finalize_log(
            &state,
            &snap,
            &mut meta,
            &req,
            &attempt,
            &model_display,
            started,
            ttft_ms,
            stream_termination(stream_outcome, committed, provider_failure),
            usage,
            error_message,
            key,
            trace,
        )
        .await;
        return;
    }

    let upstream = attempt.stream.take().expect("validated SSE stream present");
    let prefetched = std::mem::take(&mut attempt.prefetched);
    let replay = futures::stream::iter(prefetched.into_iter().map(Ok::<Bytes, reqwest::Error>));
    let chunks = replay.chain(upstream.bytes_stream());
    tokio::pin!(chunks);

    let mut framer = crate::sse::SseFramer::new();
    let mut terminal_seen = false;
    let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut idle_deadline = tokio::time::Instant::now() + attempt.idle_timeout;

    'outer: loop {
        if meta.disconnected.load(Ordering::Relaxed) {
            record_cancel(&state, &meta, started);
            stream_outcome = StreamOutcome::ClientCancelled;
            break;
        }
        tokio::select! {
            _ = notify.notified() => {
                if meta.disconnected.load(Ordering::Relaxed) {
                    record_cancel(&state, &meta, started);
                    stream_outcome = StreamOutcome::ClientCancelled;
                    break;
                }
            }
            _ = keepalive.tick() => {
                if tx.send(Ok(frontends::sse_comment("keepalive"))).await.is_err() {
                    stream_outcome = output_abort_outcome(&state, &meta, started);
                    break;
                }
            }
            _ = tokio::time::sleep_until(idle_deadline) => {
                stream_outcome = StreamOutcome::Timeout;
                provider_failure = Some((FailureKind::Timeout, None));
                error_message = Some("upstream stream idle timeout".into());
                break 'outer;
            }
            chunk = chunks.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        idle_deadline = tokio::time::Instant::now() + attempt.idle_timeout;
                        let frames = match framer.push(&bytes) {
                            Ok(frames) => frames,
                            Err(error) => {
                                stream_outcome = StreamOutcome::ProtocolViolation;
                                provider_failure = Some((FailureKind::MalformedUpstream, Some(502)));
                                error_message = Some(error.to_string());
                                break 'outer;
                            }
                        };
                        for frame in frames {
                            let Some(payload) = crate::sse::extract_data(&frame) else {
                                continue;
                            };
                            if payload.trim() == "[DONE]" {
                                terminal_seen = true;
                                continue;
                            }
                            if let Some(failure) = payload_error_failure(&adapter, &payload) {
                                stream_outcome = stream_outcome_for_failure(failure.kind);
                                provider_failure = Some((failure.kind, failure.status));
                                error_message = Some(failure.message.clone());
                                for out in encoder.error_frame(&failure.message) {
                                    let _ = tx.send(Ok(out)).await;
                                }
                                break 'outer;
                            }
                            let events = match adapter.parse_stream_chunk(&payload) {
                                Ok(events) => events,
                                Err(failure) => {
                                    stream_outcome = StreamOutcome::ProtocolViolation;
                                    provider_failure = Some((failure.kind, failure.status));
                                    error_message = Some(failure.message.clone());
                                    for out in encoder.error_frame(&failure.message) {
                                        let _ = tx.send(Ok(out)).await;
                                    }
                                    break 'outer;
                                }
                            };
                            if payload_is_terminal(&payload, &events) {
                                terminal_seen = true;
                            }
                            let events = tool_stream.normalize(events);
                            if let Some(ctx) = &opaque_ctx {
                                capture_opaque_state(&events, ctx, &state.opaque_state);
                            }
                            if !emit_translated_events(
                                events,
                                &mut encoder,
                                &tx,
                                &state,
                                &meta,
                                started,
                                &mut usage,
                                attempt.is_sse,
                                &mut ttft_ms,
                                &mut committed,
                                &mut trace,
                                &mut saw_reasoning,
                                &mut saw_tool,
                            )
                            .await
                            {
                                stream_outcome = output_abort_outcome(&state, &meta, started);
                                break 'outer;
                            }
                        }
                    }
                    Some(Err(error)) => {
                        let kind = if error.is_timeout() {
                            FailureKind::Timeout
                        } else {
                            FailureKind::ConnectionError
                        };
                        stream_outcome = stream_outcome_for_failure(kind);
                        provider_failure = Some((kind, None));
                        error_message = Some(classify_reqwest(&error));
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    if stream_outcome == StreamOutcome::Completed && framer.pending_bytes() > 0 {
        stream_outcome = StreamOutcome::ProtocolViolation;
        provider_failure = Some((FailureKind::MalformedUpstream, Some(502)));
        error_message = Some("upstream SSE ended with an incomplete frame".into());
    } else if stream_outcome == StreamOutcome::Completed && !terminal_seen {
        stream_outcome = StreamOutcome::UpstreamCleanEof;
        provider_failure = Some((FailureKind::MalformedUpstream, Some(502)));
        error_message = Some("upstream SSE ended before a terminal event".into());
    }

    if stream_outcome.is_upstream_failure() {
        if committed {
            state.failures_post_commit.fetch_add(1, Ordering::Relaxed);
        }
        let message = error_message
            .as_deref()
            .unwrap_or("upstream stream interrupted");
        for frame in encoder.error_frame(message) {
            let _ = tx.send(Ok(frame)).await;
        }
    } else if stream_outcome == StreamOutcome::Completed {
        for frame in encoder.finalize() {
            if tx.send(Ok(frame)).await.is_err() {
                stream_outcome = output_abort_outcome(&state, &meta, started);
                break;
            }
        }
    }

    finalize_log(
        &state,
        &snap,
        &mut meta,
        &req,
        &attempt,
        &model_display,
        started,
        ttft_ms,
        stream_termination(stream_outcome, committed, provider_failure),
        usage,
        error_message,
        key,
        trace,
    )
    .await;
}

/// Same-format passthrough streaming (FR-2.7, FR-2.10). Event payloads are
/// forwarded without JSON re-encoding, but the SSE framer normalizes line
/// endings and this driver reconstructs frame delimiters. No client bytes are
/// emitted until a semantic upstream event has been validated.
#[allow(clippy::too_many_arguments)]
async fn drive_stream_passthrough(
    state: AppState,
    snap: Arc<crate::registry::Snapshot>,
    format: FrontendFormat,
    mut meta: RequestMeta,
    req: InternalRequest,
    mut attempt: Attempt,
    encoder_ctx: EncoderCtx,
    started: Instant,
    key: Option<db::VirtualKeyRow>,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    model_display: String,
    mut trace: RouteTrace,
    notify: Arc<tokio::sync::Notify>,
) {
    let mut usage = TokenUsage::default();
    let mut ttft_ms: Option<i64> = None;
    let mut stream_outcome = StreamOutcome::Completed;
    let mut error_message: Option<String> = None;
    let mut provider_failure: Option<(FailureKind, Option<u16>)> = None;
    let mut committed = false;
    let mut saw_reasoning = false;
    let mut saw_tool = false;
    let adapter = attempt.adapter.clone();

    let upstream = attempt.stream.take().expect("validated SSE stream present");
    let prefetched = std::mem::take(&mut attempt.prefetched);
    let replay = futures::stream::iter(prefetched.into_iter().map(Ok::<Bytes, reqwest::Error>));
    let chunks = replay.chain(upstream.bytes_stream());
    tokio::pin!(chunks);

    let mut framer = crate::sse::SseFramer::new();
    let mut terminal_seen = false;
    let mut pending_frames: Vec<String> = Vec::new();
    let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut idle_deadline = tokio::time::Instant::now() + attempt.idle_timeout;

    'outer: loop {
        if meta.disconnected.load(Ordering::Relaxed) {
            record_cancel(&state, &meta, started);
            stream_outcome = StreamOutcome::ClientCancelled;
            break;
        }
        tokio::select! {
            _ = notify.notified() => {
                if meta.disconnected.load(Ordering::Relaxed) {
                    record_cancel(&state, &meta, started);
                    stream_outcome = StreamOutcome::ClientCancelled;
                    break;
                }
            }
            _ = keepalive.tick(), if committed => {
                if tx.send(Ok(frontends::sse_comment("keepalive"))).await.is_err() {
                    stream_outcome = output_abort_outcome(&state, &meta, started);
                    break;
                }
            }
            _ = tokio::time::sleep_until(idle_deadline) => {
                stream_outcome = StreamOutcome::Timeout;
                provider_failure = Some((FailureKind::Timeout, None));
                error_message = Some("upstream stream idle timeout".into());
                break 'outer;
            }
            chunk = chunks.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        idle_deadline = tokio::time::Instant::now() + attempt.idle_timeout;
                        let frames = match framer.push(&bytes) {
                            Ok(frames) => frames,
                            Err(error) => {
                                stream_outcome = StreamOutcome::ProtocolViolation;
                                provider_failure = Some((FailureKind::MalformedUpstream, Some(502)));
                                error_message = Some(error.to_string());
                                break 'outer;
                            }
                        };
                        for frame in frames {
                            let payload = crate::sse::extract_data(&frame);
                            let mut semantic = false;
                            if let Some(payload) = payload.as_deref() {
                                if payload.trim() == "[DONE]" {
                                    terminal_seen = true;
                                } else {
                                    if let Some(failure) = payload_error_failure(&adapter, payload) {
                                        stream_outcome = stream_outcome_for_failure(failure.kind);
                                        provider_failure = Some((failure.kind, failure.status));
                                        error_message = Some(failure.message);
                                        break 'outer;
                                    }
                                    match adapter.parse_stream_chunk(payload) {
                                        Ok(events) => {
                                            semantic = events.iter().any(is_semantic_event);
                                            if payload_is_terminal(payload, &events) {
                                                terminal_seen = true;
                                            }
                                            let usage_only = !events.is_empty()
                                                && events
                                                    .iter()
                                                    .all(|event| matches!(event, StreamEvent::Usage(_)));
                                            for event in &events {
                                                if let StreamEvent::Usage(value) = event {
                                                    usage.merge(value);
                                                }
                                                match event {
                                                    StreamEvent::ThinkingDelta { .. } if !saw_reasoning => {
                                                        saw_reasoning = true;
                                                        state.flight.record(
                                                            &meta.request_id,
                                                            started.elapsed().as_millis() as u64,
                                                            "reasoning_event",
                                                            "first thinking delta",
                                                        );
                                                    }
                                                    StreamEvent::ToolCallStart { .. } if !saw_tool => {
                                                        saw_tool = true;
                                                        state.flight.record(
                                                            &meta.request_id,
                                                            started.elapsed().as_millis() as u64,
                                                            "tool_call_event",
                                                            "first tool call",
                                                        );
                                                    }
                                                    _ => {}
                                                }
                                            }
                                            if format == FrontendFormat::OpenAi
                                                && !req.include_usage
                                                && usage_only
                                            {
                                                continue;
                                            }
                                        }
                                        Err(failure) => {
                                            stream_outcome = StreamOutcome::ProtocolViolation;
                                            provider_failure = Some((failure.kind, failure.status));
                                            error_message = Some(failure.message);
                                            break 'outer;
                                        }
                                    }
                                }
                            }

                            if !committed {
                                pending_frames.push(frame);
                                if !semantic {
                                    continue;
                                }
                                committed = true;
                                trace.commit();
                                state.live.mark_committed(&meta.request_id);
                                ttft_ms = Some(started.elapsed().as_millis() as i64);
                                state.live.set_ttft(&meta.request_id, ttft_ms.unwrap());
                                state.live.mark_streaming(&meta.request_id);
                                state.flight.record(
                                    &meta.request_id,
                                    started.elapsed().as_millis() as u64,
                                    "commit",
                                    "first validated passthrough event",
                                );
                                for buffered in pending_frames.drain(..) {
                                    if tx
                                        .send(Ok(Bytes::from(format!("{buffered}\n\n"))))
                                        .await
                                        .is_err()
                                    {
                                        stream_outcome = output_abort_outcome(&state, &meta, started);
                                        break 'outer;
                                    }
                                }
                            } else if tx
                                .send(Ok(Bytes::from(format!("{frame}\n\n"))))
                                .await
                                .is_err()
                            {
                                stream_outcome = output_abort_outcome(&state, &meta, started);
                                break 'outer;
                            }
                        }
                    }
                    Some(Err(error)) => {
                        let kind = if error.is_timeout() {
                            FailureKind::Timeout
                        } else {
                            FailureKind::ConnectionError
                        };
                        stream_outcome = stream_outcome_for_failure(kind);
                        provider_failure = Some((kind, None));
                        error_message = Some(classify_reqwest(&error));
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    if stream_outcome == StreamOutcome::Completed && framer.pending_bytes() > 0 {
        stream_outcome = StreamOutcome::ProtocolViolation;
        provider_failure = Some((FailureKind::MalformedUpstream, Some(502)));
        error_message = Some("upstream SSE ended with an incomplete frame".into());
    } else if stream_outcome == StreamOutcome::Completed && !terminal_seen {
        stream_outcome = StreamOutcome::UpstreamCleanEof;
        provider_failure = Some((FailureKind::MalformedUpstream, Some(502)));
        error_message = Some("upstream SSE ended before a terminal event".into());
    }

    if stream_outcome.is_upstream_failure() && committed {
        state.failures_post_commit.fetch_add(1, Ordering::Relaxed);
        let message = error_message
            .as_deref()
            .unwrap_or("upstream stream interrupted");
        if format == FrontendFormat::OpenAi {
            // Same-format passthrough has already exposed the upstream stream
            // identity. Do not synthesize a new chat completion id/model for
            // the terminal error.
            let frame = serde_json::json!({
                "error": {
                    "message": message,
                    "type": "upstream_error",
                    "code": "stream_error"
                }
            });
            let _ = tx
                .send(Ok(frontends::sse_frame(None, &frame.to_string())))
                .await;
            let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
        } else {
            let mut encoder = Encoder::new(format, encoder_ctx);
            for frame in encoder.error_frame(message) {
                let _ = tx.send(Ok(frame)).await;
            }
        }
    }

    finalize_log(
        &state,
        &snap,
        &mut meta,
        &req,
        &attempt,
        &model_display,
        started,
        ttft_ms,
        stream_termination(stream_outcome, committed, provider_failure),
        usage,
        error_message,
        key,
        trace,
    )
    .await;
}

/// Flips a disconnect flag (and wakes waiters) when the response body is
/// dropped, i.e. when the client stops reading (FR-2.9, NFR-1.10).
struct DisconnectGuard {
    flag: Arc<std::sync::atomic::AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
    at: Arc<parking_lot::Mutex<Option<Instant>>>,
    _concurrency: Option<crate::admission::ConcurrencyReservation>,
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        *self.at.lock() = Some(Instant::now());
        self.flag.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
}

fn stream_outcome_for_failure(kind: FailureKind) -> StreamOutcome {
    match kind {
        FailureKind::ClientCancelled => StreamOutcome::ClientCancelled,
        FailureKind::Timeout => StreamOutcome::Timeout,
        FailureKind::MalformedUpstream => StreamOutcome::ProtocolViolation,
        _ => StreamOutcome::UpstreamError,
    }
}

fn stream_termination(
    outcome: StreamOutcome,
    committed: bool,
    failure: Option<(FailureKind, Option<u16>)>,
) -> StreamTermination {
    StreamTermination::new(
        outcome,
        if committed {
            CommitState::PostCommit
        } else {
            CommitState::PreCommit
        },
        failure.map(|(kind, _)| kind),
        failure.and_then(|(_, status)| status),
    )
}

fn output_abort_outcome(state: &AppState, meta: &RequestMeta, started: Instant) -> StreamOutcome {
    if meta.disconnected.load(Ordering::Relaxed) {
        record_cancel(state, meta, started);
        StreamOutcome::ClientCancelled
    } else {
        StreamOutcome::GatewayAbort
    }
}

fn record_cancel(state: &AppState, meta: &RequestMeta, started: Instant) {
    state.cancellations.fetch_add(1, Ordering::Relaxed);
    // Cancellation latency is the delay between the client actually going away
    // and Kinetix acting on it (NFR-1.10). Fall back to time-since-start only
    // when the disconnect instant was never observed.
    let signal_ms = match *meta.disconnect_at.lock() {
        Some(at) => at.elapsed().as_millis() as u64,
        None => 0,
    };
    state
        .cancellation_latency_ms_total
        .fetch_add(signal_ms, Ordering::Relaxed);
    state.flight.record(
        &meta.request_id,
        started.elapsed().as_millis() as u64,
        "cancellation_issued",
        format!("client disconnected; signal latency {signal_ms}ms"),
    );
}

struct AggregateResult {
    body: Value,
    status_code: u16,
}

#[allow(clippy::too_many_arguments)]
async fn drive_aggregate(
    state: AppState,
    snap: Arc<crate::registry::Snapshot>,
    format: FrontendFormat,
    mut meta: RequestMeta,
    req: InternalRequest,
    mut attempt: Attempt,
    encoder_ctx: EncoderCtx,
    started: Instant,
    key: Option<db::VirtualKeyRow>,
    model_name: String,
    mut trace: RouteTrace,
) -> AggregateResult {
    let adapter = attempt.adapter.clone();
    let opaque_ctx =
        OpaqueCaptureContext::from_attempt(&attempt, key.as_ref(), meta.session.as_deref());
    let mut events: Vec<StreamEvent> = Vec::new();
    let mut tool_stream = ToolStreamState::new(&encoder_ctx.request_id);
    let mut usage = TokenUsage::default();
    let mut status = "success";
    let mut status_code = 200i64;
    let mut stream_outcome = StreamOutcome::Completed;
    let mut error_message: Option<String> = None;
    let mut provider_failure: Option<(FailureKind, Option<u16>)> = None;
    let mut committed = false;

    if let Some(full_events) = attempt.full_events.take() {
        let normalized = tool_stream.normalize(full_events);
        if let Some(ctx) = &opaque_ctx {
            capture_opaque_state(&normalized, ctx, &state.opaque_state);
        }
        for event in normalized {
            if let StreamEvent::Usage(value) = &event {
                usage.merge(value);
            }
            events.push(event);
        }
        committed = true;
        trace.commit();
    } else {
        let upstream = attempt.stream.take().expect("validated SSE stream present");
        let prefetched = std::mem::take(&mut attempt.prefetched);
        let replay = futures::stream::iter(prefetched.into_iter().map(Ok::<Bytes, reqwest::Error>));
        let chunks = replay.chain(upstream.bytes_stream());
        tokio::pin!(chunks);

        let mut framer = crate::sse::SseFramer::new();
        let mut terminal_seen = false;

        'outer: loop {
            let next = match tokio::time::timeout(attempt.idle_timeout, chunks.next()).await {
                Ok(next) => next,
                Err(_) => {
                    stream_outcome = StreamOutcome::Timeout;
                    status = "stream_error";
                    status_code = 504;
                    provider_failure = Some((FailureKind::Timeout, None));
                    error_message = Some("upstream stream idle timeout".into());
                    break;
                }
            };
            let Some(chunk) = next else {
                break;
            };
            match chunk {
                Ok(bytes) => {
                    let frames = match framer.push(&bytes) {
                        Ok(frames) => frames,
                        Err(error) => {
                            stream_outcome = StreamOutcome::ProtocolViolation;
                            status = "stream_error";
                            status_code = 502;
                            provider_failure = Some((FailureKind::MalformedUpstream, Some(502)));
                            error_message = Some(error.to_string());
                            break;
                        }
                    };
                    for frame in frames {
                        let Some(payload) = crate::sse::extract_data(&frame) else {
                            continue;
                        };
                        if payload.trim() == "[DONE]" {
                            terminal_seen = true;
                            continue;
                        }
                        if let Some(failure) = payload_error_failure(&adapter, &payload) {
                            stream_outcome = stream_outcome_for_failure(failure.kind);
                            status = "stream_error";
                            status_code = 502;
                            provider_failure = Some((failure.kind, failure.status));
                            error_message = Some(failure.message);
                            break 'outer;
                        }
                        match adapter.parse_stream_chunk(&payload) {
                            Ok(parsed) => {
                                if payload_is_terminal(&payload, &parsed) {
                                    terminal_seen = true;
                                }
                                let normalized = tool_stream.normalize(parsed);
                                if let Some(ctx) = &opaque_ctx {
                                    capture_opaque_state(&normalized, ctx, &state.opaque_state);
                                }
                                for event in normalized {
                                    if let StreamEvent::Usage(value) = &event {
                                        usage.merge(value);
                                    }
                                    events.push(event);
                                }
                            }
                            Err(failure) => {
                                stream_outcome = StreamOutcome::ProtocolViolation;
                                status = "stream_error";
                                status_code = 502;
                                provider_failure = Some((failure.kind, failure.status));
                                error_message = Some(failure.message);
                                break 'outer;
                            }
                        }
                    }
                }
                Err(error) => {
                    let kind = if error.is_timeout() {
                        FailureKind::Timeout
                    } else {
                        FailureKind::ConnectionError
                    };
                    stream_outcome = stream_outcome_for_failure(kind);
                    status = "stream_error";
                    status_code = if kind == FailureKind::Timeout {
                        504
                    } else {
                        502
                    };
                    provider_failure = Some((kind, None));
                    error_message = Some(classify_reqwest(&error));
                    break;
                }
            }
        }

        if status == "success" && framer.pending_bytes() > 0 {
            stream_outcome = StreamOutcome::ProtocolViolation;
            status = "stream_error";
            status_code = 502;
            provider_failure = Some((FailureKind::MalformedUpstream, Some(502)));
            error_message = Some("upstream SSE ended with an incomplete frame".into());
        } else if status == "success" && !terminal_seen {
            stream_outcome = StreamOutcome::UpstreamCleanEof;
            status = "stream_error";
            status_code = 502;
            provider_failure = Some((FailureKind::MalformedUpstream, Some(502)));
            error_message = Some("upstream SSE ended before a terminal event".into());
        }

        // Aggregated responses do not commit client bytes until the entire
        // upstream lifecycle is known to be complete.
        if status == "success" {
            committed = true;
            trace.commit();
        }
    }

    let termination = stream_termination(stream_outcome, committed, provider_failure);
    finalize_log(
        &state,
        &snap,
        &mut meta,
        &req,
        &attempt,
        &model_name,
        started,
        None,
        termination,
        usage.clone(),
        error_message.clone(),
        key.clone(),
        trace,
    )
    .await;

    if status != "success" {
        return AggregateResult {
            body: serde_json::json!({
                "error": {
                    "message": error_message.unwrap_or_else(|| "upstream stream interrupted".into()),
                    "type": "upstream_error"
                }
            }),
            status_code: status_code.clamp(400, 599) as u16,
        };
    }

    let body = frontends::aggregate_with_responses_fields(
        format,
        &model_name,
        &encoder_ctx.request_id,
        events,
        &usage,
        &encoder_ctx.responses,
    );
    if let Some(k) = &key {
        if k.body_logging != 0 {
            let _ = db::insert_body_log(
                &state.pool,
                &meta.request_id,
                &k.id,
                "response",
                &crate::crypto::redact(&body.to_string()),
                7,
            )
            .await;
        }
    }
    AggregateResult {
        body,
        status_code: 200,
    }
}

fn persisted_ttft_ms(is_sse: bool, ttft_ms: Option<i64>) -> Option<i64> {
    is_sse.then_some(ttft_ms).flatten()
}

fn persisted_request_timing(
    duration_ms: i64,
    is_sse: bool,
    ttft_ms: Option<i64>,
) -> (i64, Option<i64>) {
    (duration_ms, persisted_ttft_ms(is_sse, ttft_ms))
}

fn usage_has_reported_tokens(usage: &TokenUsage) -> bool {
    usage.input.is_some()
        || usage.output.is_some()
        || usage.cached.is_some()
        || usage.cache_write.is_some()
        || usage.thinking.is_some()
}

fn aggregate_request_accounting(
    meta: &RequestMeta,
    current_usage: Option<&TokenUsage>,
    current_cost: Option<f64>,
) -> (TokenUsage, Option<f64>) {
    let mut usages = meta
        .partial_attempts
        .iter()
        .map(|attempt| &attempt.usage)
        .collect::<Vec<_>>();
    if let Some(usage) = current_usage {
        usages.push(usage);
    }
    let sum_field = |field: fn(&TokenUsage) -> Option<u64>| {
        if usages.is_empty() {
            return None;
        }
        usages.iter().try_fold(0_u64, |total, usage| {
            field(usage).map(|value| total.saturating_add(value))
        })
    };
    let usage = TokenUsage {
        input: sum_field(|usage| usage.input),
        output: sum_field(|usage| usage.output),
        cached: sum_field(|usage| usage.cached),
        cache_write: sum_field(|usage| usage.cache_write),
        thinking: sum_field(|usage| usage.thinking),
    };

    let cost = if usages.is_empty() {
        None
    } else {
        meta.partial_attempts
            .iter()
            .try_fold(0.0, |total, attempt| attempt.cost.map(|cost| total + cost))
            .and_then(|total| {
                if current_usage.is_some() {
                    current_cost.map(|cost| total + cost)
                } else {
                    Some(total)
                }
            })
    };
    (usage, cost)
}

fn aggregate_admission_accounting(
    meta: &RequestMeta,
    current_usage: Option<&TokenUsage>,
    current_cost: Option<f64>,
) -> (TokenUsage, Option<f64>) {
    let (usage, cost) = aggregate_request_accounting(meta, current_usage, current_cost);
    (
        TokenUsage {
            input: usage.input,
            output: usage.output,
            ..TokenUsage::default()
        },
        cost,
    )
}

#[allow(clippy::too_many_arguments)]
async fn record_precommit_attempt_usage(
    state: &AppState,
    meta: &mut RequestMeta,
    target: &ResolvedTarget,
    key: Option<&db::VirtualKeyRow>,
    trace: &RouteTrace,
    upstream_request_id: Option<&str>,
    usage: TokenUsage,
    failure: &UpstreamFailure,
    termination: StreamTermination,
) {
    let computed_cost = (usage.input.is_some() && usage.output.is_some())
        .then(|| cost::compute_cost(&target.model.prices(), &usage))
        .flatten();
    let attempt_number = meta.partial_attempts.len() + 1;
    let row = usage_attempt_row(
        meta,
        target,
        key,
        upstream_request_id,
        attempt_number,
        &usage,
        None,
        None,
        termination,
        Some(&failure.message),
        &trace.opaque_route_id,
    );
    let staged_attempt_index = meta.partial_attempts.len();
    meta.partial_attempts.push(PartialAttemptUsage {
        usage,
        cost: None,
        row,
    });
    meta.active_precommit_attempt = None;

    if let Some(computed_cost) = computed_cost {
        let prices = target.model.prices();
        let (source, source_metadata) = target.model.price_provenance();
        match db::ensure_price_version(
            &state.pool,
            &target.model.id,
            &prices,
            &source,
            &source_metadata,
        )
        .await
        {
            Ok(Some(version_id)) => {
                let staged = &mut meta.partial_attempts[staged_attempt_index];
                staged.cost = Some(computed_cost);
                staged.row.cost_usd = Some(computed_cost);
                staged.row.cost_known = 1;
                staged.row.price_version_id = Some(version_id);
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                model = %target.model.id,
                %error,
                "failed to resolve price version for failed precommit attempt"
            ),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn usage_attempt_row(
    meta: &RequestMeta,
    target: &ResolvedTarget,
    key: Option<&db::VirtualKeyRow>,
    upstream_request_id: Option<&str>,
    attempt_number: usize,
    usage: &TokenUsage,
    cost: Option<f64>,
    price_version_id: Option<&str>,
    termination: StreamTermination,
    error_message: Option<&str>,
    opaque_route_id: &str,
) -> db::UsageAttemptRow {
    db::UsageAttemptRow {
        id: format!("usage_attempt_{}", uuid::Uuid::new_v4().simple()),
        request_id: meta.request_id.clone(),
        attempt_number: attempt_number as i64,
        ts: db::now_iso(),
        key_id: key.map(|key| key.id.clone()),
        key_name: key.map(|key| key.name.clone()),
        effective_model: Some(target.model.display_name.clone()),
        route_id: meta.route_id.clone(),
        route_name: meta.route_name.clone(),
        serving_account_id: Some(target.account.id.clone()),
        serving_account: Some(target.account.label.clone()),
        serving_provider: Some(target.provider.name.clone()),
        upstream_request_id: upstream_request_id.map(str::to_owned),
        status: termination.request_status().to_string(),
        status_code: termination.status_code(),
        input_tokens: usage.input.map(|value| value as i64),
        output_tokens: usage.output.map(|value| value as i64),
        cached_tokens: usage.cached.map(|value| value as i64),
        cache_write_tokens: usage.cache_write.map(|value| value as i64),
        thinking_tokens: usage.thinking.map(|value| value as i64),
        cost_usd: cost,
        cost_known: cost.is_some() as i64,
        price_version_id: price_version_id.map(str::to_owned),
        usage_confidence: if usage_has_reported_tokens(usage) {
            "provider_reported".to_string()
        } else {
            "unknown".to_string()
        },
        commit_state: termination.commit_state.as_usage_str().to_string(),
        error_message: error_message.map(str::to_owned),
        opaque_route_id: Some(opaque_route_id.to_string()),
    }
}

fn precommit_request_status(failure: &UpstreamFailure) -> &'static str {
    match failure.kind {
        FailureKind::RateLimit => "rate_limited",
        FailureKind::QuotaExhausted => "quota_exhausted",
        FailureKind::BadRequest | FailureKind::PolicyRejected | FailureKind::TargetError => {
            "client_error"
        }
        FailureKind::Timeout | FailureKind::MalformedUpstream => "stream_error",
        FailureKind::ClientCancelled => "client_disconnect",
        FailureKind::AuthError
        | FailureKind::ServerError
        | FailureKind::ConnectionError
        | FailureKind::PluginFailure => "upstream_error",
    }
}

#[allow(clippy::too_many_arguments)]
fn record_precommit_request_log(
    state: &AppState,
    meta: &RequestMeta,
    req: &InternalRequest,
    target: &ResolvedTarget,
    key: Option<&db::VirtualKeyRow>,
    trace: &RouteTrace,
    upstream_request_id: Option<&str>,
    failure: &UpstreamFailure,
    client_error: &ProxyError,
    termination: StreamTermination,
    started: Instant,
    attempts_done: usize,
) {
    let (usage, cost) = aggregate_request_accounting(meta, None, None);
    let admission_cost_usd = if usage.input.is_some() && usage.output.is_some() && cost.is_some() {
        None
    } else {
        meta.upstream_dispatched
            .load(Ordering::Acquire)
            .then(|| {
                meta.admission
                    .as_ref()
                    .and_then(|admission| admission.conservative_cost_estimate())
            })
            .flatten()
    };
    let usage_confidence = if usage_has_reported_tokens(&usage) {
        "provider_reported"
    } else {
        "unknown"
    };
    let row = UsageLogRow {
        id: format!("usage_{}", uuid::Uuid::new_v4().simple()),
        request_id: meta.request_id.clone(),
        ts: db::now_iso(),
        key_id: key.map(|key| key.id.clone()),
        key_name: key.map(|key| key.name.clone()),
        client_format: meta.client_format.to_string(),
        requested_model: req.requested_model.clone(),
        effective_model: Some(target.model.display_name.clone()),
        route_id: meta.route_id.clone(),
        route_name: meta.route_name.clone(),
        fallback_hops: attempts_done.saturating_sub(1) as i64,
        fallback_path: serde_json::to_string(&meta.fallback_path).unwrap_or_else(|_| "[]".into()),
        status: precommit_request_status(failure).to_string(),
        status_code: client_error.http_status() as i64,
        latency_ms: Some(started.elapsed().as_millis() as i64),
        ttft_ms: None,
        input_tokens: usage.input.map(|value| value as i64),
        output_tokens: usage.output.map(|value| value as i64),
        cached_tokens: usage.cached.map(|value| value as i64),
        cache_write_tokens: usage.cache_write.map(|value| value as i64),
        thinking_tokens: usage.thinking.map(|value| value as i64),
        cost_usd: cost,
        cost_known: cost.is_some() as i64,
        price_version_id: None,
        cache_status: cache_status_from_usage(&usage).to_string(),
        serving_account_id: Some(target.account.id.clone()),
        serving_account: Some(target.account.label.clone()),
        serving_provider: Some(target.provider.name.clone()),
        upstream_request_id: upstream_request_id.map(str::to_owned),
        flagged: 0,
        error_message: Some(failure.message.clone()),
        usage_confidence: usage_confidence.to_string(),
        commit_state: termination.commit_state.as_usage_str().to_string(),
        retry_count: attempts_done.saturating_sub(1) as i64,
        route_trace_id: Some(trace.opaque_route_id.clone()),
        opaque_route_id: Some(trace.opaque_route_id.clone()),
        admission_cost_usd,
    };
    state.log_queue.enqueue_bundle(db::UsageAccountingBundle {
        request: row,
        attempts: meta
            .partial_attempts
            .iter()
            .map(|attempt| attempt.row.clone())
            .collect(),
    });
    meta.accounting_enqueued.store(true, Ordering::Release);
}

/// Compute cost and enqueue the usage row (never blocks the request path),
/// persist the Route Trace, and record the flight-recorder terminal event.
#[allow(clippy::too_many_arguments)]
async fn finalize_log(
    state: &AppState,
    snap: &crate::registry::Snapshot,
    meta: &mut RequestMeta,
    req: &InternalRequest,
    attempt: &Attempt,
    model_display: &str,
    started: Instant,
    ttft_ms: Option<i64>,
    termination: StreamTermination,
    usage: TokenUsage,
    error_message: Option<String>,
    key: Option<db::VirtualKeyRow>,
    mut trace: RouteTrace,
) {
    let status = termination.request_status();
    let status_code = termination.status_code();
    if termination.outcome == StreamOutcome::Completed {
        recover_successful_account(
            state,
            &attempt.target.account.id,
            attempt.target.account.account_state_version,
            attempt.account_probe,
        )
        .await;
    }
    let provider_failure = termination
        .outcome
        .is_upstream_failure()
        .then_some(termination.failure_kind)
        .flatten()
        .map(|kind| (kind, termination.upstream_status));

    if let Some(permit) = attempt.traffic_permit.as_ref() {
        let outcome = match termination.outcome {
            StreamOutcome::Completed => crate::upstream_traffic::TrafficOutcome::Success,
            StreamOutcome::ClientCancelled => crate::upstream_traffic::TrafficOutcome::Cancelled,
            StreamOutcome::GatewayAbort => crate::upstream_traffic::TrafficOutcome::Neutral,
            StreamOutcome::Timeout => crate::upstream_traffic::TrafficOutcome::Timeout,
            _ => crate::upstream_traffic::TrafficOutcome::Error,
        };
        permit.finish(outcome);
    }

    let telemetry_outcome = match termination.outcome {
        StreamOutcome::Completed => crate::target_telemetry::TelemetryOutcome::Success,
        StreamOutcome::ClientCancelled => crate::target_telemetry::TelemetryOutcome::Cancelled,
        StreamOutcome::GatewayAbort => crate::target_telemetry::TelemetryOutcome::Neutral,
        _ => provider_failure
            .map(|(kind, _)| telemetry_outcome_for_failure(kind))
            .unwrap_or(crate::target_telemetry::TelemetryOutcome::Neutral),
    };
    let terminal_circuit_transition = match termination.outcome {
        StreamOutcome::Completed => attempt.provider_attempt.finish_success(),
        StreamOutcome::ClientCancelled | StreamOutcome::GatewayAbort => {
            attempt.provider_attempt.finish_neutral()
        }
        _ => provider_failure
            .map(|(kind, upstream_status)| {
                attempt
                    .provider_attempt
                    .finish_failure(kind, upstream_status)
            })
            .unwrap_or_else(|| attempt.provider_attempt.finish_neutral()),
    };
    let circuit_transition = attempt
        .provider_circuit_transition
        .map(|validated| validated.merge(terminal_circuit_transition))
        .unwrap_or(terminal_circuit_transition);
    let attempt_offset_ms = attempt
        .attempt_started
        .saturating_duration_since(started)
        .as_millis() as i64;
    let target_ttft_ms = persisted_ttft_ms(attempt.is_sse, ttft_ms)
        .and_then(|value| value.checked_sub(attempt_offset_ms))
        .map(|value| value.max(0) as u64);
    if termination.outcome == StreamOutcome::GatewayAbort {
        trace_provider_circuit_transition(
            &mut trace,
            &attempt.target.provider.name,
            circuit_transition,
        );
    } else {
        record_target_telemetry(
            state,
            &attempt.target,
            telemetry_outcome,
            attempt.attempt_started,
            target_ttft_ms,
            meta.fallback_hops > 0,
            false,
            circuit_transition,
            &mut trace,
        );
    }

    if let Some((kind, upstream_status)) = provider_failure {
        let failure = UpstreamFailure {
            kind,
            status: upstream_status,
            retry_after_secs: None,
            message: error_message
                .clone()
                .unwrap_or_else(|| "upstream stream failed".into()),
            quota_reset_at: None,
        };
        handle_key_failure(state, &attempt.target, &failure, meta, &mut trace).await;
    }
    let route = meta
        .route_id
        .as_ref()
        .and_then(|route_id| snap.routes.get(route_id));
    let fallback_allowed =
        match termination.outcome {
            StreamOutcome::Completed => None,
            StreamOutcome::ClientCancelled | StreamOutcome::GatewayAbort => Some(false),
            _ => Some(termination.fallback_allowed(|kind| {
                meta.fallback_enabled && route_allows_fallback(route, kind)
            })),
        };
    trace.stream_termination(termination, fallback_allowed);

    let prices = attempt.target.model.prices();
    let computed_cost = cost::compute_cost(&prices, &usage);
    let (cost, price_version_id) = if let Some(computed_cost) = computed_cost {
        let (source, source_metadata) = attempt.target.model.price_provenance();
        match db::ensure_price_version(
            &state.pool,
            &attempt.target.model.id,
            &prices,
            &source,
            &source_metadata,
        )
        .await
        {
            Ok(Some(version_id)) => (Some(computed_cost), Some(version_id)),
            Ok(None) => (None, None),
            Err(error) => {
                // A known cost without the exact immutable rate snapshot is not
                // auditable. Prefer unknown cost over silently mis-attributing
                // historical usage to a later price.
                tracing::warn!(
                    model = %attempt.target.model.id,
                    %error,
                    "failed to resolve price version; persisting usage with unknown cost"
                );
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    let attempt_number = meta.partial_attempts.len() + 1;
    let attempt_row = usage_attempt_row(
        meta,
        &attempt.target,
        key.as_ref(),
        attempt.upstream_request_id.as_deref(),
        attempt_number,
        &usage,
        cost,
        price_version_id.as_deref(),
        termination,
        error_message.as_deref(),
        &trace.opaque_route_id,
    );
    let (request_usage, request_cost) = aggregate_request_accounting(meta, Some(&usage), cost);

    // Only complete token totals and cost replace the reservation with actuals.
    // Persist its conservative estimate otherwise, so restart admission matches
    // the live ledger.
    let (admission_usage, admission_cost) =
        aggregate_admission_accounting(meta, Some(&usage), cost);
    let admission_cost_usd = if admission_usage.input.is_some()
        && admission_usage.output.is_some()
        && admission_cost.is_some()
    {
        None
    } else {
        meta.admission
            .as_ref()
            .and_then(|admission| admission.conservative_cost_estimate())
    };
    if let Some(admission) = meta.admission.take() {
        admission.reconcile(&admission_usage, admission_cost);
    }

    // Accounting truthfulness (FR-6.8): provider-reported vs unknown.
    let usage_confidence = if request_usage.input.is_some() || request_usage.output.is_some() {
        "provider_reported"
    } else {
        "unknown"
    };

    // Persist authoritative cache status from final provider-reported usage.
    // A cache read wins over a simultaneous cache write because reuse occurred.
    meta.cache_status = cache_status_from_usage(&usage);

    // Persist the successful session target for both affinity and opaque-state
    // provenance. Affinity only changes routing when its route switch is enabled;
    // provenance is read by FR-2.11 to identify first-attempt provider changes.
    if let (Some(session), Some(route_id)) = (&meta.session, &meta.route_id) {
        if snap.routes.contains_key(route_id) && status == "success" {
            state.sticky_remember(
                session,
                format!(
                    "{}|{}|{}",
                    route_id, attempt.target.account.id, attempt.target.model.id
                ),
            );
        }
    }

    meta.commit_state = termination.commit_state.as_usage_str();
    if status != "success" && status != "client_disconnect" {
        if let Some((kind, upstream_status)) = provider_failure {
            trace.failure(
                Some(attempt.target.account.label.clone()),
                error_message.as_deref().unwrap_or("upstream stream failed"),
                kind,
                upstream_status,
            );
        }
    }
    trace.finish(match status {
        "success" => "success",
        "client_disconnect" => "cancelled",
        _ => "failed",
    });

    let (latency_ms, ttft_ms) = persisted_request_timing(
        started.elapsed().as_millis() as i64,
        attempt.is_sse,
        ttft_ms,
    );
    let row = UsageLogRow {
        id: format!("usage_{}", uuid::Uuid::new_v4().simple()),
        request_id: meta.request_id.clone(),
        ts: db::now_iso(),
        key_id: key.as_ref().map(|k| k.id.clone()),
        key_name: key.as_ref().map(|k| k.name.clone()),
        client_format: meta.client_format.to_string(),
        requested_model: req.requested_model.clone(),
        effective_model: Some(model_display.to_string()),
        route_id: meta.route_id.clone(),
        route_name: meta.route_name.clone(),
        fallback_hops: meta.fallback_hops,
        fallback_path: serde_json::to_string(&meta.fallback_path).unwrap_or_else(|_| "[]".into()),
        status: status.to_string(),
        status_code,
        latency_ms: Some(latency_ms),
        ttft_ms,
        input_tokens: request_usage.input.map(|v| v as i64),
        output_tokens: request_usage.output.map(|v| v as i64),
        cached_tokens: request_usage.cached.map(|v| v as i64),
        cache_write_tokens: request_usage.cache_write.map(|v| v as i64),
        thinking_tokens: request_usage.thinking.map(|v| v as i64),
        cost_usd: request_cost,
        cost_known: request_cost.is_some() as i64,
        // A request that spans provider models/rates has no single pricing
        // version. Its auditable per-attempt versions live in usage_attempts.
        price_version_id: if meta.partial_attempts.is_empty() {
            price_version_id
        } else {
            None
        },
        cache_status: meta.cache_status.to_string(),
        serving_account_id: Some(attempt.target.account.id.clone()),
        serving_account: Some(attempt.target.account.label.clone()),
        serving_provider: Some(attempt.target.provider.name.clone()),
        upstream_request_id: attempt.upstream_request_id.clone(),
        flagged: (request_usage.input.is_none() && status == "success") as i64,
        error_message,
        usage_confidence: usage_confidence.to_string(),
        commit_state: meta.commit_state.to_string(),
        retry_count: meta.retry_count,
        route_trace_id: Some(trace.opaque_route_id.clone()),
        opaque_route_id: Some(trace.opaque_route_id.clone()),
        admission_cost_usd,
    };
    let mut attempt_rows = meta
        .partial_attempts
        .iter()
        .map(|attempt| attempt.row.clone())
        .collect::<Vec<_>>();
    attempt_rows.push(attempt_row);
    state.log_queue.enqueue_bundle(db::UsageAccountingBundle {
        request: row,
        attempts: attempt_rows,
    });
    meta.accounting_enqueued.store(true, Ordering::Release);

    // Read-only usage hook (§6.6): fire-and-forget on the bounded async queue,
    // after accounting is recorded, so it can never block or fail the request.
    if let Some(manager) = state.plugin_manager().cloned() {
        let json = serde_json::json!({
            "request_id": meta.request_id,
            "model": model_display,
            "provider": attempt.target.provider.name,
            "account": attempt.target.account.label,
            "route": meta.route_name,
            "status": status,
            "commit_state": meta.commit_state,
            "retry_count": meta.retry_count,
            "input_tokens": request_usage.input,
            "output_tokens": request_usage.output,
            "cached_tokens": request_usage.cached,
            "cache_write_tokens": request_usage.cache_write,
            "thinking_tokens": request_usage.thinking,
            "cost_usd": request_cost,
            "latency_ms": started.elapsed().as_millis() as i64,
        })
        .to_string();
        state.spawn_hook(move || async move {
            let mut hooks = tokio::task::JoinSet::new();
            for id in manager.plugins_with_hook("on_usage_finalized").await {
                let manager = manager.clone();
                let json = json.clone();
                hooks.spawn(async move {
                    let _ = manager.hook_usage_finalized(&id, &json).await;
                });
            }
            while hooks.join_next().await.is_some() {}
        });
    }

    // Route Trace (metadata only, FR-12.14).
    let _ = db::insert_route_trace(&state.pool, &trace).await;

    state.flight.record(
        &meta.request_id,
        started.elapsed().as_millis() as u64,
        "usage_finalized",
        format!("status={status} tokens={:?}", usage.input),
    );
    state.live.finish(
        &meta.request_id,
        status,
        started.elapsed().as_millis() as u64,
        usage.input,
        usage.output,
    );

    // Optional per-key body logging (FR-6.5): off unless the key opts in, the
    // body is redacted, and retention is short (7 days). Request bodies are
    // logged here for every path; non-streaming responses are additionally
    // logged by drive_aggregate. Streaming responses are not buffered (NFR-1.3),
    // so only their request is retained.
    if let Some(k) = &key {
        if k.body_logging != 0 {
            let request_body = match &req.raw_body {
                Some(raw) => crate::crypto::redact(raw),
                None => serde_json::json!({
                    "model": model_display,
                    "messages": req.messages.len(),
                })
                .to_string(),
            };
            let _ = db::insert_body_log(
                &state.pool,
                &meta.request_id,
                &k.id,
                "request",
                &request_body,
                7,
            )
            .await;
        }
    }
}

/// A Route Dry Run (FR-8.7): compute candidate ordering, predicate outcomes,
/// capability/limit eligibility, account state, and the selected target when
/// ordering is deterministic. Stochastic runtime ordering is reported without
/// claiming an exact target. Never mutates production state or calls upstream.
///
/// `descriptor` mirrors the fields a representative request would carry.
#[derive(Debug, Default, serde::Deserialize)]
pub struct DryRunRequest {
    #[serde(default)]
    pub frontend: Option<String>,
    #[serde(default)]
    pub key_tag: Option<String>,
    #[serde(default)]
    pub has_tools: bool,
    #[serde(default)]
    pub has_images: bool,
    #[serde(default)]
    pub has_reasoning: bool,
    #[serde(default)]
    pub has_structured_output: bool,
    #[serde(default)]
    pub has_parallel_tools: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Providers the calling key is restricted to (FR-12.19); empty = no
    /// restriction. The dashboard passes the selected key's allowlist so the
    /// dry run reflects access restrictions, not just predicate/capability.
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    /// Whether the descriptor's key/account soft quota is already reached
    /// (FR-12.8). When true, candidates are marked ineligible for the same
    /// reason the data path would skip them.
    #[serde(default)]
    pub soft_quota_reached: bool,
    /// Whether request-level fallback is permitted. Omitted means enabled.
    #[serde(default)]
    pub allow_fallback: Option<bool>,
    #[serde(default)]
    pub session: Option<String>,
}

pub async fn dry_run(
    state: &AppState,
    requested_model: &str,
    descriptor: &DryRunRequest,
) -> Result<serde_json::Value, ProxyError> {
    let snap = state.registry.snapshot();
    let resolved =
        crate::registry::Registry::resolve_in(&snap, requested_model).ok_or_else(|| {
            ProxyError::not_found(format!("model '{requested_model}' is not configured"))
        })?;

    let frontend = descriptor.frontend.as_deref().unwrap_or("openai");
    let needs = crate::types::CapabilityNeeds {
        vision: descriptor.has_images,
        tools: descriptor.has_tools,
        parallel_tools: descriptor.has_parallel_tools,
        streaming: descriptor.streaming,
        reasoning: descriptor.has_reasoning,
        structured_output: descriptor.has_structured_output,
    };

    let simulation_seed = stable_route_hash(
        descriptor.input_tokens.unwrap_or_default() ^ u64::from(descriptor.has_tools),
        requested_model.as_bytes(),
    );
    let (targets, route) = match resolved {
        Resolved::Single {
            provider_id,
            model_id,
        } => {
            let model = snap
                .models
                .get(&model_id)
                .cloned()
                .ok_or_else(|| ProxyError::not_found("model not found"))?;
            let provider = snap
                .providers
                .get(&provider_id)
                .cloned()
                .ok_or_else(|| ProxyError::not_found("provider not found"))?;
            let accounts = select_accounts_for_simulation(&snap, &provider_id, simulation_seed);
            (
                accounts
                    .into_iter()
                    .map(|account| ResolvedTarget {
                        account,
                        model: model.clone(),
                        provider: provider.clone(),
                        route_target_id: None,
                        priority: 1,
                        weight: 1,
                        predicate: TargetPredicate::default(),
                        param_overrides: Value::Null,
                    })
                    .collect::<Vec<_>>(),
                None,
            )
        }
        Resolved::Route { route, targets } => (targets, Some(route)),
    };

    let adaptive_route = route
        .as_ref()
        .is_some_and(|route| route.strategy == "adaptive");

    let request_facts = RequestFacts {
        frontend,
        requested_model,
        requested_route: route.as_ref().map(|r| r.name.as_str()),
        key_tag: descriptor.key_tag.as_deref(),
        has_tools: descriptor.has_tools,
        has_images: descriptor.has_images,
        has_reasoning: descriptor.has_reasoning,
        input_tokens: descriptor.input_tokens.unwrap_or(0),
    };
    let plugin_facts = gather_plugin_facts(state, &request_facts, None).await;

    let mut hard_eligible = Vec::new();
    let mut evaluations = std::collections::HashMap::new();
    for target in &targets {
        let decision = evaluate_request_eligibility(
            target,
            &request_facts,
            &plugin_facts,
            &needs,
            &descriptor.allowed_providers,
        );
        let protocol_ok = dry_run_provider_accepts_frontend(&target.provider, frontend);
        if decision.eligible() && protocol_ok {
            hard_eligible.push(target.clone());
        }
        evaluations.insert(adaptive_candidate_key(target), (decision, protocol_ok));
    }

    let dry_run_traffic = adaptive_route.then(|| snapshot_traffic_targets(state, &hard_eligible));
    let mut affinity_candidate_id = None;
    let route_rank = if let Some(route) = &route {
        let ordered =
            order_route_targets_for_simulation(state, route, hard_eligible, simulation_seed)
                .await
                .targets;
        if let (Some(session), true) = (
            descriptor.session.as_deref(),
            route.cache_affinity != 0 || route.sticky_routing != 0,
        ) {
            if let Some(sticky_key) = state.sticky_lookup(session, STICKY_TTL) {
                if let Some(target) = ordered
                    .iter()
                    .find(|target| target_key(route, target) == sticky_key)
                {
                    affinity_candidate_id = Some(adaptive_candidate_key(target));
                }
            }
        }
        Some(
            ordered
                .iter()
                .enumerate()
                .map(|(rank, target)| (adaptive_candidate_key(target), rank))
                .collect::<std::collections::HashMap<_, _>>(),
        )
    } else {
        Some(
            targets
                .iter()
                .enumerate()
                .map(|(rank, target)| (adaptive_candidate_key(target), rank))
                .collect::<std::collections::HashMap<_, _>>(),
        )
    };

    let mut candidates = Vec::new();
    let mut dispatch_candidates = Vec::with_capacity(targets.len());
    let allow_fallback = descriptor.allow_fallback.unwrap_or(true);
    let quota_fallback_allowed =
        allow_fallback && route_allows_fallback(route.as_ref(), FailureKind::QuotaExhausted);
    for t in &targets {
        let (decision, protocol_ok) = evaluations
            .get(&adaptive_candidate_key(t))
            .expect("every dry-run candidate has an evaluation")
            .clone();
        let elig = &decision.predicate;
        let capability = &decision.capability;
        let ctx_ok = decision.context_eligible;
        let provider_allowed = decision.provider_permitted;
        let integration_features_allowed = decision.integration_features_allowed;
        let status = pool::effective_status(&t.account);
        let half_open_probe =
            matches!(status, pool::AccountStatus::CircuitOpen) && pool::should_probe(&t.account);
        let account_eligible = matches!(status, pool::AccountStatus::Healthy) || half_open_probe;
        let account_quota_reached = pool::soft_quota_reached(&state.pool, &t.account)
            .await
            .map_err(|_| ProxyError::internal("could not inspect account quota state"))?;
        let quota_override_available = !descriptor.soft_quota_reached;
        let adaptive_capacity_ok = dry_run_traffic
            .as_ref()
            .and_then(|snapshots| snapshots.get(&traffic_key(t)))
            .map(|snapshot| snapshot.has_capacity)
            .unwrap_or(true);
        let provider_circuit = state.provider_circuits.availability(&t.provider.id);
        let route_capacity = route
            .as_ref()
            .map(|route| {
                state
                    .admission
                    .route_capacity_available(&route.id, route.max_concurrent_requests)
            })
            .unwrap_or(true);
        let request_eligible = decision.eligible() && protocol_ok;
        let pre_dispatch_facts = crate::pre_dispatch::PreDispatchFacts {
            request_eligible,
            account_eligible,
            account_quota_reached: Some(account_quota_reached),
            quota_fallback_allowed,
            provider_circuit_available: Some(provider_circuit.available),
            route_capacity_available: route_capacity,
            quota_override_available,
            adaptive_capacity_available: adaptive_capacity_ok,
        };
        let candidate_id = adaptive_candidate_key(t);
        let rank = route_rank
            .as_ref()
            .and_then(|ranks| ranks.get(&candidate_id).copied());
        dispatch_candidates.push(crate::pre_dispatch::DispatchCandidate {
            id: candidate_id.clone(),
            rank,
            group_id: t.route_target_id.clone(),
            account_priority: t.account.priority,
            weight: t.weight,
            defer_for_circuit: matches!(status, pool::AccountStatus::CircuitOpen)
                && !half_open_probe,
            facts: pre_dispatch_facts,
        });
        let mut reasons = Vec::new();
        if !elig.eligible {
            reasons.push("predicate".to_string());
        }
        if let Some(error) = decision.execution_profile_error.as_deref() {
            reasons.push(format!("invalid_execution_profile: {error}"));
        }
        if !account_eligible {
            reasons.push("account_state".to_string());
        }
        if !capability.eligible {
            reasons.extend(capability.reasons.iter().cloned());
        }
        if !ctx_ok {
            reasons.push("context_window".to_string());
        }
        if !provider_allowed {
            reasons.push("provider_not_permitted".to_string());
        }
        if !integration_features_allowed {
            reasons.push("integration_feature_ceiling".to_string());
        }
        if !protocol_ok {
            reasons.push("input_protocol".to_string());
        }
        if !provider_circuit.available {
            reasons.push("provider_circuit_open".to_string());
        }
        if !route_capacity {
            reasons.push("route_concurrency".to_string());
        }
        if !quota_override_available {
            reasons.push("soft_quota".to_string());
        }
        if account_quota_reached {
            reasons.push("account_soft_quota".to_string());
        }
        if !adaptive_capacity_ok {
            reasons.push("adaptive_saturated".to_string());
        }
        candidates.push(serde_json::json!({
            "candidate_id": candidate_id,
            "strategy_rank": rank,
            "target": format!("{} @ {}", t.model.display_name, t.account.label),
            "model": t.model.display_name,
            "model_id": t.model.id,
            "provider": t.provider.name,
            "provider_id": t.provider.id,
            "account": t.account.label,
            "account_id": t.account.id,
            "account_status": status.as_str(),
            "half_open_probe": half_open_probe,
            "route_target_id": t.route_target_id.as_deref(),
            "priority": t.priority,
            "weight": t.weight,
            "predicate_result": elig.result.as_str(),
            "predicate_explanation": elig.explanation,
            "predicate_eligible": elig.eligible,
            "capability_eligible": capability.eligible,
            "capability_details": capability.details,
            "integration_features_allowed": integration_features_allowed,
            "context_eligible": ctx_ok,
            "provider_permitted": provider_allowed,
            "quota_available": quota_override_available,
            "account_quota_available": !account_quota_reached,
            "provider_circuit_state": provider_circuit.state,
            "provider_circuit_available": provider_circuit.available,
            "provider_circuit_retry_at": provider_circuit.retry_at,
            "route_capacity_available": route_capacity,
            "adaptive_capacity_available": adaptive_route.then_some(adaptive_capacity_ok),
            "not_selected_reasons": reasons,
        }));
    }

    let dispatch_plan = crate::pre_dispatch::plan_dispatch(
        &dispatch_candidates,
        route.is_some(),
        if route
            .as_ref()
            .is_some_and(|route| route.strategy == "weighted")
        {
            crate::pre_dispatch::DispatchStrategy::Weighted
        } else {
            crate::pre_dispatch::DispatchStrategy::Ordered
        },
        affinity_candidate_id.as_deref(),
        true,
    );
    let records_by_id: std::collections::HashMap<_, _> = dispatch_plan
        .candidates
        .iter()
        .map(|record| (record.candidate_id.as_str(), record))
        .collect();
    let selected_identity = match &dispatch_plan.outcome {
        crate::pre_dispatch::DispatchOutcome::Selected(candidate_id) => Some(candidate_id.as_str()),
        _ => None,
    };
    let selected = selected_identity.and_then(|candidate_id| {
        candidates
            .iter()
            .find(|candidate| candidate["candidate_id"] == candidate_id)
            .and_then(|candidate| candidate["target"].as_str())
            .map(str::to_owned)
    });
    let stochastic_selection = dispatch_plan.stochastic_selection;
    let has_quota_stop = dispatch_plan
        .candidates
        .iter()
        .any(|record| record.decision == crate::pre_dispatch::PreDispatchDecision::RateLimited);

    for candidate in &mut candidates {
        let candidate_id = candidate["candidate_id"].as_str().unwrap_or_default();
        let record = records_by_id
            .get(candidate_id)
            .expect("every dry-run candidate has a dispatch-plan record");
        candidate["strategy_rank"] = if stochastic_selection {
            serde_json::Value::Null
        } else {
            serde_json::json!(record.strategy_rank)
        };
        candidate["eligible"] = serde_json::json!(
            record.decision == crate::pre_dispatch::PreDispatchDecision::Dispatchable
        );
        candidate["selected"] = serde_json::json!(record.selected);
        candidate["decision_reason"] = serde_json::json!(record.decision_reason);
        if record.decision_reason != "selected_by_route_strategy"
            && record.decision_reason != "selected_by_account_pool"
            && record.decision_reason != "ineligible"
        {
            if let Some(reasons) = candidate["not_selected_reasons"].as_array_mut() {
                reasons.push(serde_json::json!(record.decision_reason));
            }
        }
    }

    let outcome = match &dispatch_plan.outcome {
        crate::pre_dispatch::DispatchOutcome::Stochastic => "stochastic",
        crate::pre_dispatch::DispatchOutcome::RateLimited(_) => "rate_limited",
        crate::pre_dispatch::DispatchOutcome::Selected(_) => "selected",
        crate::pre_dispatch::DispatchOutcome::NoEligibleTarget => "no_eligible_target",
    };
    let selection_mode = if stochastic_selection {
        "stochastic"
    } else {
        "deterministic"
    };
    let selection_note = if stochastic_selection && !quota_fallback_allowed && has_quota_stop {
        "Runtime selection is stochastic; selecting a quota-exhausted account can return HTTP 429 because fallback on quota is disabled."
    } else if stochastic_selection {
        "Runtime selection is stochastic; no exact target can be predicted."
    } else if matches!(
        &dispatch_plan.outcome,
        crate::pre_dispatch::DispatchOutcome::RateLimited(_)
    ) {
        "Quota is exhausted and either request-level or Route fallback is disabled; runtime would return HTTP 429 before upstream dispatch."
    } else {
        "Selection follows the current candidate ordering and availability snapshot."
    };

    Ok(serde_json::json!({
        "requested_model": requested_model,
        "route": route.as_ref().map(|r| r.name.clone()),
        "route_id": route.as_ref().map(|r| r.id.clone()),
        "strategy": route.as_ref().map(|r| r.strategy.clone()),
        "candidates": candidates,
        "would_select": selected,
        "selection_mode": selection_mode,
        "outcome": outcome,
        "selection_note": selection_note,
        "plugin_fact_failures": plugin_facts.failures.iter().map(|(plugin, reason)| {
            serde_json::json!({"plugin": plugin, "reason": reason})
        }).collect::<Vec<_>>(),
        "note": "Dry run only: no production state was mutated and no upstream call was made.",
    }))
}

#[cfg(test)]
mod route_policy_tests {
    use super::*;
    use axum::response::IntoResponse;

    #[derive(Clone)]
    struct QuotaRaceUpstream {
        request_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        primary_started: std::sync::Arc<tokio::sync::Notify>,
        release_primary: std::sync::Arc<tokio::sync::Notify>,
        credentials: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    async fn quota_race_chat_completions(
        axum::extract::State(upstream): axum::extract::State<QuotaRaceUpstream>,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        let credential = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        upstream.credentials.lock().await.push(credential);
        let request_index = upstream
            .request_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if request_index == 0 {
            upstream.primary_started.notify_one();
            upstream.release_primary.notified().await;
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "error": {"message": "temporary upstream failure", "type": "server_error"}
                })),
            )
                .into_response()
        } else {
            (
                axum::http::StatusCode::OK,
                axum::Json(serde_json::json!({
                    "id": "chatcmpl-test",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })),
            )
                .into_response()
        }
    }

    #[test]
    fn capability_eligibility_preserves_unknown_and_unsupported_states() {
        let needs = crate::types::CapabilityNeeds {
            vision: true,
            tools: false,
            parallel_tools: false,
            streaming: false,
            reasoning: false,
            structured_output: false,
        };
        let unknown = crate::adapters::ModelCapabilityFlags::default();
        let permissive = capability_eligibility(&unknown, &needs, false);
        assert!(permissive.eligible);
        assert_eq!(permissive.details["vision"]["status"], "unknown");

        let strict = capability_eligibility(&unknown, &needs, true);
        assert!(!strict.eligible);
        assert_eq!(
            strict.reasons,
            ["required vision capability is unknown under strict capability mode"]
        );

        let unsupported = crate::adapters::ModelCapabilityFlags {
            vision: Some(false),
            ..Default::default()
        };
        let permissive = capability_eligibility(&unsupported, &needs, false);
        assert!(permissive.eligible);
        assert_eq!(permissive.details["vision"]["status"], "unsupported");

        let strict = capability_eligibility(&unsupported, &needs, true);
        assert!(!strict.eligible);
        assert_eq!(
            strict.reasons,
            ["required vision capability is unsupported"]
        );
    }

    #[test]
    fn accountless_failures_from_duplicate_route_rows_share_serving_identity() {
        let circuits = crate::provider_circuit::ProviderCircuits::default();
        let mut first = target();
        first.provider.credential_mode = "none".into();
        first.model.id = "model_shared".into();
        first.route_target_id = Some("route_row_one".into());
        let mut second = first.clone();
        second.route_target_id = Some("route_row_two".into());

        assert_ne!(first.route_target_id, second.route_target_id);
        assert_eq!(
            provider_correlation_target_id(&first),
            provider_correlation_target_id(&second)
        );
        for candidate in [&first, &second] {
            circuits
                .begin_attempt_with_policy(
                    &candidate.provider.id,
                    &candidate.account.id,
                    provider_correlation_target_id(candidate),
                    crate::provider_circuit::ProviderCorrelationPolicy::AccountlessTargets,
                )
                .unwrap()
                .finish_failure(FailureKind::ServerError, Some(503));
        }

        let snapshot = circuits.snapshot(&first.provider.id);
        assert_eq!(
            snapshot.state,
            crate::provider_circuit::ProviderCircuitState::Closed
        );
        assert_eq!(snapshot.distinct_failing_targets, 1);
    }

    #[test]
    fn generic_rate_limit_reset_sets_cooldown_without_account_exhaustion() {
        let observation = crate::quota::QuotaHeaderObservation {
            remaining_fraction: 0.0,
            reset_at: Some(chrono::Utc::now() + chrono::Duration::seconds(60)),
        };
        let failure = apply_header_reset_to_rate_limit(
            UpstreamFailure {
                kind: FailureKind::RateLimit,
                status: Some(429),
                retry_after_secs: None,
                message: "rate limit".into(),
                quota_reset_at: None,
            },
            Some(observation),
        );

        assert_eq!(failure.kind, FailureKind::RateLimit);
        assert!((59..=60).contains(&failure.retry_after_secs.unwrap()));
    }

    #[test]
    fn generic_reset_does_not_downgrade_explicit_account_quota_exhaustion() {
        let observation = crate::quota::QuotaHeaderObservation {
            remaining_fraction: 0.0,
            reset_at: Some(chrono::Utc::now() + chrono::Duration::seconds(60)),
        };
        let failure = apply_header_reset_to_rate_limit(
            UpstreamFailure {
                kind: FailureKind::QuotaExhausted,
                status: Some(429),
                retry_after_secs: None,
                message: "account quota exhausted".into(),
                quota_reset_at: None,
            },
            Some(observation),
        );

        assert_eq!(failure.kind, FailureKind::QuotaExhausted);
        assert_eq!(failure.retry_after_secs, None);
    }

    #[test]
    fn persisted_timing_keeps_json_duration_separate_from_streaming_ttft() {
        assert_eq!(
            persisted_request_timing(30_000, false, Some(30_000)),
            (30_000, None),
            "a full JSON response keeps its duration but has no TTFT",
        );
        assert_eq!(
            persisted_request_timing(20_000, true, Some(300)),
            (20_000, Some(300)),
            "streaming keeps first-event TTFT distinct from total duration",
        );
    }

    #[test]
    fn provider_circuit_open_transition_is_added_to_route_trace() {
        let mut trace = RouteTrace::new("req_test".into(), "model".into());
        trace_provider_circuit_transition(
            &mut trace,
            "provider-a",
            crate::provider_circuit::ProviderCircuitTransition::default(),
        );
        assert!(trace.steps.is_empty());

        trace_provider_circuit_transition(
            &mut trace,
            "provider-a",
            crate::provider_circuit::ProviderCircuitTransition {
                opened: true,
                ..Default::default()
            },
        );
        assert_eq!(trace.steps.len(), 1);
        assert_eq!(trace.steps[0].stage, "provider_circuit");
        assert_eq!(trace.steps[0].target.as_deref(), Some("provider-a"));
        assert!(trace.steps[0].detail.contains("opened"));
    }

    #[test]
    fn partial_precommit_usage_is_added_to_admission_accounting() {
        let mut meta = RequestMeta::new(
            "req_partial_usage".into(),
            FrontendFormat::OpenAiResponses,
            "route".into(),
            true,
        );
        meta.partial_attempts.push(PartialAttemptUsage {
            usage: TokenUsage {
                input: Some(13),
                output: Some(7),
                ..TokenUsage::default()
            },
            cost: Some(0.000067),
            row: db::UsageAttemptRow::default(),
        });
        let final_usage = TokenUsage {
            input: Some(1),
            output: Some(2),
            ..TokenUsage::default()
        };
        let (usage, cost) =
            aggregate_admission_accounting(&meta, Some(&final_usage), Some(0.000003));
        assert_eq!((usage.input, usage.output), (Some(14), Some(9)));
        assert!((cost.unwrap() - 0.00007).abs() < 1e-12);

        let (failed_usage, failed_cost) = aggregate_admission_accounting(&meta, None, None);
        assert_eq!(
            (failed_usage.input, failed_usage.output),
            (Some(13), Some(7))
        );
        assert_eq!(failed_cost, Some(0.000067));

        meta.partial_attempts[0].usage.output = None;
        meta.partial_attempts[0].cost = None;
        let (incomplete_usage, incomplete_cost) =
            aggregate_admission_accounting(&meta, Some(&final_usage), Some(0.000003));
        assert_eq!(incomplete_usage.input, Some(14));
        assert_eq!(incomplete_usage.output, None);
        assert_eq!(incomplete_cost, None);
    }

    #[test]
    fn cache_status_comes_from_provider_usage() {
        assert_eq!(
            cache_status_from_usage(&TokenUsage {
                cached: Some(0),
                cache_write: Some(8000),
                ..Default::default()
            }),
            "miss"
        );
        assert_eq!(
            cache_status_from_usage(&TokenUsage {
                cached: Some(8000),
                cache_write: Some(300),
                ..Default::default()
            }),
            "hit"
        );
        assert_eq!(cache_status_from_usage(&TokenUsage::default()), "bypass");
    }

    #[test]
    fn transient_failures_are_retryable_but_not_account_scoped() {
        for kind in [
            FailureKind::ServerError,
            FailureKind::ConnectionError,
            FailureKind::Timeout,
        ] {
            assert!(kind.is_retryable());
            assert!(!kind.is_account_scoped());
        }
        for kind in [FailureKind::TargetError, FailureKind::BadRequest] {
            assert!(!kind.is_account_scoped());
        }
        for kind in [
            FailureKind::RateLimit,
            FailureKind::QuotaExhausted,
            FailureKind::AuthError,
        ] {
            assert!(kind.is_account_scoped());
        }
    }

    #[test]
    fn final_direct_timeout_uses_the_shared_policy_status() {
        let error = failure_to_error(&timeout_failure("timed out"), &target());
        assert_eq!(
            error.http_status(),
            FailureKind::Timeout.policy().client_status.unwrap()
        );
        assert_eq!(error.http_status(), 504);
    }

    #[test]
    fn host_policy_rejection_has_structured_trace_and_policy_status() {
        let mut trace = RouteTrace::new("req_test".into(), "model".into());
        let error = policy_rejection(
            &mut trace,
            Some("account-test".into()),
            "request feature is unsupported",
        );

        assert_eq!(error.http_status(), 400);
        let step = trace.steps.last().unwrap();
        assert_eq!(step.failure_kind.as_deref(), Some("policy_rejected"));
        assert_eq!(step.failure_category.as_deref(), Some("policy_rejection"));
        assert_eq!(step.failure_reason.as_deref(), Some("policy_rejected"));
        assert_eq!(step.fallback_rule.as_deref(), Some("never"));
        assert_eq!(step.account_health_effect.as_deref(), Some("none"));
        assert_eq!(step.client_status, Some(400));
        assert_eq!(step.retry_hint.as_deref(), Some("none"));
    }

    fn route(triggers: Value) -> db::RouteRow {
        db::RouteRow {
            id: "route_test".into(),
            name: "test".into(),
            description: String::new(),
            strategy: "priority".into(),
            fallback_triggers: triggers.to_string(),
            continuity_policy: "strip".into(),
            portability_policy: "strip_with_warning".into(),
            sticky_routing: 0,
            cache_affinity: 0,
            max_attempts: None,
            max_concurrent_requests: None,
            enabled: 1,
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn provider(rules: Value) -> db::ProviderRow {
        db::ProviderRow {
            id: "prov_test".into(),
            name: "test".into(),
            base_url: "https://api.example.com".into(),
            wire_format: "openai".into(),
            auth_scheme: "bearer".into(),
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: "{}".into(),
            timeout_ms: 1000,
            capability_mode: "permissive".into(),
            models_path: None,
            rate_limit_rules: rules.to_string(),
            enabled: 1,
            follow_redirects: 0,
            credential_hosts: String::new(),
            allow_insecure_tls: 0,
            created_at: "2026-01-01T00:00:00Z".into(),
            wire_plugin: String::new(),
            credential_plugin: String::new(),
            model_source_plugin: String::new(),
            credential_mode: "manual".into(),
            source_plugin_id: None,
            source_integration_id: None,
            pricing_scope: "direct_api".into(),
            integration_features: None,
            integration_protocols: None,
        }
    }

    fn request() -> InternalRequest {
        InternalRequest {
            requested_model: "route".into(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            tool_choice_name: None,
            params: Default::default(),
            stream: true,
            include_usage: false,
            thinking: None,
            extra: Default::default(),
            raw_body: Some(r#"{"model":"route","temperature":0.1}"#.into()),
        }
    }

    fn model(thinking_map: Value) -> db::ModelRow {
        db::ModelRow {
            id: "model_test".into(),
            provider_id: "prov_test".into(),
            upstream_id: "upstream".into(),
            display_name: "Model".into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: None,
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: "{}".into(),
            thinking_map: thinking_map.to_string(),
            extra_request: "{}".into(),
            discovery: "{}".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            opaque_state_plugin: String::new(),
        }
    }

    fn account() -> db::AccountRow {
        db::AccountRow {
            id: "acc_test".into(),
            provider_id: "prov_test".into(),
            label: "test".into(),
            secret_enc: String::new(),
            key_mask: String::new(),
            status: "healthy".into(),
            status_reason: "healthy".into(),
            status_changed_at: None,
            account_state_version: 0,
            cooldown_until: None,
            quota_reset_at: None,
            quota_type: "none".into(),
            quota_window_s: None,
            soft_quota_usd: None,
            priority: 1,
            weight: 1,
            last_error: None,
            last_probe_at: None,
            circuit_open_until: None,
            consecutive_failures: 0,
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn target() -> ResolvedTarget {
        ResolvedTarget {
            account: account(),
            model: model(serde_json::json!({})),
            provider: provider(serde_json::json!({})),
            route_target_id: Some("rt_test".into()),
            priority: 1,
            weight: 1,
            predicate: TargetPredicate::default(),
            param_overrides: Value::Null,
        }
    }

    fn snapshot_traffic_targets_for_test(
        traffic: &crate::upstream_traffic::UpstreamTraffic,
        targets: &[ResolvedTarget],
    ) -> std::collections::HashMap<
        crate::upstream_traffic::TargetKey,
        crate::upstream_traffic::TrafficSnapshot,
    > {
        targets
            .iter()
            .map(|target| {
                let key = traffic_key(target);
                (key.clone(), traffic.snapshot(&key))
            })
            .collect()
    }

    #[tokio::test]
    async fn adaptive_ordering_uses_error_observations_without_ttft_samples() {
        let mut primary = target();
        primary.account.id = "acc_primary".into();
        primary.priority = 1;

        let mut fallback = target();
        fallback.account.id = "acc_fallback".into();
        fallback.priority = 2;

        let traffic = crate::upstream_traffic::UpstreamTraffic::default();
        traffic
            .acquire(traffic_key(&primary), Duration::ZERO)
            .await
            .unwrap()
            .finish(traffic_outcome_for_failure(FailureKind::ServerError));
        traffic
            .acquire(traffic_key(&fallback), Duration::ZERO)
            .await
            .unwrap()
            .finish(crate::upstream_traffic::TrafficOutcome::Success);

        let targets = vec![primary.clone(), fallback.clone()];
        let snapshots = snapshot_traffic_targets_for_test(&traffic, &targets);
        let scores = build_adaptive_scores(&targets, &snapshots);

        assert_eq!(
            snapshots[&traffic_key(&primary)].ttft_samples,
            0,
            "5xx-only targets must still contribute error telemetry"
        );
        assert_eq!(
            compare_adaptive_targets(&primary, &fallback, &scores),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn adaptive_ordering_prefers_known_quota_headroom_over_unknown() {
        let mut unknown = target();
        unknown.account.id = "acc_unknown".into();
        unknown.priority = 1;

        let mut known = target();
        known.account.id = "acc_known".into();
        known.priority = 2;

        let traffic = crate::upstream_traffic::UpstreamTraffic::default();
        let targets = vec![unknown.clone(), known.clone()];
        let snapshots = snapshot_traffic_targets_for_test(&traffic, &targets);
        let quota = std::collections::HashMap::from([(
            adaptive_candidate_key(&known),
            crate::quota::QuotaSnapshot {
                remaining_fraction: Some(0.8),
                reset_at: None,
                observed_at: chrono::Utc::now(),
                source: "test".into(),
                max_age_secs: 60,
            },
        )]);
        let scores = build_adaptive_scores_with_quota(&targets, &snapshots, &quota);

        assert_eq!(
            compare_adaptive_targets(&unknown, &known, &scores),
            std::cmp::Ordering::Greater,
            "known healthy quota must beat unknown without treating unknown as full quota"
        );
    }

    #[test]
    fn adaptive_ordering_keeps_unknown_neutral_against_known_low_quota() {
        let mut low = target();
        low.account.id = "acc_low".into();
        low.priority = 1;

        let mut unknown = target();
        unknown.account.id = "acc_unknown".into();
        unknown.priority = 2;

        let traffic = crate::upstream_traffic::UpstreamTraffic::default();
        let targets = vec![low.clone(), unknown.clone()];
        let snapshots = snapshot_traffic_targets_for_test(&traffic, &targets);
        let quota = std::collections::HashMap::from([(
            adaptive_candidate_key(&low),
            crate::quota::QuotaSnapshot {
                remaining_fraction: Some(0.1),
                reset_at: None,
                observed_at: chrono::Utc::now(),
                source: "test".into(),
                max_age_secs: 60,
            },
        )]);
        let scores = build_adaptive_scores_with_quota(&targets, &snapshots, &quota);

        assert_eq!(
            compare_adaptive_targets(&low, &unknown, &scores),
            std::cmp::Ordering::Greater,
            "unknown quota stays neutral and must not be synthesized as exhausted or full"
        );
    }

    #[tokio::test]
    async fn first_event_health_prefers_streaming_fallback_before_completion() {
        let mut primary = target();
        primary.account.id = "acc_primary".into();
        primary.priority = 1;

        let mut fallback = target();
        fallback.account.id = "acc_fallback".into();
        fallback.priority = 2;

        let traffic = crate::upstream_traffic::UpstreamTraffic::default();
        traffic
            .acquire(traffic_key(&primary), Duration::ZERO)
            .await
            .unwrap()
            .finish(traffic_outcome_for_failure(FailureKind::ServerError));

        let fallback_permit = traffic
            .acquire(traffic_key(&fallback), Duration::ZERO)
            .await
            .unwrap();
        fallback_permit.mark_first_event(Duration::from_millis(100));

        let targets = vec![primary.clone(), fallback.clone()];
        let snapshots = snapshot_traffic_targets_for_test(&traffic, &targets);
        let scores = build_adaptive_scores(&targets, &snapshots);

        assert_eq!(
            snapshots[&traffic_key(&fallback)].error_observations,
            0,
            "first-event health stays provisional until the stream terminates"
        );
        assert_eq!(snapshots[&traffic_key(&fallback)].error_ewma, 0.0);
        assert_eq!(
            compare_adaptive_targets(&primary, &fallback, &scores),
            std::cmp::Ordering::Greater
        );

        fallback_permit.finish(crate::upstream_traffic::TrafficOutcome::Success);
    }

    #[tokio::test]
    async fn concurrent_failure_outranks_older_provisional_stream_health() {
        let mut primary = target();
        primary.account.id = "acc_primary".into();
        primary.priority = 1;

        let mut fallback = target();
        fallback.account.id = "acc_fallback".into();
        fallback.priority = 2;

        let traffic = crate::upstream_traffic::UpstreamTraffic::default();

        let open_stream = traffic
            .acquire(traffic_key(&primary), Duration::ZERO)
            .await
            .unwrap();
        open_stream.mark_first_event(Duration::from_millis(100));

        let failed_request = traffic
            .acquire(traffic_key(&primary), Duration::ZERO)
            .await
            .unwrap();
        failed_request.finish(crate::upstream_traffic::TrafficOutcome::Error);

        traffic
            .acquire(traffic_key(&fallback), Duration::ZERO)
            .await
            .unwrap()
            .finish(crate::upstream_traffic::TrafficOutcome::Success);

        let targets = vec![primary.clone(), fallback.clone()];
        let snapshots = snapshot_traffic_targets_for_test(&traffic, &targets);
        let scores = build_adaptive_scores(&targets, &snapshots);

        assert!(
            snapshots[&traffic_key(&primary)].error_ewma > 0.9,
            "newer failure must remain visible while an older validated stream is open"
        );
        assert_eq!(
            compare_adaptive_targets(&primary, &fallback, &scores),
            std::cmp::Ordering::Greater,
            "adaptive routing must prefer the healthy fallback after the concurrent failure"
        );

        open_stream.finish(crate::upstream_traffic::TrafficOutcome::Success);
    }

    #[tokio::test]
    async fn decayed_failure_penalty_allows_priority_primary_to_recover() {
        let mut primary = target();
        primary.account.id = "acc_primary".into();
        primary.priority = 1;

        let mut fallback = target();
        fallback.account.id = "acc_fallback".into();
        fallback.priority = 2;

        let traffic = crate::upstream_traffic::UpstreamTraffic::default();
        traffic
            .acquire(traffic_key(&primary), Duration::ZERO)
            .await
            .unwrap()
            .finish(traffic_outcome_for_failure(FailureKind::ServerError));

        let fallback_permit = traffic
            .acquire(traffic_key(&fallback), Duration::ZERO)
            .await
            .unwrap();
        fallback_permit.mark_first_event(Duration::from_millis(100));
        fallback_permit.finish(crate::upstream_traffic::TrafficOutcome::Success);

        let future = Instant::now() + Duration::from_secs(5 * 60 + 1);
        let mut recovered = std::collections::HashMap::new();
        recovered.insert(
            traffic_key(&primary),
            traffic.snapshot_at(&traffic_key(&primary), future),
        );
        recovered.insert(
            traffic_key(&fallback),
            traffic.snapshot_at(&traffic_key(&fallback), future),
        );
        let targets = vec![primary.clone(), fallback.clone()];
        let scores = build_adaptive_scores(&targets, &recovered);

        assert_eq!(recovered[&traffic_key(&primary)].error_ewma, 0.0);
        assert_eq!(
            compare_adaptive_targets(&primary, &fallback, &scores),
            std::cmp::Ordering::Less,
            "once the transient failure penalty decays to neutral, configured priority should win"
        );
    }

    #[tokio::test]
    async fn adaptive_ttft_score_is_transitive_with_observed_cold_observed_targets() {
        let mut a = target();
        a.account.id = "acc_a".into();
        a.priority = 1;

        let mut b = target();
        b.account.id = "acc_b".into();
        b.priority = 2;

        let mut c_target = target();
        c_target.account.id = "acc_c".into();
        c_target.priority = 3;

        let traffic = crate::upstream_traffic::UpstreamTraffic::default();

        let a_permit = traffic
            .acquire(traffic_key(&a), Duration::ZERO)
            .await
            .unwrap();
        a_permit.mark_first_event(Duration::from_millis(200));
        a_permit.finish(crate::upstream_traffic::TrafficOutcome::Success);

        let c_permit = traffic
            .acquire(traffic_key(&c_target), Duration::ZERO)
            .await
            .unwrap();
        c_permit.mark_first_event(Duration::from_millis(100));
        c_permit.finish(crate::upstream_traffic::TrafficOutcome::Success);

        let targets = vec![a.clone(), b.clone(), c_target.clone()];
        let snapshots = snapshot_traffic_targets_for_test(&traffic, &targets);
        let scores = build_adaptive_scores(&targets, &snapshots);

        assert_eq!(scores[&adaptive_candidate_key(&a)].ttft_ms, 200.0);
        assert_eq!(scores[&adaptive_candidate_key(&b)].ttft_ms, 150.0);
        assert_eq!(scores[&adaptive_candidate_key(&c_target)].ttft_ms, 100.0);

        assert_eq!(
            compare_adaptive_targets(&c_target, &b, &scores),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_adaptive_targets(&b, &a, &scores),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_adaptive_targets(&c_target, &a, &scores),
            std::cmp::Ordering::Less
        );

        let mut first = targets.clone();
        first.sort_by(|left, right| compare_adaptive_targets(left, right, &scores));
        let mut second = targets;
        second.sort_by(|left, right| compare_adaptive_targets(left, right, &scores));

        let first_ids: Vec<_> = first
            .iter()
            .map(|target| target.account.id.as_str())
            .collect();
        let second_ids: Vec<_> = second
            .iter()
            .map(|target| target.account.id.as_str())
            .collect();
        assert_eq!(first_ids, vec!["acc_c", "acc_b", "acc_a"]);
        assert_eq!(first_ids, second_ids);
    }

    #[tokio::test]
    async fn stale_ttft_becomes_neutral_and_priority_can_recover() {
        let mut primary = target();
        primary.account.id = "acc_primary".into();
        primary.priority = 1;

        let mut fallback = target();
        fallback.account.id = "acc_fallback".into();
        fallback.priority = 2;

        let traffic = crate::upstream_traffic::UpstreamTraffic::default();

        let primary_permit = traffic
            .acquire(traffic_key(&primary), Duration::ZERO)
            .await
            .unwrap();
        primary_permit.mark_first_event(Duration::from_secs(5));
        primary_permit.finish(crate::upstream_traffic::TrafficOutcome::Success);

        let fallback_permit = traffic
            .acquire(traffic_key(&fallback), Duration::ZERO)
            .await
            .unwrap();
        fallback_permit.mark_first_event(Duration::from_millis(100));
        fallback_permit.finish(crate::upstream_traffic::TrafficOutcome::Success);

        let targets = vec![primary.clone(), fallback.clone()];
        let current = snapshot_traffic_targets_for_test(&traffic, &targets);
        let current_scores = build_adaptive_scores(&targets, &current);
        assert_eq!(
            compare_adaptive_targets(&primary, &fallback, &current_scores),
            std::cmp::Ordering::Greater
        );

        let future = Instant::now() + Duration::from_secs(5 * 60 + 1);
        let mut recovered = current;
        recovered.insert(
            traffic_key(&primary),
            traffic.snapshot_at(&traffic_key(&primary), future),
        );
        assert_eq!(recovered[&traffic_key(&primary)].fast_ttft_ms, None);

        let recovered_scores = build_adaptive_scores(&targets, &recovered);
        assert_eq!(
            recovered_scores[&adaptive_candidate_key(&primary)].ttft_ms,
            recovered_scores[&adaptive_candidate_key(&fallback)].ttft_ms
        );
        assert_eq!(
            compare_adaptive_targets(&primary, &fallback, &recovered_scores),
            std::cmp::Ordering::Less,
            "stale TTFT must become neutral so configured priority can reclaim the target"
        );
    }

    #[tokio::test]
    async fn ineligible_observed_target_does_not_change_eligible_adaptive_ordering() {
        let mut a = target();
        a.account.id = "acc_a".into();
        a.priority = 2;

        let mut b = target();
        b.account.id = "acc_b".into();
        b.priority = 1;

        let mut ineligible = target();
        ineligible.account.id = "acc_ineligible".into();
        ineligible.priority = 3;

        let traffic = crate::upstream_traffic::UpstreamTraffic::default();

        let a_permit = traffic
            .acquire(traffic_key(&a), Duration::ZERO)
            .await
            .unwrap();
        a_permit.mark_first_event(Duration::from_millis(100));
        a_permit.finish(crate::upstream_traffic::TrafficOutcome::Success);

        let ineligible_permit = traffic
            .acquire(traffic_key(&ineligible), Duration::ZERO)
            .await
            .unwrap();
        ineligible_permit.mark_first_event(Duration::from_secs(10));
        ineligible_permit.finish(crate::upstream_traffic::TrafficOutcome::Success);

        let all_targets = vec![a.clone(), b.clone(), ineligible.clone()];
        let all_snapshots = snapshot_traffic_targets_for_test(&traffic, &all_targets);
        let eligible = vec![a.clone(), b.clone()];

        let scores_with_extra_snapshot = build_adaptive_scores(&eligible, &all_snapshots);
        let mut eligible_snapshots = all_snapshots.clone();
        eligible_snapshots.remove(&traffic_key(&ineligible));
        let scores_without_extra_snapshot = build_adaptive_scores(&eligible, &eligible_snapshots);

        assert_eq!(
            scores_with_extra_snapshot[&adaptive_candidate_key(&b)].ttft_ms,
            100.0
        );
        assert_eq!(
            compare_adaptive_targets(&b, &a, &scores_with_extra_snapshot),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_adaptive_targets(&b, &a, &scores_without_extra_snapshot),
            std::cmp::Ordering::Less
        );

        let all_scores = build_adaptive_scores(&all_targets, &all_snapshots);
        assert_eq!(
            all_scores[&adaptive_candidate_key(&b)].ttft_ms,
            5_050.0,
            "test setup must prove the excluded 10s sample would otherwise move the neutral median"
        );
    }

    #[tokio::test]
    async fn adaptive_order_and_trace_share_frozen_account_global_quota_evidence() {
        let (state, root, _, _, _) = adaptive_dry_run_state().await;
        let mut route_row = route(serde_json::json!({}));
        route_row.strategy = "adaptive".into();

        let mut preferred = target();
        preferred.route_target_id = Some("rt_preferred".into());
        preferred.account.id = "acc_preferred".into();
        let mut fallback = target();
        fallback.route_target_id = Some("rt_fallback".into());
        fallback.account.id = "acc_fallback".into();

        state.quota.observe_account_global(
            "prov_test",
            "acc_preferred",
            Some(0.75),
            None,
            "plugin_health_probe",
            Duration::from_secs(60),
        );
        state.quota.observe_account_global(
            "prov_test",
            "acc_fallback",
            Some(0.25),
            None,
            "plugin_health_probe",
            Duration::from_secs(60),
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-remaining-requests", "0".parse().unwrap());
        state
            .quota
            .observe_headers("prov_test", "acc_preferred", &headers)
            .unwrap();

        let ordering =
            order_route_targets(&state, &route_row, vec![preferred.clone(), fallback]).await;
        assert_eq!(ordering.targets[0].account.id, "acc_preferred");
        assert_eq!(
            state
                .quota
                .snapshot("prov_test", "acc_preferred")
                .unwrap()
                .remaining_fraction,
            Some(0.0),
            "the latest generic bucket remains diagnostic"
        );
        let frozen = ordering.quota_evidence[&adaptive_candidate_key(&preferred)]
            .as_ref()
            .unwrap();
        assert_eq!(frozen.remaining_fraction, Some(0.75));
        assert_eq!(frozen.source, "plugin_health_probe");

        // A later observation must not change the evidence attached to this ordering pass.
        state.quota.observe_account_global(
            "prov_test",
            "acc_preferred",
            Some(0.10),
            None,
            "later_health_probe",
            Duration::from_secs(60),
        );
        let mut trace = RouteTrace::new("request".into(), "model".into());
        trace_adaptive_quota_evidence(&mut trace, &preferred, &ordering.quota_evidence);
        assert_eq!(
            trace.steps[0].target.as_deref(),
            Some(preferred.account.label.as_str())
        );
        let detail = &trace.steps[0].detail;
        assert!(detail.contains("remaining=75.0%"), "{detail}");
        assert!(detail.contains("source=plugin_health_probe"), "{detail}");
        assert!(detail.contains("scope=account-global"), "{detail}");

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn adaptive_ordering_ignores_unavailable_fast_sibling_telemetry() {
        let (state, root, _, _, _) = adaptive_dry_run_state().await;

        let mut route_row = route(serde_json::json!({}));
        route_row.strategy = "adaptive".into();

        let mut a_unavailable = target();
        a_unavailable.route_target_id = Some("rt_a".into());
        a_unavailable.account.id = "acc_a_exhausted".into();
        a_unavailable.account.status = "exhausted".into();
        a_unavailable.priority = 1;

        let mut a_healthy = target();
        a_healthy.route_target_id = Some("rt_a".into());
        a_healthy.account.id = "acc_a_healthy".into();
        a_healthy.priority = 1;

        let mut b_healthy = target();
        b_healthy.route_target_id = Some("rt_b".into());
        b_healthy.account.id = "acc_b_healthy".into();
        b_healthy.priority = 2;

        for (candidate, ttft) in [
            (&a_unavailable, Duration::from_millis(50)),
            (&a_healthy, Duration::from_secs(5)),
            (&b_healthy, Duration::from_millis(100)),
        ] {
            let permit = state
                .upstream_traffic
                .acquire(traffic_key(candidate), Duration::ZERO)
                .await
                .unwrap();
            permit.mark_first_event(ttft);
            permit.finish(crate::upstream_traffic::TrafficOutcome::Success);
        }

        let ordered = order_route_targets(
            &state,
            &route_row,
            vec![a_unavailable.clone(), a_healthy.clone(), b_healthy.clone()],
        )
        .await;
        let account_ids: Vec<_> = ordered
            .targets
            .iter()
            .map(|candidate| candidate.account.id.as_str())
            .collect();

        assert_eq!(
            account_ids,
            vec!["acc_b_healthy", "acc_a_healthy", "acc_a_exhausted"],
            "the exhausted 50ms sibling must neither represent target A nor outrank dispatchable accounts"
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    async fn adaptive_dry_run_state() -> (AppState, std::path::PathBuf, String, String, Vec<String>)
    {
        let root = std::env::temp_dir().join(format!(
            "kinetix-adaptive-dry-run-{}",
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

        let provider_id = db::insert_provider(
            &state.pool,
            &db::NewProvider {
                name: "adaptive-provider",
                base_url: "https://api.example.com",
                wire_format: crate::types::WireFormat::Openai,
                auth_scheme: crate::types::AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: serde_json::json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: serde_json::json!({}),
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
                upstream_id: "adaptive-model",
                display_name: "Adaptive Model",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: serde_json::json!({}),
                prices: serde_json::json!({}),
                parameters: serde_json::json!({}),
                thinking_map: serde_json::json!({}),
                extra_request: serde_json::json!({}),
                discovery: serde_json::json!({}),
            },
        )
        .await
        .unwrap();

        let mut account_ids = Vec::new();
        for label in ["primary", "fallback"] {
            account_ids.push(
                db::insert_account(&state.pool, &provider_id, label, "", "", 1, 1, None, "none")
                    .await
                    .unwrap(),
            );
        }

        let route_id = db::insert_route(
            &state.pool,
            &db::NewRoute {
                name: "adaptive-dry-run",
                description: "",
                strategy: "adaptive",
                fallback_triggers: serde_json::json!({}),
                portability_policy: "strip_with_warning",
                sticky_routing: false,
                cache_affinity: false,
                max_attempts: None,
                max_concurrent_requests: None,
            },
        )
        .await
        .unwrap();
        for (priority, account_id) in account_ids.iter().enumerate() {
            db::insert_route_target(
                &state.pool,
                &route_id,
                Some(account_id),
                &model_id,
                priority as i64 + 1,
                1,
                "{}",
                "{}",
            )
            .await
            .unwrap();
        }
        state.registry.reload(&state.pool).await.unwrap();

        (state, root, provider_id, model_id, account_ids)
    }

    #[tokio::test]
    async fn successful_recovery_reloads_registry_only_after_a_transition() {
        let (state, root, _, _, account_ids) = adaptive_dry_run_state().await;
        let account_id = &account_ids[0];

        let initial_state_version = state
            .registry
            .account(account_id)
            .unwrap()
            .account_state_version;
        sqlx::query("UPDATE accounts SET last_probe_at='db-only' WHERE id=?")
            .bind(account_id)
            .execute(&state.pool)
            .await
            .unwrap();
        recover_successful_account(&state, account_id, initial_state_version, false).await;
        assert!(state
            .registry
            .account(account_id)
            .unwrap()
            .last_probe_at
            .is_none());

        db::record_account_failure(&state.pool, account_id, 1, 30)
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        assert!(state
            .registry
            .account(account_id)
            .unwrap()
            .circuit_open_until
            .is_some());

        let probe_state_version = state
            .registry
            .account(account_id)
            .unwrap()
            .account_state_version;
        recover_successful_account(&state, account_id, probe_state_version, true).await;
        let recovered = state.registry.account(account_id).unwrap();
        assert!(recovered.circuit_open_until.is_none());
        assert_eq!(recovered.consecutive_failures, 0);
        assert_eq!(recovered.last_probe_at.as_deref(), Some("db-only"));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_surfaces_unknown_capabilities_and_applies_provider_mode() {
        let (state, root, provider_id, _, _) = adaptive_dry_run_state().await;
        let permissive = dry_run(
            &state,
            "adaptive-dry-run",
            &DryRunRequest {
                has_images: true,
                ..DryRunRequest::default()
            },
        )
        .await
        .unwrap();
        let candidates = permissive["candidates"].as_array().unwrap();
        assert!(candidates.iter().all(|candidate| {
            candidate["capability_details"]["vision"]["status"] == "unknown"
                && candidate["capability_eligible"] == true
        }));

        sqlx::query("UPDATE providers SET capability_mode='strict' WHERE id=?")
            .bind(provider_id)
            .execute(&state.pool)
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        let strict = dry_run(
            &state,
            "adaptive-dry-run",
            &DryRunRequest {
                has_images: true,
                ..DryRunRequest::default()
            },
        )
        .await
        .unwrap();
        assert!(strict["would_select"].is_null());
        assert!(strict["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|candidate| {
                candidate["capability_details"]["vision"]["status"] == "unknown"
                    && candidate["capability_eligible"] == false
            }));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_reports_provider_restrictions_and_disabled_accounts() {
        let (state, root, _, _, account_ids) = adaptive_dry_run_state().await;
        let restricted = dry_run(
            &state,
            "adaptive-dry-run",
            &DryRunRequest {
                allowed_providers: vec!["unrelated-provider".into()],
                ..DryRunRequest::default()
            },
        )
        .await
        .unwrap();
        assert!(restricted["would_select"].is_null());
        assert!(restricted["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|candidate| {
                candidate["provider_permitted"] == false
                    && candidate["not_selected_reasons"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|reason| reason == "provider_not_permitted")
            }));

        sqlx::query("UPDATE accounts SET status='disabled' WHERE id IN (?, ?)")
            .bind(&account_ids[0])
            .bind(&account_ids[1])
            .execute(&state.pool)
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        let disabled = dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
            .await
            .unwrap();
        assert!(disabled["would_select"].is_null());
        assert!(disabled["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|candidate| {
                candidate["account_status"] == "disabled"
                    && candidate["not_selected_reasons"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|reason| reason == "account_state")
            }));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_reports_route_concurrency_saturation() {
        let (state, root, _, _, _) = adaptive_dry_run_state().await;
        let route_id = state
            .registry
            .snapshot()
            .routes
            .values()
            .find(|route| route.name == "adaptive-dry-run")
            .unwrap()
            .id
            .clone();
        sqlx::query("UPDATE routes SET max_concurrent_requests=1 WHERE id=?")
            .bind(&route_id)
            .execute(&state.pool)
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        let _reservation = state
            .admission
            .reserve_concurrency(None, Some((&route_id, Some(1))))
            .unwrap();

        let result = dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
            .await
            .unwrap();
        assert!(result["would_select"].is_null());
        assert!(result["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|candidate| {
                candidate["route_capacity_available"] == false
                    && candidate["not_selected_reasons"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|reason| reason == "route_concurrency")
            }));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_marks_randomized_selection_without_claiming_exact_target() {
        let (state, root, _, _, _) = adaptive_dry_run_state().await;
        let route_id = state
            .registry
            .snapshot()
            .routes
            .values()
            .find(|route| route.name == "adaptive-dry-run")
            .unwrap()
            .id
            .clone();

        for strategy in ["round-robin", "weighted"] {
            sqlx::query("UPDATE routes SET strategy=? WHERE id=?")
                .bind(strategy)
                .bind(&route_id)
                .execute(&state.pool)
                .await
                .unwrap();
            state.registry.reload(&state.pool).await.unwrap();

            let cursor_before = state.rr_counter_snapshot(&route_id);
            let first = dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
                .await
                .unwrap();
            let second = dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
                .await
                .unwrap();
            assert_eq!(first, second, "{strategy} dry runs must be repeatable");
            assert_eq!(state.rr_counter_snapshot(&route_id), cursor_before);
            if strategy == "weighted" {
                sqlx::query(
                    "UPDATE route_targets SET weight=CASE WHEN priority=1 THEN 1 ELSE 0 END WHERE route_id=?",
                )
                .bind(&route_id)
                .execute(&state.pool)
                .await
                .unwrap();
                state.registry.reload(&state.pool).await.unwrap();
                let zero_weight_result =
                    dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
                        .await
                        .unwrap();
                assert_eq!(zero_weight_result["selection_mode"], "stochastic");
                assert_eq!(zero_weight_result["outcome"], "stochastic");
                assert!(zero_weight_result["would_select"].is_null());
                assert_eq!(first["selection_mode"], "stochastic");
                assert_eq!(first["outcome"], "stochastic");
                assert!(first["would_select"].is_null());
                assert!(first["candidates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|candidate| {
                        candidate["strategy_rank"].is_null()
                            && candidate["selected"] == false
                            && candidate["decision_reason"] == "stochastic_selection"
                    }));
            } else {
                assert_eq!(first["selection_mode"], "deterministic");
                assert!(first["would_select"].is_string());
            }
        }

        let direct = dry_run(
            &state,
            "adaptive-provider/adaptive-model",
            &DryRunRequest::default(),
        )
        .await
        .unwrap();
        assert_eq!(direct["selection_mode"], "stochastic");
        assert_eq!(direct["outcome"], "stochastic");
        assert!(direct["would_select"].is_null());
        assert!(direct["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|candidate| {
                candidate["strategy_rank"].is_null()
                    && candidate["selected"] == false
                    && candidate["decision_reason"] == "stochastic_selection"
            }));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runtime_refreshes_fallback_quota_after_dispatch_plan_creation() {
        let (state, root, provider_id, model_id, account_ids) = adaptive_dry_run_state().await;
        let upstream = QuotaRaceUpstream {
            request_count: std::sync::Arc::default(),
            primary_started: std::sync::Arc::new(tokio::sync::Notify::new()),
            release_primary: std::sync::Arc::new(tokio::sync::Notify::new()),
            credentials: std::sync::Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream_app = axum::Router::new()
            .route(
                "/chat/completions",
                axum::routing::post(quota_race_chat_completions),
            )
            .with_state(upstream.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream_app).await.unwrap();
        });

        let route_id = state
            .registry
            .snapshot()
            .routes
            .values()
            .find(|route| route.name == "adaptive-dry-run")
            .unwrap()
            .id
            .clone();
        sqlx::query("UPDATE providers SET base_url=? WHERE id=?")
            .bind(format!("http://{addr}"))
            .bind(&provider_id)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE routes SET strategy='priority', max_attempts=3, fallback_triggers='{\"onQuota\":false}' WHERE id=?",
        )
        .bind(&route_id)
        .execute(&state.pool)
        .await
        .unwrap();
        let later_account_id = db::insert_account(
            &state.pool,
            &provider_id,
            "later",
            "",
            "",
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        db::insert_route_target(
            &state.pool,
            &route_id,
            Some(&later_account_id),
            &model_id,
            3,
            1,
            "{}",
            "{}",
        )
        .await
        .unwrap();
        for (account_id, key) in [
            (&account_ids[0], "primary-key"),
            (&account_ids[1], "fallback-key"),
            (&later_account_id, "later-key"),
        ] {
            sqlx::query("UPDATE accounts SET secret_enc=?, key_mask=? WHERE id=?")
                .bind(state.crypto.encrypt(key).unwrap())
                .bind(key)
                .bind(account_id)
                .execute(&state.pool)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE accounts SET soft_quota_usd=0.5, quota_type='daily' WHERE id=?")
            .bind(&account_ids[1])
            .execute(&state.pool)
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        assert!(!pool::soft_quota_reached(
            &state.pool,
            &state.registry.account(&account_ids[1]).unwrap()
        )
        .await
        .unwrap());

        let pool = state.pool.clone();
        let runtime = tokio::spawn(async move {
            let request = InternalRequest {
                requested_model: "adaptive-dry-run".into(),
                system: Vec::new(),
                messages: Vec::new(),
                tools: Vec::new(),
                tool_choice: None,
                tool_choice_name: None,
                params: crate::types::SamplingParams::default(),
                stream: false,
                include_usage: false,
                thinking: None,
                extra: serde_json::Map::new(),
                raw_body: None,
            };
            run(
                &state,
                FrontendFormat::OpenAi,
                None,
                request,
                "quota-live-refresh-runtime".into(),
                true,
                None,
                Vec::new(),
            )
            .await
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            upstream.primary_started.notified(),
        )
        .await
        .expect("primary upstream attempt should start");
        sqlx::query(
            "INSERT INTO usage_logs (id, request_id, ts, client_format, requested_model, status, status_code, cost_usd, cost_known, serving_account_id) VALUES (?, ?, ?, 'openai', 'adaptive-dry-run', 'success', 200, 1.0, 1, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(db::now_iso())
        .bind(&account_ids[1])
        .execute(&pool)
        .await
        .unwrap();
        assert!(pool::soft_quota_reached(
            &pool,
            &db::get_account(&pool, &account_ids[1])
                .await
                .unwrap()
                .unwrap()
        )
        .await
        .unwrap());
        upstream.release_primary.notify_one();

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), runtime)
            .await
            .expect("runtime request should finish")
            .unwrap();
        assert!(matches!(
            result,
            Err(error) if error.kind == crate::types::ErrorKind::RateLimited
        ));
        assert_eq!(
            upstream
                .request_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            upstream.credentials.lock().await.as_slice(),
            ["Bearer primary-key"]
        );

        server.abort();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_reports_account_quota_without_mutating_account_state() {
        let (state, root, _, _, account_ids) = adaptive_dry_run_state().await;
        let quota_account = &account_ids[0];
        sqlx::query("UPDATE accounts SET soft_quota_usd=0.5, quota_type='daily' WHERE id=?")
            .bind(quota_account)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO usage_logs (id, request_id, ts, client_format, requested_model, status, status_code, cost_usd, cost_known, serving_account_id) VALUES (?, ?, ?, 'openai', 'adaptive-dry-run', 'success', 200, 1.0, 1, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(db::now_iso())
        .bind(quota_account)
        .execute(&state.pool)
        .await
        .unwrap();
        state.registry.reload(&state.pool).await.unwrap();

        let result = dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
            .await
            .unwrap();
        let candidates = result["candidates"].as_array().unwrap();
        let quota_candidate = candidates
            .iter()
            .find(|candidate| candidate["account_id"] == *quota_account)
            .unwrap();
        assert_eq!(quota_candidate["account_quota_available"], false);
        assert!(quota_candidate["not_selected_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "account_soft_quota"));
        assert_ne!(quota_candidate["account_status"], "exhausted");
        assert_eq!(result["would_select"], "Adaptive Model @ fallback");

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_honors_disabled_quota_fallback() {
        let (state, root, _, _, account_ids) = adaptive_dry_run_state().await;
        let route_id = state
            .registry
            .snapshot()
            .routes
            .values()
            .find(|route| route.name == "adaptive-dry-run")
            .unwrap()
            .id
            .clone();
        sqlx::query("UPDATE routes SET strategy='priority', fallback_triggers=? WHERE id=?")
            .bind(r#"{"onQuota":false}"#)
            .bind(&route_id)
            .execute(&state.pool)
            .await
            .unwrap();
        let quota_account = &account_ids[0];
        sqlx::query("UPDATE accounts SET soft_quota_usd=0.5, quota_type='daily' WHERE id=?")
            .bind(quota_account)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO usage_logs (id, request_id, ts, client_format, requested_model, status, status_code, cost_usd, cost_known, serving_account_id) VALUES (?, ?, ?, 'openai', 'adaptive-dry-run', 'success', 200, 1.0, 1, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(db::now_iso())
        .bind(quota_account)
        .execute(&state.pool)
        .await
        .unwrap();
        state.registry.reload(&state.pool).await.unwrap();

        let result = dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
            .await
            .unwrap();

        assert!(result["would_select"].is_null());
        assert_eq!(result["outcome"], "rate_limited");
        let candidates = result["candidates"].as_array().unwrap();
        let quota_candidate = candidates
            .iter()
            .find(|candidate| candidate["account_id"] == *quota_account)
            .unwrap();
        assert_eq!(
            quota_candidate["decision_reason"],
            "quota_fallback_disabled"
        );
        let unreachable_fallback = candidates
            .iter()
            .find(|candidate| candidate["account_id"] == account_ids[1])
            .unwrap();
        assert_eq!(
            unreachable_fallback["decision_reason"],
            "unreachable_after_quota_failure"
        );

        sqlx::query("UPDATE routes SET fallback_triggers='{}' WHERE id=?")
            .bind(&route_id)
            .execute(&state.pool)
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        let request_fallback_disabled = dry_run(
            &state,
            "adaptive-dry-run",
            &DryRunRequest {
                allow_fallback: Some(false),
                ..DryRunRequest::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(request_fallback_disabled["outcome"], "rate_limited");

        sqlx::query("UPDATE routes SET fallback_triggers=? WHERE id=?")
            .bind(r#"{"onQuota":false}"#)
            .bind(&route_id)
            .execute(&state.pool)
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();

        let runtime_request = InternalRequest {
            requested_model: "adaptive-dry-run".into(),
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            tool_choice_name: None,
            params: crate::types::SamplingParams::default(),
            stream: false,
            include_usage: false,
            thinking: None,
            extra: serde_json::Map::new(),
            raw_body: None,
        };
        let runtime = run(
            &state,
            FrontendFormat::OpenAi,
            None,
            runtime_request,
            "quota-fallback-disabled-runtime".into(),
            true,
            None,
            Vec::new(),
        )
        .await;
        assert!(matches!(
            runtime,
            Err(error) if error.kind == crate::types::ErrorKind::RateLimited
        ));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_quota_stop_precedes_open_provider_circuit_like_runtime() {
        let (state, root, provider_id, _model_id, account_ids) = adaptive_dry_run_state().await;
        let fallback_provider_id = db::insert_provider(
            &state.pool,
            &db::NewProvider {
                name: "healthy-fallback-provider",
                base_url: "https://fallback.example.com",
                wire_format: crate::types::WireFormat::Openai,
                auth_scheme: crate::types::AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: serde_json::json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: serde_json::json!({}),
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
        let fallback_model_id = db::insert_model(
            &state.pool,
            &db::NewModel {
                provider_id: &fallback_provider_id,
                upstream_id: "fallback-model",
                display_name: "Healthy Fallback",
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: serde_json::json!({}),
                prices: serde_json::json!({}),
                parameters: serde_json::json!({}),
                thinking_map: serde_json::json!({}),
                extra_request: serde_json::json!({}),
                discovery: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
        let fallback_account_id = db::insert_account(
            &state.pool,
            &fallback_provider_id,
            "healthy-fallback",
            "",
            "",
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        let route_id = state
            .registry
            .snapshot()
            .routes
            .values()
            .find(|route| route.name == "adaptive-dry-run")
            .unwrap()
            .id
            .clone();
        sqlx::query("UPDATE routes SET strategy='priority', fallback_triggers=? WHERE id=?")
            .bind(r#"{"onQuota":false}"#)
            .bind(&route_id)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE route_targets SET model_id=?, account_id=?, priority=2 WHERE route_id=? AND account_id=?",
        )
        .bind(&fallback_model_id)
        .bind(&fallback_account_id)
        .bind(&route_id)
        .bind(&account_ids[1])
        .execute(&state.pool)
        .await
        .unwrap();
        let quota_account = &account_ids[0];
        sqlx::query("UPDATE accounts SET soft_quota_usd=0.5, quota_type='daily' WHERE id=?")
            .bind(quota_account)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO usage_logs (id, request_id, ts, client_format, requested_model, status, status_code, cost_usd, cost_known, serving_account_id) VALUES (?, ?, ?, 'openai', 'adaptive-dry-run', 'success', 200, 1.0, 1, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(db::now_iso())
        .bind(quota_account)
        .execute(&state.pool)
        .await
        .unwrap();
        for (account_id, target_id) in [
            (account_ids[0].as_str(), "primary-target"),
            (account_ids[1].as_str(), "sibling-target"),
        ] {
            state
                .provider_circuits
                .begin_attempt(&provider_id, account_id, target_id)
                .unwrap()
                .finish_failure(FailureKind::ServerError, Some(503));
        }
        assert_eq!(
            state.provider_circuits.snapshot(&provider_id).state,
            crate::provider_circuit::ProviderCircuitState::Open
        );
        state.registry.reload(&state.pool).await.unwrap();

        let simulation = dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
            .await
            .unwrap();
        assert_eq!(simulation["outcome"], "rate_limited");
        assert!(simulation["would_select"].is_null());
        assert_eq!(
            simulation["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["account_id"] == *quota_account)
                .unwrap()["decision_reason"],
            "quota_fallback_disabled"
        );
        assert_eq!(
            simulation["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["account_id"] == fallback_account_id)
                .unwrap()["decision_reason"],
            "unreachable_after_quota_failure"
        );

        let runtime_request = InternalRequest {
            requested_model: "adaptive-dry-run".into(),
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            tool_choice_name: None,
            params: crate::types::SamplingParams::default(),
            stream: false,
            include_usage: false,
            thinking: None,
            extra: serde_json::Map::new(),
            raw_body: None,
        };
        let runtime = run(
            &state,
            FrontendFormat::OpenAi,
            None,
            runtime_request,
            "quota-before-provider-circuit-runtime".into(),
            true,
            None,
            Vec::new(),
        )
        .await;
        assert!(matches!(
            runtime,
            Err(error) if error.kind == crate::types::ErrorKind::RateLimited
        ));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_reports_provider_circuit_without_recording_a_reject() {
        let (state, root, provider_id, model_id, account_ids) = adaptive_dry_run_state().await;
        for account_id in &account_ids {
            state
                .provider_circuits
                .begin_attempt(
                    &provider_id,
                    account_id,
                    &format!("{model_id}-{account_id}"),
                )
                .unwrap()
                .finish_failure(FailureKind::ServerError, Some(503));
        }
        let before = state.provider_circuits.snapshot(&provider_id);
        assert_eq!(
            before.state,
            crate::provider_circuit::ProviderCircuitState::Open
        );

        let result = dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
            .await
            .unwrap();
        assert!(result["would_select"].is_null());
        assert!(result["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|candidate| {
                candidate["provider_circuit_available"] == false
                    && candidate["not_selected_reasons"]
                        .as_array()
                        .is_some_and(|reasons| {
                            reasons
                                .iter()
                                .any(|reason| reason == "provider_circuit_open")
                        })
            }));
        assert_eq!(
            state.provider_circuits.snapshot(&provider_id).rejects,
            before.rejects,
            "simulation must not increment provider-circuit rejects"
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_promotes_a_session_affine_target() {
        let (state, root, _, model_id, account_ids) = adaptive_dry_run_state().await;
        let route_id = state
            .registry
            .snapshot()
            .routes
            .values()
            .find(|route| route.name == "adaptive-dry-run")
            .unwrap()
            .id
            .clone();
        sqlx::query("UPDATE routes SET strategy='priority', sticky_routing=1 WHERE id=?")
            .bind(&route_id)
            .execute(&state.pool)
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        state.sticky_remember(
            "sticky-session",
            format!("{route_id}|{}|{model_id}", account_ids[1]),
        );

        let result = dry_run(
            &state,
            "adaptive-dry-run",
            &DryRunRequest {
                session: Some("sticky-session".into()),
                ..DryRunRequest::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result["would_select"], "Adaptive Model @ fallback");

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_affinity_makes_weighted_target_selection_deterministic() {
        let (state, root, _, model_id, account_ids) = adaptive_dry_run_state().await;
        let route = state
            .registry
            .snapshot()
            .routes
            .values()
            .find(|route| route.name == "adaptive-dry-run")
            .unwrap()
            .clone();
        sqlx::query("UPDATE routes SET strategy='weighted', sticky_routing=1 WHERE id=?")
            .bind(&route.id)
            .execute(&state.pool)
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        state.sticky_remember(
            "weighted-sticky-session",
            format!("{}|{}|{}", route.id, account_ids[1], model_id),
        );

        let result = dry_run(
            &state,
            "adaptive-dry-run",
            &DryRunRequest {
                session: Some("weighted-sticky-session".into()),
                ..DryRunRequest::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result["selection_mode"], "deterministic");
        assert_eq!(result["would_select"], "Adaptive Model @ fallback");

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_marks_reachable_lower_priority_account_tier_stochastic() {
        let (state, root, provider_id, _, account_ids) = adaptive_dry_run_state().await;
        let exhausted_by_quota = &account_ids[0];
        sqlx::query(
            "UPDATE accounts SET priority=1, soft_quota_usd=0.5, quota_type='daily' WHERE id=?",
        )
        .bind(exhausted_by_quota)
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query("UPDATE accounts SET priority=2 WHERE id=?")
            .bind(&account_ids[1])
            .execute(&state.pool)
            .await
            .unwrap();
        let third_account = db::insert_account(
            &state.pool,
            &provider_id,
            "third",
            "",
            "",
            2,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO usage_logs (id, request_id, ts, client_format, requested_model, status, status_code, cost_usd, cost_known, serving_account_id) VALUES (?, ?, ?, 'openai', 'adaptive-provider/adaptive-model', 'success', 200, 1.0, 1, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(db::now_iso())
        .bind(exhausted_by_quota)
        .execute(&state.pool)
        .await
        .unwrap();
        state.registry.reload(&state.pool).await.unwrap();

        let result = dry_run(
            &state,
            "adaptive-provider/adaptive-model",
            &DryRunRequest::default(),
        )
        .await
        .unwrap();
        assert_eq!(result["selection_mode"], "stochastic");
        assert_eq!(result["outcome"], "stochastic");
        assert!(result["would_select"].is_null());
        let reachable: Vec<_> = result["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|candidate| {
                candidate["account_id"] == account_ids[1]
                    || candidate["account_id"] == third_account
            })
            .collect();
        assert_eq!(reachable.len(), 2);
        assert!(reachable
            .iter()
            .all(|candidate| candidate["eligible"] == true));

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dry_run_reports_no_selection_when_all_adaptive_targets_are_saturated() {
        let (state, root, provider_id, model_id, account_ids) = adaptive_dry_run_state().await;
        let mut permits = Vec::new();

        for account_id in &account_ids {
            let key = crate::upstream_traffic::TargetKey::new(
                provider_id.clone(),
                account_id.clone(),
                model_id.clone(),
            );
            while let Ok(permit) = state
                .upstream_traffic
                .acquire(key.clone(), Duration::ZERO)
                .await
            {
                permits.push(permit);
            }
        }

        let result = dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
            .await
            .unwrap();

        assert!(result["would_select"].is_null());
        let candidates = result["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|candidate| {
            candidate["eligible"].as_bool() == Some(false)
                && candidate["adaptive_capacity_available"].as_bool() == Some(false)
                && candidate["not_selected_reasons"]
                    .as_array()
                    .is_some_and(|reasons| {
                        reasons
                            .iter()
                            .any(|reason| reason.as_str() == Some("adaptive_saturated"))
                    })
        }));

        drop(permits);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn integration_ceiling_vetoes_capability_requests_for_permissive_provider() {
        let (state, root, provider_id, model_id, _) = adaptive_dry_run_state().await;
        let ceiling = crate::plugins::types::IntegrationFeaturesV1 {
            schema_version: 1,
            streaming: false,
            tools: true,
            parallel_tools: false,
            vision: false,
            reasoning: false,
            structured_output: false,
            model_discovery: false,
            quota_probe: false,
            health_probe: false,
        };
        db::set_provider_integration_features(&state.pool, &provider_id, Some(&ceiling))
            .await
            .unwrap();
        sqlx::query("UPDATE providers SET source_plugin_id=?, source_integration_id=? WHERE id=?")
            .bind("plugin.test")
            .bind("ceiling")
            .bind(&provider_id)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE models SET capabilities=? WHERE id=?")
            .bind(
                serde_json::json!({
                    "vision": true,
                    "reasoning": true,
                    "structured_output": true,
                    "tool_calling": true,
                    "parallel_tools": true
                })
                .to_string(),
            )
            .bind(&model_id)
            .execute(&state.pool)
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        let provider = state.registry.provider(&provider_id).unwrap();
        assert!(!provider.strict());
        assert_eq!(provider.source_plugin_id.as_deref(), Some("plugin.test"));
        assert_eq!(provider.source_integration_id.as_deref(), Some("ceiling"));

        for request in [
            DryRunRequest {
                has_images: true,
                ..Default::default()
            },
            DryRunRequest {
                has_reasoning: true,
                ..Default::default()
            },
            DryRunRequest {
                has_structured_output: true,
                ..Default::default()
            },
            DryRunRequest {
                streaming: true,
                ..Default::default()
            },
            DryRunRequest {
                has_tools: true,
                has_parallel_tools: true,
                ..Default::default()
            },
        ] {
            let result = dry_run(&state, "adaptive-dry-run", &request).await.unwrap();
            assert!(
                result["would_select"].is_null(),
                "request unexpectedly routed despite explicit integration veto: {result}"
            );
            assert!(result["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .all(|candidate| {
                    candidate["not_selected_reasons"]
                        .as_array()
                        .is_some_and(|reasons| {
                            reasons.iter().any(|reason| {
                                reason.as_str() == Some("integration_feature_ceiling")
                            })
                        })
                }));
        }

        let request_bodies = [
            serde_json::json!({
                "model": "adaptive-dry-run",
                "messages": [{
                    "role": "user",
                    "content": [{
                        "type": "image_url",
                        "image_url": { "url": "https://example.com/image.png" }
                    }]
                }]
            }),
            serde_json::json!({
                "model": "adaptive-dry-run",
                "messages": [{ "role": "user", "content": "think" }],
                "reasoning_effort": "high"
            }),
            serde_json::json!({
                "model": "adaptive-dry-run",
                "messages": [{ "role": "user", "content": "return JSON" }],
                "response_format": {
                    "type": "json_schema",
                    "json_schema": { "name": "answer", "schema": { "type": "object" } }
                }
            }),
            serde_json::json!({
                "model": "adaptive-dry-run",
                "messages": [{ "role": "user", "content": "stream this" }],
                "stream": true
            }),
            serde_json::json!({
                "model": "adaptive-dry-run",
                "messages": [{ "role": "user", "content": "use tools" }],
                "parallel_tool_calls": true,
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "parameters": { "type": "object", "properties": {} }
                    }
                }]
            }),
        ];
        let mut errors = Vec::new();
        for body in request_bodies {
            let request = crate::frontends::openai::decode_request(body).unwrap();
            errors.push(
                run(
                    &state,
                    crate::frontends::FrontendFormat::OpenAi,
                    None,
                    request,
                    uuid::Uuid::new_v4().to_string(),
                    false,
                    None,
                    Vec::new(),
                )
                .await
                .expect_err("request without manual-provider credentials must fail"),
            );
        }
        assert!(
            errors.iter().all(|error| {
                error.kind == crate::types::ErrorKind::Unsupported
                    && error.message.contains("no configured target can satisfy")
            }),
            "requests must be rejected by capability eligibility, got: {errors:?}"
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn integration_input_protocol_ceiling_rejects_other_frontends_before_credentials() {
        let (state, root, provider_id, _, _) = adaptive_dry_run_state().await;
        let protocols = crate::plugins::types::IntegrationProtocolsV1 {
            input: vec!["anthropic".into()],
            upstream: vec!["openai-chat".into()],
        };
        db::set_provider_integration_protocols(&state.pool, &provider_id, Some(&protocols))
            .await
            .unwrap();
        state.registry.reload(&state.pool).await.unwrap();
        let provider = state.registry.provider(&provider_id).unwrap();
        assert!(provider.allows_input_protocol("anthropic").unwrap());
        assert!(!provider.allows_input_protocol("openai-chat").unwrap());

        let dry_run = dry_run(&state, "adaptive-dry-run", &DryRunRequest::default())
            .await
            .unwrap();
        assert!(dry_run["would_select"].is_null());
        assert!(dry_run["candidates"][0]["not_selected_reasons"]
            .as_array()
            .is_some_and(|reasons| reasons.iter().any(|reason| reason == "input_protocol")));

        let request = crate::frontends::openai::decode_request(serde_json::json!({
            "model": "adaptive-dry-run",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .unwrap();
        let error = run(
            &state,
            FrontendFormat::OpenAi,
            None,
            request,
            uuid::Uuid::new_v4().to_string(),
            false,
            None,
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, crate::types::ErrorKind::Unsupported);
        assert!(error.message.contains("protocols"), "{error}");

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    struct PluginThinkingTestAdapter {
        handles_thinking: bool,
    }

    #[async_trait::async_trait]
    impl Adapter for PluginThinkingTestAdapter {
        fn wire_format(&self) -> &'static str {
            "test-plugin"
        }

        fn handles_thinking_translation(&self) -> bool {
            self.handles_thinking
        }

        fn build_url(&self, _ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
            Ok("https://api.example.com/v1/chat".into())
        }

        fn apply_auth(
            &self,
            _ctx: &UpstreamContext<'_>,
            req: reqwest::RequestBuilder,
        ) -> Result<reqwest::RequestBuilder, UpstreamFailure> {
            Ok(req)
        }

        fn build_body(
            &self,
            _ctx: &UpstreamContext<'_>,
            req: &InternalRequest,
        ) -> Result<Value, UpstreamFailure> {
            let request: Value =
                serde_json::from_str(&crate::plugins::adapter::request_to_json(req)).unwrap();
            let level = request
                .pointer("/thinking/level")
                .and_then(Value::as_str)
                .ok_or_else(|| UpstreamFailure {
                    kind: FailureKind::BadRequest,
                    status: None,
                    retry_after_secs: None,
                    message: "missing canonical thinking level".into(),
                    quota_reset_at: None,
                })?;
            Ok(serde_json::json!({"reasoning_effort": level}))
        }

        fn classify_error(
            &self,
            status: u16,
            _body: &str,
            _headers: &reqwest::header::HeaderMap,
        ) -> UpstreamFailure {
            UpstreamFailure {
                kind: FailureKind::BadRequest,
                status: Some(status),
                retry_after_secs: None,
                message: "test plugin error".into(),
                quota_reset_at: None,
            }
        }

        fn parse_stream_chunk(&self, _data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
            Ok(Vec::new())
        }

        fn parse_full_response(&self, _body: &Value) -> Result<Vec<StreamEvent>, UpstreamFailure> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn adaptive_anthropic_passthrough_normalizes_legacy_manual_thinking() {
        let mut p = provider(serde_json::json!({}));
        p.wire_format = "anthropic".into();
        let m = model(serde_json::json!({
            "mode": "adaptive",
            "levels": {"high": "high"},
            "level_field": "output_config.effort"
        }));
        let raw = serde_json::json!({
            "model": "sonnet",
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}],
            "thinking": {"type": "enabled", "budget_tokens": 32000},
            "vendor_extension": {"keep": true}
        })
        .to_string();
        let mut req =
            crate::frontends::anthropic::decode_request(serde_json::from_str(&raw).unwrap())
                .unwrap();
        req.raw_body = Some(raw);
        assert_eq!(req.thinking, Some(crate::types::ThinkingLevel::High));

        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: Some("acc_test"),
            session_context: None,
            credential: "sk-ant-api03-test".into(),
        };
        let adapter = crate::adapters::anthropic::AnthropicAdapter::new();

        let body = build_upstream_body(&adapter, &ctx, &req, true).unwrap();

        assert_eq!(body["model"], "upstream");
        assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
        assert_eq!(body["output_config"]["effort"], "high");
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert_eq!(body["vendor_extension"]["keep"], true);
    }

    #[test]
    fn adaptive_anthropic_passthrough_preserves_native_adaptive_max_effort() {
        let mut p = provider(serde_json::json!({}));
        p.wire_format = "anthropic".into();
        let m = model(serde_json::json!({
            "mode": "adaptive",
            "levels": {"max": "max"},
            "level_field": "output_config.effort"
        }));
        let raw = serde_json::json!({
            "model": "sonnet",
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}],
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "max"}
        })
        .to_string();
        let mut req =
            crate::frontends::anthropic::decode_request(serde_json::from_str(&raw).unwrap())
                .unwrap();
        req.raw_body = Some(raw);
        assert_eq!(req.thinking, Some(crate::types::ThinkingLevel::Max));

        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: Some("acc_test"),
            session_context: None,
            credential: "sk-ant-api03-test".into(),
        };
        let adapter = crate::adapters::anthropic::AnthropicAdapter::new();

        let body = build_upstream_body(&adapter, &ctx, &req, true).unwrap();

        assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
        assert_eq!(body["output_config"]["effort"], "max");
        assert!(body["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn adaptive_anthropic_passthrough_preserves_omitted_effort() {
        let mut p = provider(serde_json::json!({}));
        p.wire_format = "anthropic".into();
        let m = model(serde_json::json!({
            "mode": "adaptive",
            "levels": {"high": "high"},
            "level_field": "output_config.effort"
        }));
        let raw = serde_json::json!({
            "model": "opus",
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}],
            "thinking": {"type": "adaptive"}
        })
        .to_string();
        let mut req =
            crate::frontends::anthropic::decode_request(serde_json::from_str(&raw).unwrap())
                .unwrap();
        req.raw_body = Some(raw);
        assert_eq!(req.thinking, Some(crate::types::ThinkingLevel::Default));

        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: Some("acc_test"),
            session_context: None,
            credential: "sk-ant-api03-test".into(),
        };
        let adapter = crate::adapters::anthropic::AnthropicAdapter::new();

        let body = build_upstream_body(&adapter, &ctx, &req, true).unwrap();

        assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn adaptive_anthropic_passthrough_normalizes_without_matching_level() {
        let mut p = provider(serde_json::json!({}));
        p.wire_format = "anthropic".into();
        let m = model(serde_json::json!({
            "mode": "adaptive",
            "levels": {},
            "level_field": "output_config.effort"
        }));
        let raw = serde_json::json!({
            "model": "sonnet",
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}],
            "thinking": {"type": "enabled", "budget_tokens": 32000}
        })
        .to_string();
        let mut req =
            crate::frontends::anthropic::decode_request(serde_json::from_str(&raw).unwrap())
                .unwrap();
        req.raw_body = Some(raw);

        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: Some("acc_test"),
            session_context: None,
            credential: "sk-ant-api03-test".into(),
        };
        let adapter = crate::adapters::anthropic::AnthropicAdapter::new();

        let body = build_upstream_body(&adapter, &ctx, &req, true).unwrap();

        assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn adaptive_anthropic_passthrough_rejects_off_without_model_mapping() {
        let mut p = provider(serde_json::json!({}));
        p.wire_format = "anthropic".into();
        let m = model(serde_json::json!({
            "mode": "adaptive",
            "levels": {"off": "low"},
            "level_field": "output_config.effort"
        }));
        let raw = serde_json::json!({
            "model": "opus",
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}],
            "thinking": {"type": "disabled"}
        })
        .to_string();
        let mut req =
            crate::frontends::anthropic::decode_request(serde_json::from_str(&raw).unwrap())
                .unwrap();
        req.raw_body = Some(raw);

        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: Some("acc_test"),
            session_context: None,
            credential: "sk-ant-api03-test".into(),
        };
        let adapter = crate::adapters::anthropic::AnthropicAdapter::new();

        let failure = build_upstream_body(&adapter, &ctx, &req, true).unwrap_err();

        assert_eq!(failure.kind, FailureKind::BadRequest);
        assert!(failure.message.contains("thinking level 'off'"));
    }

    #[test]
    fn adaptive_anthropic_passthrough_preserves_explicit_thinking_off() {
        let mut p = provider(serde_json::json!({}));
        p.wire_format = "anthropic".into();
        let m = model(serde_json::json!({
            "mode": "adaptive",
            "levels": {"off": {"thinking.type": "disabled"}},
            "level_field": "output_config.effort"
        }));
        let raw = serde_json::json!({
            "model": "sonnet",
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}],
            "thinking": {"type": "disabled", "budget_tokens": 32000}
        })
        .to_string();
        let mut req =
            crate::frontends::anthropic::decode_request(serde_json::from_str(&raw).unwrap())
                .unwrap();
        req.raw_body = Some(raw);
        assert_eq!(req.thinking, Some(crate::types::ThinkingLevel::Off));

        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: Some("acc_test"),
            session_context: None,
            credential: "sk-ant-api03-test".into(),
        };
        let adapter = crate::adapters::anthropic::AnthropicAdapter::new();

        let body = build_upstream_body(&adapter, &ctx, &req, true).unwrap();

        assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn anthropic_error_passthrough_is_sanitized_and_keeps_retry_metadata() {
        let adapter = crate::adapters::anthropic::AnthropicAdapter::new();
        let failure = UpstreamFailure {
            kind: FailureKind::ServerError,
            status: Some(529),
            retry_after_secs: Some(2),
            message: "overloaded".into(),
            quota_reset_at: None,
        };
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after-ms", "1500".parse().unwrap());
        headers.insert("x-should-retry", "true".parse().unwrap());
        headers.insert(
            "anthropic-ratelimit-unified-remaining",
            "0".parse().unwrap(),
        );
        headers.insert("request-id", "req_upstream_123".parse().unwrap());
        headers.insert("set-cookie", "do-not-forward=true".parse().unwrap());

        let error = preserve_anthropic_error(
            ProxyError::upstream("overloaded"),
            FrontendFormat::Anthropic,
            &adapter,
            &failure,
            529,
            &headers,
            Some(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"busy","debug":"drop"},"request_id":"req_upstream_123","internal":"drop"}"#,
            ),
        );

        assert_eq!(error.http_status_override, Some(529));
        assert_eq!(
            error.body_override.as_ref().unwrap(),
            &serde_json::json!({
                "type": "error",
                "error": { "type": "overloaded_error", "message": "busy" },
                "request_id": "req_upstream_123"
            })
        );
        assert!(error
            .headers
            .iter()
            .any(|(name, value)| name == "x-should-retry" && value == "true"));
        assert!(error
            .headers
            .iter()
            .any(|(name, _)| name == "anthropic-ratelimit-unified-remaining"));
        assert!(!error.headers.iter().any(|(name, _)| name == "set-cookie"));
    }

    #[test]
    fn anthropic_upstream_auth_failures_stay_internal() {
        let adapter = crate::adapters::anthropic::AnthropicAdapter::new();
        let failure = UpstreamFailure {
            kind: FailureKind::AuthError,
            status: Some(401),
            retry_after_secs: None,
            message: "bad upstream credential".into(),
            quota_reset_at: None,
        };
        let error = preserve_anthropic_error(
            ProxyError::upstream("upstream auth failed"),
            FrontendFormat::Anthropic,
            &adapter,
            &failure,
            401,
            &reqwest::header::HeaderMap::new(),
            Some(
                r#"{"type":"error","error":{"type":"authentication_error","message":"credential detail"}}"#,
            ),
        );
        assert_eq!(error.http_status_override, None);
        assert_eq!(error.body_override, None);
    }

    #[test]
    fn token_count_estimate_includes_tool_schema() {
        let base = request();
        let base_count = estimated_input_tokens(&base);

        let mut with_tool = base;
        with_tool.tools.push(crate::types::ToolDef {
            name: "read_file".into(),
            description: Some("Read a file from disk".into()),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
            defer_loading: None,
        });

        assert!(estimated_input_tokens(&with_tool) > base_count);
    }

    #[test]
    fn tool_stream_indexes_are_canonical_across_chunk_local_resets() {
        let mut state = ToolStreamState::new("req_test");
        let first = state.normalize(vec![
            StreamEvent::TextDelta("a".into()),
            StreamEvent::ToolCallStart {
                index: 3,
                id: Some("call_a".into()),
                name: "alpha".into(),
                signature: None,
            },
            StreamEvent::ToolCallArgsDelta {
                index: 3,
                args: "{}".into(),
            },
        ]);
        let second = state.normalize(vec![
            StreamEvent::TextDelta("b".into()),
            StreamEvent::ToolCallStart {
                index: 0,
                id: None,
                name: "beta".into(),
                signature: None,
            },
            StreamEvent::ToolCallArgsDelta {
                index: 0,
                args: "{\"x\":1}".into(),
            },
        ]);

        assert!(matches!(
            &first[1],
            StreamEvent::ToolCallStart { index: 0, id: Some(id), .. } if id == "call_a"
        ));
        assert!(matches!(
            &first[2],
            StreamEvent::ToolCallArgsDelta { index: 0, .. }
        ));
        assert!(matches!(
            &second[1],
            StreamEvent::ToolCallStart { index: 1, id: Some(id), .. }
                if id == "call_reqtest_1"
        ));
        assert!(matches!(
            &second[2],
            StreamEvent::ToolCallArgsDelta { index: 1, .. }
        ));
    }

    #[test]
    fn generated_tool_call_id_keeps_signature_attached() {
        let mut state = ToolStreamState::new("req_test");
        let out = state.normalize(vec![StreamEvent::ToolCallStart {
            index: 0,
            id: None,
            name: "bash".into(),
            signature: Some("SIG".into()),
        }]);
        assert!(matches!(
            &out[0],
            StreamEvent::ToolCallStart { id: Some(id), signature: Some(sig), .. }
                if id == "call_reqtest_0" && sig == "SIG"
        ));
    }

    #[test]
    fn explicit_tool_call_id_keeps_signature_attached() {
        let mut state = ToolStreamState::new("req_test");
        let out = state.normalize(vec![StreamEvent::ToolCallStart {
            index: 0,
            id: Some("call_123".into()),
            name: "bash".into(),
            signature: Some("SIG".into()),
        }]);
        assert!(matches!(
            &out[0],
            StreamEvent::ToolCallStart { id: Some(id), signature: Some(sig), .. }
                if id == "call_123" && sig == "SIG"
        ));
    }

    #[test]
    fn signature_only_then_function_call_attaches_pending_signature() {
        let mut state = ToolStreamState::new("req_test");
        let first = state.normalize(vec![StreamEvent::ThinkingDelta {
            block_index: None,
            text: String::new(),
            signature: Some("SIG".into()),
        }]);
        // The raw thinking event is passed through unchanged.
        assert!(matches!(
            &first[0],
            StreamEvent::ThinkingDelta { text, signature: Some(sig), .. }
                if text.is_empty() && sig == "SIG"
        ));

        let second = state.normalize(vec![StreamEvent::ToolCallStart {
            index: 0,
            id: None,
            name: "bash".into(),
            signature: None,
        }]);
        assert!(matches!(
            &second[0],
            StreamEvent::ToolCallStart { id: Some(id), signature: Some(sig), .. }
                if id == "call_reqtest_0" && sig == "SIG"
        ));
    }

    #[tokio::test]
    async fn signed_content_marker_reaches_opaque_capture() {
        let dir = std::env::temp_dir().join(format!(
            "kinetix-pipeline-opaque-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let store = OpaqueStateStore::new(pool, Arc::new(crate::crypto::Crypto::new(&[11u8; 32])));
        let scope = OpaqueClientScope::internal();
        let target = OpaqueStateTarget {
            kind: crate::opaque_state::OpaqueStateKind::GeminiThoughtSignature,
            provider_id: "provider_test".into(),
            family: "gemini".into(),
            producer: "plugin:plugin.test:gemini-thought-signature:v1".into(),
            model_id: "gemini-3.1-flash-lite".into(),
        };
        let ctx = OpaqueCaptureContext {
            scope: scope.clone(),
            session: None,
            target: target.clone(),
        };

        let mut thinking_state = ToolStreamState::new("req_thinking");
        let thinking_events = thinking_state.normalize(vec![
            StreamEvent::ThinkingDelta {
                block_index: None,
                text: "reasoning...".into(),
                signature: None,
            },
            StreamEvent::ThinkingDelta {
                block_index: None,
                text: String::new(),
                signature: Some("SIG_THINK".into()),
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: Some("call_think".into()),
                name: "bash".into(),
                signature: None,
            },
        ]);
        assert!(matches!(
            &thinking_events[2],
            StreamEvent::ToolCallStart {
                signature: Some(signature),
                ..
            } if signature == "SIG_THINK"
        ));
        capture_opaque_state(&thinking_events, &ctx, &store);
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&target), None, "call_think", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_THINK".into())
        );

        let mut text_state = ToolStreamState::new("req_text");
        let text_events = text_state.normalize(vec![
            StreamEvent::TextDelta("visible response".into()),
            StreamEvent::ThinkingDelta {
                block_index: None,
                text: String::new(),
                signature: Some("SIG_TEXT".into()),
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: Some("call_text".into()),
                name: "read".into(),
                signature: None,
            },
        ]);
        assert!(matches!(
            &text_events[2],
            StreamEvent::ToolCallStart {
                signature: Some(signature),
                ..
            } if signature == "SIG_TEXT"
        ));
        capture_opaque_state(&text_events, &ctx, &store);
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&target), None, "call_text", "read")
                .await,
            OpaqueLookupResult::Compatible("SIG_TEXT".into())
        );
    }

    #[test]
    fn pending_signature_not_copied_to_parallel_calls() {
        let mut state = ToolStreamState::new("req_test");
        state.normalize(vec![StreamEvent::ThinkingDelta {
            block_index: None,
            text: String::new(),
            signature: Some("SIG_A".into()),
        }]);
        let out = state.normalize(vec![
            StreamEvent::ToolCallStart {
                index: 0,
                id: Some("A".into()),
                name: "read".into(),
                signature: None,
            },
            StreamEvent::ToolCallStart {
                index: 1,
                id: Some("B".into()),
                name: "bash".into(),
                signature: None,
            },
        ]);
        assert!(matches!(
            &out[0],
            StreamEvent::ToolCallStart { id: Some(id), signature: Some(sig), .. }
                if id == "A" && sig == "SIG_A"
        ));
        assert!(matches!(
            &out[1],
            StreamEvent::ToolCallStart { id: Some(id), signature: None, .. }
                if id == "B"
        ));
    }

    #[test]
    fn explicit_tool_call_signature_wins_over_pending() {
        let mut state = ToolStreamState::new("req_test");
        state.normalize(vec![StreamEvent::ThinkingDelta {
            block_index: None,
            text: String::new(),
            signature: Some("PENDING".into()),
        }]);
        let out = state.normalize(vec![StreamEvent::ToolCallStart {
            index: 0,
            id: Some("A".into()),
            name: "read".into(),
            signature: Some("EXPLICIT".into()),
        }]);
        assert!(matches!(
            &out[0],
            StreamEvent::ToolCallStart { signature: Some(sig), .. } if sig == "EXPLICIT"
        ));
    }

    #[test]
    fn pending_signature_cleared_by_text_delta() {
        // A stale signature-only part followed by ordinary text (and then an
        // unrelated tool call) must not leak the earlier signature onward.
        let mut state = ToolStreamState::new("req_test");
        state.normalize(vec![StreamEvent::ThinkingDelta {
            block_index: None,
            text: String::new(),
            signature: Some("STALE".into()),
        }]);
        state.normalize(vec![StreamEvent::TextDelta("hello".into())]);
        let out = state.normalize(vec![StreamEvent::ToolCallStart {
            index: 0,
            id: Some("later".into()),
            name: "read".into(),
            signature: None,
        }]);
        assert!(matches!(
            &out[0],
            StreamEvent::ToolCallStart {
                signature: None,
                ..
            }
        ));
    }

    #[test]
    fn pending_signature_cleared_by_refusal_delta() {
        let mut state = ToolStreamState::new("req_test");
        state.normalize(vec![StreamEvent::ThinkingDelta {
            block_index: None,
            text: String::new(),
            signature: Some("STALE".into()),
        }]);
        let refusal = state.normalize(vec![StreamEvent::RefusalDelta("no".into())]);
        assert!(matches!(&refusal[0], StreamEvent::RefusalDelta(text) if text == "no"));
        let out = state.normalize(vec![StreamEvent::ToolCallStart {
            index: 0,
            id: Some("later".into()),
            name: "read".into(),
            signature: None,
        }]);
        assert!(matches!(
            &out[0],
            StreamEvent::ToolCallStart {
                signature: None,
                ..
            }
        ));
    }

    #[test]
    fn pending_signature_cleared_by_real_thinking_delta() {
        let mut state = ToolStreamState::new("req_test");
        state.normalize(vec![StreamEvent::ThinkingDelta {
            block_index: None,
            text: String::new(),
            signature: Some("STALE".into()),
        }]);
        state.normalize(vec![StreamEvent::ThinkingDelta {
            block_index: None,
            text: "real reasoning".into(),
            signature: None,
        }]);
        let out = state.normalize(vec![StreamEvent::ToolCallStart {
            index: 0,
            id: Some("later".into()),
            name: "read".into(),
            signature: None,
        }]);
        assert!(matches!(
            &out[0],
            StreamEvent::ToolCallStart {
                signature: None,
                ..
            }
        ));
    }

    #[test]
    fn portability_strip_updates_canonical_and_raw_request_state() {
        let mut req = request();
        req.messages = vec![crate::types::Message {
            role: crate::types::Role::Assistant,
            parts: vec![
                crate::types::Part::Thinking {
                    text: "hidden".into(),
                    signature: Some("sig".into()),
                },
                crate::types::Part::Text("answer".into()),
            ],
        }];
        req.raw_body = Some(
            serde_json::json!({
                "model":"route",
                "messages":[{
                    "role":"assistant",
                    "content":[
                        {"type":"thinking","thinking":"hidden","signature":"sig"},
                        {"type":"text","text":"answer"}
                    ]
                }]
            })
            .to_string(),
        );

        let route = route(serde_json::json!({}));
        let target = target();
        let mut trace = RouteTrace::new("req_test".into(), "route".into());
        let report = OpaqueHydrationReport::default();
        apply_portability(
            &mut req, &route, &target, true, &report, None, false, &mut trace,
        )
        .unwrap();

        assert!(!request_has_nonportable_inline_state(&req, false, true));
        assert!(!trace.warnings.is_empty());
        let raw: Value = serde_json::from_str(req.raw_body.as_deref().unwrap()).unwrap();
        assert_eq!(raw["messages"][0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(raw["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn portability_reject_refuses_opaque_state() {
        let mut req = request();
        req.messages = vec![crate::types::Message {
            role: crate::types::Role::Assistant,
            parts: vec![crate::types::Part::Thinking {
                text: "hidden".into(),
                signature: Some("sig".into()),
            }],
        }];
        let mut route = route(serde_json::json!({}));
        route.portability_policy = "reject".into();
        let mut trace = RouteTrace::new("req_test".into(), "route".into());
        let report = OpaqueHydrationReport::default();
        assert!(apply_portability(
            &mut req,
            &route,
            &target(),
            true,
            &report,
            None,
            false,
            &mut trace,
        )
        .is_err());
    }

    #[test]
    fn portability_strip_handles_stored_nonportable_state() {
        // No inline state, but a stored record exists that this target cannot
        // carry: strip_with_warning must warn without touching the request.
        let mut req = request();
        let route = route(serde_json::json!({}));
        let target = target();
        let mut trace = RouteTrace::new("req_test".into(), "route".into());
        let report = OpaqueHydrationReport {
            incompatible: 1,
            ..Default::default()
        };
        apply_portability(
            &mut req, &route, &target, false, &report, None, false, &mut trace,
        )
        .unwrap();
        assert_eq!(trace.warnings.len(), 1);
    }

    #[test]
    fn portability_placeholder_translates_incompatible_stored_calls() {
        // A stored signature exists for a different model in the same family:
        // the target cannot carry the real value. With a documented placeholder
        // the incompatible historical call is translated rather than left
        // unsigned (which the provider would reject), while a call with no
        // stored state is never given a signature it has no state for.
        let mut req = request();
        req.messages = vec![crate::types::Message {
            role: crate::types::Role::Assistant,
            parts: vec![
                crate::types::Part::ToolCall {
                    id: Some("call_incompatible".into()),
                    name: "bash".into(),
                    arguments: "{}".into(),
                    signature: None,
                },
                crate::types::Part::ToolCall {
                    id: Some("call_unrelated".into()),
                    name: "read".into(),
                    arguments: "{}".into(),
                    signature: None,
                },
            ],
        }];
        let route = route(serde_json::json!({}));
        let target = target();
        let mut trace = RouteTrace::new("req_test".into(), "route".into());
        let report = OpaqueHydrationReport {
            incompatible: 1,
            incompatible_ids: vec!["call_incompatible".into()],
            ..Default::default()
        };

        apply_portability(
            &mut req,
            &route,
            &target,
            false,
            &report,
            Some("PLACEHOLDER"),
            false,
            &mut trace,
        )
        .unwrap();

        assert!(matches!(
            &req.messages[0].parts[0],
            crate::types::Part::ToolCall { signature: Some(sig), .. } if sig == "PLACEHOLDER"
        ));
        assert!(matches!(
            &req.messages[0].parts[1],
            crate::types::Part::ToolCall {
                signature: None,
                ..
            }
        ));
        assert_eq!(trace.warnings.len(), 1);
        assert!(
            trace.warnings[0].contains("placeholder"),
            "a substituted placeholder must be reported, got {:?}",
            trace.warnings[0]
        );
    }

    #[test]
    fn portability_without_placeholder_strips_incompatible_stored_calls() {
        // No adapter placeholder (e.g. an OpenAI target): the old behaviour
        // stands, the incompatible call is stripped and the warning does not
        // claim a substitution happened.
        let mut req = request();
        req.messages = vec![crate::types::Message {
            role: crate::types::Role::Assistant,
            parts: vec![crate::types::Part::ToolCall {
                id: Some("call_incompatible".into()),
                name: "bash".into(),
                arguments: "{}".into(),
                signature: None,
            }],
        }];
        let route = route(serde_json::json!({}));
        let target = target();
        let mut trace = RouteTrace::new("req_test".into(), "route".into());
        let report = OpaqueHydrationReport {
            incompatible: 1,
            incompatible_ids: vec!["call_incompatible".into()],
            ..Default::default()
        };

        apply_portability(
            &mut req, &route, &target, false, &report, None, false, &mut trace,
        )
        .unwrap();

        assert!(matches!(
            &req.messages[0].parts[0],
            crate::types::Part::ToolCall {
                signature: None,
                ..
            }
        ));
        assert_eq!(trace.warnings.len(), 1);
        assert!(!trace.warnings[0].contains("placeholder"));
    }

    #[test]
    fn portability_reject_handles_stored_nonportable_state() {
        let mut req = request();
        let mut route = route(serde_json::json!({}));
        route.portability_policy = "reject".into();
        let target = target();
        let mut trace = RouteTrace::new("req_test".into(), "route".into());
        let report = OpaqueHydrationReport {
            incompatible: 1,
            ..Default::default()
        };
        assert!(apply_portability(
            &mut req, &route, &target, false, &report, None, false, &mut trace,
        )
        .is_err());
    }

    #[test]
    fn portability_noop_when_no_opaque_state() {
        let mut req = request();
        let route = route(serde_json::json!({}));
        let target = target();
        let mut trace = RouteTrace::new("req_test".into(), "route".into());
        let report = OpaqueHydrationReport::default();
        apply_portability(
            &mut req, &route, &target, false, &report, None, false, &mut trace,
        )
        .unwrap();
        assert!(trace.warnings.is_empty());
    }

    #[test]
    fn hydrate_opaque_state_restores_signature_and_preserves_explicit_one() {
        let mut req = request();
        req.messages = vec![crate::types::Message {
            role: crate::types::Role::Assistant,
            parts: vec![
                crate::types::Part::ToolCall {
                    id: Some("call_a".into()),
                    name: "bash".into(),
                    arguments: "{}".into(),
                    signature: None,
                },
                crate::types::Part::ToolCall {
                    id: Some("call_b".into()),
                    name: "read".into(),
                    arguments: "{}".into(),
                    signature: Some("EXPLICIT".into()),
                },
            ],
        }];
        let report = OpaqueHydrationReport {
            restored: 1,
            restorations: vec![("call_a".into(), "RESTORED".into())],
            ..Default::default()
        };
        hydrate_opaque_state(&mut req, &report);

        let parts = &req.messages[0].parts;
        assert!(matches!(
            &parts[0],
            crate::types::Part::ToolCall { signature: Some(sig), .. } if sig == "RESTORED"
        ));
        // An explicit client/canonical signature is never overwritten.
        assert!(matches!(
            &parts[1],
            crate::types::Part::ToolCall { signature: Some(sig), .. } if sig == "EXPLICIT"
        ));
    }

    #[test]
    fn portability_strips_inline_but_hydrates_compatible_afterwards() {
        // Simulates the OpenAI->Gemini translated path: recent tool calls carry
        // no inline signature (they were recovered from the store), while a
        // stray inline thinking block is stripped. The recovered signature must
        // survive because hydration happens after the strip.
        let mut req = request();
        req.messages = vec![crate::types::Message {
            role: crate::types::Role::Assistant,
            parts: vec![
                crate::types::Part::Thinking {
                    text: "hidden".into(),
                    signature: Some("thinking-sig".into()),
                },
                crate::types::Part::ToolCall {
                    id: Some("call_a".into()),
                    name: "bash".into(),
                    arguments: "{}".into(),
                    signature: None,
                },
            ],
        }];
        let route = route(serde_json::json!({}));
        let target = target();
        let mut trace = RouteTrace::new("req_test".into(), "route".into());
        let report = OpaqueHydrationReport {
            restored: 1,
            restorations: vec![("call_a".into(), "RECOVERED".into())],
            ..Default::default()
        };

        let inline = request_has_nonportable_inline_state(&req, false, true);
        assert!(inline);
        apply_portability(
            &mut req, &route, &target, inline, &report, None, false, &mut trace,
        )
        .unwrap();
        hydrate_opaque_state(&mut req, &report);

        assert_eq!(req.messages[0].parts.len(), 1);
        assert!(matches!(
            &req.messages[0].parts[0],
            crate::types::Part::ToolCall { signature: Some(sig), .. } if sig == "RECOVERED"
        ));
    }

    #[test]
    fn discovered_plugin_reasoning_reaches_adapter_translation_without_core_map() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "reasoning": {
                "supported": true,
                "mode": "level",
                "levels": ["low", "high"],
                "default": "high",
                "can_disable": false
            }
        });
        let capability =
            crate::adapters::normalize_plugin_reasoning_capability_v1(&metadata).unwrap();
        assert!(crate::adapters::thinking_map_for_reasoning_with_wire(
            &capability,
            crate::types::WireFormat::Plugin,
        )
        .is_none());

        let mut target = target();
        target.provider.wire_format = "plugin".into();
        target.provider.wire_plugin = "plugin:test/provider-adapter".into();
        target.model.discovery = serde_json::json!({
            "reasoning_capability": capability
        })
        .to_string();

        let registry = crate::adapters::AdapterRegistry::new();
        registry.register_plugin(
            "plugin:test/provider-adapter",
            Arc::new(PluginThinkingTestAdapter {
                handles_thinking: true,
            }),
        );
        let adapter = registry.for_provider(&target.provider);
        assert!(adapter.handles_thinking_translation());

        let mut req = request();
        req.thinking = Some(crate::types::ThinkingLevel::High);

        assert!(check_thinking_translation(&target, &req).is_err());
        assert!(
            check_thinking_translation_for_adapter(adapter.as_ref(), &target, &req).is_ok(),
            "plugin-owned thinking must reach the adapter even without a core ThinkingMap"
        );

        let ctx = UpstreamContext {
            provider: &target.provider,
            model: &target.model,
            account_id: Some(target.account.id.as_str()),
            session_context: None,
            credential: "test".into(),
        };
        let body = build_upstream_body(adapter.as_ref(), &ctx, &req, false).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn plugin_adapter_without_thinking_opt_in_keeps_core_validation() {
        let mut target = target();
        target.provider.wire_format = "plugin".into();
        target.provider.wire_plugin = "plugin:test/provider-adapter".into();
        let registry = crate::adapters::AdapterRegistry::new();
        registry.register_plugin(
            "plugin:test/provider-adapter",
            Arc::new(PluginThinkingTestAdapter {
                handles_thinking: false,
            }),
        );
        let adapter = registry.for_provider(&target.provider);
        assert!(!adapter.handles_thinking_translation());

        let mut req = request();
        req.thinking = Some(crate::types::ThinkingLevel::High);

        let error =
            check_thinking_translation_for_adapter(adapter.as_ref(), &target, &req).unwrap_err();
        assert!(error.message.contains("has no executable mapping"));
    }

    #[test]
    fn discovered_v1_openai_disable_is_executable_and_emits_none() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "transport": {"format": "openai"},
            "reasoning": {
                "supported": true,
                "mode": "level",
                "levels": ["low", "high"],
                "can_disable": true
            }
        });
        let capability =
            crate::adapters::normalize_plugin_reasoning_capability_v1(&metadata).unwrap();
        let map = crate::adapters::thinking_map_for_reasoning_with_wire(
            &capability,
            crate::types::WireFormat::Openai,
        )
        .unwrap();
        assert_eq!(map.levels.get("off"), Some(&serde_json::json!("none")));

        let mut target = target();
        target.model = model(serde_json::to_value(&map).unwrap());
        let mut req = request();
        req.thinking = Some(crate::types::ThinkingLevel::Off);

        assert!(check_thinking_translation(&target, &req).is_ok());

        let ctx = UpstreamContext {
            provider: &target.provider,
            model: &target.model,
            account_id: Some(target.account.id.as_str()),
            session_context: None,
            credential: "test".into(),
        };
        let adapter = crate::adapters::openai::OpenAiAdapter::new();
        let body = adapter.build_body(&ctx, &req).unwrap();
        assert_eq!(body["reasoning_effort"], "none");
    }

    #[test]
    fn translated_thinking_requires_an_explicit_model_mapping() {
        let mut req = request();
        let mut target = target();

        req.thinking = Some(crate::types::ThinkingLevel::High);
        assert!(check_thinking_translation(&target, &req).is_err());

        target.model = model(serde_json::json!({
            "levels": {
                "minimal": {"reasoning_effort": "minimal"},
                "low": {"reasoning_effort": "low"},
                "medium": {"reasoning_effort": "medium"},
                "high": {"reasoning_effort": "high"},
                "xhigh": {"reasoning_effort": "xhigh"},
                "max": {"reasoning_effort": "max"}
            }
        }));
        for level in [
            crate::types::ThinkingLevel::Minimal,
            crate::types::ThinkingLevel::Low,
            crate::types::ThinkingLevel::Medium,
            crate::types::ThinkingLevel::High,
            crate::types::ThinkingLevel::XHigh,
            crate::types::ThinkingLevel::Max,
        ] {
            req.thinking = Some(level);
            assert!(check_thinking_translation(&target, &req).is_ok());
        }

        req.thinking = Some(crate::types::ThinkingLevel::High);
        target.model = model(serde_json::json!({
            "levels": {"high": null}
        }));
        assert!(check_thinking_translation(&target, &req).is_err());

        target.model = model(serde_json::json!({
            "levels": {"high": 4096}
        }));
        assert!(check_thinking_translation(&target, &req).is_err());

        target.model = model(serde_json::json!({
            "levels": {"high": 4096},
            "budget_field": "thinking.budget_tokens"
        }));
        assert!(check_thinking_translation(&target, &req).is_ok());

        req.thinking = Some(crate::types::ThinkingLevel::Default);
        target.model = model(serde_json::json!({
            "mode": "adaptive",
            "levels": {},
            "level_field": "output_config.effort"
        }));
        target.provider.wire_format = "anthropic".into();
        assert!(check_thinking_translation(&target, &req).is_ok());

        req.thinking = Some(crate::types::ThinkingLevel::Off);
        target.model = model(serde_json::json!({}));
        assert!(check_thinking_translation(&target, &req).is_err());

        target.model = model(serde_json::json!({
            "mode": "adaptive",
            "levels": {"off": "low"},
            "level_field": "output_config.effort"
        }));
        assert!(check_thinking_translation(&target, &req).is_err());

        target.model = model(serde_json::json!({
            "mode": "adaptive",
            "levels": {"off": {"thinking.type": "disabled"}},
            "level_field": "output_config.effort"
        }));
        assert!(check_thinking_translation(&target, &req).is_ok());
    }

    #[test]
    fn route_fallback_triggers_are_executable() {
        let r = route(serde_json::json!({
            "on429": false,
            "onQuota": false,
            "on5xx": false,
            "onTimeout": false
        }));
        assert!(!route_allows_fallback(Some(&r), FailureKind::RateLimit));
        assert!(!route_allows_fallback(
            Some(&r),
            FailureKind::QuotaExhausted
        ));
        assert!(!route_allows_fallback(Some(&r), FailureKind::ServerError));
        assert!(!route_allows_fallback(
            Some(&r),
            FailureKind::ConnectionError
        ));
        assert!(!route_allows_fallback(Some(&r), FailureKind::Timeout));
        assert!(route_allows_fallback(Some(&r), FailureKind::AuthError));
        assert!(route_allows_fallback(Some(&r), FailureKind::TargetError));
        assert!(!route_allows_fallback(Some(&r), FailureKind::BadRequest));
    }

    #[test]
    fn missing_fallback_trigger_defaults_to_enabled() {
        let r = route(serde_json::json!({}));
        assert!(route_allows_fallback(Some(&r), FailureKind::RateLimit));
        assert!(route_allows_fallback(Some(&r), FailureKind::QuotaExhausted));
        assert!(route_allows_fallback(Some(&r), FailureKind::ServerError));
        assert!(route_allows_fallback(Some(&r), FailureKind::Timeout));
    }

    #[test]
    fn direct_target_error_does_not_rotate_credentials() {
        assert!(!route_allows_fallback(None, FailureKind::TargetError));
        assert!(route_allows_fallback(None, FailureKind::AuthError));
    }

    #[test]
    fn same_format_chat_passthrough_preserves_max_completion_tokens() {
        let p = provider(Value::Null);
        let m = model(serde_json::json!({}));
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "k".into(),
        };
        let mut req = request();
        req.raw_body = Some(
            serde_json::json!({
                "model": "route",
                "max_completion_tokens": 512
            })
            .to_string(),
        );
        req.params.max_tokens = Some(512);

        let body =
            build_upstream_body(&crate::adapters::openai::OpenAiAdapter, &ctx, &req, true).unwrap();

        assert_eq!(body["max_completion_tokens"], 512);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn anthropic_passthrough_route_token_aliases_update_messages_max_tokens() {
        let mut p = provider(Value::Null);
        p.wire_format = "anthropic".into();
        let m = model(serde_json::json!({}));
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "k".into(),
        };

        for alias in ["max_output_tokens", "max_completion_tokens"] {
            let mut req = request();
            req.raw_body = Some(
                serde_json::json!({
                    "model": "route",
                    "messages": [{"role":"user","content":"hello"}],
                    "max_tokens": 4096
                })
                .to_string(),
            );
            req.params.max_tokens = Some(4096);
            apply_target_overrides(&mut req, &serde_json::json!({(alias): 512})).unwrap();

            let body = build_upstream_body(
                &crate::adapters::anthropic::AnthropicAdapter,
                &ctx,
                &req,
                true,
            )
            .unwrap();

            assert_eq!(body["max_tokens"], 512, "override alias: {alias}");
            assert!(body.get("max_output_tokens").is_none());
            assert!(body.get("max_completion_tokens").is_none());
        }
    }

    #[test]
    fn target_parameter_overrides_update_canonical_and_passthrough_request() {
        let mut req = request();
        apply_target_overrides(
            &mut req,
            &serde_json::json!({
                "temperature": 0.7,
                "top_p": 0.8,
                "max_tokens": 512,
                "stop": ["END"],
                "provider_specific": true
            }),
        )
        .unwrap();

        assert_eq!(req.params.temperature, Some(0.7));
        assert_eq!(req.params.top_p, Some(0.8));
        assert_eq!(req.params.max_tokens, Some(512));
        assert_eq!(req.params.stop, vec!["END"]);
        assert_eq!(req.extra.get("provider_specific"), Some(&Value::Bool(true)));

        let raw: Value = serde_json::from_str(req.raw_body.as_deref().unwrap()).unwrap();
        assert_eq!(raw["temperature"], 0.7);
        assert_eq!(raw["top_p"], 0.8);
        assert_eq!(raw["max_tokens"], 512);
        assert_eq!(raw["provider_specific"], true);
    }

    #[test]
    fn provider_failure_rules_override_native_classification() {
        let p = provider(serde_json::json!({
            "quota": {
                "statuses": [429],
                "codes": ["billing_hard_limit"]
            }
        }));
        let native = UpstreamFailure {
            kind: FailureKind::RateLimit,
            status: Some(429),
            retry_after_secs: None,
            message: "limit reached".into(),
            quota_reset_at: None,
        };
        let classified = apply_provider_failure_rules(
            &p,
            429,
            r#"{"error":{"code":"billing_hard_limit","message":"limit reached"}}"#,
            native,
        );
        assert_eq!(classified.kind, FailureKind::QuotaExhausted);
    }

    #[test]
    fn provider_failure_rule_requires_all_configured_selectors() {
        let p = provider(serde_json::json!({
            "quota": {
                "statuses": [429],
                "codes": ["billing_hard_limit"],
                "message_contains": ["daily"]
            }
        }));
        let native = UpstreamFailure {
            kind: FailureKind::RateLimit,
            status: Some(429),
            retry_after_secs: None,
            message: "limit reached".into(),
            quota_reset_at: None,
        };
        let classified = apply_provider_failure_rules(
            &p,
            429,
            r#"{"error":{"code":"billing_hard_limit","message":"per-minute limit reached"}}"#,
            native,
        );
        assert_eq!(classified.kind, FailureKind::RateLimit);
    }
}
