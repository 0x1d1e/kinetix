//! Manifest parsing and validation (§5).
//!
//! `plugin.toml` is untrusted input: it is validated before anything is
//! compiled or executed. Validation is intentionally strict and explains each
//! rejection so the admin API and `kinetix plugin install` can surface it.

use anyhow::{anyhow, bail, Result};

use super::types::{
    parse_size, Capability, CredentialMode, Manifest, MANIFEST_VERSION, SUPPORTED_PLUGIN_API_MAJORS,
};

/// The result of validating a manifest, including the effective host limits.
#[derive(Debug, Clone)]
pub struct ValidatedManifest {
    pub manifest: Manifest,
    /// Effective limits after applying `min(manifest request, host policy)`.
    pub effective: EffectiveLimits,
}

/// Host policy defaults (§14). These are not ABI constants; operators may lower
/// or raise them.
#[derive(Debug, Clone, Copy)]
pub struct HostPolicy {
    pub max_memory: u64,
    pub max_wall_time_ms: u64,
    pub max_outbound_requests: u32,
    pub max_http_body: u64,
    pub max_storage: u64,
    /// Development-only escape hatch mirroring Kinetix's upstream SSRF
    /// override. Production defaults to blocking private/reserved destinations.
    pub allow_private_network: bool,
}

impl Default for HostPolicy {
    fn default() -> Self {
        HostPolicy {
            max_memory: 64 * 1024 * 1024,
            max_wall_time_ms: 5000,
            max_outbound_requests: 4,
            max_http_body: 4 * 1024 * 1024,
            max_storage: 10 * 1024 * 1024,
            allow_private_network: false,
        }
    }
}

/// The effective limits a plugin actually runs under.
#[derive(Debug, Clone, Copy)]
pub struct EffectiveLimits {
    pub memory: u64,
    pub wall_time_ms: u64,
    pub max_outbound_requests: u32,
    pub max_http_body: u64,
    pub storage: u64,
}

/// Parse and validate a `plugin.toml` string.
pub fn parse_and_validate(toml_str: &str, policy: HostPolicy) -> Result<ValidatedManifest> {
    let manifest: Manifest =
        toml::from_str(toml_str).map_err(|e| anyhow!("invalid plugin.toml: {e}"))?;
    validate(manifest, policy)
}

/// Validate an already-parsed manifest.
pub fn validate(manifest: Manifest, policy: HostPolicy) -> Result<ValidatedManifest> {
    let host_version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION must be valid semantic version");
    validate_for_host(manifest, policy, &host_version)
}

fn validate_for_host(
    manifest: Manifest,
    policy: HostPolicy,
    host_version: &semver::Version,
) -> Result<ValidatedManifest> {
    if manifest.manifest_version != MANIFEST_VERSION {
        bail!(
            "unsupported manifest_version {} (host supports {})",
            manifest.manifest_version,
            MANIFEST_VERSION
        );
    }
    validate_id(&manifest.id)?;
    if manifest.name.trim().is_empty() {
        bail!("manifest `name` must not be empty");
    }
    semver::Version::parse(&manifest.version)
        .map_err(|e| anyhow!("invalid manifest `version` '{}': {e}", manifest.version))?;
    match manifest.api_major() {
        Some(major) if SUPPORTED_PLUGIN_API_MAJORS.contains(&major) => {}
        Some(major) => bail!(
            "incompatible plugin_api '{}': host supports majors {SUPPORTED_PLUGIN_API_MAJORS:?}, plugin requests {major}",
            manifest.plugin_api
        ),
        None => bail!("invalid plugin_api '{}'", manifest.plugin_api),
    }
    validate_host_compatibility(&manifest, host_version)?;

    let provided = manifest.provides.provided();
    if provided.is_empty() {
        bail!("manifest provides no capabilities");
    }
    let mut capability_names = std::collections::HashSet::new();
    for p in &provided {
        validate_capability_name(p.capability, &p.name)?;
        if !capability_names.insert((p.capability, p.name.as_str())) {
            bail!(
                "duplicate {} capability name '{}'",
                p.capability.manifest_key(),
                p.name
            );
        }
    }
    if manifest.provides.thinking_translation && manifest.provides.provider_adapters.is_empty() {
        bail!("provides.thinking_translation requires at least one provider_adapter");
    }
    // Hooks are not addressable by name in the same way; require at least a
    // recognizable hook name when hooks are declared.
    for h in &manifest.provides.hooks {
        if !matches!(
            h.as_str(),
            "on_request_normalized" | "on_target_candidate" | "on_usage_finalized"
        ) {
            bail!("unknown hook '{h}'");
        }
    }

    let mut integration_ids = std::collections::HashSet::new();
    for integration in &manifest.integrations {
        validate_integration_id(&integration.id)?;
        if !integration_ids.insert(integration.id.as_str()) {
            bail!("duplicate integration id '{}'", integration.id);
        }
        if integration.name.trim().is_empty() {
            bail!("integration '{}' name must not be empty", integration.id);
        }
        match (&integration.features, &integration.protocols) {
            (Some(features), Some(protocols)) => {
                features.validate().map_err(|error| {
                    anyhow!("integration '{}' features: {error}", integration.id)
                })?;
                protocols.validate().map_err(|error| {
                    anyhow!("integration '{}' protocols: {error}", integration.id)
                })?;
                if features.model_discovery != integration.model_source.is_some() {
                    bail!(
                        "integration '{}' model_discovery must match whether model_source is declared",
                        integration.id
                    );
                }
            }
            (None, None) => {}
            _ => bail!(
                "integration '{}' must declare features and protocols together",
                integration.id
            ),
        }
        if let Some(mode) = integration.credential_mode {
            match mode {
                CredentialMode::AuthFlow
                    if integration.auth_flow.is_none()
                        || integration.credential_strategy.is_none() =>
                {
                    bail!(
                        "integration '{}' credential_mode 'auth_flow' requires auth_flow and credential_strategy",
                        integration.id
                    );
                }
                CredentialMode::None
                    if integration.auth_flow.is_some()
                        || integration.credential_strategy.is_some() =>
                {
                    bail!(
                        "integration '{}' credential_mode 'none' may not declare auth_flow or credential_strategy",
                        integration.id
                    );
                }
                CredentialMode::Manual if integration.auth_flow.is_some() => {
                    bail!(
                        "integration '{}' credential_mode 'manual' may not declare auth_flow",
                        integration.id
                    );
                }
                _ => {}
            }
        }
        if integration.provider_adapter.is_none()
            && integration.credential_strategy.is_none()
            && integration.auth_flow.is_none()
            && integration.model_source.is_none()
        {
            bail!(
                "integration '{}' must reference at least one provided capability",
                integration.id
            );
        }
        if let Some(name) = &integration.provider_adapter {
            if !manifest.provides.provider_adapters.contains(name) {
                bail!(
                    "integration '{}' references unknown provider_adapter '{}'",
                    integration.id,
                    name
                );
            }
        }
        if let Some(name) = &integration.credential_strategy {
            if !manifest.provides.credential_strategies.contains(name) {
                bail!(
                    "integration '{}' references unknown credential_strategy '{}'",
                    integration.id,
                    name
                );
            }
        }
        if let Some(name) = &integration.auth_flow {
            if !manifest.provides.auth_flows.contains(name) {
                bail!(
                    "integration '{}' references unknown auth_flow '{}'",
                    integration.id,
                    name
                );
            }
        }
        if let Some(name) = &integration.model_source {
            let legacy = manifest.provides.model_sources.contains(name);
            let account = manifest.provides.account_model_sources.contains(name);
            if !legacy && !account {
                bail!(
                    "integration '{}' references unknown model_source '{}'",
                    integration.id,
                    name
                );
            }
            if legacy && account {
                bail!(
                    "integration '{}' model_source '{}' is declared in both legacy and account-aware discovery lists",
                    integration.id,
                    name
                );
            }
        }
        if let Some(provider) = &integration.provider {
            let parsed = url::Url::parse(&provider.base_url).map_err(|e| {
                anyhow!(
                    "integration '{}' provider base_url is invalid: {e}",
                    integration.id
                )
            })?;
            if parsed.scheme() != "https" {
                bail!(
                    "integration '{}' provider base_url must use https",
                    integration.id
                );
            }
            if !parsed.username().is_empty() || parsed.password().is_some() {
                bail!(
                    "integration '{}' provider base_url may not contain userinfo",
                    integration.id
                );
            }
            if parsed.host_str().is_none() {
                bail!(
                    "integration '{}' provider base_url has no host",
                    integration.id
                );
            }

            if !matches!(
                provider.wire_format.as_str(),
                "openai" | "anthropic" | "gemini" | "plugin"
            ) {
                bail!(
                    "integration '{}' provider has unknown wire_format '{}'",
                    integration.id,
                    provider.wire_format
                );
            }
            if integration.provider_adapter.is_some() && provider.wire_format != "plugin" {
                bail!(
                    "integration '{}' provider_adapter requires wire_format 'plugin'",
                    integration.id
                );
            }
            if provider.wire_format == "plugin" && integration.provider_adapter.is_none() {
                bail!(
                    "integration '{}' provider wire_format 'plugin' requires provider_adapter",
                    integration.id
                );
            }
            if let Some(protocols) = &integration.protocols {
                protocols
                    .validate_upstream_wire_format(
                        &provider.wire_format,
                        integration.provider_adapter.is_some(),
                    )
                    .map_err(|error| {
                        anyhow!("integration '{}' protocols: {error}", integration.id)
                    })?;
            }

            match provider.auth_scheme.as_str() {
                "bearer" => {}
                "custom_header" => {
                    if provider
                        .custom_header_name
                        .as_deref()
                        .unwrap_or("")
                        .trim()
                        .is_empty()
                    {
                        bail!(
                            "integration '{}' custom_header auth requires custom_header_name",
                            integration.id
                        );
                    }
                }
                "query_param" => {
                    if provider
                        .custom_param_name
                        .as_deref()
                        .unwrap_or("")
                        .trim()
                        .is_empty()
                    {
                        bail!(
                            "integration '{}' query_param auth requires custom_param_name",
                            integration.id
                        );
                    }
                }
                other => bail!(
                    "integration '{}' provider has unknown auth_scheme '{}'",
                    integration.id,
                    other
                ),
            }

            if provider.timeout_ms == 0 || provider.timeout_ms > 600_000 {
                bail!(
                    "integration '{}' provider timeout_ms must be 1..=600000",
                    integration.id
                );
            }
            if !matches!(provider.capability_mode.as_str(), "permissive" | "strict") {
                bail!(
                    "integration '{}' provider has invalid capability_mode '{}'",
                    integration.id,
                    provider.capability_mode
                );
            }
            if let Some(path) = &provider.models_path {
                if !path.starts_with('/') || path.contains("://") {
                    bail!(
                        "integration '{}' provider models_path must be an absolute URL path",
                        integration.id
                    );
                }
            }
            for host in &provider.credential_hosts {
                if host.trim().is_empty()
                    || host.contains('/')
                    || host.contains(':')
                    || host.contains('*')
                    || host.contains(' ')
                {
                    bail!(
                        "integration '{}' provider credential host '{}' is invalid",
                        integration.id,
                        host
                    );
                }
            }
            for (name, value) in &provider.extra_headers {
                if name.trim().is_empty()
                    || name.chars().any(|c| matches!(c, '\r' | '\n' | ':'))
                    || value.chars().any(|c| matches!(c, '\r' | '\n'))
                {
                    bail!(
                        "integration '{}' provider contains an invalid extra header",
                        integration.id
                    );
                }
            }
        }
    }

    let mut ui_setting_keys = std::collections::HashSet::new();
    for setting in &manifest.ui.settings {
        validate_ui_id(&setting.key)?;
        if !ui_setting_keys.insert(setting.key.as_str()) {
            bail!("duplicate ui setting key '{}'", setting.key);
        }
        if setting.label.trim().is_empty() {
            bail!("ui setting '{}' label must not be empty", setting.key);
        }
        if !matches!(
            setting.kind.as_str(),
            "text" | "secret" | "boolean" | "select"
        ) {
            bail!(
                "ui setting '{}' has unsupported kind '{}'",
                setting.key,
                setting.kind
            );
        }
        if setting.kind == "select" {
            if setting.options.is_empty() {
                bail!("select ui setting '{}' requires options", setting.key);
            }
            let mut options = std::collections::HashSet::new();
            for option in &setting.options {
                if option.is_empty() || !options.insert(option.as_str()) {
                    bail!(
                        "ui setting '{}' has invalid or duplicate option",
                        setting.key
                    );
                }
            }
            if let Some(default) = &setting.default {
                if !setting.options.contains(default) {
                    bail!(
                        "ui setting '{}' default '{}' is not in options",
                        setting.key,
                        default
                    );
                }
            }
        } else if !setting.options.is_empty() {
            bail!(
                "ui setting '{}' options are only valid for select settings",
                setting.key
            );
        }
        if setting.kind == "boolean" {
            if let Some(default) = &setting.default {
                if !matches!(default.as_str(), "true" | "false") {
                    bail!(
                        "boolean ui setting '{}' default must be 'true' or 'false'",
                        setting.key
                    );
                }
            }
        }
        if setting.kind == "secret" && setting.default.is_some() {
            bail!(
                "secret ui setting '{}' may not declare a default",
                setting.key
            );
        }
    }

    let mut ui_action_ids = std::collections::HashSet::new();
    for action in &manifest.ui.actions {
        validate_ui_id(&action.id)?;
        if !ui_action_ids.insert(action.id.as_str()) {
            bail!("duplicate ui action id '{}'", action.id);
        }
        if action.label.trim().is_empty() {
            bail!("ui action '{}' label must not be empty", action.id);
        }
        if action.kind != "auth" {
            bail!(
                "ui action '{}' has unsupported kind '{}': expected 'auth'",
                action.id,
                action.kind
            );
        }
        let integration = manifest
            .integrations
            .iter()
            .find(|integration| integration.id == action.integration)
            .ok_or_else(|| {
                anyhow!(
                    "ui action '{}' references unknown integration '{}'",
                    action.id,
                    action.integration
                )
            })?;
        if integration.auth_flow.is_none() || integration.credential_strategy.is_none() {
            bail!(
                "auth ui action '{}' requires integration '{}' to declare auth_flow and credential_strategy",
                action.id,
                action.integration
            );
        }
    }

    let mut network_hosts = std::collections::HashSet::new();
    for host in &manifest.permissions.network_hosts {
        validate_network_host(host)?;
        if !network_hosts.insert(host.as_str()) {
            bail!("duplicate network_hosts entry '{host}'");
        }
    }
    let mut credential_scopes = std::collections::HashSet::new();
    for scope in &manifest.permissions.credential_scopes {
        if !credential_scopes.insert(scope.as_str()) {
            bail!("duplicate credential_scopes entry '{scope}'");
        }
        if scope == "*" {
            continue;
        }
        if let Some(provider_id) = scope.strip_prefix("provider:") {
            if provider_id.trim().is_empty() {
                bail!("credential scope 'provider:' requires a provider id");
            }
            continue;
        }
        if let Some(strategy) = scope.strip_prefix("credential_strategy:") {
            if !manifest
                .provides
                .credential_strategies
                .iter()
                .any(|provided| provided == strategy)
            {
                bail!(
                    "credential scope '{}' references a credential strategy this plugin does not provide",
                    scope
                );
            }
            continue;
        }
        bail!(
            "invalid credential scope '{}': expected '*', 'provider:<id>', or 'credential_strategy:<name>'",
            scope
        );
    }

    // §6.4: a routing-fact provider must declare a determinism mode the host can
    // enforce. `pure` means no outbound HTTP on the request path (enforced by
    // the host); `cached` means the guest publishes host-stamped facts.
    if !manifest.provides.routing_facts.is_empty()
        && !matches!(manifest.routing_facts_mode.as_str(), "pure" | "cached")
    {
        bail!(
            "invalid routing_facts_mode '{}': expected 'pure' or 'cached'",
            manifest.routing_facts_mode
        );
    }
    if !manifest.provides.routing_facts.is_empty()
        && manifest.routing_facts_mode == "cached"
        && !(5_000..=3_600_000).contains(&manifest.routing_facts_refresh_ms)
    {
        bail!("routing_facts_refresh_ms must be 5000..=3600000 for cached routing facts");
    }

    // §7.1: an adapter plugin is a pure translation library and must not hold
    // outbound network authority. This is enforced at runtime (the adapter-world
    // store refuses buffered host-http); a plugin that also legitimately
    // provides a control-plane capability needing network (e.g. a credential
    // strategy) is unaffected because that capability runs against the
    // `plugin`-world store.

    let effective = effective_limits(&manifest, policy)?;
    Ok(ValidatedManifest {
        manifest,
        effective,
    })
}

fn validate_host_compatibility(manifest: &Manifest, host_version: &semver::Version) -> Result<()> {
    let min_host = manifest
        .compatibility
        .min_host_version
        .as_deref()
        .map(semver::Version::parse)
        .transpose()
        .map_err(|e| anyhow!("invalid compatibility.min_host_version: {e}"))?;
    let max_host = manifest
        .compatibility
        .max_host_version
        .as_deref()
        .map(semver::Version::parse)
        .transpose()
        .map_err(|e| anyhow!("invalid compatibility.max_host_version: {e}"))?;

    if matches!((&min_host, &max_host), (Some(min), Some(max)) if min > max) {
        bail!("compatibility.min_host_version must not exceed max_host_version");
    }
    if let Some(min) = &min_host {
        if host_version < min {
            bail!("plugin requires Kinetix >= {min}, but this host is {host_version}");
        }
    }
    if let Some(max) = &max_host {
        if host_version > max {
            bail!("plugin supports Kinetix <= {max}, but this host is {host_version}");
        }
    }
    Ok(())
}

/// Apply host policy to the manifest's requested limits (§5, §14). The plugin
/// can never self-grant more than the host allows.
pub fn effective_limits(manifest: &Manifest, policy: HostPolicy) -> Result<EffectiveLimits> {
    let memory = parse_size(&manifest.limits.memory)
        .ok_or_else(|| anyhow!("invalid limits.memory '{}'", manifest.limits.memory))?;
    let max_http_body = parse_size(&manifest.limits.max_http_body).ok_or_else(|| {
        anyhow!(
            "invalid limits.max_http_body '{}'",
            manifest.limits.max_http_body
        )
    })?;
    let storage = parse_size(&manifest.limits.storage)
        .ok_or_else(|| anyhow!("invalid limits.storage '{}'", manifest.limits.storage))?;

    Ok(EffectiveLimits {
        memory: memory.min(policy.max_memory),
        wall_time_ms: manifest.limits.wall_time_ms.min(policy.max_wall_time_ms),
        max_outbound_requests: manifest
            .limits
            .max_outbound_requests
            .min(policy.max_outbound_requests),
        max_http_body: max_http_body.min(policy.max_http_body),
        storage: storage.min(policy.max_storage),
    })
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 {
        bail!("plugin id must be 1..=128 characters");
    }
    let ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' || c == '_');
    if !ok {
        bail!("plugin id '{id}' may only contain lowercase letters, digits, '.', '-', '_'");
    }
    if id.starts_with('.') || id.ends_with('.') || id.contains("..") {
        bail!("plugin id '{id}' has an invalid dot placement");
    }
    Ok(())
}

fn validate_ui_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        bail!("ui action id must be 1..=64 characters");
    }
    let ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !ok {
        bail!("ui action id '{id}' may only contain lowercase letters, digits, '-', '_'");
    }
    Ok(())
}

fn validate_integration_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        bail!("integration id must be 1..=64 characters");
    }
    let ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !ok {
        bail!("integration id '{id}' may only contain lowercase letters, digits, '-', '_'");
    }
    Ok(())
}

fn validate_capability_name(cap: Capability, name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!(
            "{} capability name must be 1..=64 characters",
            cap.manifest_key()
        );
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !ok {
        bail!(
            "{} name '{name}' may only contain lowercase letters, digits, '-', '_'",
            cap.manifest_key()
        );
    }
    Ok(())
}

/// §9: wildcards are allowed but conservative — a single leading `*.` label.
/// A bare `*` or a wildcard in the middle of a host is rejected.
fn validate_network_host(host: &str) -> Result<()> {
    let host = host.trim();
    if host.is_empty() {
        bail!("network host must not be empty");
    }
    if host == "*" || host.contains('*') && !host.starts_with("*.") {
        bail!("network host '{host}': only a single leading '*.' wildcard is allowed");
    }
    let base = host.strip_prefix("*.").unwrap_or(host);
    if base.contains('*') {
        bail!("network host '{host}': only a single leading '*.' wildcard is allowed");
    }
    if base.is_empty() || base.contains('/') || base.contains(' ') {
        bail!("network host '{host}' is not a valid hostname");
    }
    if base.parse::<std::net::IpAddr>().is_ok() {
        bail!("network host '{host}': IP literals are not allowed; declare a hostname");
    }
    Ok(())
}

/// Whether a concrete destination host matches a declared `network_hosts`
/// entry. Wildcard entries match exactly one additional DNS label (§9).
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    let host = host.trim().to_ascii_lowercase();
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // one DNS label plus the suffix, and no further subdomain labels
        if let Some(prefix) = host.strip_suffix(suffix) {
            let prefix = prefix.strip_suffix('.').unwrap_or(prefix);
            return !prefix.is_empty() && !prefix.contains('.');
        }
        false
    } else {
        pattern == host
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
manifest_version = 1
id = "dev.example.foo"
name = "Foo"
version = "1.2.0"
plugin_api = "1"

[provides]
credential_strategies = ["foo-auth"]
auth_flows = ["foo-login"]
model_sources = ["foo-models"]

[[integrations]]
id = "foo"
name = "Foo Cloud"
description = "Foo provider integration"
credential_strategy = "foo-auth"
auth_flow = "foo-login"
model_source = "foo-models"

[[ui.actions]]
id = "connect"
label = "Connect account"
kind = "auth"
integration = "foo"

[permissions]
network_hosts = ["api.foo.example", "*.svc.example"]

[limits]
memory = "128MiB"
storage = "2MiB"
"#;

    #[test]
    fn parses_and_validates_versioned_integration_metadata() {
        let metadata = GOOD.replace(
            "[[ui.actions]]",
            "[integrations.features]\nschema_version = 1\nstreaming = true\ntools = true\nparallel_tools = true\nvision = false\nreasoning = true\nstructured_output = false\nmodel_discovery = true\nquota_probe = false\nhealth_probe = false\n\n[integrations.protocols]\ninput = [\"openai-chat\", \"anthropic\"]\nupstream = [\"plugin-native\"]\n\n[[ui.actions]]",
        );
        let validated = parse_and_validate(&metadata, HostPolicy::default()).unwrap();
        let integration = &validated.manifest.integrations[0];
        assert_eq!(integration.features.as_ref().unwrap().schema_version, 1);
        assert!(integration.features.as_ref().unwrap().parallel_tools);
        assert_eq!(
            integration.protocols.as_ref().unwrap().upstream,
            ["plugin-native".to_string()]
        );
    }

    #[test]
    fn rejects_invalid_versioned_integration_metadata() {
        let metadata = GOOD.replace(
            "[[ui.actions]]",
            "[integrations.features]\nschema_version = 1\nstreaming = true\ntools = false\nparallel_tools = true\nvision = false\nreasoning = true\nstructured_output = false\nmodel_discovery = true\nquota_probe = false\nhealth_probe = false\n\n[integrations.protocols]\ninput = [\"openai-chat\"]\nupstream = [\"plugin-native\"]\n\n[[ui.actions]]",
        );
        let error = parse_and_validate(&metadata, HostPolicy::default()).unwrap_err();
        assert!(
            error.to_string().contains("parallel_tools requires tools"),
            "{error}"
        );

        let metadata = metadata.replace("tools = false", "tools = true");
        let metadata =
            metadata.replace("upstream = [\"plugin-native\"]", "upstream = [\"unknown\"]");
        let error = parse_and_validate(&metadata, HostPolicy::default()).unwrap_err();
        assert!(error.to_string().contains("unknown protocol"), "{error}");

        let metadata =
            metadata.replace("upstream = [\"unknown\"]", "upstream = [\"plugin-native\"]");
        let metadata = metadata.replace("model_discovery = true", "model_discovery = false");
        let error = parse_and_validate(&metadata, HostPolicy::default()).unwrap_err();
        assert!(
            error.to_string().contains("model_discovery must match"),
            "{error}"
        );
    }

    #[test]
    fn validates_a_good_manifest() {
        let v = parse_and_validate(GOOD, HostPolicy::default()).unwrap();
        assert_eq!(v.manifest.id, "dev.example.foo");
        // 128MiB request is clamped to the 64MiB host policy.
        assert_eq!(v.effective.memory, 64 * 1024 * 1024);
        assert_eq!(v.effective.storage, 2 * 1024 * 1024);
        assert!(!v.manifest.provides.thinking_translation);
    }

    #[test]
    fn thinking_translation_is_explicit_and_requires_provider_adapter() {
        let opted_in = GOOD.replace(
            "model_sources = [\"foo-models\"]",
            "model_sources = [\"foo-models\"]\nprovider_adapters = [\"foo-adapter\"]\nthinking_translation = true",
        );
        let validated = parse_and_validate(&opted_in, HostPolicy::default()).unwrap();
        assert!(validated.manifest.provides.thinking_translation);

        let invalid = GOOD.replace(
            "model_sources = [\"foo-models\"]",
            "model_sources = [\"foo-models\"]\nthinking_translation = true",
        );
        let error = parse_and_validate(&invalid, HostPolicy::default()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("thinking_translation requires at least one provider_adapter"),
            "{error}"
        );
    }

    #[test]
    fn accepts_api_v1_and_v2_manifests() {
        for major in [1, 2] {
            let manifest = GOOD.replace("plugin_api = \"1\"", &format!("plugin_api = \"{major}\""));
            let validated = parse_and_validate(&manifest, HostPolicy::default()).unwrap();
            assert!(validated.manifest.compatible());
            assert_eq!(validated.manifest.api_major(), Some(major));
        }
    }

    #[test]
    fn rejects_incompatible_api() {
        let bad = GOOD.replace("plugin_api = \"1\"", "plugin_api = \"3\"");
        let error = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(error.to_string().contains("host supports majors [1, 2]"));
    }

    #[test]
    fn unbounded_api_v1_remains_compatible_across_kinetix_1x() {
        let manifest: Manifest = toml::from_str(GOOD).unwrap();
        for host in ["1.0.0", "1.5.0", "1.99.99"] {
            let host = semver::Version::parse(host).unwrap();
            assert!(manifest.compatible_with_host_version(&host));
        }
    }

    #[test]
    fn supported_api_v2_is_compatible_with_this_host_version() {
        let api_v2 = GOOD.replace("plugin_api = \"1\"", "plugin_api = \"2\"");
        let api_v2: Manifest = toml::from_str(&api_v2).unwrap();
        let host = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        assert!(api_v2.compatible_with_host_version(&host));
    }

    #[test]
    fn host_version_compatibility_range_is_inclusive_and_enforced() {
        let bounded = GOOD.replace(
            "plugin_api = \"1\"",
            "plugin_api = \"1\"\n\n[compatibility]\nmin_host_version = \"1.0.0\"\nmax_host_version = \"2.0.0\"",
        );

        let below = toml::from_str(&bounded).unwrap();
        let error = validate_for_host(
            below,
            HostPolicy::default(),
            &semver::Version::parse("0.9.9").unwrap(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("requires Kinetix >= 1.0.0"),
            "{error}"
        );

        for version in ["1.0.0", "2.0.0"] {
            let manifest = toml::from_str(&bounded).unwrap();
            validate_for_host(
                manifest,
                HostPolicy::default(),
                &semver::Version::parse(version).unwrap(),
            )
            .unwrap();
        }

        let above = toml::from_str(&bounded).unwrap();
        let error = validate_for_host(
            above,
            HostPolicy::default(),
            &semver::Version::parse("2.0.1").unwrap(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("supports Kinetix <= 2.0.0"),
            "{error}"
        );
    }

    #[test]
    fn rejects_malformed_plugin_and_host_versions() {
        let bad_release = GOOD.replace("version = \"1.2.0\"", "version = \"release-1\"");
        assert!(parse_and_validate(&bad_release, HostPolicy::default()).is_err());

        let bad_api = GOOD.replace("plugin_api = \"1\"", "plugin_api = \"1.invalid\"");
        assert!(parse_and_validate(&bad_api, HostPolicy::default()).is_err());

        let bad_host_range = GOOD.replace(
            "plugin_api = \"1\"",
            "plugin_api = \"1\"\n\n[compatibility]\nmin_host_version = \"2.0.0\"\nmax_host_version = \"1.0.0\"",
        );
        assert!(parse_and_validate(&bad_host_range, HostPolicy::default()).is_err());
    }

    #[test]
    fn rejects_unknown_and_duplicate_capability_declarations() {
        let unknown = GOOD.replace("[provides]", "[provides]\nmodel_source = [\"misspelled\"]");
        let error = parse_and_validate(&unknown, HostPolicy::default()).unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");

        let duplicate = GOOD.replace(
            "model_sources = [\"foo-models\"]",
            "model_sources = [\"foo-models\", \"foo-models\"]",
        );
        let error = parse_and_validate(&duplicate, HostPolicy::default()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate model_sources capability name"),
            "{error}"
        );
    }

    #[test]
    fn rejects_no_capabilities() {
        let bad = GOOD
            .replace("credential_strategies = [\"foo-auth\"]", "")
            .replace("auth_flows = [\"foo-login\"]", "")
            .replace("model_sources = [\"foo-models\"]", "");
        assert!(parse_and_validate(&bad, HostPolicy::default()).is_err());
    }

    #[test]
    fn credential_modes_validate_and_legacy_inference_is_stable() {
        const AUTH_ACTION: &str = r#"
[[ui.actions]]
id = "connect"
label = "Connect account"
kind = "auth"
integration = "foo"
"#;

        let auth_flow = GOOD.replace(
            "description = \"Foo provider integration\"",
            "description = \"Foo provider integration\"\ncredential_mode = \"auth_flow\"",
        );
        let parsed = parse_and_validate(&auth_flow, HostPolicy::default()).unwrap();
        let integration = &parsed.manifest.integrations[0];
        assert_eq!(
            integration.effective_credential_mode(&parsed.manifest.permissions),
            CredentialMode::AuthFlow
        );

        let manual = GOOD
            .replace(AUTH_ACTION, "")
            .replace("auth_flow = \"foo-login\"\n", "")
            .replace(
                "description = \"Foo provider integration\"",
                "description = \"Foo provider integration\"\ncredential_mode = \"manual\"",
            );
        assert!(parse_and_validate(&manual, HostPolicy::default()).is_ok());

        let none = GOOD
            .replace(AUTH_ACTION, "")
            .replace("credential_strategy = \"foo-auth\"\n", "")
            .replace("auth_flow = \"foo-login\"\n", "")
            .replace(
                "description = \"Foo provider integration\"",
                "description = \"Foo provider integration\"\ncredential_mode = \"none\"",
            );
        assert!(parse_and_validate(&none, HostPolicy::default()).is_ok());

        let legacy = parse_and_validate(GOOD, HostPolicy::default()).unwrap();
        assert_eq!(
            legacy.manifest.integrations[0].effective_credential_mode(&legacy.manifest.permissions),
            CredentialMode::AuthFlow
        );

        let legacy_manual = GOOD
            .replace(AUTH_ACTION, "")
            .replace("auth_flow = \"foo-login\"\n", "")
            .replace(
                "network_hosts = [\"api.foo.example\", \"*.svc.example\"]",
                "network_hosts = [\"api.foo.example\", \"*.svc.example\"]\ncredential_scopes = [\"credential_strategy:foo-auth\"]",
            );
        let legacy_manual = parse_and_validate(&legacy_manual, HostPolicy::default()).unwrap();
        assert_eq!(
            legacy_manual.manifest.integrations[0]
                .effective_credential_mode(&legacy_manual.manifest.permissions),
            CredentialMode::Manual
        );

        let legacy_none = GOOD
            .replace(AUTH_ACTION, "")
            .replace("credential_strategy = \"foo-auth\"\n", "")
            .replace("auth_flow = \"foo-login\"\n", "");
        let legacy_none = parse_and_validate(&legacy_none, HostPolicy::default()).unwrap();
        assert_eq!(
            legacy_none.manifest.integrations[0]
                .effective_credential_mode(&legacy_none.manifest.permissions),
            CredentialMode::None
        );
    }

    #[test]
    fn rejects_contradictory_credential_modes() {
        let auth_without_flow = GOOD.replace("auth_flow = \"foo-login\"\n", "").replace(
            "description = \"Foo provider integration\"",
            "description = \"Foo provider integration\"\ncredential_mode = \"auth_flow\"",
        );
        assert!(
            parse_and_validate(&auth_without_flow, HostPolicy::default())
                .unwrap_err()
                .to_string()
                .contains("requires auth_flow and credential_strategy")
        );

        let auth_without_strategy = GOOD
            .replace("credential_strategy = \"foo-auth\"\n", "")
            .replace(
                "description = \"Foo provider integration\"",
                "description = \"Foo provider integration\"\ncredential_mode = \"auth_flow\"",
            );
        assert!(
            parse_and_validate(&auth_without_strategy, HostPolicy::default())
                .unwrap_err()
                .to_string()
                .contains("requires auth_flow and credential_strategy")
        );

        let none_with_strategy = GOOD.replace("auth_flow = \"foo-login\"\n", "").replace(
            "description = \"Foo provider integration\"",
            "description = \"Foo provider integration\"\ncredential_mode = \"none\"",
        );
        assert!(
            parse_and_validate(&none_with_strategy, HostPolicy::default())
                .unwrap_err()
                .to_string()
                .contains("may not declare auth_flow or credential_strategy")
        );

        let none_with_flow = GOOD.replace(
            "description = \"Foo provider integration\"",
            "description = \"Foo provider integration\"\ncredential_mode = \"none\"",
        );
        assert!(parse_and_validate(&none_with_flow, HostPolicy::default()).is_err());

        let manual_with_flow = GOOD.replace(
            "description = \"Foo provider integration\"",
            "description = \"Foo provider integration\"\ncredential_mode = \"manual\"",
        );
        assert!(parse_and_validate(&manual_with_flow, HostPolicy::default())
            .unwrap_err()
            .to_string()
            .contains("may not declare auth_flow"));
    }

    #[test]
    fn rejects_integration_referencing_missing_capability() {
        let bad = GOOD.replace(
            "model_source = \"foo-models\"",
            "model_source = \"missing-models\"",
        );
        let err = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown model_source 'missing-models'"),
            "{err}"
        );
    }

    #[test]
    fn rejects_duplicate_integration_ids() {
        let duplicate = format!(
            "{GOOD}\n[[integrations]]\nid = \"foo\"\nname = \"Duplicate\"\nmodel_source = \"foo-models\"\n"
        );
        let err = parse_and_validate(&duplicate, HostPolicy::default()).unwrap_err();
        assert!(
            err.to_string().contains("duplicate integration id 'foo'"),
            "{err}"
        );
    }

    #[test]
    fn rejects_empty_integration_binding() {
        let bad = GOOD
            .replace("credential_strategy = \"foo-auth\"", "")
            .replace("auth_flow = \"foo-login\"", "")
            .replace("model_source = \"foo-models\"", "");
        let err = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(
            err.to_string()
                .contains("must reference at least one provided capability"),
            "{err}"
        );
    }

    #[test]
    fn rejects_integration_referencing_missing_auth_flow() {
        let bad = GOOD.replace("auth_flow = \"foo-login\"", "auth_flow = \"missing-login\"");
        let err = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown auth_flow 'missing-login'"),
            "{err}"
        );
    }

    #[test]
    fn rejects_invalid_secret_setting_default() {
        let bad = GOOD.replace(
            "[[ui.actions]]",
            "[[ui.settings]]\nkey = \"token\"\nlabel = \"Token\"\nkind = \"secret\"\ndefault = \"bad\"\n\n[[ui.actions]]",
        );
        let err = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(
            err.to_string().contains("may not declare a default"),
            "{err}"
        );
    }

    #[test]
    fn rejects_select_setting_without_options() {
        let bad = GOOD.replace(
            "[[ui.actions]]",
            "[[ui.settings]]\nkey = \"region\"\nlabel = \"Region\"\nkind = \"select\"\n\n[[ui.actions]]",
        );
        let err = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(err.to_string().contains("requires options"), "{err}");
    }

    #[test]
    fn rejects_adapter_provider_template_without_plugin_wire_sentinel() {
        let bad = GOOD.replace(
            "model_source = \"foo-models\"",
            "provider_adapter = \"foo-adapter\"\nmodel_source = \"foo-models\"\n\n[integrations.provider]\nbase_url = \"https://api.foo.example\"\nwire_format = \"openai\"",
        ).replace(
            "model_sources = [\"foo-models\"]",
            "model_sources = [\"foo-models\"]\nprovider_adapters = [\"foo-adapter\"]",
        );
        let err = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(
            err.to_string()
                .contains("provider_adapter requires wire_format 'plugin'"),
            "{err}"
        );
    }

    #[test]
    fn rejects_insecure_integration_provider_url() {
        let bad = GOOD.replace(
            "model_source = \"foo-models\"",
            "model_source = \"foo-models\"\n\n[integrations.provider]\nbase_url = \"http://api.foo.example\"\nwire_format = \"gemini\"",
        );
        let err = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(err.to_string().contains("base_url must use https"), "{err}");
    }

    #[test]
    fn rejects_ui_action_with_unknown_integration() {
        let bad = GOOD.replace("integration = \"foo\"", "integration = \"missing\"");
        let err = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(
            err.to_string()
                .contains("references unknown integration 'missing'"),
            "{err}"
        );
    }

    #[test]
    fn rejects_unsupported_ui_action_kind() {
        let bad = GOOD.replace("kind = \"auth\"", "kind = \"script\"");
        let err = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(
            err.to_string().contains("unsupported kind 'script'"),
            "{err}"
        );
    }

    #[test]
    fn rejects_binding_scope_for_unknown_credential_strategy() {
        let bad = GOOD.replace(
            "network_hosts = [\"api.foo.example\", \"*.svc.example\"]",
            "network_hosts = [\"api.foo.example\", \"*.svc.example\"]\ncredential_scopes = [\"credential_strategy:missing\"]",
        );
        let err = parse_and_validate(&bad, HostPolicy::default()).unwrap_err();
        assert!(
            err.to_string()
                .contains("references a credential strategy this plugin does not provide"),
            "{err}"
        );
    }

    #[test]
    fn cached_routing_facts_require_bounded_refresh_cadence() {
        let cached = GOOD
            .replace(
                "plugin_api = \"1\"",
                "plugin_api = \"1\"\nrouting_facts_mode = \"cached\"\nrouting_facts_refresh_ms = 30000",
            )
            .replace(
                "model_sources = [\"foo-models\"]",
                "model_sources = [\"foo-models\"]\nrouting_facts = [\"capacity\"]",
            );
        assert!(parse_and_validate(&cached, HostPolicy::default()).is_ok());

        let too_fast = cached.replace(
            "routing_facts_refresh_ms = 30000",
            "routing_facts_refresh_ms = 1000",
        );
        let err = parse_and_validate(&too_fast, HostPolicy::default()).unwrap_err();
        assert!(
            err.to_string()
                .contains("routing_facts_refresh_ms must be 5000..=3600000"),
            "{err}"
        );
    }

    #[test]
    fn rejects_bad_wildcard() {
        let bad = GOOD.replace("\"*.svc.example\"", "\"api.*.example\"");
        assert!(parse_and_validate(&bad, HostPolicy::default()).is_err());
        let bare = GOOD.replace("\"*.svc.example\"", "\"*\"");
        assert!(parse_and_validate(&bare, HostPolicy::default()).is_err());
    }

    #[test]
    fn wildcard_matches_one_label() {
        assert!(host_matches("*.svc.example", "a.svc.example"));
        assert!(!host_matches("*.svc.example", "a.b.svc.example"));
        assert!(!host_matches("*.svc.example", "svc.example"));
        assert!(host_matches("api.foo.example", "api.foo.example"));
        assert!(!host_matches("api.foo.example", "evil.foo.example"));
    }
}
