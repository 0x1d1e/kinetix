//! Plugin type definitions shared across the host, manifest, store, and manager.
//!
//! A plugin declares the capabilities it provides and the permissions it
//! requests; the host enforces everything else.

use serde::{Deserialize, Serialize};

/// The latest plugin API major version this build implements.
pub const PLUGIN_API_MAJOR: u32 = 2;

/// API majors accepted concurrently by this host. API v2 adds a separately
/// versioned session-aware adapter world; all other API-v1 worlds remain live.
pub const SUPPORTED_PLUGIN_API_MAJORS: [u32; 2] = [1, PLUGIN_API_MAJOR];

/// The supported manifest schema version.
pub const MANIFEST_VERSION: u32 = 1;

/// The names of the capabilities a plugin can provide (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    CredentialStrategy,
    AuthFlow,
    ModelSource,
    AccountModelSource,
    ProviderAdapter,
    RoutingFacts,
    HealthProbe,
    Hooks,
}

impl Capability {
    /// The `[provides]` key for this capability.
    pub fn manifest_key(&self) -> &'static str {
        match self {
            Capability::CredentialStrategy => "credential_strategies",
            Capability::AuthFlow => "auth_flows",
            Capability::ModelSource => "model_sources",
            Capability::AccountModelSource => "account_model_sources",
            Capability::ProviderAdapter => "provider_adapters",
            Capability::RoutingFacts => "routing_facts",
            Capability::HealthProbe => "health_probes",
            Capability::Hooks => "hooks",
        }
    }

    /// Whether this capability requires the streaming adapter world (§7.1).
    pub fn is_adapter(&self) -> bool {
        matches!(self, Capability::ProviderAdapter)
    }

    /// Whether a routing-facts capability is subject to the determinism rules
    /// in §6.4 (pure or cached).
    pub fn is_routing_fact(&self) -> bool {
        matches!(self, Capability::RoutingFacts)
    }
}

/// A concrete provided capability: `(capability, name)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Provided {
    pub capability: Capability,
    pub name: String,
}

/// The namespaced reference form `plugin:<id>/<capability-name>` (§6.0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRef {
    pub plugin_id: String,
    pub capability: String,
}

impl PluginRef {
    pub fn parse(s: &str) -> Option<PluginRef> {
        let rest = s.strip_prefix("plugin:")?;
        let (id, capability) = rest.split_once('/')?;
        if id.is_empty() || capability.is_empty() {
            return None;
        }
        Some(PluginRef {
            plugin_id: id.to_string(),
            capability: capability.to_string(),
        })
    }

    pub fn to_string_ref(&self) -> String {
        format!("plugin:{}/{}", self.plugin_id, self.capability)
    }

    /// The adapter-registry key: the plugin id itself, since one plugin hosts a
    /// single `provider-adapter` capability. Accepts either the full
    /// `plugin:<id>/<cap>` reference or a bare plugin id.
    pub fn adapter_key(s: &str) -> String {
        match Self::parse(s) {
            Some(r) => r.plugin_id,
            None => s.to_string(),
        }
    }
}

/// Declared permissions (§5, §8, §9).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default)]
    pub network_hosts: Vec<String>,
    #[serde(default)]
    pub credential_scopes: Vec<String>,
    /// Plaintext credential access (§8.2). Higher risk; off unless requested.
    #[serde(default)]
    pub credential_read: bool,
}

/// Manifest `[limits]` (§5, §14). These are *requests*; host policy wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default = "default_wall_time")]
    pub wall_time_ms: u64,
    #[serde(default = "default_outbound")]
    pub max_outbound_requests: u32,
    #[serde(default = "default_body")]
    pub max_http_body: String,
    #[serde(default = "default_storage")]
    pub storage: String,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            memory: default_memory(),
            wall_time_ms: default_wall_time(),
            max_outbound_requests: default_outbound(),
            max_http_body: default_body(),
            storage: default_storage(),
        }
    }
}

fn default_memory() -> String {
    "64MiB".into()
}
fn default_wall_time() -> u64 {
    5000
}
fn default_outbound() -> u32 {
    4
}
fn default_body() -> String {
    "4MiB".into()
}
fn default_storage() -> String {
    "2MiB".into()
}

fn default_integration_wire_format() -> String {
    "plugin".into()
}

fn default_integration_auth_scheme() -> String {
    "bearer".into()
}

fn default_integration_timeout_ms() -> u64 {
    120_000
}

fn default_integration_capability_mode() -> String {
    "permissive".into()
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PricingScope {
    #[default]
    Integration,
    DirectApi,
}

impl PricingScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Integration => "integration",
            Self::DirectApi => "direct_api",
        }
    }
}

/// Host-owned provider defaults for a user-facing integration. Kinetix derives
/// plugin capability bindings from the parent Integration; the template cannot
/// point at capabilities from another plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationProvider {
    pub base_url: String,
    #[serde(default = "default_integration_wire_format")]
    pub wire_format: String,
    #[serde(default = "default_integration_auth_scheme")]
    pub auth_scheme: String,
    #[serde(default)]
    pub custom_header_name: Option<String>,
    #[serde(default)]
    pub custom_param_name: Option<String>,
    #[serde(default)]
    pub extra_headers: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_integration_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_integration_capability_mode")]
    pub capability_mode: String,
    #[serde(default)]
    pub models_path: Option<String>,
    #[serde(default)]
    pub follow_redirects: bool,
    #[serde(default)]
    pub credential_hosts: Vec<String>,
    /// Serving-economics contract for catalog pricing. Plugin integrations are
    /// isolated by default; opt into direct API pricing only when billing is
    /// contractually identical to the provider's public API.
    #[serde(default)]
    pub pricing_scope: PricingScope,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialMode {
    Manual,
    AuthFlow,
    None,
}

impl CredentialMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialMode::Manual => "manual",
            CredentialMode::AuthFlow => "auth_flow",
            CredentialMode::None => "none",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "manual" => Some(Self::Manual),
            "auth_flow" => Some(Self::AuthFlow),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// Versioned integration-level feature declarations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationFeaturesV1 {
    pub schema_version: u32,
    pub streaming: bool,
    pub tools: bool,
    pub parallel_tools: bool,
    pub vision: bool,
    pub reasoning: bool,
    pub structured_output: bool,
    pub model_discovery: bool,
    pub quota_probe: bool,
    pub health_probe: bool,
}

impl IntegrationFeaturesV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err(format!(
                "unsupported integration feature schema_version {}",
                self.schema_version
            ));
        }
        if self.parallel_tools && !self.tools {
            return Err("parallel_tools requires tools".into());
        }
        Ok(())
    }
}

/// Versioned input and upstream protocol declarations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationProtocolsV1 {
    pub input: Vec<String>,
    pub upstream: Vec<String>,
}

impl IntegrationProtocolsV1 {
    pub fn validate(&self) -> Result<(), String> {
        const FORMATS: &[&str] = &[
            "openai-chat",
            "openai-responses",
            "anthropic",
            "gemini",
            "plugin-native",
        ];
        for (name, formats) in [("input", &self.input), ("upstream", &self.upstream)] {
            let unique: std::collections::HashSet<_> = formats.iter().collect();
            if unique.len() != formats.len() {
                return Err(format!("protocols.{name} must not contain duplicates"));
            }
            if formats
                .iter()
                .any(|format| !FORMATS.contains(&format.as_str()))
            {
                return Err(format!("protocols.{name} contains an unknown protocol"));
            }
        }
        Ok(())
    }

    /// Ensure every declared upstream protocol can be served by this provider
    /// transport. `plugin-native` is valid only with a configured adapter.
    pub fn validate_upstream_wire_format(
        &self,
        wire_format: &str,
        has_provider_adapter: bool,
    ) -> Result<(), String> {
        self.validate()?;
        for protocol in &self.upstream {
            let compatible = match protocol.as_str() {
                "openai-chat" | "openai-responses" => wire_format == "openai",
                "anthropic" => wire_format == "anthropic",
                "gemini" => wire_format == "gemini",
                "plugin-native" => wire_format == "plugin" && has_provider_adapter,
                _ => false,
            };
            if !compatible {
                return Err(format!(
                    "upstream protocol '{protocol}' is incompatible with provider wire_format '{wire_format}'"
                ));
            }
        }
        Ok(())
    }

    pub fn allows_input(&self, protocol: &str) -> bool {
        self.input.iter().any(|allowed| allowed == protocol)
    }
}

/// A user-facing integration assembled from one or more capabilities provided
/// by the same plugin. This metadata is declarative only: it grants no
/// authority and contains no browser-executable code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Integration {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub credential_mode: Option<CredentialMode>,
    #[serde(default)]
    pub provider_adapter: Option<String>,
    #[serde(default)]
    pub credential_strategy: Option<String>,
    #[serde(default)]
    pub auth_flow: Option<String>,
    #[serde(default)]
    pub model_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<IntegrationFeaturesV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocols: Option<IntegrationProtocolsV1>,
    #[serde(default)]
    pub provider: Option<IntegrationProvider>,
}

impl Integration {
    pub fn effective_credential_mode(&self, permissions: &Permissions) -> CredentialMode {
        self.credential_mode.unwrap_or_else(|| {
            if self.auth_flow.is_some() {
                CredentialMode::AuthFlow
            } else if permissions.credential_read || !permissions.credential_scopes.is_empty() {
                CredentialMode::Manual
            } else {
                CredentialMode::None
            }
        })
    }
}

/// A native dashboard action declared by a plugin. Actions are metadata only:
/// the dashboard renders host-owned controls and invokes an already-authorized
/// Kinetix operation. No plugin JavaScript is loaded into the admin origin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiAction {
    pub id: String,
    pub label: String,
    /// v1 supports `auth`; future kinds can be added without exposing JS.
    pub kind: String,
    pub integration: String,
    #[serde(default)]
    pub description: String,
}

/// A host-owned plugin setting rendered by the dashboard. Values are stored
/// encrypted under the reserved `_config:` plugin-KV namespace. Guests may
/// read that namespace but cannot mutate it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSetting {
    pub key: String,
    pub label: String,
    /// text | secret | boolean | select
    pub kind: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
}

/// Declarative dashboard metadata. Empty by default for backward compatibility.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginUi {
    #[serde(default)]
    pub actions: Vec<UiAction>,
    #[serde(default)]
    pub settings: Vec<UiSetting>,
}

/// Host-version bounds declared by a plugin package.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCompatibility {
    #[serde(default)]
    pub min_host_version: Option<String>,
    #[serde(default)]
    pub max_host_version: Option<String>,
}

/// A parsed `plugin.toml` (§5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub plugin_api: String,
    #[serde(default)]
    pub compatibility: HostCompatibility,
    #[serde(default)]
    pub provides: Provides,
    #[serde(default)]
    pub integrations: Vec<Integration>,
    #[serde(default)]
    pub ui: PluginUi,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub limits: Limits,
    /// Routing-fact determinism mode (§6.4): `"pure"` (default) or `"cached"`.
    /// A `pure` plugin may not import outbound HTTP.
    #[serde(default = "default_routing_mode")]
    pub routing_facts_mode: String,
    /// Host-owned refresh cadence for `cached` routing facts. Ignored in
    /// `pure` mode.
    #[serde(default = "default_routing_refresh_ms")]
    pub routing_facts_refresh_ms: u64,
}

fn default_routing_mode() -> String {
    "pure".into()
}

fn default_routing_refresh_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provides {
    #[serde(default)]
    pub credential_strategies: Vec<String>,
    #[serde(default)]
    pub auth_flows: Vec<String>,
    #[serde(default)]
    pub model_sources: Vec<String>,
    #[serde(default)]
    pub account_model_sources: Vec<String>,
    #[serde(default)]
    pub provider_adapters: Vec<String>,
    /// Provider adapter accepts canonical `thinking.level` and owns its
    /// translation/rejection. False by default for API-v1 compatibility.
    #[serde(default)]
    pub thinking_translation: bool,
    #[serde(default)]
    pub routing_facts: Vec<String>,
    #[serde(default)]
    pub health_probes: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
}

impl Provides {
    pub fn provided(&self) -> Vec<Provided> {
        let mut out = Vec::new();
        let mut add = |cap: Capability, names: &[String]| {
            for n in names {
                out.push(Provided {
                    capability: cap,
                    name: n.clone(),
                });
            }
        };
        add(Capability::CredentialStrategy, &self.credential_strategies);
        add(Capability::AuthFlow, &self.auth_flows);
        add(Capability::ModelSource, &self.model_sources);
        add(Capability::AccountModelSource, &self.account_model_sources);
        add(Capability::ProviderAdapter, &self.provider_adapters);
        add(Capability::RoutingFacts, &self.routing_facts);
        add(Capability::HealthProbe, &self.health_probes);
        add(Capability::Hooks, &self.hooks);
        out
    }
}

impl Manifest {
    /// The declared API major, e.g. `"1"` or `"1.2.0"` -> `1`.
    pub fn api_major(&self) -> Option<u32> {
        let mut parts = self.plugin_api.split('.');
        let major = parts.next()?;
        if !major.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let major = major.parse::<u32>().ok()?;
        let mut count = 1;
        for part in parts {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            count += 1;
        }
        (count <= 3).then_some(major)
    }

    /// Whether the manifest is compatible with a specific Kinetix host.
    pub fn compatible_with_host_version(&self, host_version: &semver::Version) -> bool {
        if self.manifest_version != MANIFEST_VERSION
            || !self
                .api_major()
                .is_some_and(|major| SUPPORTED_PLUGIN_API_MAJORS.contains(&major))
        {
            return false;
        }
        let min_host = self
            .compatibility
            .min_host_version
            .as_deref()
            .map(semver::Version::parse)
            .transpose();
        let max_host = self
            .compatibility
            .max_host_version
            .as_deref()
            .map(semver::Version::parse)
            .transpose();
        let (Ok(min_host), Ok(max_host)) = (min_host, max_host) else {
            return false;
        };
        if matches!((&min_host, &max_host), (Some(min), Some(max)) if min > max) {
            return false;
        }
        min_host
            .map(|version| host_version >= &version)
            .unwrap_or(true)
            && max_host
                .map(|version| host_version <= &version)
                .unwrap_or(true)
    }

    /// Whether the manifest is compatible with this host build.
    pub fn compatible(&self) -> bool {
        semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .map(|host_version| self.compatible_with_host_version(&host_version))
            .unwrap_or(false)
    }
}

/// Runtime status of an installed plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStatus {
    /// Installed but not enabled; cannot be referenced by config.
    Disabled,
    Enabled,
    /// Disabled by the host after repeated faults (circuit breaker).
    Faulted,
}

impl PluginStatus {
    pub fn parse(s: &str) -> Self {
        match s {
            "enabled" => PluginStatus::Enabled,
            "faulted" => PluginStatus::Faulted,
            _ => PluginStatus::Disabled,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            PluginStatus::Disabled => "disabled",
            PluginStatus::Enabled => "enabled",
            PluginStatus::Faulted => "faulted",
        }
    }
    pub fn is_enabled(&self) -> bool {
        matches!(self, PluginStatus::Enabled)
    }
}

/// Circuit-breaker state (§15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl CircuitState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CircuitState::Closed => "closed",
            CircuitState::Open => "open",
            CircuitState::HalfOpen => "half_open",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "open" => CircuitState::Open,
            "half_open" => CircuitState::HalfOpen,
            _ => CircuitState::Closed,
        }
    }
}

/// Parse a human byte size like `"64MiB"`, `"2MiB"`, `"4KiB"`, or a bare
/// number of bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = if let Some(n) = s.strip_suffix("GiB") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("MiB") {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("KiB") {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix("GB") {
        (n, 1_000_000_000)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n, 1_000_000)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n, 1_000)
    } else if let Some(n) = s.strip_suffix('B') {
        (n, 1)
    } else {
        (s, 1)
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_ref_round_trips() {
        let r = PluginRef::parse("plugin:dev.example.foo/foo-oauth").unwrap();
        assert_eq!(r.plugin_id, "dev.example.foo");
        assert_eq!(r.capability, "foo-oauth");
        assert_eq!(r.to_string_ref(), "plugin:dev.example.foo/foo-oauth");
        assert!(PluginRef::parse("dev.example.foo/foo-oauth").is_none());
        assert!(PluginRef::parse("plugin:nocap").is_none());
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("64MiB"), Some(64 * 1024 * 1024));
        assert_eq!(parse_size("2MiB"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size("4KiB"), Some(4096));
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn integration_upstream_protocols_must_match_provider_transport() {
        let protocols = IntegrationProtocolsV1 {
            input: vec!["openai-chat".into(), "anthropic".into()],
            upstream: vec!["openai-chat".into(), "openai-responses".into()],
        };
        assert!(protocols
            .validate_upstream_wire_format("openai", false)
            .is_ok());
        assert!(protocols
            .validate_upstream_wire_format("anthropic", false)
            .is_err());

        let plugin_protocols = IntegrationProtocolsV1 {
            input: vec!["anthropic".into()],
            upstream: vec!["plugin-native".into()],
        };
        assert!(plugin_protocols
            .validate_upstream_wire_format("plugin", true)
            .is_ok());
        assert!(plugin_protocols
            .validate_upstream_wire_format("plugin", false)
            .is_err());
    }
}
