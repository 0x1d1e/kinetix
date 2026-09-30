//! Host-owned non-secret connection parameters and constrained URL expansion.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParameterType {
    Identifier,
}

/// Identifiers are ASCII letters/digits, underscores and hyphens. Delimiters,
/// dots and percent escapes are forbidden, including when double-encoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    #[serde(rename = "type")]
    pub kind: ParameterType,
    pub min_length: usize,
    pub max_length: usize,
}

/// Persisted separately from account credentials. Network policy is captured
/// from the declaring manifest, not weakened by parameter resolution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionParameters {
    #[serde(default)]
    pub declarations: BTreeMap<String, Parameter>,
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    #[serde(default)]
    pub network_hosts: Vec<String>,
}

impl ConnectionParameters {
    pub fn validate_declarations(
        &self,
        base_url: &str,
        models_path: Option<&str>,
    ) -> Result<(), String> {
        if self.declarations.len() > 16 || self.network_hosts.len() > 64 {
            return Err("connection parameters exceed declaration limits".into());
        }
        for (name, field) in &self.declarations {
            if !identifier(name)
                || name.len() > 64
                || field.min_length == 0
                || field.min_length > field.max_length
                || field.max_length > 256
            {
                return Err(format!("invalid connection parameter declaration '{name}'"));
            }
        }
        let url = Url::parse(base_url).map_err(|_| "invalid connection base_url")?;
        if !matches!(url.scheme(), "https" | "http")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "connection base_url must be an HTTP(S) URL without userinfo, query or fragment"
                    .into(),
            );
        }
        // URL parsers normalize encoded delimiters and dot segments. Inspect
        // the original authority/path before parsing or substituting anything.
        let (_, remainder) = base_url
            .split_once("://")
            .ok_or("invalid connection base_url")?;
        let (authority, path) = remainder.split_once('/').unwrap_or((remainder, ""));
        if authority.contains(['{', '}', '%']) {
            return Err("connection parameters are permitted only in URL path segments".into());
        }
        self.validate_path(path)?;
        if let Some(path) = models_path {
            if !path.starts_with('/') || path.starts_with("//") {
                return Err("connection models_path must be an absolute path".into());
            }
            self.validate_path(path)?;
        }
        for host in &self.network_hosts {
            if host.is_empty() || host.contains(['/', ':', '{', '}', '%', ' ', '?', '#']) {
                return Err("invalid connection network host".into());
            }
        }
        self.authorize_url(&url)?;
        Ok(())
    }

    fn validate_path(&self, path: &str) -> Result<(), String> {
        if path.len() > 4096 || path.contains(['%', '\\', '?', '#']) {
            return Err("connection path contains forbidden encoding or delimiters".into());
        }
        for segment in path.split('/') {
            if segment == "." || segment == ".." {
                return Err("connection path must not contain traversal segments".into());
            }
            if segment.contains(['{', '}']) {
                let name = segment
                    .strip_prefix('{')
                    .and_then(|s| s.strip_suffix('}'))
                    .ok_or("connection template variables must occupy a complete path segment")?;
                if !self.declarations.contains_key(name) {
                    return Err(format!("undeclared connection parameter '{name}'"));
                }
            }
        }
        Ok(())
    }

    pub fn validate_values(&self) -> Result<(), String> {
        if self
            .values
            .keys()
            .any(|name| !self.declarations.contains_key(name))
        {
            return Err("unexpected connection parameter".into());
        }
        for (name, field) in &self.declarations {
            let value = self
                .values
                .get(name)
                .ok_or_else(|| format!("missing connection parameter '{name}'"))?;
            if value.len() < field.min_length
                || value.len() > field.max_length
                || !identifier(value)
            {
                return Err(format!(
                    "invalid connection parameter '{name}': expected bounded ASCII identifier"
                ));
            }
        }
        Ok(())
    }

    pub fn resolve(
        &self,
        base_url: &str,
        models_path: Option<&str>,
    ) -> Result<(String, Option<String>), String> {
        self.validate_declarations(base_url, models_path)?;
        self.validate_values()?;
        let expand = |template: &str| {
            let mut resolved = template.to_string();
            for (name, value) in &self.values {
                // Validated identifiers consist entirely of URL-unreserved
                // bytes, so their encoded representation is identical.
                resolved = resolved.replace(&format!("{{{name}}}"), value);
            }
            resolved
        };
        let resolved = expand(base_url);
        let before = Url::parse(base_url).map_err(|_| "invalid connection base_url")?;
        let after = Url::parse(&resolved).map_err(|_| "invalid resolved connection URL")?;
        if before.origin() != after.origin() {
            return Err("connection expansion changed URL origin".into());
        }
        self.authorize_url(&after)?;
        Ok((resolved, models_path.map(expand)))
    }

    pub fn authorize_url(&self, url: &Url) -> Result<(), String> {
        if !self.network_hosts.is_empty()
            && !url.host_str().is_some_and(|host| {
                self.network_hosts
                    .iter()
                    .any(|allowed| crate::plugins::manifest::host_matches(allowed, host))
            })
        {
            return Err("connection destination is outside declared network_hosts".into());
        }
        Ok(())
    }
}

pub fn resolve_endpoint(
    base_url: &str,
    models_path: Option<&str>,
    parameters: Option<&ConnectionParameters>,
) -> Result<(String, Option<String>), String> {
    match parameters {
        Some(parameters) => parameters.resolve(base_url, models_path),
        None if base_url.contains(['{', '}'])
            || models_path.is_some_and(|path| path.contains(['{', '}'])) =>
        {
            Err("URL templates require declared connection parameters".into())
        }
        None => Ok((base_url.to_string(), models_path.map(str::to_string))),
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parameters() -> ConnectionParameters {
        serde_json::from_value(serde_json::json!({
            "declarations": {"account_id": {"type": "identifier", "min_length": 1, "max_length": 32}},
            "values": {"account_id": "abc123"},
            "network_hosts": ["api.example.com"]
        })).unwrap()
    }

    #[test]
    fn inference_and_discovery_share_public_identifier() {
        let (base, path) = parameters()
            .resolve(
                "https://api.example.com/accounts/{account_id}/ai",
                Some("/accounts/{account_id}/ai/models"),
            )
            .unwrap();
        assert_eq!(base, "https://api.example.com/accounts/abc123/ai");
        assert_eq!(path.as_deref(), Some("/accounts/abc123/ai/models"));
        let round_trip: ConnectionParameters =
            serde_json::from_str(&serde_json::to_string(&parameters()).unwrap()).unwrap();
        assert_eq!(round_trip.values["account_id"], "abc123");
    }

    #[test]
    fn rejects_missing_unexpected_oversized_and_injection_values() {
        let mut p = parameters();
        p.values.clear();
        assert!(p.validate_values().is_err());
        p.values.insert("extra".into(), "x".into());
        assert!(p.validate_values().is_err());
        p.values.remove("extra");
        for value in [
            "..",
            "../evil",
            "a/b",
            "a?x=y",
            "a#b",
            "a@evil",
            "%2f",
            "%252e",
            "a\\b",
            "é",
            "",
            &"a".repeat(33),
        ] {
            p.values.insert("account_id".into(), value.into());
            assert!(
                p.resolve("https://api.example.com/{account_id}", None)
                    .is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn rejects_origin_templates_traversal_and_undeclared_fields() {
        for template in [
            "https://{account_id}.example.com/x",
            "https://api.example.com/{unknown}",
            "https://api.example.com/prefix-{account_id}",
            "https://api.example.com/../{account_id}",
            "https://api.example.com/%2e%2e/{account_id}",
            "https://api.example.com/x?q={account_id}",
            "https://evil.example/{account_id}",
        ] {
            assert!(parameters().resolve(template, None).is_err(), "{template}");
        }
        assert!(parameters()
            .resolve(
                "https://api.example.com/{account_id}",
                Some("//evil.example/x")
            )
            .is_err());
        assert!(parameters()
            .authorize_url(&Url::parse("https://evil.example/x").unwrap())
            .is_err());
    }
}
