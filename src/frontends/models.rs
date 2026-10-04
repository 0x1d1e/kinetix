//! Model listing (`GET /v1/models`, FR-1.2, FR-10.10) and format-correct error
//! body encoding.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};

use std::collections::BTreeMap;

use crate::adapters::TargetTransport;
use crate::frontends::FrontendFormat;
use crate::model_capabilities::ModelCapabilityFlags;
use crate::model_state::{RuntimeModel, ThinkingSupport, OPERATOR_SOURCE};
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
    let snapshot = registry.snapshot();
    let entries = model_entries_in(&snapshot, key_allowed, key_allowed_providers);
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
                    let targets = runtime_targets(&snapshot, &entry.name, key_allowed_providers);
                    if let Some(listing) = Listing::intersect(&targets) {
                        listing.write(&mut obj);
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

fn model_entries_in(
    snap: &crate::registry::Snapshot,
    key_allowed: &[String],
    key_allowed_providers: &[String],
) -> Vec<ModelEntry> {
    // Client-facing names: aliases and Routes first, then upstream IDs. Add
    // provider-qualified IDs whenever the model grant permits the qualified name.
    let mut entries: Vec<ModelEntry> = Vec::new();
    let mut push = |name: &str| {
        if !entries.iter().any(|entry| entry.name == name) {
            entries.push(ModelEntry {
                name: name.to_string(),
            });
        }
    };
    for alias in snap.aliases.values() {
        push(&alias.alias);
    }
    for route in snap.routes.values().filter(|route| route.enabled != 0) {
        push(&route.name);
    }
    for model in snap.models.values().filter(|model| model.enabled != 0) {
        push(&model.upstream_id);
    }
    for model in snap.models.values().filter(|model| model.enabled != 0) {
        let Some(provider) = snap.providers.get(&model.provider_id) else {
            continue;
        };
        let mut provider_names = [provider.name.as_str(), provider.id.as_str()];
        provider_names.sort_unstable();
        for provider_name in provider_names {
            let name = format!("{provider_name}/{}", model.upstream_id);
            if crate::db::VirtualKeyRow::model_is_allowed(key_allowed, &name) {
                push(&name);
            }
        }
    }

    entries.retain(|entry| {
        crate::db::VirtualKeyRow::model_is_allowed(key_allowed, &entry.name)
            && provider_policy_allows(snap, &entry.name, key_allowed_providers)
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

/// Effective runtime semantics of every eligible target `name` resolves to
/// for a key: the resolved model, or each permitted, resolvable Route target.
fn runtime_targets(
    snapshot: &crate::registry::Snapshot,
    name: &str,
    allowed_providers: &[String],
) -> Vec<RuntimeModel> {
    use crate::registry::Resolved;

    match Registry::resolve_in(snapshot, name) {
        Some(Resolved::Single {
            provider_id,
            model_id,
        }) => {
            let (Some(provider), Some(model)) = (
                snapshot.providers.get(&provider_id),
                snapshot.models.get(&model_id),
            ) else {
                return Vec::new();
            };
            RuntimeModel::resolve(provider, model).into_iter().collect()
        }
        Some(Resolved::Route { targets, .. }) => {
            let mut seen = std::collections::HashSet::new();
            targets
                .iter()
                .filter(|target| {
                    provider_ids_allowed(
                        std::iter::once(target.provider.id.as_str()),
                        allowed_providers,
                    )
                })
                .filter(|target| seen.insert(target.model.id.clone()))
                .filter_map(|target| RuntimeModel::resolve(&target.provider, &target.model).ok())
                .collect()
        }
        None => Vec::new(),
    }
}

fn client_model_metadata(
    snapshot: &crate::registry::Snapshot,
    name: &str,
    allowed_providers: &[String],
) -> crate::client_profiles::ClientModelMetadata {
    let targets = runtime_targets(snapshot, name, allowed_providers);
    let Some(listing) = Listing::intersect(&targets) else {
        return crate::client_profiles::ClientModelMetadata::default();
    };
    // A reasoning flag is shared only when every target agrees.
    let reasoning = targets
        .iter()
        .map(|target| target.capabilities.reasoning)
        .collect::<Option<Vec<_>>>()
        .and_then(|values| {
            let first = *values.first()?;
            values.iter().all(|value| *value == first).then_some(first)
        });
    let mut input = Vec::new();
    if listing.capabilities.text == Some(true) {
        input.push("text".to_string());
    }
    if listing.capabilities.vision == Some(true) {
        input.push("image".to_string());
    }
    crate::client_profiles::ClientModelMetadata {
        reasoning,
        input: (!input.is_empty()).then_some(input),
        context_window: listing.context_window,
        max_output_tokens: listing.max_output_tokens,
    }
}

/// Client-visible accepted semantics of a listed name (#198). For a Route it
/// is the conservative intersection of its eligible targets: a capability is
/// `true` only when every target supports it, `false` when any target lacks
/// it, and omitted while unknown. Raw observations are never listed.
struct Listing {
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    capabilities: ModelCapabilityFlags,
    thinking: Option<ThinkingSupport>,
    transport: Option<&'static str>,
    provenance: BTreeMap<&'static str, &'static str>,
}

impl Listing {
    fn intersect(targets: &[RuntimeModel]) -> Option<Self> {
        let (first, rest) = targets.split_first()?;
        let limit = |field: fn(&RuntimeModel) -> Option<i64>| {
            targets
                .iter()
                .map(field)
                .collect::<Option<Vec<_>>>()
                .and_then(|limits| limits.into_iter().min())
        };
        let flag = |field: fn(&ModelCapabilityFlags) -> Option<bool>| {
            let values: Vec<_> = targets
                .iter()
                .map(|target| field(&target.capabilities))
                .collect();
            if values.contains(&Some(false)) {
                Some(false)
            } else if values.iter().all(|value| *value == Some(true)) {
                Some(true)
            } else {
                None
            }
        };
        let transport = public_transport(&first.transport);
        let transport = rest
            .iter()
            .all(|target| public_transport(&target.transport) == transport)
            .then_some(transport);
        let provenance = first
            .provenance
            .iter()
            .map(|(field, source)| (*field, public_source(source)))
            .filter(|(field, source)| {
                rest.iter().all(|target| {
                    target
                        .provenance
                        .get(field)
                        .map(|other| public_source(other))
                        == Some(*source)
                })
            })
            .collect();
        Some(Self {
            context_window: limit(|target| target.context_window),
            max_output_tokens: limit(|target| target.max_output_tokens),
            capabilities: ModelCapabilityFlags {
                text: flag(|caps| caps.text),
                reasoning: flag(|caps| caps.reasoning),
                vision: flag(|caps| caps.vision),
                tool_calling: flag(|caps| caps.tool_calling),
                parallel_tools: flag(|caps| caps.parallel_tools),
                structured_output: flag(|caps| caps.structured_output),
            },
            thinking: shared_thinking(targets),
            transport,
            provenance,
        })
    }

    /// Add the listing to an OpenAI model object. Unknown values are omitted.
    fn write(&self, obj: &mut Value) {
        if let Some(context_window) = self.context_window {
            obj["context_window"] = json!(context_window);
        }
        if let Some(max_output_tokens) = self.max_output_tokens {
            obj["max_output_tokens"] = json!(max_output_tokens);
        }
        // Every listed model streams: the gateway always serves streaming
        // requests, synthesizing a stream for non-streaming upstreams.
        let mut capabilities = serde_json::Map::from_iter([("streaming".into(), json!(true))]);
        for (key, value) in [
            ("tools", self.capabilities.tool_calling),
            ("parallel_tools", self.capabilities.parallel_tools),
            ("images", self.capabilities.vision),
            ("structured_output", self.capabilities.structured_output),
        ] {
            if let Some(value) = value {
                capabilities.insert(key.into(), json!(value));
            }
        }
        if let Some(thinking) = &self.thinking {
            capabilities.insert("thinking".into(), json!(thinking));
        }
        obj["capabilities"] = Value::Object(capabilities);

        let mut kinetix = json!({ "state": "accepted" });
        if let Some(transport) = self.transport {
            kinetix["transport"] = json!(transport);
        }
        if !self.provenance.is_empty() {
            kinetix["provenance"] = json!(self.provenance);
        }
        obj["kinetix"] = kinetix;
    }
}

/// Thinking modes and levels every target supports; unknown when any target's
/// support is unknown or no mode is shared.
fn shared_thinking(targets: &[RuntimeModel]) -> Option<ThinkingSupport> {
    let (first, rest) = targets.split_first()?;
    let mut shared = first.thinking.clone()?;
    for target in rest {
        let other = target.thinking.as_ref()?;
        shared.modes.retain(|mode| other.modes.contains(mode));
        shared.levels.retain(|level| other.levels.contains(level));
    }
    // Effort levels only apply to a shared levelled mode.
    if shared
        .modes
        .iter()
        .all(|mode| mode == "off" || mode == "on")
    {
        shared.levels.clear();
    }
    (!shared.modes.is_empty()).then_some(shared)
}

/// Public transport family. Plugin references stay private.
fn public_transport(transport: &TargetTransport) -> &'static str {
    match transport {
        TargetTransport::OpenAiChat => "openai",
        TargetTransport::OpenAiResponses => "openai-responses",
        TargetTransport::Anthropic => "anthropic",
        TargetTransport::Gemini => "gemini",
        TargetTransport::Plugin(_) => "plugin",
    }
}

/// Coarse public source category. Internal source details stay private.
fn public_source(source: &str) -> &'static str {
    if source == OPERATOR_SOURCE {
        "operator"
    } else if source.starts_with("models.dev") {
        "models.dev"
    } else if source.contains("plugin") {
        "plugin"
    } else if source.contains("probe") {
        "probe"
    } else {
        "provider"
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
                ErrorKind::ClientCancelled => "server_error",
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
                ErrorKind::ClientCancelled => "api_error",
            };
            json!({
                "type": "error",
                "error": { "type": etype, "message": err.message }
            })
        }
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
                max_concurrent_requests: None,
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
        let expected = vec![
            "mixed-route".to_string(),
            "permitted-model".to_string(),
            "permitted-route".to_string(),
            "permitted/permitted-model".to_string(),
            format!("{permitted_provider}/permitted-model"),
        ];
        assert_eq!(ids, expected);

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

    async fn listing_registry(models: &[(&str, Value, Value)]) -> (Registry, String, Vec<String>) {
        let root = std::env::temp_dir().join(format!(
            "kinetix-model-listing-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let database_url = format!("sqlite://{}", root.join("kinetix.db").display());
        let pool = crate::db::connect(&database_url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let provider_id = insert_test_provider(&pool, "listing").await;
        let account_id = crate::db::insert_account(
            &pool,
            &provider_id,
            "listing-account",
            "encrypted-test-secret",
            "masked",
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        let mut model_ids = Vec::new();
        for (name, capabilities, discovery) in models {
            model_ids.push(
                crate::db::insert_model(
                    &pool,
                    &crate::db::NewModel {
                        provider_id: &provider_id,
                        upstream_id: name,
                        display_name: name,
                        enabled: true,
                        context_window: discovery["context_window"].as_i64(),
                        max_output_tokens: Some(8_192),
                        capabilities: capabilities.clone(),
                        prices: json!({}),
                        parameters: json!({}),
                        thinking_map: json!({}),
                        extra_request: json!({}),
                        discovery: discovery.clone(),
                    },
                )
                .await
                .unwrap(),
            );
        }
        let targets: Vec<(&str, &str)> = model_ids
            .iter()
            .map(|model_id| (account_id.as_str(), model_id.as_str()))
            .collect();
        insert_test_route(&pool, "listing-route", &targets).await;
        let registry = Registry::new();
        registry.reload(&pool).await.unwrap();
        (registry, provider_id, model_ids)
    }

    fn listed<'a>(body: &'a Value, id: &str) -> &'a Value {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == id)
            .unwrap()
    }

    fn adaptive_discovery(context_window: i64) -> Value {
        json!({
            "imported_from_discovery": true,
            "context_window": context_window,
            "reasoning_capability": {
                "mode": "adaptive",
                "levels": ["off", "low", "medium", "high"],
                "can_disable": true
            },
            "capability_sources": {
                "context_window": "models.dev:canonical",
                "reasoning": "models.dev:canonical"
            },
            "operator_capability_overrides": {},
            "latest_observation": {
                "context_window": 1_000_000,
                "capabilities": {"tool_calling": false, "vision": false}
            }
        })
    }

    #[tokio::test]
    async fn model_listing_exposes_accepted_capabilities_and_provenance() {
        let (registry, _, _) = listing_registry(&[(
            "adaptive-model",
            json!({"text": true, "reasoning": true, "tool_calling": true, "vision": true, "structured_output": false}),
            adaptive_discovery(200_000),
        )])
        .await;
        let body = models_body(FrontendFormat::OpenAi, &registry, &["*".into()], &[]);
        assert_eq!(body["object"], "list");
        let model = listed(&body, "adaptive-model").clone();
        assert_eq!(model["object"], "model");
        assert_eq!(model["owned_by"], "kinetix");
        assert_eq!(model["context_window"], 200_000);
        assert_eq!(model["max_output_tokens"], 8_192);
        // Accepted values only: the newer observation (tools=false, 1M) is
        // not listed until accepted.
        assert_eq!(
            model["capabilities"],
            json!({
                "tools": true,
                "images": true,
                "streaming": true,
                "structured_output": false,
                "thinking": {"modes": ["off", "adaptive"], "levels": ["low", "medium", "high"]}
            })
        );
        assert_eq!(
            model["kinetix"],
            json!({
                "state": "accepted",
                "transport": "openai",
                "provenance": {
                    "context_window": "models.dev",
                    "thinking": "models.dev",
                    "transport": "provider"
                }
            })
        );
    }

    #[tokio::test]
    async fn route_listing_reports_the_conservative_target_intersection() {
        let (registry, _, _) = listing_registry(&[
            (
                "tools-model",
                json!({"text": true, "reasoning": true, "tool_calling": true, "vision": true}),
                adaptive_discovery(200_000),
            ),
            (
                "no-tools-model",
                json!({"text": true, "reasoning": true, "tool_calling": false}),
                json!({
                    "context_window": 100_000,
                    "reasoning_capability": {"mode": "level", "levels": ["low", "high"], "can_disable": true},
                    "operator_capability_overrides": {}
                }),
            ),
        ])
        .await;
        let body = models_body(FrontendFormat::OpenAi, &registry, &["*".into()], &[]);
        let route = listed(&body, "listing-route");
        assert_eq!(route["context_window"], 100_000);
        // Only some targets support tools: never `tools: true`. Vision is
        // unknown on one target, so it is omitted.
        assert_eq!(
            route["capabilities"],
            json!({
                "tools": false,
                "streaming": true,
                "thinking": {"modes": ["off"]}
            })
        );
        assert_eq!(
            route["kinetix"],
            json!({
                "state": "accepted",
                "transport": "openai",
                "provenance": {"transport": "provider"}
            })
        );
        assert_eq!(listed(&body, "tools-model")["capabilities"]["tools"], true);
    }

    #[tokio::test]
    async fn route_listing_only_intersects_targets_the_key_can_reach() {
        let (registry, provider_id, _) = listing_registry(&[(
            "tools-model",
            json!({"tool_calling": true}),
            json!({"context_window": 64_000}),
        )])
        .await;
        let body = models_body(
            FrontendFormat::OpenAi,
            &registry,
            &["*".into()],
            std::slice::from_ref(&provider_id),
        );
        assert_eq!(
            listed(&body, "listing-route")["capabilities"]["tools"],
            true
        );
        let anthropic = models_body(FrontendFormat::Anthropic, &registry, &["*".into()], &[]);
        let model = &anthropic["data"][0];
        assert!(model.get("capabilities").is_none() && model.get("kinetix").is_none());
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
