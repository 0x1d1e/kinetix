//! Outbound adapter interface (FR-11.1). One adapter per wire format; chosen by
//! the provider's configured wire format, never by vendor. Built-in code goes
//! through exactly this interface so a plugin can attach later without rewrites.

use async_trait::async_trait;
use futures::stream::BoxStream;
use std::sync::Arc;

pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod openai_responses;

use crate::model_capabilities::{
    apply_integration_feature_ceiling, ModelCapabilityFlags, ReasoningCapability,
    ReasoningCapabilityMode,
};
use crate::opaque_state::OpaqueStateTarget;
use crate::types::{
    InternalRequest, ParamSpec, ProxyError, StreamEvent, ThinkingMap, UpstreamFailure, WireFormat,
};

/// Canonical outbound execution transport. Unlike `WireFormat`, this can
/// distinguish multiple execution surfaces for one provider family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetTransport {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
    Gemini,
    Plugin(String),
}

impl TargetTransport {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "openai" | "openai-chat" => Some(Self::OpenAiChat),
            "openai-responses" => Some(Self::OpenAiResponses),
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            _ => crate::plugins::PluginRef::parse(value)
                .map(|reference| Self::Plugin(reference.to_string_ref())),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::OpenAiChat => "openai",
            Self::OpenAiResponses => "openai-responses",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::Plugin(reference) => reference,
        }
    }

    fn default_for_provider(provider: &crate::db::ProviderRow) -> Result<Self, ProxyError> {
        if let Some(reference) = provider.wire_plugin_ref() {
            return Ok(Self::Plugin(reference.to_string_ref()));
        }
        match WireFormat::parse(&provider.wire_format).ok_or_else(|| {
            ProxyError::unsupported(format!(
                "unsupported provider wire format '{}'",
                provider.wire_format
            ))
        })? {
            WireFormat::Openai => Ok(Self::OpenAiChat),
            WireFormat::Anthropic => Ok(Self::Anthropic),
            WireFormat::Gemini => Ok(Self::Gemini),
            WireFormat::Plugin => Err(ProxyError::unsupported(
                "plugin transport requires a valid provider adapter binding",
            )),
        }
    }

    pub(crate) fn provider_wire_format(&self) -> Option<WireFormat> {
        match self {
            Self::OpenAiChat | Self::OpenAiResponses => Some(WireFormat::Openai),
            Self::Anthropic => Some(WireFormat::Anthropic),
            Self::Gemini => Some(WireFormat::Gemini),
            Self::Plugin(_) => None,
        }
    }
}

/// One target-local decision made before adapter dispatch. Every failover
/// candidate resolves a fresh profile; no decision is carried across targets.
#[derive(Debug, Clone)]
pub struct ResolvedExecutionProfile {
    pub transport: TargetTransport,
    pub reasoning: Option<ReasoningCapability>,
    pub parameters: std::collections::HashMap<String, ParamSpec>,
    pub capabilities: ModelCapabilityFlags,
    pub thinking_map: ThinkingMap,
}

fn discovered_transport(discovery: &serde_json::Value) -> Result<Option<&str>, ProxyError> {
    let normalized = discovery.get("transport").filter(|value| !value.is_null());
    let raw_metadata = discovery
        .get("raw_metadata")
        .filter(|value| !value.is_null());
    let observed = if let Some(transport) = normalized {
        Some(transport.get("format").ok_or_else(|| {
            ProxyError::unsupported("discovered model transport metadata is missing its format")
        })?)
    } else if let Some(transport) = raw_metadata.and_then(|metadata| metadata.get("transport")) {
        if transport.is_null() {
            None
        } else {
            Some(transport.get("format").ok_or_else(|| {
                ProxyError::unsupported("raw discovered transport metadata is missing its format")
            })?)
        }
    } else {
        None
    };
    observed
        .map(|value| {
            value.as_str().ok_or_else(|| {
                ProxyError::unsupported("discovered model transport format must be a string")
            })
        })
        .transpose()
}

fn fresh_probe_status_entry<'a>(
    evidence: &'a serde_json::Value,
    provider_id: &str,
    model_id: &str,
    account_id: Option<&str>,
    transport: &TargetTransport,
) -> Option<&'a str> {
    let account_id = account_id?;
    let scope = evidence.get("scope")?;
    if scope.get("provider_id").and_then(serde_json::Value::as_str) != Some(provider_id)
        || scope.get("model_id").and_then(serde_json::Value::as_str) != Some(model_id)
        || scope.get("account_id").and_then(serde_json::Value::as_str) != Some(account_id)
        || scope.get("transport").and_then(serde_json::Value::as_str) != Some(transport.as_str())
    {
        return None;
    }
    let fresh_until = evidence
        .get("fresh_until")
        .and_then(serde_json::Value::as_str)?;
    let fresh_until = chrono::DateTime::parse_from_rfc3339(fresh_until).ok()?;
    if fresh_until.with_timezone(&chrono::Utc) <= chrono::Utc::now() {
        return None;
    }
    evidence.get("status").and_then(serde_json::Value::as_str)
}

fn fresh_conclusive_probe_status_entry<'a>(
    evidence: &'a serde_json::Value,
    provider_id: &str,
    model_id: &str,
    account_id: Option<&str>,
    transport: &TargetTransport,
) -> Option<&'a str> {
    let status = fresh_probe_status_entry(evidence, provider_id, model_id, account_id, transport)?;
    matches!(status, "supported" | "unsupported").then_some(status)
}

fn fresh_probe_status<'a>(
    evidence: &'a serde_json::Value,
    provider_id: &str,
    model_id: &str,
    account_id: Option<&str>,
    transport: &TargetTransport,
) -> Option<&'a str> {
    match evidence {
        // New storage keeps probe history per execution scope. Search
        // newest-first for fresh conclusive evidence; inconclusive attempts do
        // not supersede a still-fresh supported/unsupported result.
        serde_json::Value::Array(entries) => entries.iter().rev().find_map(|entry| {
            fresh_conclusive_probe_status_entry(entry, provider_id, model_id, account_id, transport)
        }),
        // Legacy single-entry storage remains readable.
        _ => fresh_conclusive_probe_status_entry(
            evidence,
            provider_id,
            model_id,
            account_id,
            transport,
        ),
    }
}

/// Resolve the model's effective outbound transport.
pub fn resolve_model_transport(
    provider: &crate::db::ProviderRow,
    model: &crate::db::ModelRow,
) -> Result<TargetTransport, ProxyError> {
    let discovery = serde_json::from_str::<serde_json::Value>(&model.discovery)
        .unwrap_or_else(|_| serde_json::json!({}));
    resolve_model_transport_from_discovery(provider, &discovery)
}

fn resolve_model_transport_from_discovery(
    provider: &crate::db::ProviderRow,
    discovery: &serde_json::Value,
) -> Result<TargetTransport, ProxyError> {
    let provider_wire = WireFormat::parse(&provider.wire_format).ok_or_else(|| {
        ProxyError::unsupported(format!(
            "unsupported provider wire format '{}'",
            provider.wire_format
        ))
    })?;
    let configured = match discovery.get("configured_transport") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(value.as_str().ok_or_else(|| {
            ProxyError::unsupported("configured model transport must be a string")
        })?),
    };
    let observed = discovered_transport(discovery)?;
    let provider_plugin = provider.wire_plugin_ref();

    let transport = if let Some(reference) = &provider_plugin {
        let bound = TargetTransport::Plugin(reference.to_string_ref());
        if let Some(configured) = configured {
            let parsed = TargetTransport::parse(configured).ok_or_else(|| {
                ProxyError::unsupported(format!(
                    "unknown configured model transport '{configured}'"
                ))
            })?;
            if parsed != bound {
                return Err(ProxyError::unsupported(
                    "model transport override conflicts with the provider's explicit plugin adapter",
                ));
            }
        }
        bound
    } else if let Some(configured) = configured {
        TargetTransport::parse(configured).ok_or_else(|| {
            ProxyError::unsupported(format!("unknown configured model transport '{configured}'"))
        })?
    } else if let Some(observed) = observed {
        TargetTransport::parse(observed).ok_or_else(|| {
            ProxyError::unsupported(format!(
                "unsupported discovered model transport '{observed}'"
            ))
        })?
    } else {
        TargetTransport::default_for_provider(provider)?
    };

    if provider_plugin.is_none() && provider_wire == WireFormat::Plugin {
        return Err(ProxyError::unsupported(
            "plugin transport requires a valid provider adapter binding",
        ));
    }
    Ok(transport)
}

/// Resolve model transport, reasoning, parameters, and capabilities without an
/// account-bound probe scope. Scoped probe evidence is intentionally ignored.
pub fn resolve_execution_profile(
    provider: &crate::db::ProviderRow,
    model: &crate::db::ModelRow,
) -> Result<ResolvedExecutionProfile, ProxyError> {
    resolve_execution_profile_for_target(provider, model, None)
}

/// Resolve the execution profile for one concrete account target. Fresh probe
/// evidence is consumed only when provider, account, model, and transport all
/// match the active execution target.
pub fn resolve_execution_profile_for_target(
    provider: &crate::db::ProviderRow,
    model: &crate::db::ModelRow,
    account_id: Option<&str>,
) -> Result<ResolvedExecutionProfile, ProxyError> {
    let discovery = serde_json::from_str::<serde_json::Value>(&model.discovery)
        .unwrap_or_else(|_| serde_json::json!({}));
    let transport = resolve_model_transport_from_discovery(provider, &discovery)?;

    let reasoning_ownership = discovery.get("operator_reasoning_overrides");
    let operator_reasoning_overrides = reasoning_ownership.and_then(serde_json::Value::as_object);
    let owned_reasoning_capability =
        operator_reasoning_overrides.and_then(|overrides| overrides.get("reasoning_capability"));
    let reasoning_capability_owned = owned_reasoning_capability.is_some();
    let mut reasoning = if let Some(value) = owned_reasoning_capability {
        crate::model_capabilities::normalize_reasoning_capability(&serde_json::json!({
            "reasoning_capability": value
        }))
    } else {
        crate::model_capabilities::normalize_reasoning_capability(&discovery)
    };
    let admin_thinking = model.thinking();
    let admin_thinking_configured = !admin_thinking.levels.is_empty()
        || admin_thinking.mode.is_some()
        || admin_thinking.budget_field.is_some()
        || admin_thinking.level_field.is_some();
    let thinking_ownership = discovery.get("operator_thinking_overrides");
    let owned_thinking_map_value = thinking_ownership
        .and_then(serde_json::Value::as_object)
        .and_then(|overrides| overrides.get("thinking_map"));
    let explicitly_owned_thinking_map = owned_thinking_map_value.is_some();
    let owned_thinking_map = owned_thinking_map_value
        .and_then(|value| serde_json::from_value::<ThinkingMap>(value.clone()).ok());
    let thinking_map_owned = explicitly_owned_thinking_map
        || (thinking_ownership.is_none() && admin_thinking_configured);

    // A reasoning-disable probe changes runtime executability, not just
    // descriptive capability metadata. Apply it before deriving the effective
    // map, while keeping an explicit operator thinking map authoritative.
    let reasoning_disable_status = if thinking_map_owned || reasoning_capability_owned {
        None
    } else {
        discovery
            .get("probe_evidence")
            .and_then(serde_json::Value::as_object)
            .and_then(|evidence| evidence.get("reasoning_disable"))
            .and_then(|item| {
                fresh_probe_status(item, &provider.id, &model.id, account_id, &transport)
            })
    };
    if let Some(capability) = reasoning.as_mut() {
        match reasoning_disable_status {
            Some("supported") => capability.can_disable = true,
            Some("unsupported") => capability.can_disable = false,
            _ => {}
        }
    }

    let mut thinking_map = if thinking_map_owned {
        owned_thinking_map.unwrap_or(admin_thinking)
    } else {
        discovery
            .get("thinking_map")
            .and_then(|value| serde_json::from_value::<ThinkingMap>(value.clone()).ok())
            .filter(|map| {
                !map.levels.is_empty()
                    || map.mode.is_some()
                    || map.budget_field.is_some()
                    || map.level_field.is_some()
            })
            .or_else(|| admin_thinking_configured.then_some(admin_thinking))
            .or_else(|| {
                reasoning
                    .as_ref()
                    .and_then(|capability| thinking_map_for_transport(capability, &transport))
            })
            .unwrap_or_default()
    };

    if !thinking_map_owned {
        match reasoning_disable_status {
            Some("unsupported") => {
                thinking_map.levels.remove("off");
            }
            Some("supported") if !thinking_map.level_is_executable("off") => {
                if let Some(candidate) = reasoning
                    .as_ref()
                    .and_then(|capability| thinking_map_for_transport(capability, &transport))
                {
                    let compatible = thinking_map.levels.is_empty()
                        || (thinking_map.mode == candidate.mode
                            && thinking_map.budget_field == candidate.budget_field
                            && thinking_map.level_field == candidate.level_field);
                    if compatible {
                        if thinking_map.levels.is_empty() {
                            thinking_map = candidate;
                        } else if let Some(off) = candidate.levels.get("off").cloned() {
                            thinking_map.levels.insert("off".to_string(), off);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Effort probes are evidence about one canonical level, not about the
    // model's entire reasoning capability. A rejected "max" must never turn
    // "reasoning" false. Fresh unsupported evidence filters discovered
    // mappings, while an explicit operator thinking map remains authoritative.
    let mut verified_reasoning_supported = false;
    if let Some(evidence) = discovery
        .get("probe_evidence")
        .and_then(serde_json::Value::as_object)
    {
        for (key, item) in evidence {
            if let Some(level) = key.strip_prefix("reasoning_effort_") {
                match fresh_probe_status(item, &provider.id, &model.id, account_id, &transport) {
                    Some("supported") => {
                        verified_reasoning_supported = true;
                        if !reasoning_capability_owned && thinking_map.level_is_executable(level) {
                            if let Some(capability) = reasoning.as_mut() {
                                if !capability.levels.iter().any(|candidate| candidate == level) {
                                    capability.levels.push(level.to_string());
                                }
                            }
                        }
                    }
                    Some("unsupported") if !thinking_map_owned && !reasoning_capability_owned => {
                        thinking_map.levels.remove(level);
                        if let Some(capability) = reasoning.as_mut() {
                            capability.levels.retain(|candidate| candidate != level);
                            if capability.default.as_deref() == Some(level) {
                                capability.default = None;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let configured_capabilities = serde_json::from_str::<serde_json::Value>(&model.capabilities)
        .unwrap_or_else(|_| serde_json::json!({}));
    let capability_ownership = discovery.get("operator_capability_overrides");
    let operator_overrides = capability_ownership.and_then(serde_json::Value::as_object);
    let discovered_capabilities = discovery.get("capabilities");
    let probe_capability = |name: &str| {
        let evidence = discovery
            .get("probe_evidence")
            .and_then(|value| value.get(name))?;
        match fresh_probe_status(evidence, &provider.id, &model.id, account_id, &transport) {
            Some("supported") => Some(true),
            Some("unsupported") => Some(false),
            _ => None,
        }
    };
    let capability = |name: &str| {
        if let Some(value) = operator_overrides.and_then(|overrides| overrides.get(name)) {
            // Presence is authoritative even for an explicit null/unknown.
            return value.as_bool();
        }
        // Before capability ownership was tracked, model.capabilities was the
        // only configuration surface. Preserve those legacy values as
        // operator-owned. An explicit ownership object (including {}) opts the
        // model into probe/discovery refinement for unowned capabilities.
        if capability_ownership.is_none() {
            if let Some(value) = configured_capabilities.get(name) {
                return value.as_bool();
            }
        }
        if let Some(value) = probe_capability(name) {
            return Some(value);
        }
        if name == "reasoning" && verified_reasoning_supported {
            return Some(true);
        }
        if let Some(value) = discovered_capabilities.and_then(|value| value.get(name)) {
            // A present null is an explicit unknown observation and must not be
            // collapsed into a legacy false value from the configured model.
            return value.as_bool();
        }
        configured_capabilities
            .get(name)
            .and_then(serde_json::Value::as_bool)
    };
    let mut capabilities = ModelCapabilityFlags {
        text: capability("text"),
        reasoning: capability("reasoning"),
        vision: capability("vision"),
        tool_calling: capability("tool_calling"),
        parallel_tools: capability("parallel_tools"),
        structured_output: capability("structured_output"),
    };
    let integration_features = provider.integration_feature_ceiling().map_err(|error| {
        ProxyError::unsupported(format!("invalid integration feature ceiling: {error}"))
    })?;
    apply_integration_feature_ceiling(&mut capabilities, integration_features.as_ref());

    let mut parameters = model.params();
    let parameter_ownership = discovery.get("operator_parameter_overrides");
    let operator_parameter_overrides = parameter_ownership.and_then(serde_json::Value::as_object);
    if let Some(evidence) = discovery
        .get("probe_evidence")
        .and_then(serde_json::Value::as_object)
    {
        for (key, item) in evidence {
            let Some(parameter) = key.strip_prefix("parameter_") else {
                continue;
            };
            let Some(spec) = parameters.get_mut(parameter) else {
                continue;
            };
            if let Some(supported) = operator_parameter_overrides
                .and_then(|overrides| overrides.get(parameter))
                .and_then(serde_json::Value::as_bool)
            {
                spec.supported = supported;
                continue;
            }
            // Before parameter ownership was tracked, model.parameters was the
            // only configuration surface. Preserve those legacy values as
            // operator-owned. A present ownership object (possibly empty)
            // opts the model into probe refinement for unowned parameters.
            if parameter_ownership.is_none() {
                continue;
            }
            match fresh_probe_status(item, &provider.id, &model.id, account_id, &transport) {
                Some("supported") => spec.supported = true,
                Some("unsupported") => spec.supported = false,
                _ => {}
            }
        }
    }

    Ok(ResolvedExecutionProfile {
        transport,
        reasoning,
        parameters,
        capabilities,
        thinking_map,
    })
}

pub(crate) fn thinking_map_for_transport(
    capability: &ReasoningCapability,
    transport: &TargetTransport,
) -> Option<ThinkingMap> {
    if capability.mode != Some(ReasoningCapabilityMode::Level) {
        return None;
    }
    let level_field = match (transport, capability.upstream_format.as_str()) {
        (
            TargetTransport::OpenAiChat,
            "openai_effort"
            | "responses_effort"
            | "provider_supported_thinking_efforts"
            | "provider_reasoning_supported_efforts"
            | "provider_supported_reasoning_levels",
        ) => "reasoning_effort",
        (
            TargetTransport::OpenAiResponses,
            "openai_effort"
            | "responses_effort"
            | "provider_supported_thinking_efforts"
            | "provider_reasoning_supported_efforts"
            | "provider_supported_reasoning_levels",
        ) => "reasoning.effort",
        (TargetTransport::Gemini, "gemini_thinking_level") => "thinkingConfig.thinkingLevel",
        _ => return None,
    };
    let mut levels: std::collections::HashMap<String, serde_json::Value> = capability
        .levels
        .iter()
        .map(|level| {
            let upstream = capability
                .upstream_level(level)
                .unwrap_or(level)
                .to_string();
            (level.clone(), serde_json::Value::String(upstream))
        })
        .collect();
    if capability.can_disable
        && matches!(
            transport,
            TargetTransport::OpenAiChat | TargetTransport::OpenAiResponses
        )
    {
        let upstream = capability
            .upstream_level("off")
            .unwrap_or("none")
            .to_string();
        levels
            .entry("off".to_string())
            .or_insert_with(|| serde_json::Value::String(upstream));
    }
    Some(ThinkingMap {
        levels,
        mode: Some(crate::types::ThinkingMode::Level),
        budget_field: None,
        level_field: Some(level_field.to_string()),
    })
}

/// Everything an adapter needs to build and authenticate one upstream call.
pub struct UpstreamContext<'a> {
    pub provider: &'a crate::db::ProviderRow,
    pub model: &'a crate::db::ModelRow,
    /// Selected account identity. This is non-secret context for account-scoped
    /// plugin state; built-in adapters do not use it.
    pub account_id: Option<&'a str>,
    /// Raw recognized client-session value, retained inside Kinetix for
    /// routing. PluginManager derives an opaque identity before API-v2 calls.
    pub session_context: Option<&'a str>,
    /// The decrypted credential for the chosen account.
    pub credential: String,
    /// Non-secret metadata supplied separately by the credential strategy.
    pub credential_metadata: Option<&'a crate::credentials::CredentialMetadata>,
}

/// The result of a successful (accepted) upstream call.
pub struct UpstreamStream {
    pub upstream_request_id: Option<String>,
    pub events: BoxStream<'static, Result<StreamEvent, UpstreamFailure>>,
}

#[async_trait]
pub trait Adapter: Send + Sync {
    /// Wire format this adapter speaks.
    fn wire_format(&self) -> &'static str;

    /// Build the outbound URL for a model call.
    fn build_url(&self, ctx: &UpstreamContext<'_>) -> Result<String, ProxyError>;

    /// Whether this adapter owns translation/validation of canonical thinking
    /// levels. Built-in adapters rely on an executable core ThinkingMap;
    /// plugin adapters must explicitly opt in through their manifest.
    fn handles_thinking_translation(&self) -> bool {
        false
    }

    /// Whether this adapter provides an exact upstream token-count API.
    fn supports_count_tokens(&self) -> bool {
        false
    }

    /// Optional exact token-count endpoint for this wire adapter. Returning
    /// `None` means Kinetix must use its documented local estimate.
    fn count_tokens_url(&self, _ctx: &UpstreamContext<'_>) -> Result<Option<String>, ProxyError> {
        Ok(None)
    }

    /// Apply authentication to a request builder (header or query param).
    fn apply_auth(
        &self,
        ctx: &UpstreamContext<'_>,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, UpstreamFailure>;

    /// Translate the internal request into the upstream JSON body.
    fn build_body(
        &self,
        ctx: &UpstreamContext<'_>,
        req: &InternalRequest,
    ) -> Result<serde_json::Value, UpstreamFailure>;

    /// Apply model-aware policy to a same-format passthrough body before dispatch.
    /// Most adapters preserve passthrough content unchanged.
    fn normalize_passthrough_body(
        &self,
        _ctx: &UpstreamContext<'_>,
        _req: &InternalRequest,
        _body: &mut serde_json::Value,
    ) -> Result<(), UpstreamFailure> {
        Ok(())
    }

    /// Parse an upstream non-2xx response into a classified failure.
    fn classify_error(
        &self,
        status: u16,
        body: &str,
        headers: &reqwest::header::HeaderMap,
    ) -> UpstreamFailure;

    /// Parse one SSE `data:` payload (or one JSON object) into stream events.
    /// Returning an empty vec is fine (e.g. keepalive or metadata-only chunk).
    fn parse_stream_chunk(&self, data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure>;

    /// Parse a full non-streaming response body into events (thin fallback path).
    fn parse_full_response(
        &self,
        body: &serde_json::Value,
    ) -> Result<Vec<StreamEvent>, UpstreamFailure>;

    /// The path used for model discovery.
    fn default_models_path(&self) -> &'static str {
        "/models"
    }

    /// Extract model IDs (and suggested limits) from a discovery response.
    fn parse_model_list(&self, body: &serde_json::Value) -> Vec<DiscoveredModel> {
        let _ = body;
        Vec::new()
    }

    /// Declares whether this adapter produces/consumes opaque provider
    /// continuation state (e.g. Gemini `thoughtSignature`) that Kinetix should
    /// automatically persist and replay for translated client protocols.
    ///
    /// Returning `None` (the default) disables automatic persistence/replay.
    /// Built-in native Gemini is the only adapter that opts in today; plugin
    /// adapters must not be assumed Gemini-compatible just because they also
    /// populate `ToolCallStart.signature` — a plugin (e.g. Antigravity) may
    /// multiplex several unrelated opaque-state protocols behind one adapter,
    /// and blindly replaying one family's token into another would be a
    /// correctness/security bug, not just a missed optimization.
    fn opaque_state_target(&self, _model: &crate::db::ModelRow) -> Option<OpaqueStateTarget> {
        None
    }

    /// A protocol-defined placeholder to place on a historical function-call
    /// part whose *real* opaque signature this target cannot carry (for example
    /// a different model in the same protocol family, where the provider only
    /// accepts the originating model's signature). Returning `Some` means the
    /// adapter has a documented, provider-accepted way to keep the call in the
    /// history without inventing real reasoning state; returning `None` (the
    /// default) means the pipeline must fall back to the ordinary
    /// strip/reject portability policy.
    ///
    /// This is a wire-protocol detail, so it lives on the adapter, never in the
    /// pipeline: the value must be exactly what the upstream documents.
    fn opaque_state_placeholder(&self, _model: &crate::db::ModelRow) -> Option<&'static str> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
}

/// An adapter registry: selects the built-in adapter for a wire format.
/// Adding a new wire format (or a plugin) means adding an entry here; no
/// frontend code changes (NFR-5.1).
#[derive(Clone)]
pub struct AdapterRegistry {
    gemini: Arc<dyn Adapter>,
    openai: Arc<dyn Adapter>,
    openai_responses: Arc<dyn Adapter>,
    anthropic: Arc<dyn Adapter>,
    /// Plugin-host-backed adapters keyed by `plugin:<id>/<capability>` (§6.0).
    /// A `DashMap` so a plugin adapter can be registered at enable time without
    /// rebuilding the shared registry.
    plugin: Arc<dashmap::DashMap<String, Arc<dyn Adapter>>>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        AdapterRegistry {
            gemini: Arc::new(crate::adapters::gemini::GeminiAdapter::new()),
            openai: Arc::new(crate::adapters::openai::OpenAiAdapter::new()),
            openai_responses: Arc::new(
                crate::adapters::openai_responses::OpenAiResponsesAdapter::new(),
            ),
            anthropic: Arc::new(crate::adapters::anthropic::AnthropicAdapter::new()),
            plugin: Arc::new(dashmap::DashMap::new()),
        }
    }

    pub fn for_format(&self, format: WireFormat) -> Arc<dyn Adapter> {
        match format {
            WireFormat::Gemini => self.gemini.clone(),
            WireFormat::Openai => self.openai.clone(),
            WireFormat::Anthropic => self.anthropic.clone(),
            WireFormat::Plugin => Arc::new(UnimplementedAdapter { format: "plugin" }),
        }
    }

    /// Resolve the adapter from the already selected target transport. Missing
    /// plugin adapters fail closed before any credential or upstream dispatch.
    pub fn for_transport(
        &self,
        transport: &TargetTransport,
    ) -> Result<Arc<dyn Adapter>, ProxyError> {
        match transport {
            TargetTransport::OpenAiChat => Ok(self.openai.clone()),
            TargetTransport::OpenAiResponses => Ok(self.openai_responses.clone()),
            TargetTransport::Anthropic => Ok(self.anthropic.clone()),
            TargetTransport::Gemini => Ok(self.gemini.clone()),
            TargetTransport::Plugin(reference) => self
                .plugin
                .get(reference)
                .or_else(|| {
                    crate::plugins::PluginRef::parse(reference)
                        .and_then(|parsed| self.plugin.get(&parsed.plugin_id))
                })
                .map(|adapter| adapter.clone())
                .ok_or_else(|| {
                    ProxyError::unsupported(format!(
                        "configured plugin adapter '{reference}' is unavailable"
                    ))
                }),
        }
    }

    /// Select the adapter for a provider. A provider bound to a plugin adapter
    /// (`wire_plugin`, §6.0) resolves to the plugin adapter when the host
    /// provides it and the plugin is usable; otherwise selection fails closed
    /// with an unimplemented-format error (never a silent native fallback).
    pub fn for_provider(&self, provider: &crate::db::ProviderRow) -> Arc<dyn Adapter> {
        if let Some(r) = provider.wire_plugin_ref() {
            let key = r.to_string_ref();
            if let Some(adapter) = self.plugin.get(&key) {
                return adapter.clone();
            }
            // Registration keys by plugin id (one adapter capability per plugin);
            // accept a bare-id reference too.
            if let Some(adapter) = self.plugin.get(&r.plugin_id) {
                return adapter.clone();
            }
            return Arc::new(UnimplementedAdapter {
                format: Box::leak(
                    format!("plugin adapter '{key}' is unavailable").into_boxed_str(),
                ),
            });
        }
        self.for_format(provider.wire())
    }

    /// Register a plugin-backed adapter under its namespaced reference.
    pub fn register_plugin(&self, reference: impl Into<String>, adapter: Arc<dyn Adapter>) {
        self.plugin.insert(reference.into(), adapter);
    }

    /// Remove every adapter registered for a plugin id: the namespaced
    /// `plugin:<id>/<cap>` keys and the bare id. Called when a plugin is
    /// disabled or removed so a stale adapter cannot keep serving traffic (and
    /// cannot hold a live handle to the plugin manager).
    pub fn unregister_plugin(&self, id: &str) {
        let namespaced = format!("plugin:{id}/");
        self.plugin
            .retain(|key, _| key != id && !key.starts_with(&namespaced));
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A no-op adapter used when a provider's wire format has no built-in
/// implementation yet. Keeps the seam honest: configuration is accepted, calls
/// fail with a clear, format-correct error.
pub struct UnimplementedAdapter {
    pub format: &'static str,
}

#[async_trait]
impl Adapter for UnimplementedAdapter {
    fn wire_format(&self) -> &'static str {
        self.format
    }
    fn build_url(&self, _ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
        Err(ProxyError::unsupported(format!(
            "outbound wire format '{}' is not implemented in this build",
            self.format
        )))
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
        _req: &InternalRequest,
    ) -> Result<serde_json::Value, UpstreamFailure> {
        Err(UpstreamFailure {
            kind: crate::types::FailureKind::ServerError,
            status: None,
            retry_after_secs: None,
            message: "adapter not implemented".into(),
            quota_reset_at: None,
        })
    }
    fn classify_error(
        &self,
        status: u16,
        _body: &str,
        _headers: &reqwest::header::HeaderMap,
    ) -> UpstreamFailure {
        UpstreamFailure {
            kind: crate::types::FailureKind::ServerError,
            status: Some(status),
            retry_after_secs: None,
            message: "adapter not implemented".into(),
            quota_reset_at: None,
        }
    }
    fn parse_stream_chunk(&self, _data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        Ok(Vec::new())
    }
    fn parse_full_response(
        &self,
        _body: &serde_json::Value,
    ) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod execution_profile_tests {
    use super::*;

    #[test]
    fn native_adapters_do_not_mutate_auth_for_anonymous_providers() {
        let adapters: Vec<Box<dyn Adapter>> = vec![
            Box::new(openai::OpenAiAdapter::new()),
            Box::new(openai_responses::OpenAiResponsesAdapter::new()),
            Box::new(anthropic::AnthropicAdapter::new()),
            Box::new(gemini::GeminiAdapter::new()),
        ];
        let mut provider = provider();
        provider.auth_scheme = "none".into();
        provider.credential_mode = "none".into();
        let model = model();
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: None,
            session_context: None,
            credential: "must-not-be-sent".into(),
            credential_metadata: None,
        };
        for adapter in &adapters {
            let request = adapter
                .apply_auth(
                    &ctx,
                    reqwest::Client::new()
                        .get("https://example.test/models")
                        .header("x-metadata", "preserved"),
                )
                .unwrap()
                .build()
                .unwrap();
            assert_eq!(request.url().as_str(), "https://example.test/models");
            assert_eq!(request.headers().len(), 1);
            assert_eq!(request.headers()["x-metadata"], "preserved");
            for status in [429, 503] {
                let failure = adapter.classify_error(
                    status,
                    "temporary failure",
                    &reqwest::header::HeaderMap::new(),
                );
                assert_ne!(failure.kind, crate::types::FailureKind::AuthError);
            }
        }
    }

    fn provider() -> crate::db::ProviderRow {
        crate::db::ProviderRow {
            id: "provider".into(),
            name: "provider".into(),
            base_url: "https://example.test/v1".into(),
            wire_format: "openai".into(),
            auth_scheme: "bearer".into(),
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: "{}".into(),
            timeout_ms: 1000,
            capability_mode: "permissive".into(),
            models_path: None,
            rate_limit_rules: "{}".into(),
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
            connection_parameters: None,
            connection_parameters_attested: None,
        }
    }

    fn model() -> crate::db::ModelRow {
        crate::db::ModelRow {
            id: "model".into(),
            provider_id: "provider".into(),
            upstream_id: "upstream".into(),
            display_name: "model".into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: None,
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: "{}".into(),
            thinking_map: "{}".into(),
            extra_request: "{}".into(),
            discovery: "{}".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            opaque_state_plugin: String::new(),
        }
    }

    #[test]
    fn transport_precedence_is_override_then_discovery_then_provider() {
        let provider = provider();
        let mut model = model();
        model.discovery = serde_json::json!({
            "transport": {"format": "anthropic"},
            "configured_transport": "openai-responses"
        })
        .to_string();
        assert_eq!(
            resolve_execution_profile(&provider, &model)
                .unwrap()
                .transport,
            TargetTransport::OpenAiResponses
        );

        model.discovery = serde_json::json!({"transport":{"format":"anthropic"}}).to_string();
        assert_eq!(
            resolve_execution_profile(&provider, &model)
                .unwrap()
                .transport,
            TargetTransport::Anthropic
        );

        model.discovery = serde_json::json!({
            "transport": {"format": "openai-chat"}
        })
        .to_string();
        assert_eq!(
            resolve_execution_profile(&provider, &model)
                .unwrap()
                .transport,
            TargetTransport::OpenAiChat
        );

        model.discovery = "{}".into();
        assert_eq!(
            resolve_execution_profile(&provider, &model)
                .unwrap()
                .transport,
            TargetTransport::OpenAiChat
        );
    }

    #[test]
    fn model_plugin_transports_remain_runtime_supported_without_provider_binding() {
        let provider = provider();
        let mut model = model();
        model.discovery = serde_json::json!({
            "configured_transport": "plugin:other/adapter"
        })
        .to_string();
        assert_eq!(
            resolve_execution_profile(&provider, &model)
                .unwrap()
                .transport,
            TargetTransport::Plugin("plugin:other/adapter".into())
        );

        model.discovery = serde_json::json!({
            "transport": {"format": "plugin:other/adapter"}
        })
        .to_string();
        assert_eq!(
            resolve_execution_profile(&provider, &model)
                .unwrap()
                .transport,
            TargetTransport::Plugin("plugin:other/adapter".into())
        );

        let mut bound_provider = provider;
        bound_provider.wire_plugin = "plugin:trusted/adapter".into();
        model.discovery = serde_json::json!({
            "configured_transport": "plugin:trusted/adapter"
        })
        .to_string();
        assert_eq!(
            resolve_execution_profile(&bound_provider, &model)
                .unwrap()
                .transport,
            TargetTransport::Plugin("plugin:trusted/adapter".into())
        );
    }

    #[test]
    fn invalid_explicit_transport_and_provider_wire_fail_closed() {
        let provider = provider();
        let mut model = model();
        model.discovery = serde_json::json!({
            "transport": {"format": "not-a-transport"}
        })
        .to_string();
        assert!(resolve_execution_profile(&provider, &model).is_err());

        model.discovery = serde_json::json!({
            "configured_transport": "not-a-transport"
        })
        .to_string();
        assert!(resolve_execution_profile(&provider, &model).is_err());

        model.discovery = serde_json::json!({
            "configured_transport": "openai-responses"
        })
        .to_string();
        let mut provider = provider;
        provider.wire_format = "typo-openai".into();
        assert!(resolve_execution_profile(&provider, &model).is_err());

        provider.wire_format = "plugin".into();
        assert!(resolve_execution_profile(&provider, &model).is_err());
    }

    #[test]
    fn expired_probe_is_not_authoritative_but_remains_in_discovery_history() {
        let provider = provider();
        let mut model = model();
        let evidence = serde_json::json!({
            "status": "supported",
            "verified_at": "2025-01-01T00:00:00Z",
            "fresh_until": "2025-01-02T00:00:00Z",
            "scope": {
                "provider_id": "provider",
                "account_id": "account",
                "model_id": "model",
                "transport": "openai"
            }
        });
        model.discovery = serde_json::json!({
            "probe_evidence": { "tool_calling": [evidence] }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account")).unwrap();
        assert_eq!(profile.capabilities.tool_calling, None);
        let history = serde_json::from_str::<serde_json::Value>(&model.discovery).unwrap();
        assert_eq!(
            history["probe_evidence"]["tool_calling"][0]["status"],
            "supported"
        );
    }

    #[test]
    fn keeps_absent_capabilities_unknown_and_explicit_values() {
        let provider = provider();
        let mut model = model();
        let profile = resolve_execution_profile(&provider, &model).unwrap();
        assert_eq!(profile.capabilities.text, None);
        assert_eq!(profile.capabilities.vision, None);

        model.capabilities = serde_json::json!({"text": false}).to_string();
        model.discovery = serde_json::json!({
            "capabilities": {"vision": true}
        })
        .to_string();
        let profile = resolve_execution_profile(&provider, &model).unwrap();
        assert_eq!(profile.capabilities.text, Some(false));
        assert_eq!(profile.capabilities.vision, Some(true));
        assert_eq!(profile.capabilities.tool_calling, None);

        model.discovery = serde_json::json!({
            "operator_capability_overrides": {},
            "capabilities": {"text": null, "vision": true}
        })
        .to_string();
        let profile = resolve_execution_profile(&provider, &model).unwrap();
        assert_eq!(profile.capabilities.text, None);
    }

    #[test]
    fn integration_feature_ceiling_vetoes_model_capabilities() {
        let mut provider = provider();
        provider.integration_features = Some(
            serde_json::json!({
                "schema_version": 1,
                "streaming": true,
                "tools": false,
                "parallel_tools": false,
                "vision": false,
                "reasoning": false,
                "structured_output": false,
                "model_discovery": true,
                "quota_probe": false,
                "health_probe": false
            })
            .to_string(),
        );
        let mut model = model();
        model.capabilities = serde_json::json!({
            "vision": true,
            "tool_calling": true,
            "parallel_tools": true,
            "reasoning": true,
            "structured_output": true
        })
        .to_string();

        let profile = resolve_execution_profile(&provider, &model).unwrap();
        assert_eq!(profile.capabilities.vision, Some(false));
        assert_eq!(profile.capabilities.tool_calling, Some(false));
        assert_eq!(profile.capabilities.parallel_tools, Some(false));
        assert_eq!(profile.capabilities.reasoning, Some(false));
        assert_eq!(profile.capabilities.structured_output, Some(false));
    }

    #[test]
    fn integration_feature_ceiling_does_not_invent_model_capabilities() {
        let mut provider = provider();
        provider.integration_features = Some(
            serde_json::json!({
                "schema_version": 1,
                "streaming": true,
                "tools": true,
                "parallel_tools": true,
                "vision": true,
                "reasoning": true,
                "structured_output": true,
                "model_discovery": true,
                "quota_probe": true,
                "health_probe": true
            })
            .to_string(),
        );

        let profile = resolve_execution_profile(&provider, &model()).unwrap();
        assert_eq!(profile.capabilities.vision, None);
        assert_eq!(profile.capabilities.tool_calling, None);
        assert_eq!(profile.capabilities.parallel_tools, None);
        assert_eq!(profile.capabilities.reasoning, None);
        assert_eq!(profile.capabilities.structured_output, None);
    }

    #[test]
    fn legacy_configured_capability_remains_operator_owned_against_probe() {
        let provider = provider();
        let mut model = model();
        model.capabilities = serde_json::json!({"tool_calling": false}).to_string();
        model.discovery = serde_json::json!({
            "probe_evidence": {
                "tool_calling": {
                    "status": "supported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert_eq!(profile.capabilities.tool_calling, Some(false));
    }

    #[test]
    fn explicit_empty_capability_ownership_allows_probe_refinement() {
        let provider = provider();
        let mut model = model();
        model.capabilities = serde_json::json!({"tool_calling": false}).to_string();
        model.discovery = serde_json::json!({
            "operator_capability_overrides": {},
            "probe_evidence": {
                "tool_calling": {
                    "status": "supported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert_eq!(profile.capabilities.tool_calling, Some(true));
    }

    #[test]
    fn pinned_capability_override_beats_later_probe() {
        let provider = provider();
        let mut model = model();
        model.capabilities = serde_json::json!({"tool_calling": false}).to_string();
        model.discovery = serde_json::json!({
            "operator_capability_overrides": {
                "tool_calling": false
            },
            "probe_evidence": {
                "tool_calling": {
                    "status": "supported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert_eq!(profile.capabilities.tool_calling, Some(false));
    }

    #[test]
    fn scoped_probe_evidence_isolated_by_account_and_transport() {
        let provider = provider();
        let mut model = model();
        model.discovery = serde_json::json!({
            "probe_evidence": {
                "tool_calling": {
                    "status": "unsupported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let account_a =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert_eq!(account_a.capabilities.tool_calling, Some(false));

        let account_b =
            resolve_execution_profile_for_target(&provider, &model, Some("account-b")).unwrap();
        assert_eq!(account_b.capabilities.tool_calling, None);

        model.discovery = serde_json::json!({
            "configured_transport": "openai-responses",
            "probe_evidence": {
                "tool_calling": {
                    "status": "unsupported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();
        let responses =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert_eq!(responses.transport, TargetTransport::OpenAiResponses);
        assert_eq!(responses.capabilities.tool_calling, None);
    }

    #[test]
    fn scoped_probe_evidence_can_coexist_across_accounts_and_transports() {
        let provider = provider();
        let mut model = model();
        let entries = serde_json::json!([
            {
                "status": "supported",
                "fresh_until": "2999-01-01T00:00:00Z",
                "scope": {
                    "provider_id": "provider",
                    "account_id": "account-a",
                    "model_id": "model",
                    "transport": "openai"
                }
            },
            {
                "status": "unsupported",
                "fresh_until": "2999-01-01T00:00:00Z",
                "scope": {
                    "provider_id": "provider",
                    "account_id": "account-b",
                    "model_id": "model",
                    "transport": "openai"
                }
            },
            {
                "status": "unsupported",
                "fresh_until": "2999-01-01T00:00:00Z",
                "scope": {
                    "provider_id": "provider",
                    "account_id": "account-a",
                    "model_id": "model",
                    "transport": "openai-responses"
                }
            }
        ]);

        model.discovery = serde_json::json!({
            "probe_evidence": {
                "tool_calling": entries.clone()
            }
        })
        .to_string();
        let account_a_chat =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        let account_b_chat =
            resolve_execution_profile_for_target(&provider, &model, Some("account-b")).unwrap();
        assert_eq!(account_a_chat.capabilities.tool_calling, Some(true));
        assert_eq!(account_b_chat.capabilities.tool_calling, Some(false));

        model.discovery = serde_json::json!({
            "configured_transport": "openai-responses",
            "probe_evidence": {
                "tool_calling": entries
            }
        })
        .to_string();
        let account_a_responses =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert_eq!(
            account_a_responses.transport,
            TargetTransport::OpenAiResponses
        );
        assert_eq!(account_a_responses.capabilities.tool_calling, Some(false));
    }

    #[test]
    fn inconclusive_probe_does_not_override_fresh_conclusive_evidence() {
        let provider = provider();
        let mut model = model();
        model.discovery = serde_json::json!({
            "probe_evidence": {
                "tool_calling": [
                    {
                        "status": "supported",
                        "fresh_until": "2999-01-01T00:00:00Z",
                        "scope": {
                            "provider_id": "provider",
                            "account_id": "account-a",
                            "model_id": "model",
                            "transport": "openai"
                        }
                    },
                    {
                        "status": "inconclusive",
                        "fresh_until": "2999-01-01T00:00:00Z",
                        "scope": {
                            "provider_id": "provider",
                            "account_id": "account-a",
                            "model_id": "model",
                            "transport": "openai"
                        }
                    }
                ]
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert_eq!(profile.capabilities.tool_calling, Some(true));
    }

    #[test]
    fn inconclusive_reasoning_probe_keeps_discovered_level_executable() {
        let provider = provider();
        let mut model = model();
        model.discovery = serde_json::json!({
            "thinking_map": {
                "levels": {"high": "high"},
                "mode": "level",
                "level_field": "reasoning_effort"
            },
            "probe_evidence": {
                "reasoning_effort_high": {
                    "status": "inconclusive",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert!(profile.thinking_map.level_is_executable("high"));
    }

    #[test]
    fn scoped_parameter_probe_updates_only_matching_target() {
        let provider = provider();
        let mut model = model();
        model.parameters = serde_json::json!({
            "temperature": {
                "supported": true,
                "policy": "reject"
            }
        })
        .to_string();
        model.discovery = serde_json::json!({
            "operator_parameter_overrides": {},
            "probe_evidence": {
                "parameter_temperature": {
                    "status": "unsupported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let matching =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert!(!matching.parameters["temperature"].supported);

        let other =
            resolve_execution_profile_for_target(&provider, &model, Some("account-b")).unwrap();
        assert!(other.parameters["temperature"].supported);
    }

    #[test]
    fn legacy_configured_parameter_is_operator_owned() {
        let provider = provider();
        let mut model = model();
        model.parameters = serde_json::json!({
            "temperature": {
                "supported": false,
                "policy": "reject"
            }
        })
        .to_string();
        model.discovery = serde_json::json!({
            "probe_evidence": {
                "parameter_temperature": {
                    "status": "supported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert!(!profile.parameters["temperature"].supported);
    }

    #[test]
    fn operator_parameter_override_beats_supported_probe() {
        let provider = provider();
        let mut model = model();
        model.parameters = serde_json::json!({
            "temperature": {
                "supported": false,
                "policy": "reject"
            }
        })
        .to_string();
        model.discovery = serde_json::json!({
            "operator_parameter_overrides": {
                "temperature": false
            },
            "probe_evidence": {
                "parameter_temperature": {
                    "status": "supported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert!(!profile.parameters["temperature"].supported);
    }

    #[test]
    fn operator_parameter_override_beats_unsupported_probe() {
        let provider = provider();
        let mut model = model();
        model.parameters = serde_json::json!({
            "temperature": {
                "supported": true,
                "policy": "reject"
            }
        })
        .to_string();
        model.discovery = serde_json::json!({
            "operator_parameter_overrides": {
                "temperature": true
            },
            "probe_evidence": {
                "parameter_temperature": {
                    "status": "unsupported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();
        assert!(profile.parameters["temperature"].supported);
    }

    #[test]
    fn responses_transport_uses_responses_reasoning_path_and_admin_mapping_wins() {
        let provider = provider();
        let mut model = model();
        model.discovery = serde_json::json!({
            "transport": {"format": "openai-responses"},
            "reasoning_capability": {
                "mode": "level",
                "levels": ["low", "high"],
                "can_disable": false,
                "upstream_format": "responses_effort"
            }
        })
        .to_string();
        let discovered = resolve_execution_profile(&provider, &model).unwrap();
        assert_eq!(
            discovered.thinking_map.level_field.as_deref(),
            Some("reasoning.effort")
        );

        model.thinking_map = serde_json::json!({
            "levels": {"high": "vendor_high"},
            "mode": "level",
            "level_field": "custom.reasoning"
        })
        .to_string();
        let configured = resolve_execution_profile(&provider, &model).unwrap();
        assert_eq!(
            configured.thinking_map.level_field.as_deref(),
            Some("custom.reasoning")
        );
        assert_eq!(
            configured.thinking_map.levels.get("high"),
            Some(&serde_json::json!("vendor_high"))
        );
    }

    #[test]
    fn imported_thinking_map_is_refined_by_scoped_reasoning_probes() {
        let provider = provider();
        let mut model = model();
        let imported_map = serde_json::json!({
            "mode": "level",
            "levels": {"low": "low", "high": "high", "max": "max"},
            "level_field": "reasoning_effort"
        });
        model.thinking_map = imported_map.to_string();
        model.discovery = serde_json::json!({
            "operator_thinking_overrides": {},
            "reasoning_capability": {
                "mode": "level",
                "levels": ["low", "high", "max"],
                "can_disable": false,
                "upstream_format": "openai_effort"
            },
            "thinking_map": imported_map,
            "probe_evidence": {
                "reasoning_effort_max": {
                    "status": "unsupported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                },
                "reasoning_disable": {
                    "status": "supported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();

        assert!(profile.thinking_map.level_is_executable("low"));
        assert!(profile.thinking_map.level_is_executable("high"));
        assert!(!profile.thinking_map.level_is_executable("max"));
        assert!(profile.thinking_map.level_is_executable("off"));
        let reasoning = profile.reasoning.as_ref().unwrap();
        assert!(!reasoning.levels.iter().any(|level| level == "max"));
        assert!(reasoning.can_disable);
    }

    #[test]
    fn operator_owned_thinking_map_beats_reasoning_probes() {
        let provider = provider();
        let mut model = model();
        let owned_map = serde_json::json!({
            "mode": "level",
            "levels": {"low": "low", "high": "high", "max": "vendor-max"},
            "level_field": "reasoning_effort"
        });
        model.thinking_map = owned_map.to_string();
        model.discovery = serde_json::json!({
            "operator_thinking_overrides": {
                "thinking_map": owned_map
            },
            "reasoning_capability": {
                "mode": "level",
                "levels": ["low", "high", "max"],
                "can_disable": false,
                "upstream_format": "openai_effort"
            },
            "probe_evidence": {
                "reasoning_effort_max": {
                    "status": "unsupported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                },
                "reasoning_disable": {
                    "status": "supported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();

        assert_eq!(
            profile.thinking_map.levels.get("max"),
            Some(&serde_json::json!("vendor-max"))
        );
        assert!(!profile.thinking_map.level_is_executable("off"));
        assert!(!profile.reasoning.as_ref().unwrap().can_disable);
    }

    #[test]
    fn reasoning_disable_supported_probe_enables_executable_off_mapping() {
        let provider = provider();
        let mut model = model();
        model.discovery = serde_json::json!({
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
            "probe_evidence": {
                "reasoning_disable": {
                    "status": "supported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();

        assert!(profile.reasoning.as_ref().unwrap().can_disable);
        assert!(profile.thinking_map.level_is_executable("off"));
        assert_eq!(
            profile.thinking_map.levels.get("off"),
            Some(&serde_json::json!("none"))
        );
    }

    #[test]
    fn reasoning_disable_unsupported_probe_removes_executable_off_mapping() {
        let provider = provider();
        let mut model = model();
        model.discovery = serde_json::json!({
            "reasoning_capability": {
                "mode": "level",
                "levels": ["off", "low", "high"],
                "can_disable": true,
                "upstream_format": "openai_effort",
                "upstream_levels": {"off": "none"}
            },
            "thinking_map": {
                "mode": "level",
                "levels": {"off": "none", "low": "low", "high": "high"},
                "level_field": "reasoning_effort"
            },
            "probe_evidence": {
                "reasoning_disable": {
                    "status": "unsupported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();

        assert!(!profile.reasoning.as_ref().unwrap().can_disable);
        assert!(!profile.thinking_map.level_is_executable("off"));
        assert!(!profile.thinking_map.levels.contains_key("off"));
    }

    #[test]
    fn operator_owned_reasoning_capability_beats_disable_probe() {
        let provider = provider();
        let mut model = model();
        let owned = serde_json::json!({
            "mode": "level",
            "levels": ["low", "high"],
            "can_disable": true,
            "upstream_format": "openai_effort"
        });
        model.discovery = serde_json::json!({
            "reasoning_capability": owned.clone(),
            "operator_reasoning_overrides": {
                "reasoning_capability": owned
            },
            "probe_evidence": {
                "reasoning_disable": {
                    "status": "unsupported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();

        let reasoning = profile.reasoning.as_ref().unwrap();
        assert!(reasoning.can_disable);
        assert!(reasoning.levels.iter().any(|level| level == "high"));
        assert!(profile.thinking_map.level_is_executable("off"));
    }

    #[test]
    fn operator_owned_reasoning_capability_beats_effort_probe() {
        let provider = provider();
        let mut model = model();
        let owned = serde_json::json!({
            "mode": "level",
            "levels": ["low", "high"],
            "can_disable": true,
            "upstream_format": "openai_effort"
        });
        model.discovery = serde_json::json!({
            "reasoning_capability": owned.clone(),
            "operator_reasoning_overrides": {
                "reasoning_capability": owned
            },
            "probe_evidence": {
                "reasoning_effort_high": {
                    "status": "unsupported",
                    "fresh_until": "2999-01-01T00:00:00Z",
                    "scope": {
                        "provider_id": "provider",
                        "account_id": "account-a",
                        "model_id": "model",
                        "transport": "openai"
                    }
                }
            }
        })
        .to_string();

        let profile =
            resolve_execution_profile_for_target(&provider, &model, Some("account-a")).unwrap();

        let reasoning = profile.reasoning.as_ref().unwrap();
        assert!(reasoning.can_disable);
        assert!(reasoning.levels.iter().any(|level| level == "high"));
        assert!(profile.thinking_map.level_is_executable("high"));
    }
}
