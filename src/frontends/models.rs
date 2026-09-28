//! Model listing (`GET /v1/models`, FR-1.2, FR-10.10) and format-correct error
//! body encoding.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};

use crate::db::ModelRow;
use crate::frontends::FrontendFormat;
use crate::registry::Registry;
use crate::types::{ErrorKind, ProxyError};

impl axum::response::IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut builder = Response::builder().status(status);
        let has_retry_after = self
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("retry-after"));
        if let Some(retry) = self.retry_after_secs {
            if !has_retry_after {
                builder = builder.header("retry-after", retry.to_string());
            }
        }
        for (k, v) in &self.headers {
            builder = builder.header(k, v);
        }
        let body = self
            .body_override
            .clone()
            .unwrap_or_else(|| serde_json::json!({"error": {"message": self.message}}));
        builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap_or_else(|_| Response::new(Body::from("internal error")))
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ClientProfileModel {
    pub id: String,
    #[serde(skip)]
    pub metadata: crate::client_profiles::ClientModelMetadata,
}

#[derive(Clone)]
struct ModelEntry {
    name: String,
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    capabilities: Option<Value>,
}

/// Return client-visible names that the key can actually resolve to at least
/// one provider. Routes remain available when any of their targets is allowed.
pub fn client_profile_models(
    registry: &Registry,
    key_allowed: &[String],
    key_allowed_providers: &[String],
) -> Vec<ClientProfileModel> {
    let snapshot = registry.snapshot();
    model_entries_in(&snapshot, key_allowed, key_allowed_providers)
        .into_iter()
        .map(|entry| ClientProfileModel {
            metadata: client_model_metadata(&snapshot, &entry.name, key_allowed_providers),
            id: entry.name,
        })
        .collect()
}

/// Build the model-list body in the calling frontend's format.
pub fn models_body(
    format: FrontendFormat,
    registry: &Registry,
    key_allowed: &[String],
    key_allowed_providers: &[String],
) -> Value {
    let entries = model_entries(registry, key_allowed, key_allowed_providers);
    let created = chrono::Utc::now().timestamp();

    match format {
        FrontendFormat::OpenAi | FrontendFormat::OpenAiResponses => {
            let data: Vec<Value> = entries
                .iter()
                .map(|entry| {
                    let mut obj = json!({
                        "id": entry.name,
                        "object": "model",
                        "created": created,
                        "owned_by": "kinetix",
                    });
                    if let Some(c) = entry.context_window {
                        obj["context_window"] = json!(c);
                    }
                    if let Some(m) = entry.max_output_tokens {
                        obj["max_output_tokens"] = json!(m);
                    }
                    // Only explicitly configured capabilities are exposed; unknown
                    // metadata stays omitted, never assumed (FR-10.10).
                    if let Some(c) = &entry.capabilities {
                        obj["capabilities"] = c.clone();
                    }
                    obj
                })
                .collect();
            json!({ "object": "list", "data": data })
        }
        FrontendFormat::Anthropic => {
            let data: Vec<Value> = entries
                .iter()
                .map(|entry| {
                    json!({
                        "type": "model",
                        "id": entry.name,
                        "display_name": entry.name,
                        "created_at": chrono::Utc::now().to_rfc3339(),
                    })
                })
                .collect();
            let first = entries.first().map(|entry| entry.name.clone());
            let last = entries.last().map(|entry| entry.name.clone());
            json!({
                "data": data,
                "has_more": false,
                "first_id": first,
                "last_id": last,
            })
        }
    }
}

fn model_entries(
    registry: &Registry,
    key_allowed: &[String],
    key_allowed_providers: &[String],
) -> Vec<ModelEntry> {
    let snapshot = registry.snapshot();
    model_entries_in(&snapshot, key_allowed, key_allowed_providers)
}

fn model_entries_in(
    snap: &crate::registry::Snapshot,
    key_allowed: &[String],
    key_allowed_providers: &[String],
) -> Vec<ModelEntry> {
    // Client-facing names: aliases and Routes first, then upstream IDs. Add
    // provider-qualified IDs only when a key explicitly grants one.
    let mut entries: Vec<ModelEntry> = Vec::new();
    for alias in snap.aliases.values() {
        let (context_window, max_output_tokens, capabilities) = match alias.target_type.as_str() {
            "route" => (None, None, None),
            _ => snap
                .models
                .get(&alias.target_id)
                .map(|model| {
                    (
                        model.context_window,
                        model.max_output_tokens,
                        declared_caps(model),
                    )
                })
                .unwrap_or((None, None, None)),
        };
        entries.push(ModelEntry {
            name: alias.alias.clone(),
            context_window,
            max_output_tokens,
            capabilities,
        });
    }
    for route in snap.routes.values() {
        if route.enabled != 0 && !entries.iter().any(|entry| entry.name == route.name) {
            entries.push(ModelEntry {
                name: route.name.clone(),
                context_window: None,
                max_output_tokens: None,
                capabilities: None,
            });
        }
    }
    for model in snap.models.values() {
        if model.enabled != 0 && !entries.iter().any(|entry| entry.name == model.upstream_id) {
            entries.push(ModelEntry {
                name: model.upstream_id.clone(),
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
                capabilities: declared_caps(model),
            });
        }
    }
    for model in snap.models.values().filter(|model| model.enabled != 0) {
        let Some(provider) = snap.providers.get(&model.provider_id) else {
            continue;
        };
        let mut provider_names = [provider.name.as_str(), provider.id.as_str()];
        provider_names.sort_unstable();
        for provider_name in provider_names {
            let name = format!("{provider_name}/{}", model.upstream_id);
            if !has_provider_qualified_grant(key_allowed, &name)
                || entries.iter().any(|entry| entry.name == name)
            {
                continue;
            }
            entries.push(ModelEntry {
                name,
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
                capabilities: declared_caps(model),
            });
        }
    }

    entries.retain(|entry| {
        crate::db::VirtualKeyRow::model_is_allowed(key_allowed, &entry.name)
            && provider_policy_allows(&snap, &entry.name, key_allowed_providers)
    });
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

fn has_provider_qualified_grant(allowed: &[String], model: &str) -> bool {
    // Only expose qualified names when a grant explicitly scopes a provider.
    // A wildcard-only key keeps the existing concise model list.
    allowed.iter().any(|grant| {
        grant.contains('/')
            && crate::db::VirtualKeyRow::model_is_allowed(std::slice::from_ref(grant), model)
    })
}

fn provider_policy_allows(
    snapshot: &crate::registry::Snapshot,
    name: &str,
    allowed_providers: &[String],
) -> bool {
    let Some(resolved) = Registry::resolve_in(snapshot, name) else {
        return false;
    };
    match resolved {
        crate::registry::Resolved::Single { provider_id, .. } => {
            provider_ids_allowed(std::iter::once(provider_id.as_str()), allowed_providers)
        }
        crate::registry::Resolved::Route { targets, .. } => provider_ids_allowed(
            targets.iter().map(|target| target.provider.id.as_str()),
            allowed_providers,
        ),
    }
}

fn provider_ids_allowed<'a>(
    provider_ids: impl IntoIterator<Item = &'a str>,
    allowed_providers: &[String],
) -> bool {
    allowed_providers.is_empty()
        || provider_ids.into_iter().any(|provider_id| {
            allowed_providers
                .iter()
                .any(|allowed| allowed == provider_id)
        })
}

fn client_model_metadata(
    snapshot: &crate::registry::Snapshot,
    name: &str,
    allowed_providers: &[String],
) -> crate::client_profiles::ClientModelMetadata {
    use crate::registry::Resolved;

    let Some(resolved) = Registry::resolve_in(snapshot, name) else {
        return crate::client_profiles::ClientModelMetadata::default();
    };
    let models: Vec<ModelRow> = match resolved {
        Resolved::Single { model_id, .. } => snapshot
            .models
            .get(&model_id)
            .cloned()
            .into_iter()
            .collect(),
        Resolved::Route { targets, .. } => {
            let mut seen = std::collections::HashSet::new();
            targets
                .into_iter()
                .filter(|target| {
                    provider_ids_allowed(
                        std::iter::once(target.provider.id.as_str()),
                        allowed_providers,
                    )
                })
                .filter(|target| seen.insert(target.model.id.clone()))
                .map(|target| target.model)
                .collect()
        }
    };
    if models.is_empty() {
        return crate::client_profiles::ClientModelMetadata::default();
    }

    let capabilities: Vec<_> = models.iter().map(declared_caps).collect();
    let reasoning = capabilities
        .iter()
        .map(|caps| caps.as_ref()?.get("reasoning")?.as_bool())
        .collect::<Option<Vec<_>>>()
        .and_then(|values| {
            let first = *values.first()?;
            values.iter().all(|value| *value == first).then_some(first)
        });
    let mut input = Vec::new();
    if capabilities.iter().all(|caps| {
        caps.as_ref()
            .and_then(|caps| caps.get("text"))
            .and_then(Value::as_bool)
            == Some(true)
    }) {
        input.push("text".to_string());
    }
    if capabilities.iter().all(|caps| {
        caps.as_ref()
            .and_then(|caps| caps.get("vision"))
            .and_then(Value::as_bool)
            == Some(true)
    }) {
        input.push("image".to_string());
    }

    crate::client_profiles::ClientModelMetadata {
        reasoning,
        input: (!input.is_empty()).then_some(input),
        context_window: models
            .iter()
            .map(|model| model.context_window)
            .collect::<Option<Vec<_>>>()
            .and_then(|limits| limits.into_iter().min()),
        max_output_tokens: models
            .iter()
            .map(|model| model.max_output_tokens)
            .collect::<Option<Vec<_>>>()
            .and_then(|limits| limits.into_iter().min()),
    }
}

/// The explicitly declared boolean capabilities of a model, or `None` when the
/// model declares no capability metadata. Unknown metadata is never invented.
fn declared_caps(m: &ModelRow) -> Option<Value> {
    let v: Value = serde_json::from_str(&m.capabilities).ok()?;
    let obj = v.as_object()?;
    let mut out = serde_json::Map::new();
    for (k, val) in obj {
        if val.is_boolean() {
            out.insert(k.clone(), val.clone());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn insert_test_provider(pool: &crate::db::Pool, name: &str) -> String {
        crate::db::insert_provider(
            pool,
            &crate::db::NewProvider {
                name,
                base_url: "https://example.invalid",
                wire_format: crate::types::WireFormat::Openai,
                auth_scheme: crate::types::AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 1_000,
                capability_mode: "permissive",
                models_path: None,
                rate_limit_rules: json!({}),
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
        .unwrap()
    }

    async fn insert_test_model(pool: &crate::db::Pool, provider_id: &str, name: &str) -> String {
        insert_test_model_with_metadata(pool, provider_id, name, None, None, json!({})).await
    }

    async fn insert_test_model_with_metadata(
        pool: &crate::db::Pool,
        provider_id: &str,
        name: &str,
        context_window: Option<i64>,
        max_output_tokens: Option<i64>,
        capabilities: Value,
    ) -> String {
        crate::db::insert_model(
            pool,
            &crate::db::NewModel {
                provider_id,
                upstream_id: name,
                display_name: name,
                enabled: true,
                context_window,
                max_output_tokens,
                capabilities,
                prices: json!({}),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: json!({}),
            },
        )
        .await
        .unwrap()
    }

    async fn insert_test_route(pool: &crate::db::Pool, name: &str, targets: &[(&str, &str)]) {
        let route_id = crate::db::insert_route(
            pool,
            &crate::db::NewRoute {
                name,
                description: "",
                strategy: "priority",
                fallback_triggers: json!({}),
                portability_policy: "strip_with_warning",
                sticky_routing: false,
                cache_affinity: false,
                max_attempts: None,
            },
        )
        .await
        .unwrap();
        for (account_id, model_id) in targets {
            crate::db::insert_route_target(
                pool,
                &route_id,
                Some(account_id),
                model_id,
                1,
                1,
                "{}",
                "{}",
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn client_profile_models_respect_key_model_and_provider_grants() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-client-profile-models-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("kinetix.db");
        let database_url = format!("sqlite://{}", database.display());
        let pool = crate::db::connect(&database_url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();

        let permitted_provider = insert_test_provider(&pool, "permitted").await;
        let blocked_provider = insert_test_provider(&pool, "blocked").await;
        let permitted_account = crate::db::insert_account(
            &pool,
            &permitted_provider,
            "test-account",
            "encrypted-test-secret",
            "masked",
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        let blocked_account = crate::db::insert_account(
            &pool,
            &blocked_provider,
            "test-account",
            "encrypted-test-secret",
            "masked",
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        let permitted_model =
            insert_test_model(&pool, &permitted_provider, "permitted-model").await;
        let blocked_model = insert_test_model(&pool, &blocked_provider, "blocked-model").await;

        insert_test_route(
            &pool,
            "permitted-route",
            &[(&permitted_account, &permitted_model)],
        )
        .await;
        insert_test_route(
            &pool,
            "blocked-route",
            &[(&blocked_account, &blocked_model)],
        )
        .await;
        insert_test_route(
            &pool,
            "mixed-route",
            &[
                (&permitted_account, &permitted_model),
                (&blocked_account, &blocked_model),
            ],
        )
        .await;

        let registry = Registry::new();
        registry.reload(&pool).await.unwrap();
        let visible = client_profile_models(
            &registry,
            &["*".into()],
            std::slice::from_ref(&permitted_provider),
        );
        let ids: Vec<_> = visible.into_iter().map(|model| model.id).collect();
        assert_eq!(ids, ["mixed-route", "permitted-model", "permitted-route"]);

        let restricted = client_profile_models(
            &registry,
            &["permitted-*".into()],
            std::slice::from_ref(&permitted_provider),
        );
        let ids: Vec<_> = restricted.into_iter().map(|model| model.id).collect();
        assert_eq!(ids, ["permitted-model", "permitted-route"]);

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn client_profile_route_metadata_is_shared_and_conservative_across_targets() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-client-profile-route-metadata-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("kinetix.db");
        let database_url = format!("sqlite://{}", database.display());
        let pool = crate::db::connect(&database_url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();

        let provider_id = insert_test_provider(&pool, "profile-metadata").await;
        let account_id = crate::db::insert_account(
            &pool,
            &provider_id,
            "profile-metadata-account",
            "encrypted-test-secret",
            "masked",
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        let first_model = insert_test_model_with_metadata(
            &pool,
            &provider_id,
            "vision-model",
            Some(200_000),
            Some(8_192),
            json!({"text": true, "vision": true, "reasoning": true}),
        )
        .await;
        let second_model = insert_test_model_with_metadata(
            &pool,
            &provider_id,
            "text-model",
            Some(100_000),
            Some(4_096),
            json!({"text": true, "vision": false, "reasoning": true}),
        )
        .await;
        insert_test_route(
            &pool,
            "profile-route",
            &[(&account_id, &first_model), (&account_id, &second_model)],
        )
        .await;

        let registry = Registry::new();
        registry.reload(&pool).await.unwrap();
        let visible = client_profile_models(
            &registry,
            &["profile-route".into()],
            std::slice::from_ref(&provider_id),
        );
        let route = visible
            .iter()
            .find(|model| model.id == "profile-route")
            .unwrap();
        assert_eq!(route.metadata.reasoning, Some(true));
        assert_eq!(
            route.metadata.input.as_deref(),
            Some(["text".to_string()].as_slice())
        );
        assert_eq!(route.metadata.context_window, Some(100_000));
        assert_eq!(route.metadata.max_output_tokens, Some(4_096));
        assert_eq!(
            serde_json::to_value(route).unwrap(),
            json!({"id": "profile-route"})
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn client_profile_models_include_provider_qualified_exact_grants() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-qualified-client-profile-models-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("kinetix.db");
        let database_url = format!("sqlite://{}", database.display());
        let pool = crate::db::connect(&database_url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();

        let provider_a = insert_test_provider(&pool, "provider-a").await;
        let provider_b = insert_test_provider(&pool, "provider-b").await;
        insert_test_model(&pool, &provider_a, "model").await;
        insert_test_model(&pool, &provider_b, "model").await;

        let registry = Registry::new();
        registry.reload(&pool).await.unwrap();
        let visible = client_profile_models(
            &registry,
            &["provider-b/model".into()],
            std::slice::from_ref(&provider_b),
        );

        assert_eq!(
            visible
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>(),
            ["provider-b/model"]
        );
        assert!(matches!(
            registry.resolve("provider-b/model"),
            Some(crate::registry::Resolved::Single { provider_id, .. }) if provider_id == provider_b
        ));

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn model_visibility_applies_exact_and_prefix_key_grants() {
        use crate::db::VirtualKeyRow;

        assert!(VirtualKeyRow::model_is_allowed(
            &["route/*".into()],
            "route/coder"
        ));
        assert!(VirtualKeyRow::model_is_allowed(
            &["*".into()],
            "route/coder"
        ));
        assert!(VirtualKeyRow::model_is_allowed(&["exact".into()], "exact"));
        assert!(!VirtualKeyRow::model_is_allowed(
            &["route".into()],
            "route/coder"
        ));
        assert!(!VirtualKeyRow::model_is_allowed(
            &["other*".into()],
            "route/coder"
        ));
        assert!(!VirtualKeyRow::model_is_allowed(&[], "route/coder"));
    }

    #[test]
    fn route_visibility_requires_at_least_one_permitted_provider_target() {
        assert!(provider_ids_allowed(
            ["provider-a", "provider-b"],
            &["provider-b".into()]
        ));
        assert!(!provider_ids_allowed(
            ["provider-a", "provider-b"],
            &["provider-c".into()]
        ));
        assert!(provider_ids_allowed(["provider-a"], &[]));
    }
}

/// Encode an error body in the calling frontend's format (FR-2.8, NFR-6.2).
pub fn error_body(format: FrontendFormat, err: &ProxyError) -> Value {
    match format {
        FrontendFormat::OpenAi | FrontendFormat::OpenAiResponses => {
            let etype = match err.kind {
                ErrorKind::BadRequest | ErrorKind::Unsupported => "invalid_request_error",
                ErrorKind::Unauthorized => "authentication_error",
                ErrorKind::Forbidden => "permission_error",
                ErrorKind::NotFound => "invalid_request_error",
                ErrorKind::RateLimited => "rate_limit_error",
                ErrorKind::BudgetExceeded => "budget_exceeded",
                ErrorKind::AllTargetsUnavailable => "upstream_unavailable",
                ErrorKind::Upstream => "upstream_error",
                ErrorKind::Internal => "internal_error",
                ErrorKind::ServiceUnavailable => "service_unavailable",
            };
            json!({
                "error": {
                    "message": err.message,
                    "type": etype,
                    "param": null,
                    "code": etype
                }
            })
        }
        FrontendFormat::Anthropic => {
            let etype = match err.kind {
                ErrorKind::BadRequest | ErrorKind::Unsupported => "invalid_request_error",
                ErrorKind::Unauthorized => "authentication_error",
                ErrorKind::Forbidden => "permission_error",
                ErrorKind::NotFound => "not_found_error",
                ErrorKind::RateLimited => "rate_limit_error",
                ErrorKind::BudgetExceeded => "rate_limit_error",
                ErrorKind::AllTargetsUnavailable => "overloaded_error",
                ErrorKind::Upstream => "api_error",
                ErrorKind::Internal => "api_error",
                ErrorKind::ServiceUnavailable => "overloaded_error",
            };
            json!({
                "type": "error",
                "error": { "type": etype, "message": err.message }
            })
        }
    }
}
