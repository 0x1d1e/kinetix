//! Observed, accepted, and runtime model state (#197).
//!
//! ```text
//! ObservedModel  --reconciliation-->  AcceptedModel  --registry snapshot-->  RuntimeModel
//! ```
//!
//! - **Observed**: untrusted external facts from provider `/models`, plugin
//!   model sources, models.dev, and probes. Discovery stores the newest one
//!   under the model's `discovery.latest_observation` and appends each to the
//!   `model_observations` history. Observations never change accepted state.
//! - **Accepted**: operator- or policy-approved semantics: the model columns
//!   plus the accepted discovery keys (import snapshot, `configured_transport`,
//!   `operator_*_overrides`, `effective_pricing`, `accepted_provenance`, ...).
//!   Only explicit writes change it: model create/update, reconciliation
//!   `accept`/`pin`, and pricing sync.
//! - **Runtime**: the effective execution semantics a request resolves from
//!   the immutable registry snapshot it started with
//!   ([`crate::adapters::resolve_execution_profile`]).
//!
//! Main invariant: a catalog refresh is not a runtime semantic change. The two
//! documented, policy-allowed exceptions are host-owned plugin `opaque_state`
//! (protocol metadata kept live so continuations stay correct) and fresh,
//! scope-matched probe evidence, which may refine fields the operator does not
//! own. See `docs/model-state.md`.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::adapters::TargetTransport;
use crate::db::{ModelRow, ProviderRow};
use crate::model_capabilities::{ModelCapabilityFlags, ReasoningCapabilityMode};
use crate::types::ProxyError;

/// Discovery keys holding observations or reconciliation bookkeeping rather
/// than accepted semantics.
pub const OBSERVATION_KEYS: &[&str] = &[
    "latest_observation",
    "last_seen",
    "reconciliation",
    "disappeared",
    "flagged_at",
    "probe_evidence",
    "opaque_state",
];

/// Discovery key recording where each accepted scalar limit came from.
pub const ACCEPTED_PROVENANCE_KEY: &str = "accepted_provenance";

/// Accepted scalar fields whose provenance is recorded explicitly.
pub const PROVENANCE_FIELDS: &[&str] = &["context_window", "max_output_tokens"];

/// Provenance value for operator-entered fields.
pub const OPERATOR_SOURCE: &str = "operator";

/// The accepted semantic state of a model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AcceptedModel {
    pub display_name: String,
    pub enabled: bool,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities: Value,
    pub prices: Value,
    pub parameters: Value,
    pub thinking_map: Value,
    pub extra_request: Value,
    /// Accepted discovery keys, without [`OBSERVATION_KEYS`].
    pub discovery: Map<String, Value>,
}

impl AcceptedModel {
    pub fn from_row(row: &ModelRow) -> Self {
        let parse = |raw: &str| serde_json::from_str::<Value>(raw).unwrap_or(Value::Null);
        let mut discovery = match parse(&row.discovery) {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        for key in OBSERVATION_KEYS {
            discovery.remove(*key);
        }
        Self {
            display_name: row.display_name.clone(),
            enabled: row.enabled != 0,
            context_window: row.context_window,
            max_output_tokens: row.max_output_tokens,
            capabilities: parse(&row.capabilities),
            prices: parse(&row.prices),
            parameters: parse(&row.parameters),
            thinking_map: parse(&row.thinking_map),
            extra_request: parse(&row.extra_request),
            discovery,
        }
    }
}

/// The newest observation for a model, if discovery has seen it.
pub fn latest_observation(row: &ModelRow) -> Option<Value> {
    serde_json::from_str::<Value>(&row.discovery)
        .ok()?
        .get("latest_observation")
        .filter(|value| value.is_object())
        .cloned()
}

/// Accepted thinking support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThinkingSupport {
    /// `off`, `on`, `level`, `budget`, or `adaptive`.
    pub modes: Vec<String>,
    /// Canonical effort levels, excluding `off`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<String>,
}

/// Effective runtime semantics of one model on one provider.
#[derive(Debug, Clone)]
pub struct RuntimeModel {
    pub transport: TargetTransport,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities: ModelCapabilityFlags,
    /// `None` when thinking support is unknown.
    pub thinking: Option<ThinkingSupport>,
    /// Field -> source of the accepted value. Unknown sources are omitted.
    pub provenance: BTreeMap<&'static str, String>,
}

impl RuntimeModel {
    pub fn resolve(provider: &ProviderRow, model: &ModelRow) -> Result<Self, ProxyError> {
        let profile = crate::adapters::resolve_execution_profile(provider, model)?;
        let discovery = serde_json::from_str::<Value>(&model.discovery).unwrap_or(Value::Null);

        let thinking = match &profile.reasoning {
            Some(reasoning) => {
                let mut modes = Vec::new();
                if reasoning.can_disable {
                    modes.push("off".to_string());
                }
                modes.push(
                    match reasoning.mode {
                        Some(ReasoningCapabilityMode::Toggle) | None => "on",
                        Some(ReasoningCapabilityMode::Level) => "level",
                        Some(ReasoningCapabilityMode::ManualBudget) => "budget",
                        Some(ReasoningCapabilityMode::Adaptive) => "adaptive",
                    }
                    .to_string(),
                );
                Some(ThinkingSupport {
                    modes,
                    levels: reasoning
                        .levels
                        .iter()
                        .filter(|level| level.as_str() != "off")
                        .cloned()
                        .collect(),
                })
            }
            None if profile.capabilities.reasoning == Some(false) => Some(ThinkingSupport {
                modes: vec!["off".to_string()],
                levels: Vec::new(),
            }),
            None => None,
        };

        let mut provenance = BTreeMap::new();
        provenance.insert(
            "transport",
            transport_provenance(provider, &discovery).to_string(),
        );
        for (field, value) in [
            ("context_window", model.context_window),
            ("max_output_tokens", model.max_output_tokens),
        ] {
            if let Some(source) = value.and_then(|value| limit_provenance(&discovery, field, value))
            {
                provenance.insert(field, source);
            }
        }
        if thinking.is_some() {
            if let Some(source) = thinking_provenance(model, &discovery) {
                provenance.insert("thinking", source);
            }
        }

        Ok(Self {
            transport: profile.transport,
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
            capabilities: profile.capabilities,
            thinking,
            provenance,
        })
    }
}

fn non_null<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|value| !value.is_null())
}

fn transport_provenance<'a>(provider: &ProviderRow, discovery: &'a Value) -> &'a str {
    if provider.wire_plugin_ref().is_some() {
        "plugin"
    } else if non_null(discovery, "configured_transport").is_some() {
        OPERATOR_SOURCE
    } else if non_null(discovery, "transport").is_some()
        || discovery.pointer("/raw_metadata/transport").is_some()
    {
        non_null(discovery, "transport_source")
            .and_then(Value::as_str)
            .unwrap_or("discovery")
    } else {
        "provider"
    }
}

/// Recorded provenance, else the import snapshot's source when the accepted
/// value still equals the imported value.
fn limit_provenance(discovery: &Value, field: &str, value: i64) -> Option<String> {
    if let Some(source) = discovery
        .get(ACCEPTED_PROVENANCE_KEY)
        .and_then(|provenance| provenance.get(field))
        .and_then(Value::as_str)
    {
        return Some(source.to_string());
    }
    (discovery.get(field).and_then(Value::as_i64) == Some(value))
        .then(|| discovery.pointer(&format!("/capability_sources/{field}")))
        .flatten()
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn thinking_provenance(model: &ModelRow, discovery: &Value) -> Option<String> {
    let owned = |key: &str, field: &str| {
        discovery
            .get(key)
            .and_then(Value::as_object)
            .is_some_and(|overrides| overrides.contains_key(field))
    };
    let admin_thinking = model.thinking();
    let legacy_owned = discovery.get("operator_thinking_overrides").is_none()
        && (!admin_thinking.levels.is_empty()
            || admin_thinking.mode.is_some()
            || admin_thinking.budget_field.is_some()
            || admin_thinking.level_field.is_some());
    if owned("operator_reasoning_overrides", "reasoning_capability")
        || owned("operator_thinking_overrides", "thinking_map")
        || legacy_owned
    {
        return Some(OPERATOR_SOURCE.to_string());
    }
    discovery
        .pointer("/capability_sources/reasoning")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Discovery patch recording `source` as the provenance of `fields`, merged
/// over the provenance already recorded on `discovery`.
pub fn accepted_provenance_patch(discovery: &Value, fields: &[(&str, &str)]) -> Option<Value> {
    if fields.is_empty() {
        return None;
    }
    let mut provenance = discovery
        .get(ACCEPTED_PROVENANCE_KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (field, source) in fields {
        provenance.insert((*field).to_string(), Value::String((*source).to_string()));
    }
    Some(Value::Object(provenance))
}

#[cfg(test)]
mod tests {
    //! Lifecycle matrix for #197: catalog refreshes update observations only;
    //! accepted state and runtime semantics change only on acceptance.

    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path, State};
    use serde_json::json;

    use crate::admin::{self, ReconciliationActionBody};
    use crate::admin_contract::Json;
    use crate::app::AppState;
    use crate::auth::AdminAuth;
    use crate::db;
    use crate::registry::{Registry, Resolved};
    use crate::types::{AuthScheme, WireFormat};

    const MODEL: &str = "lifecycle-model";

    struct Harness {
        state: AppState,
        catalog: Arc<Mutex<Value>>,
        base_url: String,
        model_id: String,
    }

    fn admin() -> AdminAuth {
        AdminAuth {
            actor: "admin".into(),
            token: "test".into(),
        }
    }

    /// Accepted state: tools=true, thinking=adaptive (can disable),
    /// context=200k, imported from a models.dev-sourced observation.
    fn accepted_discovery() -> Value {
        json!({
            "imported_from_discovery": true,
            "context_window": 200000,
            "max_output_tokens": 64000,
            "capabilities": {"text": true, "reasoning": true, "tool_calling": true},
            "reasoning_capability": {
                "mode": "adaptive",
                "levels": ["off", "low", "medium", "high"],
                "can_disable": true
            },
            "capability_sources": {
                "context_window": "models.dev:provider",
                "max_output_tokens": "models.dev:provider",
                "reasoning": "models.dev:provider",
                "tool_calling": "models.dev:provider"
            },
            "operator_capability_overrides": {},
            "operator_thinking_overrides": {}
        })
    }

    async fn harness(wire_format: WireFormat, upstream: Value) -> Harness {
        let catalog = Arc::new(Mutex::new(upstream));
        let served = catalog.clone();
        let app = axum::Router::new().route(
            "/models",
            axum::routing::get(move || {
                let served = served.clone();
                async move { axum::Json(served.lock().unwrap().clone()) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let home = std::env::temp_dir().join(format!(
            "kinetix-model-state-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let database_url = format!("sqlite://{}", home.join("kinetix.db").display());
        let config = Arc::new(
            crate::config::Config::build(crate::config::CliOverrides {
                home: Some(home),
                database_url: Some(database_url.clone()),
                master_key: Some(hex::encode([7u8; 32])),
                admin_token: Some("test-admin-password".into()),
                allow_private_upstreams: Some(true),
                allow_insecure_tls: Some(true),
                ..Default::default()
            })
            .unwrap(),
        );
        let pool = db::connect(&database_url).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let crypto = Arc::new(crate::crypto::Crypto::new(&config.master_key));
        let provider_id = db::insert_provider(
            &pool,
            &db::NewProvider {
                name: "lifecycle",
                base_url: &base_url,
                wire_format,
                auth_scheme: AuthScheme::Bearer,
                custom_header_name: None,
                custom_param_name: None,
                extra_headers: json!({}),
                timeout_ms: 2000,
                capability_mode: "permissive",
                models_path: Some("/models"),
                rate_limit_rules: json!({}),
                follow_redirects: false,
                credential_hosts: "",
                allow_insecure_tls: true,
                wire_plugin: "",
                credential_plugin: "",
                model_source_plugin: "",
                credential_mode: "manual",
                source_plugin_id: None,
                source_integration_id: None,
            },
        )
        .await
        .unwrap();
        db::insert_account(
            &pool,
            &provider_id,
            "default",
            &crypto.encrypt("test-api-key").unwrap(),
            "test:****",
            1,
            1,
            None,
            "none",
        )
        .await
        .unwrap();
        let model_id = db::insert_model(
            &pool,
            &db::NewModel {
                provider_id: &provider_id,
                upstream_id: MODEL,
                display_name: "Lifecycle Model",
                enabled: true,
                context_window: Some(200000),
                max_output_tokens: Some(64000),
                capabilities: json!({"text": true, "reasoning": true, "tool_calling": true}),
                prices: json!({"input_per_1m": 3.0, "output_per_1m": 15.0}),
                parameters: json!({}),
                thinking_map: json!({}),
                extra_request: json!({}),
                discovery: accepted_discovery(),
            },
        )
        .await
        .unwrap();
        let registry = Arc::new(Registry::new());
        registry.reload(&pool).await.unwrap();
        let state = AppState::new(
            config,
            pool.clone(),
            registry,
            crypto,
            reqwest::Client::new(),
            crate::logqueue::UsageLogQueue::new(pool, 16),
            0,
        );
        Harness {
            state,
            catalog,
            base_url,
            model_id,
        }
    }

    impl Harness {
        fn serve(&self, upstream: Value) {
            *self.catalog.lock().unwrap() = upstream;
        }

        async fn refresh(&self) -> Value {
            let provider_id = self.row().await.provider_id;
            admin::discover_models(State(self.state.clone()), admin(), Path(provider_id))
                .await
                .unwrap()
                .0
        }

        async fn row(&self) -> ModelRow {
            db::get_model(&self.state.pool, &self.model_id)
                .await
                .unwrap()
                .unwrap()
        }

        async fn accepted(&self) -> AcceptedModel {
            AcceptedModel::from_row(&self.row().await)
        }

        async fn discovery(&self) -> Value {
            serde_json::from_str(&self.row().await.discovery).unwrap()
        }

        /// Runtime view from a freshly reloaded registry snapshot, so a
        /// refresh cannot hide behind a stale snapshot.
        async fn runtime(&self) -> RuntimeSnapshot {
            self.state.registry.reload(&self.state.pool).await.unwrap();
            let snapshot = self.state.registry.snapshot();
            let model = snapshot.models.get(&self.model_id).unwrap().clone();
            let provider = snapshot.providers.get(&model.provider_id).unwrap().clone();
            let eligible = matches!(
                Registry::resolve_in(&snapshot, MODEL),
                Some(Resolved::Single { ref model_id, .. }) if *model_id == self.model_id
            );
            RuntimeSnapshot::new(RuntimeModel::resolve(&provider, &model).unwrap(), eligible)
        }

        async fn accept(&self, fields: &[&str]) {
            admin::update_model_reconciliation(
                State(self.state.clone()),
                admin(),
                Path(self.model_id.clone()),
                Json(ReconciliationActionBody {
                    action: "accept".into(),
                    fields: fields.iter().map(|field| field.to_string()).collect(),
                }),
            )
            .await
            .unwrap();
        }
    }

    /// Comparable projection of [`RuntimeModel`].
    #[derive(Debug, PartialEq)]
    struct RuntimeSnapshot {
        eligible: bool,
        transport: String,
        context_window: Option<i64>,
        capabilities: ModelCapabilityFlags,
        thinking: Option<ThinkingSupport>,
        provenance: BTreeMap<&'static str, String>,
    }

    impl RuntimeSnapshot {
        fn new(runtime: RuntimeModel, eligible: bool) -> Self {
            Self {
                eligible,
                transport: runtime.transport.as_str().to_string(),
                context_window: runtime.context_window,
                capabilities: runtime.capabilities,
                thinking: runtime.thinking,
                provenance: runtime.provenance,
            }
        }
    }

    fn openai_list(models: Value) -> Value {
        json!({ "object": "list", "data": models })
    }

    fn diff_fields(discovery: &Value) -> Vec<String> {
        discovery
            .pointer("/reconciliation/diff")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("field").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    }

    #[tokio::test]
    async fn catalog_refresh_does_not_change_runtime_until_accepted() {
        let h = harness(
            WireFormat::Openai,
            openai_list(json!([{
                "id": MODEL,
                "context_window": 200000,
                "capabilities": {"tool_calling": true},
                "reasoning": {"mode": "adaptive", "levels": ["off", "low", "medium", "high"]}
            }])),
        )
        .await;
        let accepted = h.accepted().await;
        let runtime = h.runtime().await;
        assert!(runtime.eligible);
        assert_eq!(runtime.context_window, Some(200000));
        assert_eq!(runtime.capabilities.tool_calling, Some(true));
        assert_eq!(
            runtime.thinking,
            Some(ThinkingSupport {
                modes: vec!["off".into(), "adaptive".into()],
                levels: vec!["low".into(), "medium".into(), "high".into()],
            })
        );
        assert_eq!(
            runtime.provenance.get("context_window").map(String::as_str),
            Some("models.dev:provider")
        );
        assert_eq!(
            runtime.provenance.get("thinking").map(String::as_str),
            Some("models.dev:provider")
        );

        // Upstream now claims tools=false and 128k, and omits thinking metadata.
        h.serve(openai_list(json!([{
            "id": MODEL,
            "context_window": 128000,
            "capabilities": {"tool_calling": false}
        }])));
        h.refresh().await;

        let discovery = h.discovery().await;
        let observed = &discovery["latest_observation"];
        assert_eq!(observed["context_window"], 128000);
        assert_eq!(observed["capabilities"]["tool_calling"], false);
        // Omitted upstream metadata is unknown, not a retraction: the prior
        // thinking evidence is carried forward.
        assert_eq!(observed["reasoning_capability"]["mode"], "adaptive");
        let diff = diff_fields(&discovery);
        assert!(diff.contains(&"context_window".to_string()), "{diff:?}");
        assert!(
            diff.contains(&"capabilities.tool_calling".to_string()),
            "{diff:?}"
        );
        assert_eq!(h.accepted().await, accepted);
        assert_eq!(h.runtime().await, runtime);

        // Accepting the reviewed fields is what changes runtime semantics.
        h.accept(&["context_window", "capabilities.tool_calling"])
            .await;
        let after = h.runtime().await;
        assert!(after.eligible);
        assert_eq!(after.context_window, Some(128000));
        assert_eq!(after.capabilities.tool_calling, Some(false));
        assert_eq!(after.thinking, runtime.thinking);
        assert_eq!(
            after.provenance.get("context_window").map(String::as_str),
            Some("upstream_discovery")
        );
        assert_ne!(h.accepted().await, accepted);
    }

    #[tokio::test]
    async fn removal_and_reappearance_keep_runtime_eligibility() {
        let listed = openai_list(json!([{ "id": MODEL, "context_window": 200000 }]));
        let h = harness(WireFormat::Openai, listed.clone()).await;
        let accepted = h.accepted().await;
        let runtime = h.runtime().await;

        h.serve(openai_list(json!([])));
        let result = h.refresh().await;
        assert_eq!(result["disappeared"][0]["upstream_id"], MODEL);
        let discovery = h.discovery().await;
        assert_eq!(discovery["disappeared"], true);
        assert!(discovery.get("flagged_at").is_some());
        assert_eq!(h.accepted().await, accepted);
        assert_eq!(h.runtime().await, runtime);
        assert!(runtime.eligible);

        h.serve(listed);
        h.refresh().await;
        let discovery = h.discovery().await;
        assert_eq!(discovery["disappeared"], false);
        assert!(discovery.get("flagged_at").is_none());
        assert_eq!(h.accepted().await, accepted);
        assert_eq!(h.runtime().await, runtime);
    }

    #[tokio::test]
    async fn pricing_transport_and_unknown_capability_changes_stay_observational() {
        let h = harness(
            WireFormat::Openai,
            openai_list(json!([{ "id": MODEL, "context_window": 200000 }])),
        )
        .await;
        h.refresh().await;
        let accepted = h.accepted().await;
        let runtime = h.runtime().await;
        let prices_before = h.row().await.prices;
        assert_eq!(runtime.transport, "openai");
        assert_eq!(
            runtime.provenance.get("transport").map(String::as_str),
            Some("provider")
        );

        h.serve(openai_list(json!([{
            "id": MODEL,
            "context_window": 200000,
            "prices": {"input_per_1m": 9.0, "output_per_1m": 45.0},
            "transport": {"format": "openai-responses"},
            "capabilities": {"tool_calling": null}
        }])));
        h.refresh().await;

        let discovery = h.discovery().await;
        let observed = &discovery["latest_observation"];
        assert_eq!(observed["prices"]["input_per_1m"], 9.0);
        assert_eq!(observed["transport"]["format"], "openai-responses");
        // A null capability is unknown and never erases prior evidence.
        assert_eq!(observed["capabilities"]["tool_calling"], true);
        let diff = diff_fields(&discovery);
        for field in ["prices.input_per_1m", "prices.output_per_1m", "transport"] {
            assert!(diff.contains(&field.to_string()), "{field}: {diff:?}");
        }
        assert_eq!(h.row().await.prices, prices_before);
        assert_eq!(h.accepted().await, accepted);
        assert_eq!(h.runtime().await, runtime);

        // Transport is never part of the default accept set; it needs an
        // explicit decision.
        h.accept(&[]).await;
        assert_eq!(h.runtime().await.transport, "openai");
        h.accept(&["transport"]).await;
        let after = h.runtime().await;
        assert_eq!(after.transport, "openai-responses");
        assert_eq!(
            after.provenance.get("transport").map(String::as_str),
            Some(OPERATOR_SOURCE)
        );
        assert_eq!(after.capabilities.tool_calling, Some(true));
    }

    #[tokio::test]
    async fn upstream_display_name_change_is_observational() {
        let h = harness(
            WireFormat::Anthropic,
            json!({"data": [{"id": MODEL, "display_name": "Lifecycle Model"}]}),
        )
        .await;
        h.refresh().await;
        let accepted = h.accepted().await;
        let runtime = h.runtime().await;

        h.serve(json!({"data": [{"id": MODEL, "display_name": "Lifecycle Model (Renamed)"}]}));
        h.refresh().await;
        let discovery = h.discovery().await;
        assert_eq!(
            discovery["latest_observation"]["display_name"],
            "Lifecycle Model (Renamed)"
        );
        assert!(diff_fields(&discovery).contains(&"display_name".to_string()));
        assert_eq!(h.accepted().await, accepted);
        assert_eq!(h.row().await.display_name, "Lifecycle Model");
        assert_eq!(h.runtime().await, runtime);

        h.accept(&["display_name"]).await;
        assert_eq!(h.row().await.display_name, "Lifecycle Model (Renamed)");
        assert_eq!(h.runtime().await, runtime);
    }

    #[tokio::test]
    async fn models_dev_conflict_with_provider_discovery_stays_observational() {
        let h = harness(
            WireFormat::Openai,
            openai_list(json!([{ "id": MODEL, "context_window": 128000 }])),
        )
        .await;
        crate::model_catalog::ModelsDevCatalog::register_for_test(
            &h.base_url,
            crate::model_catalog::ModelsDevCatalog::from_parts(
                json!({
                    "acme/lifecycle-model": {
                        "id": "acme/lifecycle-model",
                        "name": "Lifecycle Model",
                        "tool_call": false,
                        "reasoning": true,
                        "limit": {"context": 1000000, "output": 128000}
                    }
                }),
                json!({}),
            )
            .unwrap(),
        );
        let accepted = h.accepted().await;
        let runtime = h.runtime().await;

        h.refresh().await;
        let discovery = h.discovery().await;
        let observed = &discovery["latest_observation"];
        // The live provider listing wins the observed context window over the
        // conflicting catalog value.
        assert_eq!(observed["context_window"], 128000);
        assert_eq!(
            observed["capability_sources"]["context_window"],
            "upstream_discovery"
        );
        assert_eq!(
            observed["canonical_identity"]["canonical_model_id"],
            "acme/lifecycle-model"
        );
        // The catalog disagrees with accepted state on tools and output limit.
        assert_eq!(observed["capabilities"]["tool_calling"], false);
        assert_eq!(
            observed["capability_sources"]["tool_calling"],
            "models.dev:canonical"
        );
        assert_eq!(observed["max_output_tokens"], 128000);
        assert_eq!(h.accepted().await, accepted);
        assert_eq!(h.runtime().await, runtime);
    }
}
