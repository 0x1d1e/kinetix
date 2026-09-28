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
    model_entries(registry, key_allowed, key_allowed_providers)
        .into_iter()
        .map(|entry| ClientProfileModel { id: entry.name })
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
    let snap = registry.snapshot();
    // Client-facing names: aliases and Routes first, then bare upstream model IDs.
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

    entries.retain(|entry| {
        crate::db::VirtualKeyRow::model_is_allowed(key_allowed, &entry.name)
            && provider_policy_allows(&snap, &entry.name, key_allowed_providers)
    });
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
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
        crate::db::insert_model(
            pool,
            &crate::db::NewModel {
                provider_id,
                upstream_id: name,
                display_name: name,
                enabled: true,
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
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
