//! Validate / Dry Run for configuration edits (FR-8.6, NFR-6.4).
//!
//! Before an admin applies a provider/model/account edit, Kinetix can validate
//! the proposed change without mutating production state. This module covers
//! **schema** and **metadata** validation:
//!
//! * required fields present and well-typed;
//! * wire-format / auth-scheme validity and their required companion fields;
//! * missing/unknown price and capability data (FR-6.3, FR-6.8) — reported as
//!   `unknown` warnings, never silently treated as zero/known;
//! * parameter-spec consistency (FR-10.6).
//!
//! Outbound security (scheme/TLS/SSRF, credential-host binding) is validated by
//! the admin layer that owns those helpers; Route Dry Run (candidate ordering,
//! predicate outcomes, would-be selection) lives in `pipeline::dry_run` because
//! it needs the live snapshot and account-health logic.

use serde_json::{json, Value};

use crate::types::{ParamPolicy, Prices, WireFormat};

pub(crate) fn is_auth_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "x-api-key" | "x-goog-api-key"
    )
}

/// Schema-level validation of a proposed provider (FR-8.6).
pub fn validate_provider_schema(
    name: &str,
    base_url: &str,
    wire_format: &str,
    auth_scheme: &str,
    custom_header_name: Option<&str>,
    custom_param_name: Option<&str>,
) -> Vec<String> {
    let mut problems = Vec::new();
    if name.trim().is_empty() {
        problems.push("provider name is required".into());
    }
    if base_url.trim().is_empty() {
        problems.push("base_url is required".into());
    }
    if WireFormat::parse(wire_format).is_none() {
        problems.push(format!(
            "unknown wire_format '{wire_format}' (expected openai, anthropic, gemini, or plugin)"
        ));
    }
    match auth_scheme {
        "bearer" | "none" => {}
        "custom_header" => {
            if custom_header_name.unwrap_or("").trim().is_empty() {
                problems.push("auth_scheme 'custom_header' requires custom_header_name".into());
            }
        }
        "query_param" => {
            if custom_param_name.unwrap_or("").trim().is_empty() {
                problems.push("auth_scheme 'query_param' requires custom_param_name".into());
            }
        }
        other => problems.push(format!(
            "unknown auth_scheme '{other}' (expected none, bearer, custom_header, or query_param)"
        )),
    }
    problems
}

/// Schema + metadata validation of a proposed model (FR-8.6). Missing prices
/// and capabilities are reported as `unknown` warnings, never as errors
/// (FR-6.3/6.8 — "unknown means unknown").
pub fn validate_model(
    upstream_id: &str,
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    capabilities: &Value,
    prices: &Value,
    parameters: &Value,
) -> Value {
    let mut problems: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    if upstream_id.trim().is_empty() {
        problems.push("upstream_id is required".into());
    }
    if let (Some(c), Some(m)) = (context_window, max_output_tokens) {
        if m > c {
            problems.push(format!(
                "max_output_tokens ({m}) exceeds context_window ({c})"
            ));
        }
    }

    // Capability metadata: an empty/absent object means "unknown", which under
    // permissive mode does not reject but must be surfaced (FR-10.9/FR-12.11).
    let caps_known = capabilities
        .as_object()
        .map(|o| !o.is_empty())
        .unwrap_or(false);
    if !caps_known {
        warnings.push(
            "capabilities are not configured (unknown; permissive mode will not reject)".into(),
        );
    }

    let parsed_prices: Prices = serde_json::from_value(prices.clone()).unwrap_or_default();
    let price_state = match (
        parsed_prices.input_per_1m.is_some(),
        parsed_prices.output_per_1m.is_some(),
    ) {
        (true, true) => "known",
        (false, false) => "unknown",
        _ => "partial",
    };
    match price_state {
        "unknown" => warnings.push(
            "prices are not configured: cost will be recorded as unknown and USD budgets cannot be enforced for this model (FR-6.3)"
                .into(),
        ),
        "partial" => warnings.push(
            "prices are only partially configured: both input_per_1m and output_per_1m are required for known cost; requests that consume an unpriced base dimension will record unknown cost and USD budgets cannot be reliably enforced for this model (FR-6.3)"
                .into(),
        ),
        _ => {}
    }

    // Parameter specs: flag declared-but-inconsistent entries.
    if let Some(obj) = parameters.as_object() {
        for (key, spec) in obj {
            match serde_json::from_value::<crate::types::ParamSpec>(spec.clone()) {
                Ok(spec) => {
                    if spec.supported && spec.policy == ParamPolicy::Reject {
                        warnings.push(format!(
                            "parameter '{key}' is marked supported but uses policy 'reject'"
                        ));
                    }
                    if let (Some(min), Some(max)) = (spec.min, spec.max) {
                        if min > max {
                            problems.push(format!(
                                "parameter '{key}' has min ({min}) greater than max ({max})"
                            ));
                        }
                    }
                }
                Err(_) => problems.push(format!("parameter '{key}' has an invalid spec")),
            }
        }
    }

    json!({
        "valid": problems.is_empty(),
        "problems": problems,
        "warnings": warnings,
        "price_state": price_state,
        "capabilities_state": if caps_known { "configured" } else { "unknown" },
    })
}

pub(crate) fn credential_is_nonempty(secret: &str) -> bool {
    !secret.trim().is_empty()
}

/// Schema validation of a proposed account (FR-8.6).
pub fn validate_account(label: &str, api_key: Option<&str>, quota_type: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if label.trim().is_empty() {
        problems.push("account label is required".into());
    }
    match api_key {
        Some(k) if credential_is_nonempty(k) => {}
        _ => problems.push("an api_key is required to create an account".into()),
    }
    if !matches!(quota_type, "none" | "daily" | "monthly" | "rolling") {
        problems.push(format!(
            "unknown quota_type '{quota_type}' (expected none, daily, monthly, or rolling)"
        ));
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_validation_reports_unknown_prices_not_zero() {
        let v = validate_model(
            "up",
            Some(1000),
            Some(100),
            &json!({"text": true}),
            &json!({}),
            &json!({}),
        );
        assert_eq!(v["valid"], true);
        assert_eq!(v["price_state"], "unknown");
        assert!(!v["warnings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn model_validation_reports_partial_when_output_price_is_unknown() {
        let v = validate_model(
            "up",
            Some(1000),
            Some(100),
            &json!({"text": true}),
            &json!({"input_per_1m": 1.0, "output_per_1m": null}),
            &json!({}),
        );
        assert_eq!(v["valid"], true);
        assert_eq!(v["price_state"], "partial");
        let warnings = v["warnings"].as_array().unwrap();
        assert!(warnings.iter().any(|warning| {
            warning
                .as_str()
                .is_some_and(|warning| warning.contains("partially configured"))
        }));
    }

    #[test]
    fn model_validation_reports_partial_when_input_price_is_unknown() {
        let v = validate_model(
            "up",
            Some(1000),
            Some(100),
            &json!({"text": true}),
            &json!({"input_per_1m": null, "output_per_1m": 2.0}),
            &json!({}),
        );
        assert_eq!(v["valid"], true);
        assert_eq!(v["price_state"], "partial");
        let warnings = v["warnings"].as_array().unwrap();
        assert!(warnings.iter().any(|warning| {
            warning
                .as_str()
                .is_some_and(|warning| warning.contains("partially configured"))
        }));
    }

    #[test]
    fn model_validation_flags_max_output_over_context() {
        let v = validate_model(
            "up",
            Some(100),
            Some(500),
            &json!({}),
            &json!({}),
            &json!({}),
        );
        assert_eq!(v["valid"], false);
    }

    #[test]
    fn model_validation_flags_inverted_param_range() {
        let params = json!({"temperature": {"supported": true, "min": 2.0, "max": 1.0}});
        let v = validate_model("up", None, None, &json!({}), &json!({}), &params);
        assert_eq!(v["valid"], false);
    }

    #[test]
    fn provider_schema_requires_header_for_custom_auth() {
        let problems =
            validate_provider_schema("n", "https://x/v1", "openai", "custom_header", None, None);
        assert!(problems.iter().any(|p| p.contains("custom_header_name")));
        let problems =
            validate_provider_schema("n", "https://x/v1", "openai", "bearer", None, None);
        assert!(problems.is_empty());
        let problems =
            validate_provider_schema("n", "https://x/v1", "plugin", "bearer", None, None);
        assert!(problems.is_empty());
    }

    #[test]
    fn account_validation_requires_key_and_known_quota() {
        let problems = validate_account("acct", None, "none");
        assert!(problems.iter().any(|p| p.contains("api_key")));
        let problems = validate_account("acct", Some("sk-x"), "weekly");
        assert!(problems.iter().any(|p| p.contains("quota_type")));
        assert!(validate_account("acct", Some("sk-x"), "daily").is_empty());
    }
}
