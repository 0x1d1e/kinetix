//! Provider configuration checks shared by the Admin API (create, update, and
//! validation preview), the CLI, config import, and bootstrap seeding.
//!
//! Each entry point maps its input onto a [`ProviderDraft`] and runs [`check`]
//! (plus [`binding_problems`] when plugin bindings are configurable), so a
//! preview reports exactly what a write would reject. Transition rules that
//! depend on an existing Provider row stay with the write that owns them.

use std::collections::BTreeMap;

use crate::app::AppState;
use crate::config::Config;
use crate::provider_connection::ConnectionParameters;
use crate::types::WireFormat;

/// The operator's outbound endpoint policy (NFR-3.9, NFR-3.12).
#[derive(Debug, Clone, Copy, Default)]
pub struct OutboundPolicy {
    pub allow_insecure_tls: bool,
    pub allow_private_upstreams: bool,
}

impl From<&Config> for OutboundPolicy {
    fn from(config: &Config) -> Self {
        Self {
            allow_insecure_tls: config.allow_insecure_tls,
            allow_private_upstreams: config.allow_private_upstreams,
        }
    }
}

/// A proposed Provider configuration, independent of input syntax.
#[derive(Debug, Clone, Copy)]
pub struct ProviderDraft<'a> {
    pub name: &'a str,
    pub base_url: &'a str,
    pub wire_format: &'a str,
    pub auth_scheme: &'a str,
    pub custom_header_name: Option<&'a str>,
    pub custom_param_name: Option<&'a str>,
    pub extra_headers: &'a BTreeMap<String, String>,
    pub timeout_ms: i64,
    pub models_path: Option<&'a str>,
    pub credential_hosts: &'a str,
    pub wire_plugin: &'a str,
    pub credential_plugin: &'a str,
    pub model_source_plugin: &'a str,
    pub pricing_scope: Option<&'a str>,
    /// Whether the draft carries or expects an operator credential.
    pub configures_credential: bool,
    pub connection: Option<&'a ConnectionParameters>,
}

/// One reason a draft cannot be saved, attributed to a request field when one
/// is responsible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProblem {
    pub field: Option<&'static str>,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct ProviderCheck {
    pub problems: Vec<ProviderProblem>,
    pub warnings: Vec<String>,
    /// Whether the resolved endpoint passed the outbound policy; `None` when
    /// it could not be checked.
    pub outbound_passed: Option<bool>,
}

impl ProviderCheck {
    pub fn is_valid(&self) -> bool {
        self.problems.is_empty()
    }

    pub fn messages(&self) -> impl Iterator<Item = &str> {
        self.problems.iter().map(|problem| problem.message.as_str())
    }

    fn reject(&mut self, field: Option<&'static str>, message: impl Into<String>) {
        self.problems.push(ProviderProblem {
            field,
            message: message.into(),
        });
    }
}

/// Apply requested connection values to the declared parameters, if any.
/// Values without declarations are rejected.
pub fn connection_with_values(
    declared: Option<ConnectionParameters>,
    values: Option<&BTreeMap<String, String>>,
) -> Result<Option<ConnectionParameters>, String> {
    match (declared, values) {
        (Some(mut parameters), Some(values)) => {
            parameters.values = values.clone();
            Ok(Some(parameters))
        }
        (None, Some(values)) if !values.is_empty() => {
            Err("connection values require declared parameters".into())
        }
        (declared, _) => Ok(declared),
    }
}

/// Validate everything about a draft that does not depend on installed plugins
/// or an existing Provider row. Never touches the network.
pub fn check(policy: OutboundPolicy, draft: &ProviderDraft<'_>) -> ProviderCheck {
    let mut result = ProviderCheck::default();
    if draft.name.trim().is_empty() {
        result.reject(Some("name"), "provider name is required");
    }
    if draft.base_url.trim().is_empty() {
        result.reject(Some("base_url"), "base_url is required");
    }
    let wire = WireFormat::parse(draft.wire_format);
    if wire.is_none() {
        result.reject(
            Some("wire_format"),
            format!(
                "unknown wire_format '{}' (expected openai, anthropic, gemini, or plugin)",
                draft.wire_format
            ),
        );
    }
    match draft.auth_scheme {
        "bearer" | "none" => {}
        "custom_header" => {
            if draft.custom_header_name.unwrap_or("").trim().is_empty() {
                result.reject(
                    Some("custom_header_name"),
                    "auth_scheme 'custom_header' requires custom_header_name",
                );
            }
        }
        "query_param" => {
            if draft.custom_param_name.unwrap_or("").trim().is_empty() {
                result.reject(
                    Some("custom_param_name"),
                    "auth_scheme 'query_param' requires custom_param_name",
                );
            }
        }
        other => result.reject(
            Some("auth_scheme"),
            format!(
                "unknown auth_scheme '{other}' (expected none, bearer, custom_header, or query_param)"
            ),
        ),
    }
    if draft.auth_scheme == "none"
        && (draft.configures_credential
            || !draft.credential_plugin.is_empty()
            || draft.custom_header_name.is_some()
            || draft.custom_param_name.is_some()
            || draft
                .extra_headers
                .keys()
                .any(|name| crate::validate::is_auth_header(name)))
    {
        result.reject(
            None,
            "no-auth providers must not configure credentials or auth fields",
        );
    }
    if draft.timeout_ms <= 0 {
        result.reject(Some("timeout_ms"), "timeout_ms must be a positive integer");
    }
    if wire == Some(WireFormat::Plugin) && draft.wire_plugin.trim().is_empty() {
        result.reject(
            Some("wire_plugin"),
            "wire_format 'plugin' requires a wire_plugin binding",
        );
    }
    if draft
        .pricing_scope
        .is_some_and(|scope| !matches!(scope, "direct_api" | "integration"))
    {
        result.reject(
            Some("pricing_scope"),
            "pricing_scope must be 'direct_api' or 'integration'",
        );
    }
    if !draft.base_url.trim().is_empty() {
        match crate::provider_connection::resolve_endpoint(
            draft.base_url,
            draft.models_path,
            draft.connection,
        ) {
            Ok((resolved, _)) => {
                let outbound = check_outbound_url(policy, &resolved);
                result.outbound_passed = Some(outbound.is_ok());
                if let Err(problem) = outbound {
                    result.reject(Some("base_url"), problem);
                }
            }
            Err(problem) => result.reject(Some("base_url"), problem),
        }
    }
    // Credential-host binding (NFR-3.11): entries are bare hosts; a scheme,
    // path, or space means the binding would never match.
    for entry in draft.credential_hosts.split(',') {
        let host = entry.trim();
        if host.contains('/') || host.contains(' ') || host.contains("://") {
            result.reject(
                Some("credential_hosts"),
                format!(
                    "credential_hosts entry '{host}' is not a bare host (drop the scheme/path)"
                ),
            );
        }
    }
    if wire == Some(WireFormat::Anthropic)
        && !draft
            .extra_headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("anthropic-version"))
    {
        result.warnings.push(
            "anthropic wire format: set an 'anthropic-version' extra header (Kinetix adds no hidden defaults)"
                .into(),
        );
    }
    result
}

/// Guardrail for operator-supplied endpoints (NFR-3.9): HTTPS by default, and
/// loopback/link-local/private/metadata ranges blocked unless explicitly allowed.
pub fn check_outbound_url(policy: OutboundPolicy, url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let host = parsed.host_str().ok_or("URL must have a host")?;
    // TLS is mandatory except in the explicit, visibly-marked dev mode
    // (NFR-3.12). KINETIX_ALLOW_INSECURE_TLS is that override; the
    // private-upstreams flag must NOT silently disable TLS.
    if parsed.scheme() != "https" && !policy.allow_insecure_tls {
        return Err(
            "endpoint must use https (set KINETIX_ALLOW_INSECURE_TLS=true to override for local development)"
                .into(),
        );
    }
    // The private-upstreams flag only relaxes the blocked-host check (NFR-3.9).
    if !policy.allow_private_upstreams && crate::net::is_blocked_host(host) {
        return Err(format!(
            "host '{host}' resolves to a blocked private/metadata range; set KINETIX_ALLOW_PRIVATE_UPSTREAMS=true to allow"
        ));
    }
    Ok(())
}

/// Problems with the draft's plugin capability bindings: syntax, and whether
/// each resolves to an installed, enabled, approved plugin.
pub async fn binding_problems(state: &AppState, draft: &ProviderDraft<'_>) -> Vec<String> {
    use crate::plugins::Capability;

    let bindings = [
        (
            "wire_plugin",
            draft.wire_plugin,
            &[Capability::ProviderAdapter][..],
            "provider_adapters",
        ),
        (
            "credential_plugin",
            draft.credential_plugin,
            &[Capability::CredentialStrategy][..],
            "credential_strategies",
        ),
        (
            "model_source_plugin",
            draft.model_source_plugin,
            &[Capability::AccountModelSource, Capability::ModelSource][..],
            "account_model_sources or model_sources",
        ),
    ];

    let mut problems = Vec::new();
    for (field, reference, capabilities, provides) in bindings {
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
        let mut resolved = false;
        for capability in capabilities {
            if manager
                .resolve_binding(reference, *capability)
                .await
                .is_some()
            {
                resolved = true;
                break;
            }
        }
        if !resolved {
            problems.push(format!(
                "{field} reference '{reference}' does not resolve to an installed, enabled, approved plugin providing {provides}"
            ));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: OutboundPolicy = OutboundPolicy {
        allow_insecure_tls: false,
        allow_private_upstreams: false,
    };

    fn draft<'a>(headers: &'a BTreeMap<String, String>) -> ProviderDraft<'a> {
        ProviderDraft {
            name: "n",
            base_url: "https://api.example.com/v1",
            wire_format: "openai",
            auth_scheme: "bearer",
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: headers,
            timeout_ms: 120_000,
            models_path: None,
            credential_hosts: "",
            wire_plugin: "",
            credential_plugin: "",
            model_source_plugin: "",
            pricing_scope: None,
            configures_credential: false,
            connection: None,
        }
    }

    fn problems(draft: ProviderDraft<'_>) -> Vec<String> {
        check(POLICY, &draft)
            .problems
            .into_iter()
            .map(|problem| problem.message)
            .collect()
    }

    #[test]
    fn accepts_a_minimal_provider() {
        let headers = BTreeMap::new();
        assert!(problems(draft(&headers)).is_empty());
    }

    #[test]
    fn auth_schemes_require_their_companion_fields() {
        let headers = BTreeMap::new();
        let found = problems(ProviderDraft {
            auth_scheme: "custom_header",
            ..draft(&headers)
        });
        assert!(
            found.iter().any(|p| p.contains("custom_header_name")),
            "{found:?}"
        );
        let found = problems(ProviderDraft {
            auth_scheme: "query_param",
            ..draft(&headers)
        });
        assert!(
            found.iter().any(|p| p.contains("custom_param_name")),
            "{found:?}"
        );
    }

    #[test]
    fn no_auth_rejects_credentials_and_auth_headers() {
        let headers = BTreeMap::from([("Authorization".to_string(), "x".to_string())]);
        let found = problems(ProviderDraft {
            auth_scheme: "none",
            ..draft(&headers)
        });
        assert!(found.iter().any(|p| p.contains("no-auth")), "{found:?}");
        let empty = BTreeMap::new();
        let found = problems(ProviderDraft {
            auth_scheme: "none",
            configures_credential: true,
            ..draft(&empty)
        });
        assert!(found.iter().any(|p| p.contains("no-auth")), "{found:?}");
    }

    #[test]
    fn plugin_wire_format_requires_a_binding() {
        let headers = BTreeMap::new();
        let found = problems(ProviderDraft {
            wire_format: "plugin",
            ..draft(&headers)
        });
        assert!(found.iter().any(|p| p.contains("wire_plugin")), "{found:?}");
    }

    #[test]
    fn rejects_blocked_and_insecure_endpoints() {
        let headers = BTreeMap::new();
        for base_url in ["http://api.example.com/v1", "https://169.254.169.254/v1"] {
            let found = problems(ProviderDraft {
                base_url,
                ..draft(&headers)
            });
            assert_eq!(found.len(), 1, "{base_url}: {found:?}");
        }
    }

    #[test]
    fn rejects_malformed_scalar_fields() {
        let headers = BTreeMap::new();
        for (candidate, expected) in [
            (
                ProviderDraft {
                    name: " ",
                    ..draft(&headers)
                },
                "name",
            ),
            (
                ProviderDraft {
                    timeout_ms: 0,
                    ..draft(&headers)
                },
                "timeout_ms",
            ),
            (
                ProviderDraft {
                    credential_hosts: "a.example.com, https://b.example.com",
                    ..draft(&headers)
                },
                "credential_hosts",
            ),
            (
                ProviderDraft {
                    pricing_scope: Some("free"),
                    ..draft(&headers)
                },
                "pricing_scope",
            ),
            (
                ProviderDraft {
                    base_url: "https://api.example.com/{region}",
                    ..draft(&headers)
                },
                "connection parameters",
            ),
        ] {
            let found = problems(candidate);
            assert!(
                found.iter().any(|p| p.contains(expected)),
                "{expected}: {found:?}"
            );
        }
    }

    #[test]
    fn anthropic_without_version_header_is_a_warning() {
        let headers = BTreeMap::new();
        let result = check(
            POLICY,
            &ProviderDraft {
                wire_format: "anthropic",
                ..draft(&headers)
            },
        );
        assert!(result.is_valid());
        assert_eq!(result.warnings.len(), 1);
    }
}
