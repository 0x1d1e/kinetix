//! Model capability metadata: the versioned capability documents plugins and
//! discovery report (V1-V3), their validation, and their normalization into
//! capability flags, reasoning capability, and executable thinking maps.

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningCapabilityMode {
    Toggle,
    Level,
    ManualBudget,
    Adaptive,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ReasoningCapability {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ReasoningCapabilityMode>,
    pub levels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    pub can_disable: bool,
    pub upstream_format: String,
    #[serde(skip_serializing)]
    upstream_levels: std::collections::HashMap<String, String>,
}

impl ReasoningCapability {
    /// The upstream wire value for a canonical level, when it differs.
    pub(crate) fn upstream_level(&self, level: &str) -> Option<&str> {
        self.upstream_levels.get(level).map(String::as_str)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCapabilityFlags {
    pub text: Option<bool>,
    pub reasoning: Option<bool>,
    pub vision: Option<bool>,
    pub tool_calling: Option<bool>,
    pub parallel_tools: Option<bool>,
    pub structured_output: Option<bool>,
}

/// Intersect model observations with the integration's declared support ceiling.
/// A false integration flag vetoes support; a true flag never invents model support.
pub(crate) fn apply_integration_feature_ceiling(
    capabilities: &mut ModelCapabilityFlags,
    ceiling: Option<&crate::plugins::types::IntegrationFeaturesV1>,
) {
    let Some(ceiling) = ceiling else {
        return;
    };
    if !ceiling.tools {
        capabilities.tool_calling = Some(false);
    }
    if !ceiling.parallel_tools {
        capabilities.parallel_tools = Some(false);
    }
    if !ceiling.vision {
        capabilities.vision = Some(false);
    }
    if !ceiling.reasoning {
        capabilities.reasoning = Some(false);
    }
    if !ceiling.structured_output {
        capabilities.structured_output = Some(false);
    }
    if capabilities.parallel_tools == Some(true) {
        capabilities.parallel_tools = match capabilities.tool_calling {
            Some(true) => Some(true),
            Some(false) => Some(false),
            None => None,
        };
    }
}

fn canonical_reasoning_levels(
    value: &serde_json::Value,
) -> (Vec<String>, std::collections::HashMap<String, String>) {
    let Some(values) = value.as_array() else {
        return (Vec::new(), std::collections::HashMap::new());
    };
    let mut levels = Vec::new();
    let mut upstream_levels = std::collections::HashMap::new();
    for value in values {
        let Some(raw) = value.as_str() else {
            continue;
        };
        let normalized = match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => "off",
            "minimal" => "minimal",
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "xhigh" | "x-high" | "x_high" | "extra_high" => "xhigh",
            "max" => "max",
            _ => continue,
        };
        if !levels.iter().any(|level| level == normalized) {
            levels.push(normalized.to_string());
            upstream_levels.insert(normalized.to_string(), raw.to_string());
        }
    }
    (levels, upstream_levels)
}

fn reasoning_capability(
    levels: Vec<String>,
    upstream_levels: std::collections::HashMap<String, String>,
    mode: ReasoningCapabilityMode,
    upstream_format: impl Into<String>,
    can_disable: Option<bool>,
) -> Option<ReasoningCapability> {
    if levels.is_empty() {
        return None;
    }
    let inferred_disable = levels.iter().any(|level| level == "off");
    Some(ReasoningCapability {
        mode: Some(mode),
        levels,
        default: None,
        can_disable: can_disable.unwrap_or(inferred_disable),
        upstream_format: upstream_format.into(),
        upstream_levels,
    })
}

pub fn reasoning_metadata_declared(metadata: &serde_json::Value) -> bool {
    if metadata.get("reasoning_capability").is_some() {
        return true;
    }
    if metadata
        .get("reasoning")
        .is_some_and(serde_json::Value::is_boolean)
    {
        return true;
    }
    if metadata
        .get("reasoning")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|reasoning| {
            reasoning.contains_key("supported")
                || reasoning.contains_key("mode")
                || reasoning.contains_key("levels")
        })
    {
        return true;
    }
    [
        "/supportedThinkingEfforts",
        "/reasoning/supported_efforts",
        "/supported_reasoning_levels",
        "/thinking/levels",
        "/capabilities/effort_tiers",
    ]
    .iter()
    .any(|pointer| metadata.pointer(pointer).is_some())
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCapabilitiesV1 {
    schema_version: u32,
    transport: Option<TransportCapabilityV1>,
    text: Option<SupportCapabilityV1>,
    reasoning: Option<PluginReasoningCapabilityV1>,
    tools: Option<SupportCapabilityV1>,
    vision: Option<VisionCapabilityV1>,
    structured_output: Option<SupportCapabilityV1>,
    /// Canonical plugin-supplied pricing. Kept in the existing v1 JSON
    /// envelope so old WIT components remain ABI-compatible.
    #[allow(dead_code)]
    prices: Option<serde_json::Value>,
    /// Optional directional modality metadata from plugin discovery.
    #[allow(dead_code)]
    modalities: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCapabilitiesV2 {
    schema_version: u32,
    transport: Option<TransportCapabilityV1>,
    text: Option<SupportCapabilityV1>,
    reasoning: Option<PluginReasoningCapabilityV1>,
    tools: Option<SupportCapabilityV1>,
    vision: Option<VisionCapabilityV1>,
    structured_output: Option<SupportCapabilityV1>,
    #[allow(dead_code)]
    prices: Option<serde_json::Value>,
    #[allow(dead_code)]
    modalities: Option<serde_json::Value>,
    identity: Option<ModelIdentityV2>,
    opaque_state: Option<OpaqueStateCapabilityV1>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCapabilitiesV3 {
    schema_version: u32,
    transport: Option<ModelTransportCapabilityV3>,
    text: Option<SupportCapabilityV1>,
    reasoning: Option<PluginReasoningCapabilityV1>,
    tools: Option<SupportCapabilityV1>,
    parallel_tools: Option<SupportCapabilityV1>,
    vision: Option<VisionCapabilityV1>,
    structured_output: Option<SupportCapabilityV1>,
    #[allow(dead_code)]
    prices: Option<serde_json::Value>,
    #[allow(dead_code)]
    modalities: Option<serde_json::Value>,
    identity: Option<ModelIdentityV2>,
    opaque_state: Option<OpaqueStateCapabilityV1>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelTransportCapabilityV3 {
    format: String,
    endpoint: Option<String>,
    #[serde(default)]
    alternatives: Vec<ModelTransportOptionV3>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelTransportOptionV3 {
    format: String,
    endpoint: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelIdentityV2 {
    pub canonical_model_id: String,
    pub variant: Option<ProviderVariantV1>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderVariantV1 {
    pub kind: ProviderVariantKind,
    pub id: String,
    pub reasoning_level: Option<PluginReasoningLevelV1>,
    pub fixed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderVariantKind {
    ReasoningTier,
    ProviderAlias,
    ThinkingVariant,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpaqueStateCapabilityV1 {
    pub kind: OpaqueStateCapabilityKind,
    pub family: String,
    pub encoding_version: u32,
    pub placeholder_strategy: Option<OpaqueStatePlaceholderStrategy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OpaqueStateCapabilityKind {
    GeminiThoughtSignature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OpaqueStatePlaceholderStrategy {
    Gemini3SkipValidator,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TransportCapabilityV1 {
    format: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SupportCapabilityV1 {
    supported: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VisionCapabilityV1 {
    input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum PluginReasoningModeV1 {
    Toggle,
    Level,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum PluginReasoningLevelV1 {
    #[serde(rename = "minimal")]
    Minimal,
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    #[serde(rename = "max")]
    Max,
}

impl PluginReasoningLevelV1 {
    fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginReasoningCapabilityV1 {
    supported: bool,
    mode: Option<PluginReasoningModeV1>,
    levels: Option<Vec<PluginReasoningLevelV1>>,
    default: Option<PluginReasoningLevelV1>,
    can_disable: Option<bool>,
}

impl PluginReasoningCapabilityV1 {
    fn is_valid(&self) -> bool {
        if !self.supported {
            return self.mode.is_none()
                && self.levels.is_none()
                && self.default.is_none()
                && self.can_disable.is_none();
        }

        match self.mode {
            None => self.levels.is_none() && self.default.is_none(),
            Some(PluginReasoningModeV1::Toggle) => self.levels.is_none() && self.default.is_none(),
            Some(PluginReasoningModeV1::Level) => {
                let Some(levels) = self.levels.as_ref() else {
                    return false;
                };
                if levels.is_empty() {
                    return false;
                }
                let unique: std::collections::HashSet<_> = levels.iter().copied().collect();
                if unique.len() != levels.len() {
                    return false;
                }
                self.default
                    .map(|default| levels.contains(&default))
                    .unwrap_or(true)
            }
        }
    }
}

impl ModelCapabilitiesV1 {
    fn is_valid(&self) -> bool {
        if self.schema_version != 1 {
            return false;
        }
        if self
            .transport
            .as_ref()
            .is_some_and(|transport| transport.format.trim().is_empty())
        {
            return false;
        }
        self.reasoning
            .as_ref()
            .map(PluginReasoningCapabilityV1::is_valid)
            .unwrap_or(true)
    }
}

fn parse_model_capabilities_v1(metadata: &serde_json::Value) -> Option<ModelCapabilitiesV1> {
    let metadata: ModelCapabilitiesV1 = serde_json::from_value(metadata.clone()).ok()?;
    metadata.is_valid().then_some(metadata)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

impl ModelIdentityV2 {
    fn is_valid(&self) -> bool {
        let canonical = self.canonical_model_id.trim();
        if canonical.is_empty()
            || !canonical.contains('/')
            || canonical.chars().any(char::is_control)
        {
            return false;
        }

        self.variant
            .as_ref()
            .map(ProviderVariantV1::is_valid)
            .unwrap_or(true)
    }
}

impl ProviderVariantV1 {
    fn is_valid(&self) -> bool {
        if self.id.trim().is_empty() || self.id.chars().any(char::is_control) {
            return false;
        }
        if self.reasoning_level.is_some() && self.kind != ProviderVariantKind::ReasoningTier {
            return false;
        }
        true
    }
}

impl OpaqueStateCapabilityV1 {
    fn is_valid(&self) -> bool {
        if !valid_identifier(&self.family) || self.encoding_version == 0 {
            return false;
        }

        match self.placeholder_strategy {
            Some(OpaqueStatePlaceholderStrategy::Gemini3SkipValidator) => {
                self.kind == OpaqueStateCapabilityKind::GeminiThoughtSignature
            }
            None => true,
        }
    }
}

impl ModelCapabilitiesV2 {
    fn is_valid(&self) -> bool {
        if self.schema_version != 2 {
            return false;
        }
        if self
            .transport
            .as_ref()
            .is_some_and(|transport| transport.format.trim().is_empty())
        {
            return false;
        }
        if !self
            .reasoning
            .as_ref()
            .map(PluginReasoningCapabilityV1::is_valid)
            .unwrap_or(true)
        {
            return false;
        }
        if !self
            .identity
            .as_ref()
            .map(ModelIdentityV2::is_valid)
            .unwrap_or(true)
        {
            return false;
        }
        self.opaque_state
            .as_ref()
            .map(OpaqueStateCapabilityV1::is_valid)
            .unwrap_or(true)
    }
}

fn parse_model_capabilities_v2(metadata: &serde_json::Value) -> Option<ModelCapabilitiesV2> {
    let metadata: ModelCapabilitiesV2 = serde_json::from_value(metadata.clone()).ok()?;
    metadata.is_valid().then_some(metadata)
}

fn valid_transport_format(format: &str) -> bool {
    matches!(
        format,
        "openai-chat" | "openai-responses" | "anthropic" | "gemini" | "plugin-native"
    )
}

fn valid_transport_endpoint(endpoint: Option<&str>) -> bool {
    endpoint.is_none_or(|endpoint| {
        endpoint.starts_with('/')
            && !endpoint.starts_with("//")
            && endpoint.trim() == endpoint
            && !endpoint.chars().any(|character| {
                character.is_control()
                    || character.is_whitespace()
                    || matches!(character, '?' | '#' | '\\')
            })
            && !endpoint
                .split('/')
                .any(|segment| matches!(segment, "." | ".."))
    })
}

fn valid_capability_prices(prices: Option<&serde_json::Value>) -> bool {
    let Some(prices) = prices else {
        return true;
    };
    let Some(prices) = prices.as_object() else {
        return false;
    };
    [
        "input_per_1m",
        "output_per_1m",
        "cached_per_1m",
        "cache_write_per_1m",
        "thinking_per_1m",
    ]
    .iter()
    .all(|key| {
        let Some(value) = prices.get(*key) else {
            return true;
        };
        if value.is_null() {
            return true;
        }
        value
            .as_f64()
            .is_some_and(|value| value.is_finite() && value >= 0.0)
    })
}

impl ModelCapabilitiesV3 {
    fn is_valid(&self) -> bool {
        if self.schema_version != 3
            || self
                .parallel_tools
                .as_ref()
                .is_some_and(|parallel| parallel.supported)
                && !self.tools.as_ref().is_some_and(|tools| tools.supported)
            || !self
                .reasoning
                .as_ref()
                .map(PluginReasoningCapabilityV1::is_valid)
                .unwrap_or(true)
            || !self
                .identity
                .as_ref()
                .map(ModelIdentityV2::is_valid)
                .unwrap_or(true)
            || !self
                .opaque_state
                .as_ref()
                .map(OpaqueStateCapabilityV1::is_valid)
                .unwrap_or(true)
            || !valid_capability_prices(self.prices.as_ref())
        {
            return false;
        }
        let Some(transport) = &self.transport else {
            return true;
        };
        if !valid_transport_format(&transport.format)
            || !valid_transport_endpoint(transport.endpoint.as_deref())
        {
            return false;
        }
        let preferred = ModelTransportOptionV3 {
            format: transport.format.clone(),
            endpoint: transport.endpoint.clone(),
        };
        let mut options = std::collections::HashSet::from([preferred]);
        transport.alternatives.iter().all(|alternative| {
            valid_transport_format(&alternative.format)
                && valid_transport_endpoint(alternative.endpoint.as_deref())
                && options.insert(alternative.clone())
        })
    }
}

fn parse_model_capabilities_v3(metadata: &serde_json::Value) -> Option<ModelCapabilitiesV3> {
    let metadata: ModelCapabilitiesV3 = serde_json::from_value(metadata.clone()).ok()?;
    metadata.is_valid().then_some(metadata)
}

pub(crate) fn validated_model_capabilities_v3(
    metadata: &serde_json::Value,
) -> Option<serde_json::Value> {
    parse_model_capabilities_v3(metadata)?;
    Some(metadata.clone())
}

fn plugin_schema_version(metadata: &serde_json::Value) -> Option<u64> {
    metadata.get("schema_version")?.as_u64()
}

fn capability_flags(
    text: Option<&SupportCapabilityV1>,
    reasoning: Option<&PluginReasoningCapabilityV1>,
    vision: Option<&VisionCapabilityV1>,
    tools: Option<&SupportCapabilityV1>,
    parallel_tools: Option<&SupportCapabilityV1>,
    structured_output: Option<&SupportCapabilityV1>,
) -> ModelCapabilityFlags {
    ModelCapabilityFlags {
        text: text.map(|value| value.supported),
        reasoning: reasoning.map(|value| value.supported),
        vision: vision.map(|value| value.input),
        tool_calling: tools.map(|value| value.supported),
        parallel_tools: parallel_tools.map(|value| value.supported),
        structured_output: structured_output.map(|value| value.supported),
    }
}

pub fn plugin_capability_flags_v1(metadata: &serde_json::Value) -> Option<ModelCapabilityFlags> {
    let metadata = parse_model_capabilities_v1(metadata)?;
    Some(capability_flags(
        metadata.text.as_ref(),
        metadata.reasoning.as_ref(),
        metadata.vision.as_ref(),
        metadata.tools.as_ref(),
        None,
        metadata.structured_output.as_ref(),
    ))
}

pub fn plugin_capability_flags(metadata: &serde_json::Value) -> Option<ModelCapabilityFlags> {
    match plugin_schema_version(metadata)? {
        1 => plugin_capability_flags_v1(metadata),
        2 => {
            let metadata = parse_model_capabilities_v2(metadata)?;
            Some(capability_flags(
                metadata.text.as_ref(),
                metadata.reasoning.as_ref(),
                metadata.vision.as_ref(),
                metadata.tools.as_ref(),
                None,
                metadata.structured_output.as_ref(),
            ))
        }
        3 => {
            let metadata = parse_model_capabilities_v3(metadata)?;
            Some(capability_flags(
                metadata.text.as_ref(),
                metadata.reasoning.as_ref(),
                metadata.vision.as_ref(),
                metadata.tools.as_ref(),
                metadata.parallel_tools.as_ref(),
                metadata.structured_output.as_ref(),
            ))
        }
        _ => None,
    }
}

pub fn plugin_reasoning_support_v1(metadata: &serde_json::Value) -> Option<bool> {
    parse_model_capabilities_v1(metadata)?
        .reasoning
        .map(|reasoning| reasoning.supported)
}

pub fn plugin_reasoning_support(metadata: &serde_json::Value) -> Option<bool> {
    match plugin_schema_version(metadata)? {
        1 => plugin_reasoning_support_v1(metadata),
        2 => parse_model_capabilities_v2(metadata)?
            .reasoning
            .map(|reasoning| reasoning.supported),
        3 => parse_model_capabilities_v3(metadata)?
            .reasoning
            .map(|reasoning| reasoning.supported),
        _ => None,
    }
}

fn normalize_plugin_reasoning_fields(
    transport_format: Option<&str>,
    reasoning: PluginReasoningCapabilityV1,
) -> Option<ReasoningCapability> {
    if !reasoning.supported {
        return None;
    }

    let mode = match reasoning.mode {
        None => None,
        Some(PluginReasoningModeV1::Toggle) => Some(ReasoningCapabilityMode::Toggle),
        Some(PluginReasoningModeV1::Level) => Some(ReasoningCapabilityMode::Level),
    };
    let levels: Vec<String> = reasoning
        .levels
        .unwrap_or_default()
        .into_iter()
        .map(|level| level.as_str().to_string())
        .collect();
    let default = reasoning.default.map(|level| level.as_str().to_string());
    let upstream_format = match transport_format {
        Some("openai" | "openai-chat") => "openai_effort",
        Some("openai-responses") => "responses_effort",
        Some("gemini") => "gemini_thinking_level",
        _ => "provider_declared",
    };
    let upstream_levels = levels
        .iter()
        .map(|level| (level.clone(), level.clone()))
        .collect();

    Some(ReasoningCapability {
        mode,
        levels,
        default,
        can_disable: reasoning.can_disable.unwrap_or(false),
        upstream_format: upstream_format.to_string(),
        upstream_levels,
    })
}

pub fn normalize_plugin_reasoning_capability_v1(
    metadata: &serde_json::Value,
) -> Option<ReasoningCapability> {
    let metadata = parse_model_capabilities_v1(metadata)?;
    normalize_plugin_reasoning_fields(
        metadata
            .transport
            .as_ref()
            .map(|transport| transport.format.as_str()),
        metadata.reasoning?,
    )
}

pub fn normalize_plugin_reasoning_capability(
    metadata: &serde_json::Value,
) -> Option<ReasoningCapability> {
    match plugin_schema_version(metadata)? {
        1 => normalize_plugin_reasoning_capability_v1(metadata),
        2 => {
            let metadata = parse_model_capabilities_v2(metadata)?;
            normalize_plugin_reasoning_fields(
                metadata
                    .transport
                    .as_ref()
                    .map(|transport| transport.format.as_str()),
                metadata.reasoning?,
            )
        }
        3 => {
            let metadata = parse_model_capabilities_v3(metadata)?;
            normalize_plugin_reasoning_fields(
                metadata
                    .transport
                    .as_ref()
                    .map(|transport| transport.format.as_str()),
                metadata.reasoning?,
            )
        }
        _ => None,
    }
}

fn plugin_identity_metadata(metadata: &serde_json::Value) -> Option<ModelIdentityV2> {
    match plugin_schema_version(metadata)? {
        2 => parse_model_capabilities_v2(metadata)?.identity,
        3 => parse_model_capabilities_v3(metadata)?.identity,
        _ => None,
    }
}

fn plugin_opaque_state_metadata(metadata: &serde_json::Value) -> Option<OpaqueStateCapabilityV1> {
    match plugin_schema_version(metadata)? {
        2 => parse_model_capabilities_v2(metadata)?.opaque_state,
        3 => parse_model_capabilities_v3(metadata)?.opaque_state,
        _ => None,
    }
}

pub fn plugin_identity(metadata: &serde_json::Value) -> Option<serde_json::Value> {
    serde_json::to_value(plugin_identity_metadata(metadata)?).ok()
}

pub fn plugin_identity_hint(metadata: &serde_json::Value) -> Option<String> {
    plugin_identity_metadata(metadata).map(|identity| identity.canonical_model_id)
}

pub fn plugin_provider_variant(metadata: &serde_json::Value) -> Option<serde_json::Value> {
    serde_json::to_value(plugin_identity_metadata(metadata)?.variant?).ok()
}

pub fn plugin_opaque_state_capability(metadata: &serde_json::Value) -> Option<serde_json::Value> {
    serde_json::to_value(plugin_opaque_state_metadata(metadata)?).ok()
}

pub(crate) fn parse_plugin_opaque_state_capability(
    metadata: &serde_json::Value,
) -> Option<OpaqueStateCapabilityV1> {
    let descriptor: OpaqueStateCapabilityV1 = serde_json::from_value(metadata.clone()).ok()?;
    descriptor.is_valid().then_some(descriptor)
}

/// Normalize provider-native/legacy discovery metadata into Kinetix's canonical
/// reasoning capability. Versioned plugin capabilities_json uses the strict
/// version-dispatched parser above. Unknown provider levels are discarded rather than invented.
pub fn normalize_reasoning_capability(metadata: &serde_json::Value) -> Option<ReasoningCapability> {
    // Provider metadata may already expose a normalized-looking shape under
    // reasoning or reasoning_capability.
    let normalized = metadata.get("reasoning_capability").or_else(|| {
        metadata.get("reasoning").filter(|value| {
            value.as_object().is_some_and(|reasoning| {
                reasoning.contains_key("supported")
                    || reasoning.contains_key("mode")
                    || reasoning.contains_key("levels")
            })
        })
    });
    if let Some(value) = normalized {
        if value.get("supported").and_then(serde_json::Value::as_bool) == Some(false) {
            return None;
        }

        let (levels, mut upstream_levels) =
            canonical_reasoning_levels(value.get("levels").unwrap_or(&serde_json::Value::Null));
        let mode = match value.get("mode").and_then(serde_json::Value::as_str) {
            Some("toggle") => Some(ReasoningCapabilityMode::Toggle),
            Some("level") => Some(ReasoningCapabilityMode::Level),
            Some("manual_budget") => Some(ReasoningCapabilityMode::ManualBudget),
            Some("adaptive") => Some(ReasoningCapabilityMode::Adaptive),
            Some(_) => return None,
            None if levels.is_empty() => None,
            None => Some(ReasoningCapabilityMode::Level),
        };
        if matches!(mode, Some(ReasoningCapabilityMode::Toggle)) && !levels.is_empty() {
            return None;
        }
        if matches!(
            mode,
            Some(
                ReasoningCapabilityMode::Level
                    | ReasoningCapabilityMode::ManualBudget
                    | ReasoningCapabilityMode::Adaptive
            )
        ) && levels.is_empty()
        {
            return None;
        }

        let upstream_format = value
            .get("upstream_format")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("provider_declared");

        // Normalized plugin metadata contains canonical levels, so aliases are
        // no longer recoverable from the level strings themselves. Apply
        // dialect defaults for known formats, then allow plugins to provide an
        // explicit canonical -> upstream token map for provider-specific aliases.
        if matches!(upstream_format, "openai_effort" | "responses_effort")
            && levels.iter().any(|level| level == "off")
        {
            upstream_levels.insert("off".to_string(), "none".to_string());
        }
        if let Some(explicit) = value
            .get("upstream_levels")
            .and_then(serde_json::Value::as_object)
        {
            for (canonical, upstream) in explicit {
                if levels.iter().any(|level| level == canonical) {
                    if let Some(upstream) = upstream.as_str() {
                        upstream_levels.insert(canonical.clone(), upstream.to_string());
                    }
                }
            }
        }

        let inferred_disable = levels.iter().any(|level| level == "off");
        return Some(ReasoningCapability {
            mode,
            levels,
            default: None,
            can_disable: value
                .get("can_disable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(inferred_disable),
            upstream_format: upstream_format.to_string(),
            upstream_levels,
        });
    }

    let candidates = [
        (
            "/supportedThinkingEfforts",
            "provider_supported_thinking_efforts",
            ReasoningCapabilityMode::Level,
        ),
        (
            "/reasoning/supported_efforts",
            "provider_reasoning_supported_efforts",
            ReasoningCapabilityMode::Level,
        ),
        (
            "/supported_reasoning_levels",
            "provider_supported_reasoning_levels",
            ReasoningCapabilityMode::Level,
        ),
        (
            "/thinking/levels",
            "provider_thinking_levels",
            ReasoningCapabilityMode::Level,
        ),
        (
            "/capabilities/effort_tiers",
            "provider_effort_tiers",
            ReasoningCapabilityMode::Level,
        ),
    ];
    for (pointer, upstream_format, mode) in candidates {
        let Some(value) = metadata.pointer(pointer) else {
            continue;
        };
        let (levels, upstream_levels) = canonical_reasoning_levels(value);
        return reasoning_capability(levels, upstream_levels, mode, upstream_format, None);
    }
    None
}

/// Derive an executable legacy ThinkingMap only when the discovered dialect
/// unambiguously identifies the upstream effort field. Other normalized
/// capabilities stay descriptive until an adapter/registry supplies a mapping.
pub fn thinking_map_for_reasoning(
    capability: &ReasoningCapability,
) -> Option<crate::types::ThinkingMap> {
    if capability.mode != Some(ReasoningCapabilityMode::Level) {
        return None;
    }
    let level_field = match capability.upstream_format.as_str() {
        "openai_effort" => "reasoning_effort",
        // Responses transport is descriptive until runtime model transport
        // selection can dispatch it through a Responses-capable adapter.
        "responses_effort" => return None,
        _ => return None,
    };
    let mut levels: std::collections::HashMap<String, serde_json::Value> = capability
        .levels
        .iter()
        .map(|level| {
            let upstream = capability
                .upstream_levels
                .get(level)
                .cloned()
                .unwrap_or_else(|| level.clone());
            (level.clone(), serde_json::Value::String(upstream))
        })
        .collect();
    if capability.can_disable && capability.upstream_format == "openai_effort" {
        levels
            .entry("off".to_string())
            .or_insert_with(|| serde_json::Value::String("none".to_string()));
    }
    Some(crate::types::ThinkingMap {
        levels,
        mode: Some(crate::types::ThinkingMode::Level),
        budget_field: None,
        level_field: Some(level_field.to_string()),
    })
}

pub fn thinking_map_for_reasoning_with_wire(
    capability: &ReasoningCapability,
    wire: crate::types::WireFormat,
) -> Option<crate::types::ThinkingMap> {
    if capability.mode != Some(ReasoningCapabilityMode::Level) {
        return None;
    }

    let level_field = match (wire, capability.upstream_format.as_str()) {
        (
            crate::types::WireFormat::Openai,
            "openai_effort"
            | "provider_supported_thinking_efforts"
            | "provider_supported_reasoning_levels",
        ) => "reasoning_effort",
        (crate::types::WireFormat::Gemini, "gemini_thinking_level") => {
            "thinkingConfig.thinkingLevel"
        }
        // Per-model Responses transport is descriptive until runtime adapter
        // selection can actually dispatch this model through the Responses API.
        (crate::types::WireFormat::Openai, "responses_effort") => return None,
        _ => return None,
    };

    let mut levels: std::collections::HashMap<String, serde_json::Value> = capability
        .levels
        .iter()
        .map(|level| {
            let upstream = capability
                .upstream_levels
                .get(level)
                .cloned()
                .unwrap_or_else(|| level.clone());
            (level.clone(), serde_json::Value::String(upstream))
        })
        .collect();
    if capability.can_disable && capability.upstream_format == "openai_effort" {
        levels
            .entry("off".to_string())
            .or_insert_with(|| serde_json::Value::String("none".to_string()));
    }
    Some(crate::types::ThinkingMap {
        levels,
        mode: Some(crate::types::ThinkingMode::Level),
        budget_field: None,
        level_field: Some(level_field.to_string()),
    })
}

#[cfg(test)]
mod reasoning_discovery_tests {
    use super::*;

    #[test]
    fn normalizes_supported_thinking_efforts_and_derives_openai_map() {
        let metadata = serde_json::json!({
            "supportedThinkingEfforts": ["low", "medium", "high"]
        });
        let capability = normalize_reasoning_capability(&metadata).unwrap();
        assert_eq!(
            capability.levels,
            vec!["low".to_string(), "medium".to_string(), "high".to_string()]
        );
        assert_eq!(
            capability.upstream_format,
            "provider_supported_thinking_efforts"
        );
        assert!(!capability.can_disable);
        assert!(thinking_map_for_reasoning(&capability).is_none());

        let map =
            thinking_map_for_reasoning_with_wire(&capability, crate::types::WireFormat::Openai)
                .unwrap();
        assert_eq!(map.level_field.as_deref(), Some("reasoning_effort"));
        assert_eq!(map.levels.get("high"), Some(&serde_json::json!("high")));
    }

    #[test]
    fn nested_reasoning_efforts_do_not_infer_responses_request_dialect() {
        let metadata = serde_json::json!({
            "reasoning": {"supported_efforts": ["minimal", "low", "high"]}
        });
        let capability = normalize_reasoning_capability(&metadata).unwrap();
        assert_eq!(
            capability.upstream_format,
            "provider_reasoning_supported_efforts"
        );
        assert_eq!(
            capability.levels,
            vec!["minimal".to_string(), "low".to_string(), "high".to_string()]
        );
        assert!(thinking_map_for_reasoning(&capability).is_none());
    }

    #[test]
    fn recognizes_all_common_metadata_shapes_conservatively() {
        for metadata in [
            serde_json::json!({"supported_reasoning_levels": ["low", "high"]}),
            serde_json::json!({"thinking": {"levels": ["off", "medium", "xhigh"]}}),
            serde_json::json!({"capabilities": {"effort_tiers": ["low", "medium", "high"]}}),
        ] {
            assert!(normalize_reasoning_capability(&metadata).is_some());
        }

        let thinking = normalize_reasoning_capability(
            &serde_json::json!({"thinking": {"levels": ["off", "medium", "xhigh"]}}),
        )
        .unwrap();
        assert!(thinking.can_disable);
        assert!(thinking_map_for_reasoning(&thinking).is_none());
    }

    #[test]
    fn provider_declared_shape_wins_and_unknown_levels_are_not_invented() {
        let metadata = serde_json::json!({
            "supportedThinkingEfforts": ["low", "vendor_ultra"],
            "reasoning": {"supported_efforts": ["high"]}
        });
        let capability = normalize_reasoning_capability(&metadata).unwrap();
        assert_eq!(capability.levels, vec!["low".to_string()]);
        assert_eq!(
            capability.upstream_format,
            "provider_supported_thinking_efforts"
        );
    }

    #[test]
    fn preserves_supported_only_plugin_reasoning() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "reasoning": {
                "supported": true
            }
        });
        let capability = normalize_plugin_reasoning_capability_v1(&metadata).unwrap();
        assert_eq!(capability.mode, None);
        assert!(capability.levels.is_empty());
        assert!(thinking_map_for_reasoning(&capability).is_none());
    }

    #[test]
    fn preserves_toggle_only_plugin_reasoning() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "reasoning": {
                "supported": true,
                "mode": "toggle",
                "can_disable": true
            }
        });
        let capability = normalize_plugin_reasoning_capability_v1(&metadata).unwrap();
        assert_eq!(capability.mode, Some(ReasoningCapabilityMode::Toggle));
        assert!(capability.levels.is_empty());
        assert!(capability.can_disable);
        assert!(thinking_map_for_reasoning(&capability).is_none());
    }

    #[test]
    fn strict_plugin_v1_rejects_invalid_default_and_unknown_fields() {
        for metadata in [
            serde_json::json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": true,
                    "mode": "level",
                    "levels": ["low"],
                    "default": "max"
                }
            }),
            serde_json::json!({
                "schema_version": 1,
                "unknown": true
            }),
            serde_json::json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": true,
                    "unknown": true
                }
            }),
        ] {
            assert!(normalize_plugin_reasoning_capability_v1(&metadata).is_none());
        }
    }

    #[test]
    fn strict_plugin_v1_exposes_non_reasoning_capabilities() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "reasoning": {"supported": false},
            "tools": {"supported": true},
            "vision": {"input": true},
            "structured_output": {"supported": true}
        });
        let flags = plugin_capability_flags_v1(&metadata).unwrap();
        assert_eq!(flags.reasoning, Some(false));
        assert_eq!(flags.vision, Some(true));
        assert_eq!(flags.tool_calling, Some(true));
        assert_eq!(flags.structured_output, Some(true));
    }

    #[test]
    fn strict_plugin_v1_preserves_reasoning_default() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "transport": {"format": "openai"},
            "reasoning": {
                "supported": true,
                "mode": "level",
                "levels": ["low", "max"],
                "default": "max",
                "can_disable": false
            }
        });
        let capability = normalize_plugin_reasoning_capability_v1(&metadata).unwrap();
        assert_eq!(capability.default.as_deref(), Some("max"));
        assert_eq!(capability.upstream_format, "openai_effort");
    }

    #[test]
    fn strict_plugin_v1_derives_gemini_thinking_level_map() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "transport": {"format": "gemini"},
            "reasoning": {
                "supported": true,
                "mode": "level",
                "levels": ["low", "medium", "high"],
                "default": "medium",
                "can_disable": false
            }
        });
        let capability = normalize_plugin_reasoning_capability_v1(&metadata).unwrap();
        assert_eq!(capability.upstream_format, "gemini_thinking_level");
        let map =
            thinking_map_for_reasoning_with_wire(&capability, crate::types::WireFormat::Gemini)
                .unwrap();
        assert_eq!(
            map.level_field.as_deref(),
            Some("thinkingConfig.thinkingLevel")
        );
        assert_eq!(map.levels.get("medium"), Some(&serde_json::json!("medium")));
    }

    #[test]
    fn schema_v2_exposes_identity_variant_reasoning_and_opaque_state() {
        let metadata = serde_json::json!({
            "schema_version": 2,
            "reasoning": {
                "supported": true,
                "mode": "level",
                "levels": ["high"],
                "default": "high",
                "can_disable": false
            },
            "tools": {"supported": true},
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
        });

        let flags = plugin_capability_flags(&metadata).unwrap();
        assert_eq!(flags.reasoning, Some(true));
        assert_eq!(flags.tool_calling, Some(true));
        let reasoning = normalize_plugin_reasoning_capability(&metadata).unwrap();
        assert_eq!(reasoning.levels, vec!["high"]);
        assert_eq!(reasoning.default.as_deref(), Some("high"));
        assert_eq!(
            plugin_identity_hint(&metadata).as_deref(),
            Some("google/gemini-3.8-flash")
        );
        assert_eq!(
            plugin_provider_variant(&metadata).unwrap()["id"],
            serde_json::json!("high")
        );
        assert_eq!(
            plugin_opaque_state_capability(&metadata).unwrap()["family"],
            serde_json::json!("gemini")
        );
    }

    #[test]
    fn schema_v3_exposes_typed_transport_and_capabilities() {
        let metadata = serde_json::json!({
            "schema_version": 3,
            "transport": {
                "format": "anthropic",
                "endpoint": "/zen/v1/messages",
                "alternatives": [{"format": "openai-chat"}]
            },
            "reasoning": {
                "supported": true,
                "mode": "level",
                "levels": ["low", "high"],
                "default": "high",
                "can_disable": false
            },
            "tools": {"supported": true},
            "parallel_tools": {"supported": true},
            "vision": {"input": false},
            "identity": {
                "canonical_model_id": "opencode/union-alpha",
                "variant": {
                    "kind": "provider_alias",
                    "id": "union-alpha",
                    "fixed": false
                }
            },
            "opaque_state": {
                "kind": "gemini_thought_signature",
                "family": "gemini",
                "encoding_version": 1
            },
            "prices": {"input_per_1m": 0.0}
        });

        let flags = plugin_capability_flags(&metadata).unwrap();
        assert_eq!(flags.reasoning, Some(true));
        assert_eq!(flags.tool_calling, Some(true));
        assert_eq!(flags.vision, Some(false));
        let reasoning = normalize_plugin_reasoning_capability(&metadata).unwrap();
        assert_eq!(reasoning.levels, vec!["low", "high"]);
        assert_eq!(reasoning.default.as_deref(), Some("high"));
        assert_eq!(reasoning.upstream_format, "provider_declared");
        assert_eq!(
            plugin_identity_hint(&metadata).as_deref(),
            Some("opencode/union-alpha")
        );
        assert_eq!(
            plugin_provider_variant(&metadata).unwrap()["id"],
            "union-alpha"
        );
        assert_eq!(
            plugin_opaque_state_capability(&metadata).unwrap()["family"],
            "gemini"
        );
    }

    #[test]
    fn schema_v3_rejects_invalid_transport_and_parallel_tools() {
        for metadata in [
            serde_json::json!({
                "schema_version": 3,
                "transport": {"format": "unknown"}
            }),
            serde_json::json!({
                "schema_version": 3,
                "transport": {"format": "anthropic", "endpoint": "//evil.example"}
            }),
            serde_json::json!({
                "schema_version": 3,
                "transport": {
                    "format": "anthropic",
                    "alternatives": [{"format": "anthropic"}]
                }
            }),
            serde_json::json!({
                "schema_version": 3,
                "parallel_tools": {"supported": true}
            }),
            serde_json::json!({
                "schema_version": 3,
                "prices": {"input_per_1m": -1.0}
            }),
            serde_json::json!({"schema_version": 3, "unknown": true}),
            serde_json::json!({"schema_version": 99}),
        ] {
            assert!(plugin_capability_flags(&metadata).is_none(), "{metadata}");
            assert!(
                normalize_plugin_reasoning_capability(&metadata).is_none(),
                "{metadata}"
            );
        }
    }

    #[test]
    fn schema_v1_generic_dispatch_matches_existing_helpers() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "reasoning": {
                "supported": true,
                "mode": "level",
                "levels": ["low", "high"],
                "default": "low",
                "can_disable": false
            },
            "tools": {"supported": true}
        });
        assert_eq!(
            plugin_capability_flags(&metadata),
            plugin_capability_flags_v1(&metadata)
        );
        assert_eq!(
            plugin_reasoning_support(&metadata),
            plugin_reasoning_support_v1(&metadata)
        );
        assert_eq!(
            normalize_plugin_reasoning_capability(&metadata),
            normalize_plugin_reasoning_capability_v1(&metadata)
        );
    }

    #[test]
    fn malformed_or_unknown_schema_v2_metadata_fails_closed() {
        for metadata in [
            serde_json::json!({
                "schema_version": 2,
                "identity": {"canonical_model_id": "missing-provider-separator"}
            }),
            serde_json::json!({
                "schema_version": 2,
                "identity": {
                    "canonical_model_id": "google/gemini-3.8-flash",
                    "variant": {
                        "kind": "provider_alias",
                        "id": "alias",
                        "reasoning_level": "high",
                        "fixed": false
                    }
                }
            }),
            serde_json::json!({
                "schema_version": 2,
                "opaque_state": {
                    "kind": "gemini_thought_signature",
                    "family": "bad family",
                    "encoding_version": 1
                }
            }),
            serde_json::json!({
                "schema_version": 2,
                "opaque_state": {
                    "kind": "gemini_thought_signature",
                    "family": "gemini",
                    "encoding_version": 0
                }
            }),
            serde_json::json!({"schema_version": 2, "unknown": true}),
            serde_json::json!({"schema_version": 99}),
        ] {
            assert!(plugin_capability_flags(&metadata).is_none());
            assert!(plugin_identity(&metadata).is_none());
            assert!(plugin_opaque_state_capability(&metadata).is_none());
        }
    }

    #[test]
    fn accepts_provider_normalized_adaptive_reasoning_capability() {
        let metadata = serde_json::json!({
            "reasoning_capability": {
                "mode": "adaptive",
                "levels": ["low", "high", "max"],
                "can_disable": false,
                "upstream_format": "anthropic_effort"
            }
        });
        let capability = normalize_reasoning_capability(&metadata).unwrap();
        assert_eq!(capability.mode, Some(ReasoningCapabilityMode::Adaptive));
        assert_eq!(capability.upstream_format, "anthropic_effort");
        assert!(thinking_map_for_reasoning(&capability).is_none());
    }
}
