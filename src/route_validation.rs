//! Semantic validation for Route configurations.
//!
//! This module owns the read-only checks used by the Admin API, CLI, and the
//! runtime Route resolver. Validation is deliberately based on persisted
//! provider/model/account and plugin metadata; it never probes an upstream.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::Value;

use crate::app::AppState;
use crate::db;

#[derive(Debug, Clone)]
pub struct RouteTargetConfig {
    pub model_id: String,
    pub account_id: Option<String>,
    pub priority: i64,
    pub weight: i64,
    pub predicate: Value,
    pub param_overrides: Value,
}

#[derive(Debug, Clone)]
pub struct RouteConfig {
    pub id: Option<String>,
    pub name: String,
    pub strategy: String,
    pub portability_policy: String,
    pub fallback_triggers: Value,
    pub max_attempts: Option<i64>,
    pub max_concurrent_requests: Option<i64>,
    pub enabled: bool,
    pub targets: Vec<RouteTargetConfig>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteValidationIssue {
    pub severity: &'static str,
    pub code: String,
    pub message: String,
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteValidation {
    pub valid: bool,
    pub route: String,
    pub issues: Vec<RouteValidationIssue>,
}

impl RouteValidation {
    fn new(route: &str) -> Self {
        Self {
            valid: true,
            route: route.to_owned(),
            issues: Vec::new(),
        }
    }

    fn error(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
        target: Option<usize>,
    ) {
        self.valid = false;
        let code = code.into();
        self.issues.push(RouteValidationIssue {
            severity: "error",
            field: issue_field(&code, target),
            code,
            message: message.into(),
            target_index: target,
        });
    }

    fn warning(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
        target: Option<usize>,
    ) {
        let code = code.into();
        self.issues.push(RouteValidationIssue {
            severity: "warning",
            field: issue_field(&code, target),
            code,
            message: message.into(),
            target_index: target,
        });
    }
}

fn issue_field(code: &str, target: Option<usize>) -> String {
    let field = match code {
        "missing_route_name" | "duplicate_route_name" | "route_name_shadowed_by_alias" => "name",
        "disabled_route" => "enabled",
        "empty_route" => "targets",
        "invalid_strategy" => "strategy",
        "invalid_portability_policy" => "portability_policy",
        "invalid_max_attempts" => "max_attempts",
        "invalid_concurrency" => "max_concurrent_requests",
        "invalid_fallback_trigger" | "invalid_fallback_triggers" => "fallback_triggers",
        "invalid_param_overrides" => "param_overrides",
        "invalid_predicate" | "unreachable_predicate" => "predicate",
        "non_positive_weight" => "weight",
        "missing_model" | "disabled_model" | "missing_provider" | "disabled_provider" => "model_id",
        "missing_account"
        | "account_provider_mismatch"
        | "disabled_account"
        | "account_temporarily_unavailable" => "account_id",
        _ => "",
    };
    match target {
        Some(index) if !field.is_empty() => format!("targets[{index}].{field}"),
        Some(index) => format!("targets[{index}]"),
        None if field.is_empty() => "$".into(),
        None => field.into(),
    }
}

/// Validate route shape and target-local fields without consulting persisted state.
pub fn validate_structure(config: &RouteConfig) -> RouteValidation {
    let mut result = RouteValidation::new(&config.name);
    validate_local_shape(config, &mut result);
    let mut seen_targets = HashSet::new();
    for (index, target) in config.targets.iter().enumerate() {
        validate_target_predicate(target, index, &mut result);
        if !target.param_overrides.is_null() && !target.param_overrides.is_object() {
            result.error(
                "invalid_param_overrides",
                "target parameter overrides must be a JSON object",
                Some(index),
            );
        }
        if target.weight <= 0 {
            result.warning(
                "non_positive_weight",
                "target weight is non-positive; runtime treats it as weight 1",
                Some(index),
            );
        }
        if !seen_targets.insert((target.model_id.as_str(), target.account_id.as_deref())) {
            result.error(
                "duplicate_target",
                "the same model/account target is configured more than once",
                Some(index),
            );
        }
        if predicate_is_never_true(&target.predicate) {
            result.error(
                "unreachable_predicate",
                "target predicate is statically false and can never admit a request",
                Some(index),
            );
        }
    }
    result
}

/// Validate a proposed or persisted Route against the current control-plane
/// snapshot. Only local metadata is read; no network or state mutation occurs.
pub async fn validate(state: &AppState, config: &RouteConfig) -> anyhow::Result<RouteValidation> {
    let mut result = validate_structure(config);

    let providers = db::list_providers(&state.pool).await?;
    let accounts = db::list_accounts(&state.pool).await?;
    let models = db::list_models(&state.pool).await?;
    let routes = db::list_routes(&state.pool).await?;
    let aliases = db::list_aliases(&state.pool).await?;

    let providers_by_id: HashMap<_, _> =
        providers.iter().map(|row| (row.id.as_str(), row)).collect();
    let accounts_by_id: HashMap<_, _> = accounts.iter().map(|row| (row.id.as_str(), row)).collect();
    let models_by_id: HashMap<_, _> = models.iter().map(|row| (row.id.as_str(), row)).collect();
    let routes_by_id: HashMap<_, _> = routes.iter().map(|row| (row.id.as_str(), row)).collect();

    let enabled_plugins: HashSet<String> = match state.plugin_manager() {
        Some(manager) => manager
            .list()
            .await?
            .into_iter()
            .filter(|plugin| plugin.status().is_enabled())
            .map(|plugin| plugin.id)
            .collect(),
        None => HashSet::new(),
    };

    let mut seen_candidate_targets = HashSet::new();
    for (index, target) in config.targets.iter().enumerate() {
        let Some(model) = models_by_id.get(target.model_id.as_str()).copied() else {
            result.error(
                "missing_model",
                format!("model '{}' does not exist", target.model_id),
                Some(index),
            );
            continue;
        };
        if model.enabled == 0 {
            result.error(
                "disabled_model",
                format!("model '{}' is disabled", model.display_name),
                Some(index),
            );
            continue;
        }
        let Some(provider) = providers_by_id.get(model.provider_id.as_str()).copied() else {
            result.error(
                "missing_provider",
                format!(
                    "provider '{}' for model '{}' does not exist",
                    model.provider_id, model.display_name
                ),
                Some(index),
            );
            continue;
        };
        if provider.enabled == 0 {
            result.error(
                "disabled_provider",
                format!(
                    "provider '{}' for model '{}' is disabled",
                    provider.name, model.display_name
                ),
                Some(index),
            );
            continue;
        }
        validate_provider_plugins(
            provider,
            &enabled_plugins,
            state.plugin_manager().is_some(),
            index,
            &mut result,
        );

        let candidate_accounts: Vec<&db::AccountRow> = match target.account_id.as_deref() {
            Some(account_id) => match accounts_by_id.get(account_id).copied() {
                None => {
                    result.error(
                        "missing_account",
                        format!("pinned account '{account_id}' does not exist"),
                        Some(index),
                    );
                    Vec::new()
                }
                Some(account) if account.provider_id != provider.id => {
                    result.error(
                        "account_provider_mismatch",
                        format!(
                            "pinned account '{}' belongs to another provider",
                            account.label
                        ),
                        Some(index),
                    );
                    Vec::new()
                }
                Some(account) if account.status == "disabled" => {
                    result.warning(
                        "disabled_account",
                        format!(
                            "pinned account '{}' is disabled; this target remains non-dispatchable",
                            account.label
                        ),
                        Some(index),
                    );
                    vec![account]
                }
                Some(account) => vec![account],
            },
            None => {
                let available: Vec<_> = accounts
                    .iter()
                    .filter(|account| {
                        account.provider_id == provider.id && account.status != "disabled"
                    })
                    .collect();
                if available.is_empty() {
                    result.error(
                        "unreachable_target",
                        format!(
                            "model '{}' has no enabled account in its provider pool",
                            model.display_name
                        ),
                        Some(index),
                    );
                }
                available
            }
        };
        if candidate_accounts.is_empty() {
            continue;
        }
        for account in &candidate_accounts {
            if !seen_candidate_targets.insert((target.model_id.as_str(), account.id.as_str())) {
                result.error(
                    "duplicate_target",
                    format!(
                        "model '{}' and account '{}' are already covered by another target",
                        model.display_name, account.label
                    ),
                    Some(index),
                );
            }
        }

        let mut valid_profiles = 0;
        for account in candidate_accounts {
            if account.status != "healthy" && account.status != "disabled" {
                result.warning(
                    "account_temporarily_unavailable",
                    format!(
                        "account '{}' is currently {}; runtime may defer or skip it",
                        account.label, account.status
                    ),
                    Some(index),
                );
            }
            match crate::adapters::resolve_execution_profile_for_target(
                provider,
                model,
                Some(&account.id),
            ) {
                Ok(profile) => match state.adapters.for_transport(&profile.transport) {
                    Ok(_) => valid_profiles += 1,
                    Err(error) => result.error(
                        "missing_transport_adapter",
                        format!(
                            "target '{}' has no usable adapter: {}",
                            model.display_name, error.message
                        ),
                        Some(index),
                    ),
                },
                Err(error) => result.error(
                    "invalid_execution_profile",
                    format!(
                        "target '{}' has an invalid execution profile: {}",
                        model.display_name, error.message
                    ),
                    Some(index),
                ),
            }
        }
        if valid_profiles == 0 {
            result.error(
                "unreachable_target",
                format!(
                    "target '{}' has no account with a valid execution profile",
                    model.display_name
                ),
                Some(index),
            );
        }
    }

    validate_aliases_and_route_identity(
        config,
        &aliases,
        &routes_by_id,
        &models_by_id,
        &providers_by_id,
        &mut result,
    );
    Ok(result)
}

fn validate_local_shape(config: &RouteConfig, result: &mut RouteValidation) {
    if config.name.trim().is_empty() {
        result.error("missing_route_name", "Route name is required", None);
    }
    if !matches!(
        config.strategy.as_str(),
        "priority" | "round-robin" | "weighted" | "least-used" | "adaptive"
    ) {
        result.error(
            "invalid_strategy",
            format!("unknown strategy '{}'", config.strategy),
            None,
        );
    }
    if !matches!(
        config.portability_policy.as_str(),
        "reject" | "strip_with_warning"
    ) {
        result.error(
            "invalid_portability_policy",
            "portability policy must be 'reject' or 'strip_with_warning'",
            None,
        );
    }
    if config.max_attempts.is_some_and(|attempts| attempts <= 0) {
        result.error(
            "invalid_max_attempts",
            "max_attempts must be greater than zero",
            None,
        );
    }
    if config
        .max_concurrent_requests
        .is_some_and(|limit| limit < 0)
    {
        result.error(
            "invalid_concurrency",
            "max_concurrent_requests cannot be negative",
            None,
        );
    }
    if let Some(triggers) = config.fallback_triggers.as_object() {
        for key in ["on429", "onQuota", "on5xx", "onTimeout"] {
            if triggers.get(key).is_some_and(|value| !value.is_boolean()) {
                result.error(
                    "invalid_fallback_trigger",
                    format!("fallback trigger '{key}' must be boolean"),
                    None,
                );
            }
        }
    } else if !config.fallback_triggers.is_null() {
        result.error(
            "invalid_fallback_triggers",
            "fallback_triggers must be a JSON object",
            None,
        );
    }
    if config.targets.is_empty() {
        result.error("empty_route", "Route has no targets", None);
    }
    if !config.enabled {
        result.warning(
            "disabled_route",
            "Route is disabled and will not receive traffic",
            None,
        );
    }
}

fn validate_target_predicate(
    target: &RouteTargetConfig,
    index: usize,
    result: &mut RouteValidation,
) {
    let value = &target.predicate;
    if value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty) {
        return;
    }
    let valid = serde_json::from_value::<crate::predicate::TargetPredicate>(value.clone())
        .ok()
        .is_some_and(|predicate| predicate.expr.is_some())
        || serde_json::from_value::<crate::predicate::Predicate>(value.clone()).is_ok();
    if !valid {
        result.error(
            "invalid_predicate",
            "target predicate is not a valid predicate expression",
            Some(index),
        );
    }
}

fn predicate_is_never_true(value: &Value) -> bool {
    let target = serde_json::from_value::<crate::predicate::TargetPredicate>(value.clone()).ok();
    let expression = target
        .and_then(|target| target.expr)
        .or_else(|| serde_json::from_value::<crate::predicate::Predicate>(value.clone()).ok());
    expression.as_ref().is_some_and(predicate_cannot_be_true)
}

fn predicate_cannot_be_true(expression: &crate::predicate::Predicate) -> bool {
    use crate::predicate::Predicate;
    match expression {
        Predicate::Const(value) => !value,
        Predicate::And { and } => and.iter().any(predicate_cannot_be_true),
        Predicate::Or { or } => or.iter().all(predicate_cannot_be_true),
        Predicate::Not { not } => predicate_is_always_true(not),
        Predicate::Cmp { .. } => false,
    }
}

fn predicate_is_always_true(expression: &crate::predicate::Predicate) -> bool {
    use crate::predicate::Predicate;
    match expression {
        Predicate::Const(value) => *value,
        Predicate::And { and } => and.iter().all(predicate_is_always_true),
        Predicate::Or { or } => or.iter().any(predicate_is_always_true),
        Predicate::Not { not } => predicate_cannot_be_true(not),
        Predicate::Cmp { .. } => false,
    }
}

fn validate_provider_plugins(
    provider: &db::ProviderRow,
    enabled_plugins: &HashSet<String>,
    plugin_host_available: bool,
    index: usize,
    result: &mut RouteValidation,
) {
    for reference in [
        provider.wire_plugin_ref(),
        provider.credential_plugin_ref(),
        provider.model_source_plugin_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if !plugin_host_available || !enabled_plugins.contains(&reference.plugin_id) {
            result.error(
                "missing_or_disabled_plugin",
                format!(
                    "provider '{}' requires plugin '{}' which is missing or disabled",
                    provider.name, reference.plugin_id
                ),
                Some(index),
            );
        }
    }
}

fn validate_aliases_and_route_identity(
    config: &RouteConfig,
    aliases: &[db::AliasRow],
    routes: &HashMap<&str, &db::RouteRow>,
    models: &HashMap<&str, &db::ModelRow>,
    providers: &HashMap<&str, &db::ProviderRow>,
    result: &mut RouteValidation,
) {
    if let Some(existing) = routes
        .values()
        .find(|route| route.name == config.name && Some(route.id.as_str()) != config.id.as_deref())
    {
        result.error(
            "duplicate_route_name",
            format!(
                "Route name '{}' is already used by Route '{}'",
                config.name, existing.id
            ),
            None,
        );
    }

    let mut alias_names = HashSet::new();
    let aliases_by_identifier: HashMap<&str, &str> = aliases
        .iter()
        .flat_map(|alias| {
            [
                (alias.id.as_str(), alias.alias.as_str()),
                (alias.alias.as_str(), alias.alias.as_str()),
            ]
        })
        .collect();
    let alias_edges: HashMap<String, String> = aliases
        .iter()
        .filter(|alias| alias.target_type == "alias")
        .filter_map(|alias| {
            aliases_by_identifier
                .get(alias.target_id.as_str())
                .map(|target| (alias.alias.clone(), (*target).to_owned()))
        })
        .collect();
    let alias_cycles = find_alias_cycles(&alias_edges);
    for alias in aliases {
        alias_names.insert(alias.alias.as_str());
        match alias.target_type.as_str() {
            "route" => match routes.get(alias.target_id.as_str()) {
                None => result.error(
                    "dangling_route_alias",
                    format!("alias '{}' points to a missing Route", alias.alias),
                    None,
                ),
                Some(route) if route.enabled == 0 => result.warning(
                    "alias_to_disabled_route",
                    format!(
                        "alias '{}' points to disabled Route '{}'",
                        alias.alias, route.name
                    ),
                    None,
                ),
                Some(_) => {}
            },
            "model" => match models.get(alias.target_id.as_str()) {
                None => result.error(
                    "dangling_model_alias",
                    format!("alias '{}' points to a missing model", alias.alias),
                    None,
                ),
                Some(model) if model.enabled == 0 => result.warning(
                    "alias_to_disabled_model",
                    format!(
                        "alias '{}' points to disabled model '{}'",
                        alias.alias, model.display_name
                    ),
                    None,
                ),
                Some(model) => match providers.get(model.provider_id.as_str()) {
                    None => result.error(
                        "alias_model_missing_provider",
                        format!(
                            "alias '{}' points to a model whose provider is missing",
                            alias.alias
                        ),
                        None,
                    ),
                    Some(provider) if provider.enabled == 0 => result.warning(
                        "alias_to_disabled_provider",
                        format!(
                            "alias '{}' points to a model whose provider '{}' is disabled",
                            alias.alias, provider.name
                        ),
                        None,
                    ),
                    Some(_) => {}
                },
            },
            "alias" => {
                if alias_cycles.contains(&alias.alias) {
                    result.error(
                        "alias_cycle",
                        format!("alias '{}' participates in an alias cycle", alias.alias),
                        None,
                    );
                } else if aliases_by_identifier.contains_key(alias.target_id.as_str()) {
                    result.error(
                        "unsupported_alias_chain",
                        format!(
                            "alias '{}' targets another alias; alias chains are not supported",
                            alias.alias
                        ),
                        None,
                    );
                } else {
                    result.error(
                        "dangling_alias_target",
                        format!(
                            "alias '{}' targets a missing alias '{}'",
                            alias.alias, alias.target_id
                        ),
                        None,
                    );
                }
            }
            other => result.error(
                "invalid_alias_target_type",
                format!(
                    "alias '{}' has unsupported target type '{other}'",
                    alias.alias
                ),
                None,
            ),
        }
    }

    if alias_names.contains(config.name.as_str()) {
        if let Some(alias) = aliases.iter().find(|alias| alias.alias == config.name) {
            if alias.target_type != "route"
                || Some(alias.target_id.as_str()) != config.id.as_deref()
            {
                result.error(
                    "route_name_shadowed_by_alias",
                    format!(
                        "alias '{}' shadows this Route name and resolves elsewhere",
                        config.name
                    ),
                    None,
                );
            }
        }
    }
}

fn find_alias_cycles(edges: &HashMap<String, String>) -> HashSet<String> {
    let mut completed = HashSet::new();
    let mut cycles = HashSet::new();

    for start in edges.keys() {
        if completed.contains(start) {
            continue;
        }
        let mut path = Vec::new();
        let mut positions = HashMap::new();
        let mut current = start.clone();

        loop {
            if completed.contains(&current) {
                break;
            }
            if let Some(cycle_start) = positions.get(&current).copied() {
                cycles.extend(path[cycle_start..].iter().cloned());
                break;
            }
            positions.insert(current.clone(), path.len());
            path.push(current.clone());
            let Some(next) = edges.get(&current) else {
                break;
            };
            current = next.clone();
        }

        completed.extend(path);
    }
    cycles
}

/// Validate a Route already stored in the control plane by id or name.
pub async fn validate_saved(state: &AppState, selector: &str) -> anyhow::Result<RouteValidation> {
    let routes = db::list_routes(&state.pool).await?;
    let Some(route) = routes
        .iter()
        .find(|route| route.id == selector || route.name == selector)
    else {
        anyhow::bail!("no Route named or identified by '{selector}'");
    };
    let targets = db::route_targets(&state.pool, &route.id).await?;
    let config = RouteConfig {
        id: Some(route.id.clone()),
        name: route.name.clone(),
        strategy: route.strategy.clone(),
        portability_policy: route.portability_policy.clone(),
        fallback_triggers: serde_json::from_str(&route.fallback_triggers).unwrap_or(Value::Null),
        max_attempts: route.max_attempts,
        max_concurrent_requests: route.max_concurrent_requests,
        enabled: route.enabled != 0,
        targets: targets
            .into_iter()
            .map(|target| RouteTargetConfig {
                model_id: target.model_id,
                account_id: target.account_id,
                priority: target.priority,
                weight: target.weight,
                predicate: serde_json::from_str(&target.predicate).unwrap_or(Value::Null),
                param_overrides: serde_json::from_str(&target.param_overrides)
                    .unwrap_or(Value::Null),
            })
            .collect(),
    };
    validate(state, &config).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alias(id: &str, name: &str, target_type: &str, target_id: &str) -> db::AliasRow {
        db::AliasRow {
            id: id.to_owned(),
            alias: name.to_owned(),
            target_type: target_type.to_owned(),
            target_id: target_id.to_owned(),
            description: String::new(),
            created_at: String::new(),
        }
    }

    fn config(name: &str, id: Option<&str>) -> RouteConfig {
        RouteConfig {
            id: id.map(str::to_owned),
            name: name.to_owned(),
            strategy: "priority".into(),
            portability_policy: "strip_with_warning".into(),
            fallback_triggers: Value::Null,
            max_attempts: None,
            max_concurrent_requests: None,
            enabled: true,
            targets: Vec::new(),
        }
    }

    fn alias_validation(config: &RouteConfig, aliases: &[db::AliasRow]) -> RouteValidation {
        let mut result = RouteValidation::new(&config.name);
        validate_aliases_and_route_identity(
            config,
            aliases,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &mut result,
        );
        result
    }

    #[test]
    fn validation_fields_identify_route_and_indexed_target_inputs() {
        assert_eq!(issue_field("missing_route_name", None), "name");
        assert_eq!(issue_field("invalid_strategy", None), "strategy");
        assert_eq!(issue_field("missing_model", Some(2)), "targets[2].model_id");
        assert_eq!(
            issue_field("account_provider_mismatch", Some(1)),
            "targets[1].account_id"
        );
        assert_eq!(
            issue_field("unreachable_predicate", Some(0)),
            "targets[0].predicate"
        );
        assert_eq!(issue_field("duplicate_target", Some(0)), "targets[0]");
        assert_eq!(issue_field("empty_route", None), "targets");
        assert_eq!(issue_field("alias_cycle", None), "$");
    }

    #[test]
    fn rejects_invalid_route_shape_and_constraints() {
        let mut proposed = config("bad-route", None);
        proposed.strategy = "unknown".into();
        proposed.portability_policy = "silent-strip".into();
        proposed.max_attempts = Some(0);
        proposed.max_concurrent_requests = Some(-1);
        proposed.fallback_triggers = serde_json::json!({"onQuota": "sometimes"});
        proposed.targets.push(RouteTargetConfig {
            model_id: "model".into(),
            account_id: None,
            priority: 1,
            weight: 1,
            predicate: Value::Null,
            param_overrides: Value::Null,
        });

        let mut validation = RouteValidation::new(&proposed.name);
        validate_local_shape(&proposed, &mut validation);
        let codes: HashSet<_> = validation
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect();
        assert!(!validation.valid);
        assert!(codes.contains("invalid_strategy"));
        assert!(codes.contains("invalid_portability_policy"));
        assert!(codes.contains("invalid_max_attempts"));
        assert!(codes.contains("invalid_concurrency"));
        assert!(codes.contains("invalid_fallback_trigger"));
    }

    #[test]
    fn detects_predicates_that_can_never_admit_a_request() {
        for predicate in [
            serde_json::json!(false),
            serde_json::json!({"and": [false, {"fact": "has_tools", "value": true}]}),
            serde_json::json!({"or": [false, {"not": true}]}),
        ] {
            assert!(predicate_is_never_true(&predicate), "{predicate}");
        }
        assert!(!predicate_is_never_true(&serde_json::json!(true)));
        assert!(!predicate_is_never_true(&serde_json::json!({
            "fact": "has_tools",
            "value": true
        })));
    }

    #[test]
    fn detects_self_and_multi_alias_cycles() {
        let self_cycle = alias("id-self", "self", "alias", "id-self");
        let first = alias("id-first", "first", "alias", "id-second");
        let second = alias("id-second", "second", "alias", "id-first");
        let validation = alias_validation(&config("test", None), &[self_cycle, first, second]);
        let cycles: HashSet<_> = validation
            .issues
            .iter()
            .filter(|issue| issue.code == "alias_cycle")
            .map(|issue| issue.message.as_str())
            .collect();

        assert_eq!(cycles.len(), 3);
        assert!(!validation.valid);
    }

    #[test]
    fn does_not_misreport_an_acyclic_alias_chain_as_a_cycle() {
        let first = alias("id-first", "first", "alias", "id-second");
        let second = alias("id-second", "second", "model", "model-id");
        let validation = alias_validation(&config("test", None), &[first, second]);

        assert!(!validation.valid);
        assert!(validation
            .issues
            .iter()
            .any(|issue| issue.code == "unsupported_alias_chain"));
        assert!(!validation
            .issues
            .iter()
            .any(|issue| issue.code == "alias_cycle"));
    }

    #[test]
    fn detects_dangling_route_aliases_and_route_name_shadowing() {
        let dangling = alias("id-dangling", "dangling", "route", "missing-route");
        let dangling_validation = alias_validation(&config("test", None), &[dangling]);
        assert!(dangling_validation
            .issues
            .iter()
            .any(|issue| issue.code == "dangling_route_alias"));

        let shadow = alias("id-shadow", "test", "model", "model-id");
        let shadow_validation = alias_validation(&config("test", None), &[shadow]);
        assert!(shadow_validation
            .issues
            .iter()
            .any(|issue| issue.code == "route_name_shadowed_by_alias"));
    }
}
