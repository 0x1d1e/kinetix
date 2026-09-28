//! The internal, provider-neutral request and streaming-event model.
//!
//! Every inbound frontend decodes into these types; every outbound adapter
//! consumes them. Nothing here references a specific vendor (FR-11.1, NFR-5.1).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Capabilities & configuration value types (shared with the DB layer)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFormat {
    Openai,
    Anthropic,
    Gemini,
    /// Host-owned sentinel for providers whose actual wire translation is
    /// supplied by `wire_plugin`. It has no native adapter.
    Plugin,
}

impl WireFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            WireFormat::Openai => "openai",
            WireFormat::Anthropic => "anthropic",
            WireFormat::Gemini => "gemini",
            WireFormat::Plugin => "plugin",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(WireFormat::Openai),
            "anthropic" => Some(WireFormat::Anthropic),
            "gemini" => Some(WireFormat::Gemini),
            "plugin" => Some(WireFormat::Plugin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    Bearer,
    CustomHeader,
    QueryParam,
}

impl AuthScheme {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bearer" => Some(AuthScheme::Bearer),
            "custom_header" => Some(AuthScheme::CustomHeader),
            "query_param" => Some(AuthScheme::QueryParam),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub text: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub reasoning: bool,
    // Accept the dashboard's camelCase spelling too; always serialized snake_case.
    #[serde(default, alias = "toolCalling")]
    pub tool_calling: bool,
    #[serde(default)]
    pub audio: bool,
    #[serde(default, alias = "structuredOutput")]
    pub structured_output: bool,
}

impl Capabilities {
    /// Build from a list of capability tokens (bootstrap / discovery import).
    pub fn from_tokens(tokens: &[String]) -> Self {
        let mut c = Capabilities::default();
        for t in tokens {
            match t.as_str() {
                "text" => c.text = true,
                "vision" => c.vision = true,
                "reasoning" => c.reasoning = true,
                "tool_calling" | "tools" => c.tool_calling = true,
                "audio" => c.audio = true,
                "structured_output" | "structuredOutput" => c.structured_output = true,
                _ => {}
            }
        }
        c
    }

    /// Whether the configured capabilities can satisfy a request's needs.
    /// Unconfigured metadata counts as compatible (permissive default, FR-12.8).
    pub fn satisfies(&self, needs: &CapabilityNeeds) -> bool {
        if needs.vision && !self.vision {
            return false;
        }
        if needs.tools && !self.tool_calling {
            return false;
        }
        if needs.reasoning && !self.reasoning {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CapabilityNeeds {
    pub vision: bool,
    pub tools: bool,
    pub reasoning: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Prices {
    pub input_per_1m: Option<f64>,
    pub output_per_1m: Option<f64>,
    /// Cache-read price. Cached tokens are a breakdown already included in input.
    pub cached_per_1m: Option<f64>,
    /// Cache-write/creation price. Cache-write tokens are included in input.
    pub cache_write_per_1m: Option<f64>,
    /// Thinking/reasoning price. Thinking tokens are included in output.
    pub thinking_per_1m: Option<f64>,
}

impl Prices {
    pub fn is_configured(&self) -> bool {
        self.input_per_1m.is_some() || self.output_per_1m.is_some()
    }
}

/// Policy for a client-supplied parameter value that is unsupported/out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamPolicy {
    Drop,
    Clamp,
    Reject,
    Forward,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamSpec {
    pub supported: bool,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub default: Option<f64>,
    #[serde(default = "default_policy")]
    pub policy: ParamPolicy,
}

fn default_policy() -> ParamPolicy {
    ParamPolicy::Forward
}

/// Canonical thinking level (FR-10.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Off,
    Default,
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl ThinkingLevel {
    pub fn as_key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Default => "default",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    ManualBudget,
    Level,
    Adaptive,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingMap {
    /// Canonical level -> upstream request field value (opaque JSON).
    #[serde(default)]
    pub levels: std::collections::HashMap<String, serde_json::Value>,
    /// Optional explicit reasoning mode. Omitted preserves legacy mapping behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ThinkingMode>,
    /// Scalar field used by legacy/manual-budget mappings.
    #[serde(default)]
    pub budget_field: Option<String>,
    /// Scalar field used by categorical level/adaptive mappings (for example `output_config.effort`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_field: Option<String>,
}

impl ThinkingMap {
    pub fn is_adaptive(&self) -> bool {
        matches!(self.mode, Some(ThinkingMode::Adaptive))
    }

    pub fn uses_level_field(&self) -> bool {
        matches!(
            self.mode,
            Some(ThinkingMode::Level | ThinkingMode::Adaptive)
        )
    }

    pub fn scalar_field(&self) -> Option<&str> {
        if self.uses_level_field() {
            self.level_field.as_deref()
        } else {
            self.budget_field.as_deref()
        }
    }

    pub fn level_is_executable(&self, level: &str) -> bool {
        let Some(value) = self.levels.get(level) else {
            return false;
        };
        match value {
            serde_json::Value::Null | serde_json::Value::Array(_) => false,
            serde_json::Value::Object(fields) => {
                !fields.is_empty() && fields.keys().all(|field| !field.trim().is_empty())
            }
            serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => self
                .scalar_field()
                .map(str::trim)
                .is_some_and(|field| !field.is_empty()),
        }
    }

    pub fn anthropic_adaptive_off_is_executable(&self) -> bool {
        if !self.is_adaptive() {
            return false;
        }
        let Some(serde_json::Value::Object(fields)) = self.levels.get("off") else {
            return false;
        };
        fields
            .get("thinking.type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value == "disabled")
            || fields
                .get("thinking")
                .and_then(serde_json::Value::as_object)
                .and_then(|thinking| thinking.get("type"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value == "disabled")
    }

    pub fn validation_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let budget_field = self.budget_field.as_deref().map(str::trim);
        if budget_field.is_some_and(str::is_empty) {
            errors.push("thinking_map.budget_field must not be empty".to_string());
        }
        let level_field = self.level_field.as_deref().map(str::trim);
        if level_field.is_some_and(str::is_empty) {
            errors.push("thinking_map.level_field must not be empty".to_string());
        }

        for (level, value) in &self.levels {
            if level.trim().is_empty() {
                errors.push("thinking_map level names must not be empty".to_string());
                continue;
            }
            match value {
                serde_json::Value::Null => {
                    errors.push(format!("thinking_map level '{level}' must not be null"))
                }
                serde_json::Value::Array(_) => errors.push(format!(
                    "thinking_map level '{level}' must be an object or scalar"
                )),
                serde_json::Value::Object(fields) => {
                    if fields.is_empty() {
                        errors.push(format!(
                            "thinking_map level '{level}' must not be an empty object"
                        ));
                    } else if fields.keys().any(|field| field.trim().is_empty()) {
                        errors.push(format!(
                            "thinking_map level '{level}' contains an empty upstream field"
                        ));
                    }
                }
                serde_json::Value::Bool(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::String(_) => {
                    if self.is_adaptive() && level == "off" {
                        errors.push(
                            "thinking_map adaptive level 'off' must be an object mapping that explicitly disables thinking"
                                .to_string(),
                        );
                    } else if !self.level_is_executable(level) {
                        let field = if self.uses_level_field() {
                            "level_field"
                        } else {
                            "budget_field"
                        };
                        errors.push(format!(
                            "thinking_map level '{level}' uses a scalar mapping but {field} is not configured"
                        ));
                    }
                }
            }
        }

        errors
    }
}

// ---------------------------------------------------------------------------
// Internal request model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImageData {
    Base64 { mime: String, data: String },
    Url(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Image(ImageData),
    ToolCall {
        id: Option<String>,
        name: String,
        arguments: String,
        /// Opaque vendor signature (e.g. Gemini thoughtSignature) to round-trip.
        signature: Option<String>,
    },
    ToolResult {
        tool_call_id: String,
        name: Option<String>,
        content: String,
        is_error: bool,
    },
    Thinking {
        text: String,
        signature: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Specific,
}

#[derive(Debug, Clone, Default)]
pub struct SamplingParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<f64>,
    pub max_tokens: Option<u32>,
    pub stop: Vec<String>,
    pub seed: Option<i64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct InternalRequest {
    /// The model name the client asked for (alias, route, or provider/model-id).
    pub requested_model: String,
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: Option<ToolChoice>,
    pub tool_choice_name: Option<String>,
    pub params: SamplingParams,
    pub stream: bool,
    /// Client-visible OpenAI streaming usage preference. This is intentionally
    /// separate from Kinetix's internal need to request upstream usage for
    /// accounting.
    pub include_usage: bool,
    pub thinking: Option<ThinkingLevel>,
    /// Fields the client sent that we did not model; forwarded or stripped per config.
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// The original client body (JSON string), kept for same-format passthrough
    /// so unknown/provider-specific fields survive (FR-2.7, FR-2.10).
    pub raw_body: Option<String>,
}

impl InternalRequest {
    pub fn capability_needs(&self) -> CapabilityNeeds {
        let vision = self
            .messages
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, Part::Image(_))));
        CapabilityNeeds {
            vision,
            tools: !self.tools.is_empty(),
            reasoning: self.thinking.is_some(),
        }
    }

    /// Rough size estimate (chars) for context-window compatibility filtering.
    pub fn approx_input_tokens(&self) -> u64 {
        let mut chars: u64 = self.system.iter().map(|s| s.len() as u64).sum();
        for m in &self.messages {
            for p in &m.parts {
                chars += match p {
                    Part::Text(t) => t.len() as u64,
                    Part::Thinking { text, .. } => text.len() as u64,
                    Part::ToolCall {
                        name, arguments, ..
                    } => (name.len() + arguments.len()) as u64,
                    Part::ToolResult { content, .. } => content.len() as u64,
                    Part::Image(_) => 1000, // rough fixed cost for images
                };
            }
        }
        // ~4 chars per token, rounded up.
        chars.div_ceil(4)
    }
}

// ---------------------------------------------------------------------------
// Internal streaming events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    /// Total effective input tokens, including cache-read/write breakdowns.
    pub input: Option<u64>,
    /// Total generated tokens, including thinking/reasoning breakdowns.
    pub output: Option<u64>,
    /// Cache-read tokens already included in `input`.
    pub cached: Option<u64>,
    /// Cache-write/creation tokens already included in `input`.
    pub cache_write: Option<u64>,
    /// Thinking/reasoning tokens already included in `output`.
    pub thinking: Option<u64>,
}

impl TokenUsage {
    pub fn merge(&mut self, other: &TokenUsage) {
        if other.input.is_some() {
            self.input = other.input;
        }
        if other.output.is_some() {
            self.output = other.output;
        }
        if other.cached.is_some() {
            self.cached = other.cached;
        }
        if other.cache_write.is_some() {
            self.cache_write = other.cache_write;
        }
        if other.thinking.is_some() {
            self.thinking = other.thinking;
        }
    }
}

#[derive(Debug, Clone)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Other(String),
}

impl FinishReason {
    pub fn as_str(&self) -> &str {
        match self {
            FinishReason::Stop => "stop",
            FinishReason::Length => "length",
            FinishReason::ToolCalls => "tool_calls",
            FinishReason::ContentFilter => "content_filter",
            FinishReason::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// The upstream has accepted the request; no client bytes sent yet.
    Start {
        upstream_request_id: Option<String>,
    },
    ThinkingDelta {
        text: String,
        signature: Option<String>,
    },
    TextDelta(String),
    /// User-visible refusal text, kept distinct so Responses frontends can
    /// preserve the protocol's refusal content item while other frontends
    /// render it as ordinary assistant text.
    RefusalDelta(String),
    ToolCallStart {
        index: u32,
        id: Option<String>,
        name: String,
        signature: Option<String>,
    },
    ToolCallArgsDelta {
        index: u32,
        args: String,
    },
    Usage(TokenUsage),
    Finish(FinishReason),
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Account-level rate limit.
    RateLimit,
    /// Account-level quota exhaustion.
    QuotaExhausted,
    /// Credential-global invalid or revoked credential.
    AuthError,
    /// Provider/model-local entitlement or missing resource.
    TargetError,
    /// Transient upstream response failure.
    ServerError,
    /// Transient connection failure.
    ConnectionError,
    /// Upstream timeout.
    Timeout,
    /// Request-local malformed or unsupported request.
    BadRequest,
    /// Invalid upstream JSON, SSE framing, or response structure.
    MalformedUpstream,
    /// Plugin execution, trap, permission, or contract failure.
    PluginFailure,
    /// Request rejected by an explicit compatibility or routing policy.
    PolicyRejected,
    /// The client disconnected before the response completed.
    ClientCancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    CredentialGlobal,
    AccountQuota,
    TargetModelLocal,
    RequestLocal,
    RateLimited,
    TransientUpstream,
    Timeout,
    MalformedUpstream,
    Plugin,
    PolicyRejection,
    ClientCancellation,
}

impl FailureCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CredentialGlobal => "credential_global",
            Self::AccountQuota => "account_quota",
            Self::TargetModelLocal => "target_model_local",
            Self::RequestLocal => "request_local",
            Self::RateLimited => "rate_limited",
            Self::TransientUpstream => "transient_upstream",
            Self::Timeout => "timeout",
            Self::MalformedUpstream => "malformed_upstream",
            Self::Plugin => "plugin",
            Self::PolicyRejection => "policy_rejection",
            Self::ClientCancellation => "client_cancellation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackRule {
    Always,
    On429,
    OnQuota,
    On5xx,
    OnTimeout,
    RouteTarget,
    Never,
}

impl FallbackRule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::On429 => "on429",
            Self::OnQuota => "onQuota",
            Self::On5xx => "on5xx",
            Self::OnTimeout => "onTimeout",
            Self::RouteTarget => "route_target",
            Self::Never => "never",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountHealthEffect {
    None,
    Cooldown,
    Exhausted,
    DisableCredential,
}

impl AccountHealthEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cooldown => "cooldown",
            Self::Exhausted => "exhausted",
            Self::DisableCredential => "disable_credential",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryHint {
    None,
    RetryAfter,
    QuotaReset,
}

impl RetryHint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::RetryAfter => "retry_after",
            Self::QuotaReset => "quota_reset",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailurePolicy {
    pub category: FailureCategory,
    pub fallback: FallbackRule,
    pub account_health: AccountHealthEffect,
    pub client_status: Option<u16>,
    pub retry_hint: RetryHint,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::QuotaExhausted => "quota_exhausted",
            Self::AuthError => "auth_error",
            Self::TargetError => "target_error",
            Self::ServerError => "server_error",
            Self::ConnectionError => "connection_error",
            Self::Timeout => "timeout",
            Self::BadRequest => "bad_request",
            Self::MalformedUpstream => "malformed_upstream",
            Self::PluginFailure => "plugin_failure",
            Self::PolicyRejected => "policy_rejected",
            Self::ClientCancelled => "client_cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "rate_limit" => Self::RateLimit,
            "quota_exhausted" => Self::QuotaExhausted,
            "auth_error" => Self::AuthError,
            "target_error" => Self::TargetError,
            "server_error" => Self::ServerError,
            "connection_error" => Self::ConnectionError,
            "timeout" => Self::Timeout,
            "bad_request" => Self::BadRequest,
            "malformed_upstream" => Self::MalformedUpstream,
            "plugin_failure" => Self::PluginFailure,
            "policy_rejected" => Self::PolicyRejected,
            "client_cancelled" => Self::ClientCancelled,
            _ => return None,
        })
    }

    /// The shared policy contract used by retries, health transitions, client
    /// mapping, and Route Trace. Route fallback triggers can further restrict
    /// the fallback rule for a concrete Route.
    pub fn policy(self) -> FailurePolicy {
        use AccountHealthEffect as Health;
        use FailureCategory as Category;
        use FallbackRule as Fallback;
        use RetryHint as Retry;

        let (category, fallback, account_health, client_status, retry_hint) = match self {
            Self::RateLimit => (
                Category::RateLimited,
                Fallback::On429,
                Health::Cooldown,
                Some(429),
                Retry::RetryAfter,
            ),
            Self::QuotaExhausted => (
                Category::AccountQuota,
                Fallback::OnQuota,
                Health::Exhausted,
                Some(429),
                Retry::QuotaReset,
            ),
            Self::AuthError => (
                Category::CredentialGlobal,
                Fallback::Always,
                Health::DisableCredential,
                Some(502),
                Retry::None,
            ),
            Self::TargetError => (
                Category::TargetModelLocal,
                Fallback::RouteTarget,
                Health::None,
                Some(502),
                Retry::None,
            ),
            Self::ServerError | Self::ConnectionError => (
                Category::TransientUpstream,
                Fallback::On5xx,
                Health::None,
                Some(502),
                Retry::None,
            ),
            Self::Timeout => (
                Category::Timeout,
                Fallback::OnTimeout,
                Health::None,
                Some(504),
                Retry::None,
            ),
            Self::BadRequest => (
                Category::RequestLocal,
                Fallback::Never,
                Health::None,
                Some(400),
                Retry::None,
            ),
            Self::MalformedUpstream => (
                Category::MalformedUpstream,
                Fallback::On5xx,
                Health::None,
                Some(502),
                Retry::None,
            ),
            Self::PluginFailure => (
                Category::Plugin,
                Fallback::On5xx,
                Health::None,
                Some(502),
                Retry::None,
            ),
            Self::PolicyRejected => (
                Category::PolicyRejection,
                Fallback::Never,
                Health::None,
                Some(400),
                Retry::None,
            ),
            Self::ClientCancelled => (
                Category::ClientCancellation,
                Fallback::Never,
                Health::None,
                None,
                Retry::None,
            ),
        };
        FailurePolicy {
            category,
            fallback,
            account_health,
            client_status,
            retry_hint,
        }
    }

    pub fn client_status(self, upstream_status: Option<u16>) -> Option<u16> {
        if self == Self::TargetError {
            return match upstream_status {
                Some(status @ (403 | 404)) => Some(status),
                _ => self.policy().client_status,
            };
        }
        self.policy().client_status
    }

    pub fn reason_code(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limited",
            Self::QuotaExhausted => "account_quota_exhausted",
            Self::AuthError => "credential_rejected",
            Self::TargetError => "target_model_unavailable",
            Self::ServerError => "upstream_server_error",
            Self::ConnectionError => "upstream_connection_error",
            Self::Timeout => "upstream_timeout",
            Self::BadRequest => "request_invalid",
            Self::MalformedUpstream => "malformed_upstream_response",
            Self::PluginFailure => "plugin_failure",
            Self::PolicyRejected => "policy_rejected",
            Self::ClientCancelled => "client_cancelled",
        }
    }

    /// Whether another candidate may be attempted before client commit.
    pub fn is_retryable(self) -> bool {
        self.policy().fallback != FallbackRule::Never
    }

    pub fn retry_after_secs(self, failure: &UpstreamFailure) -> Option<u64> {
        match self.policy().retry_hint {
            RetryHint::None => None,
            RetryHint::RetryAfter => failure.retry_after_secs,
            RetryHint::QuotaReset => failure.retry_after_secs.or_else(|| {
                failure.quota_reset_at.as_ref().map(|reset| {
                    (reset.to_owned() - chrono::Utc::now()).num_seconds().max(1) as u64
                })
            }),
        }
    }

    /// Whether the failure is direct evidence that the selected credential is
    /// unavailable. Target-local and request-local failures never poison it.
    pub fn is_account_scoped(self) -> bool {
        self.policy().account_health != AccountHealthEffect::None
    }

    /// Backwards-compatible helper used by older tests/callers.
    pub fn is_key_level(self) -> bool {
        self.is_account_scoped()
    }
}

#[derive(Debug, Clone)]
pub struct UpstreamFailure {
    pub kind: FailureKind,
    pub status: Option<u16>,
    pub retry_after_secs: Option<u64>,
    /// Redacted, client-safe message.
    pub message: String,
    /// Quota reset instant parsed from the response, if any.
    pub quota_reset_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    RateLimited,
    BudgetExceeded,
    Unsupported,
    Upstream,
    AllTargetsUnavailable,
    Internal,
    /// Control-plane/administrative surface is degraded (NFR-2.7).
    ServiceUnavailable,
    ClientCancelled,
}

#[derive(Debug, Clone)]
pub struct ProxyError {
    pub kind: ErrorKind,
    pub message: String,
    pub retry_after_secs: Option<u64>,
    /// Response headers to surface (e.g. X-Kinetix-Fallback).
    pub headers: Vec<(String, String)>,
    /// Preserve a compatible upstream HTTP status when the downstream wire
    /// format can safely expose it.
    pub http_status_override: Option<u16>,
    /// Preserve a sanitized wire-compatible upstream error body.
    pub body_override: Option<serde_json::Value>,
}

impl ProxyError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, msg)
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, msg)
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unauthorized, msg)
    }
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, msg)
    }
    pub fn rate_limited(msg: impl Into<String>, retry_after: Option<u64>) -> Self {
        Self {
            kind: ErrorKind::RateLimited,
            message: msg.into(),
            retry_after_secs: retry_after,
            headers: Vec::new(),
            http_status_override: None,
            body_override: None,
        }
    }
    pub fn budget_exceeded(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::BudgetExceeded, msg)
    }
    pub fn upstream(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Upstream, msg)
    }
    pub fn all_unavailable(msg: impl Into<String>, retry_after: Option<u64>) -> Self {
        Self {
            kind: ErrorKind::AllTargetsUnavailable,
            message: msg.into(),
            retry_after_secs: retry_after,
            headers: Vec::new(),
            http_status_override: None,
            body_override: None,
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, msg)
    }
    /// Service temporarily degraded (control plane unavailable). Rendered as
    /// 503 and does not affect any inference path (NFR-2.7).
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::ServiceUnavailable, msg)
    }
    pub fn new(kind: ErrorKind, msg: impl Into<String>) -> Self {
        Self {
            kind,
            message: msg.into(),
            retry_after_secs: None,
            headers: Vec::new(),
            http_status_override: None,
            body_override: None,
        }
    }
    pub fn http_status(&self) -> u16 {
        if let Some(status) = self.http_status_override {
            return status;
        }
        match self.kind {
            ErrorKind::BadRequest => 400,
            ErrorKind::Unauthorized => 401,
            ErrorKind::Forbidden => 403,
            ErrorKind::NotFound => 404,
            ErrorKind::Unsupported => 422,
            ErrorKind::RateLimited | ErrorKind::BudgetExceeded => 429,
            ErrorKind::AllTargetsUnavailable | ErrorKind::ServiceUnavailable => 503,
            ErrorKind::ClientCancelled => 499,
            ErrorKind::Upstream | ErrorKind::Internal => 502,
        }
    }
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for ProxyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_policy_covers_every_taxonomy_category() {
        let cases = [
            (
                FailureKind::AuthError,
                "credential_global",
                "always",
                "disable_credential",
                Some(502),
                "none",
            ),
            (
                FailureKind::QuotaExhausted,
                "account_quota",
                "onQuota",
                "exhausted",
                Some(429),
                "quota_reset",
            ),
            (
                FailureKind::TargetError,
                "target_model_local",
                "route_target",
                "none",
                Some(502),
                "none",
            ),
            (
                FailureKind::BadRequest,
                "request_local",
                "never",
                "none",
                Some(400),
                "none",
            ),
            (
                FailureKind::RateLimit,
                "rate_limited",
                "on429",
                "cooldown",
                Some(429),
                "retry_after",
            ),
            (
                FailureKind::ServerError,
                "transient_upstream",
                "on5xx",
                "none",
                Some(502),
                "none",
            ),
            (
                FailureKind::Timeout,
                "timeout",
                "onTimeout",
                "none",
                Some(504),
                "none",
            ),
            (
                FailureKind::MalformedUpstream,
                "malformed_upstream",
                "on5xx",
                "none",
                Some(502),
                "none",
            ),
            (
                FailureKind::PluginFailure,
                "plugin",
                "on5xx",
                "none",
                Some(502),
                "none",
            ),
            (
                FailureKind::PolicyRejected,
                "policy_rejection",
                "never",
                "none",
                Some(400),
                "none",
            ),
            (
                FailureKind::ClientCancelled,
                "client_cancellation",
                "never",
                "none",
                None,
                "none",
            ),
        ];
        for (kind, category, fallback, health, client_status, retry_hint) in cases {
            let policy = kind.policy();
            assert_eq!(policy.category.as_str(), category, "{kind:?}");
            assert_eq!(policy.fallback.as_str(), fallback, "{kind:?}");
            assert_eq!(policy.account_health.as_str(), health, "{kind:?}");
            assert_eq!(policy.client_status, client_status, "{kind:?}");
            assert_eq!(policy.retry_hint.as_str(), retry_hint, "{kind:?}");
        }
    }

    #[test]
    fn target_and_client_failures_never_mutate_account_health() {
        for kind in [
            FailureKind::TargetError,
            FailureKind::BadRequest,
            FailureKind::MalformedUpstream,
            FailureKind::PluginFailure,
            FailureKind::PolicyRejected,
            FailureKind::ClientCancelled,
        ] {
            assert!(!kind.is_account_scoped(), "{kind:?}");
        }
        assert_eq!(FailureKind::ClientCancelled.client_status(None), None);
        assert_eq!(FailureKind::TargetError.client_status(Some(403)), Some(403));
        assert_eq!(FailureKind::TargetError.client_status(Some(404)), Some(404));
    }

    #[test]
    fn satisfies_enforces_reasoning_capability() {
        // A model that does not declare reasoning must not satisfy a request
        // that carries reasoning controls (FR-10.9/FR-12.11).
        let caps = Capabilities {
            text: true,
            vision: true,
            reasoning: false,
            tool_calling: true,
            audio: false,
            structured_output: false,
        };
        let reasoning_needed = CapabilityNeeds {
            vision: false,
            tools: false,
            reasoning: true,
        };
        assert!(!caps.satisfies(&reasoning_needed));
        let text_only = CapabilityNeeds {
            vision: false,
            tools: false,
            reasoning: false,
        };
        assert!(caps.satisfies(&text_only));
        let vision_needed = CapabilityNeeds {
            vision: true,
            tools: false,
            reasoning: false,
        };
        assert!(caps.satisfies(&vision_needed));
    }

    #[test]
    fn thinking_map_rejects_unknown_fields() {
        let malformed = serde_json::json!({
            "scale": "medium",
            "mappedField": "thinkingConfig"
        });
        assert!(serde_json::from_value::<ThinkingMap>(malformed).is_err());
    }

    #[test]
    fn thinking_map_round_trips_levels_and_budget_field() {
        let expected = serde_json::json!({
            "levels": {
                "low": 512,
                "medium": {"reasoning_effort": "medium"},
                "high": {"thinkingConfig.thinkingBudget": 4096}
            },
            "budget_field": "thinking.budget_tokens"
        });
        let parsed: ThinkingMap = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(parsed.levels.get("low"), Some(&serde_json::json!(512)));
        assert_eq!(
            parsed.levels.get("medium"),
            Some(&serde_json::json!({"reasoning_effort": "medium"}))
        );
        assert_eq!(
            parsed.budget_field.as_deref(),
            Some("thinking.budget_tokens")
        );
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }

    #[test]
    fn thinking_map_rejects_non_executable_values() {
        let null_mapping: ThinkingMap = serde_json::from_value(serde_json::json!({
            "levels": {"high": null}
        }))
        .unwrap();
        assert!(!null_mapping.validation_errors().is_empty());
        assert!(!null_mapping.level_is_executable("high"));

        let scalar_without_field: ThinkingMap = serde_json::from_value(serde_json::json!({
            "levels": {"high": 4096}
        }))
        .unwrap();
        assert!(!scalar_without_field.validation_errors().is_empty());
        assert!(!scalar_without_field.level_is_executable("high"));

        let scalar_with_field: ThinkingMap = serde_json::from_value(serde_json::json!({
            "levels": {"high": 4096},
            "budget_field": "thinking.budget_tokens"
        }))
        .unwrap();
        assert!(scalar_with_field.validation_errors().is_empty());
        assert!(scalar_with_field.level_is_executable("high"));

        let level: ThinkingMap = serde_json::from_value(serde_json::json!({
            "mode": "level",
            "levels": {"high": "high"},
            "level_field": "thinkingConfig.thinkingLevel"
        }))
        .unwrap();
        assert!(level.validation_errors().is_empty());
        assert!(level.level_is_executable("high"));
        assert_eq!(level.mode, Some(ThinkingMode::Level));

        let adaptive: ThinkingMap = serde_json::from_value(serde_json::json!({
            "mode": "adaptive",
            "levels": {"high": "high"},
            "level_field": "output_config.effort"
        }))
        .unwrap();
        assert!(adaptive.validation_errors().is_empty());
        assert!(adaptive.level_is_executable("high"));
        assert_eq!(adaptive.mode, Some(ThinkingMode::Adaptive));

        let adaptive_scalar_off: ThinkingMap = serde_json::from_value(serde_json::json!({
            "mode": "adaptive",
            "levels": {"off": "low"},
            "level_field": "output_config.effort"
        }))
        .unwrap();
        assert!(adaptive_scalar_off.level_is_executable("off"));
        assert!(!adaptive_scalar_off.validation_errors().is_empty());
        assert!(!adaptive_scalar_off.anthropic_adaptive_off_is_executable());

        let adaptive_disabled_off: ThinkingMap = serde_json::from_value(serde_json::json!({
            "mode": "adaptive",
            "levels": {"off": {"thinking.type": "disabled"}},
            "level_field": "output_config.effort"
        }))
        .unwrap();
        assert!(adaptive_disabled_off.anthropic_adaptive_off_is_executable());

        let adaptive_without_level_field: ThinkingMap = serde_json::from_value(serde_json::json!({
            "mode": "adaptive",
            "levels": {"high": "high"}
        }))
        .unwrap();
        assert!(!adaptive_without_level_field.validation_errors().is_empty());
        assert!(!adaptive_without_level_field.level_is_executable("high"));
    }
}
