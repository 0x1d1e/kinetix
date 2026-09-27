use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::types::Prices;

const MODELS_DEV_CATALOG_URL: &str = "https://models.dev/catalog.json?type=all";
const MAX_MODELS_DEV_BYTES: usize = 16 * 1024 * 1024;
const BUNDLED_CATALOG_JSON: &str = include_str!("../data/model_capabilities.json");

const MODELS_DEV_PROVIDER_BASES: &[(&str, &str)] = &[
    ("https://api.openai.com/v1", "openai"),
    ("https://api.anthropic.com/v1", "anthropic"),
    ("https://generativelanguage.googleapis.com/v1beta", "google"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSource {
    ModelsDev,
    BundledCatalog,
}

impl CatalogSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::ModelsDev => "models.dev",
            Self::BundledCatalog => "bundled_catalog",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogLayerKind {
    Canonical,
    Provider,
}

fn provenance(source: CatalogSource, kind: CatalogLayerKind) -> &'static str {
    match (source, kind) {
        (CatalogSource::ModelsDev, CatalogLayerKind::Canonical) => "models.dev:canonical",
        (CatalogSource::ModelsDev, CatalogLayerKind::Provider) => "models.dev:provider",
        (CatalogSource::BundledCatalog, _) => "bundled_catalog",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalMatchKind {
    ExplicitHint,
    ExactCanonicalId,
    ExactModelId,
    CaseInsensitiveModelId,
    ExplicitAlias,
}

impl CanonicalMatchKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::ExplicitHint => "explicit_hint",
            Self::ExactCanonicalId => "exact_canonical_id",
            Self::ExactModelId => "exact_model_id",
            Self::CaseInsensitiveModelId => "case_insensitive_model_id",
            Self::ExplicitAlias => "explicit_alias",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalIdentityStatus {
    Resolved,
    Ambiguous,
    Unresolved,
}

impl CanonicalIdentityStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Ambiguous => "ambiguous",
            Self::Unresolved => "unresolved",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalIdentity {
    pub status: CanonicalIdentityStatus,
    pub upstream_model_id: String,
    pub canonical_model_id: Option<String>,
    pub match_kind: Option<CanonicalMatchKind>,
    pub candidates: Vec<String>,
    pub source: Option<CatalogSource>,
}

impl CanonicalIdentity {
    fn unresolved(upstream_model_id: &str) -> Self {
        Self {
            status: CanonicalIdentityStatus::Unresolved,
            upstream_model_id: upstream_model_id.to_string(),
            canonical_model_id: None,
            match_kind: None,
            candidates: Vec::new(),
            source: None,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "status": self.status.label(),
            "upstream_model_id": self.upstream_model_id,
            "canonical_model_id": self.canonical_model_id,
            "match": self.match_kind.map(CanonicalMatchKind::label),
            "source": self.source.map(CatalogSource::label),
            "candidates": self.candidates,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalModelMatch {
    pub source: CatalogSource,
    pub canonical_id: String,
    pub context_window: Option<i64>,
    pub max_input_tokens: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities_json: Value,
    pub modalities: Option<Value>,
    pub model_type: Option<String>,
    pub metadata: Value,
    pub source_url: Option<String>,
}

impl CanonicalModelMatch {
    pub fn provenance(&self) -> &'static str {
        provenance(self.source, CatalogLayerKind::Canonical)
    }

    pub fn reference(&self) -> String {
        format!("{}:{}", self.provenance(), self.canonical_id)
    }
}

#[derive(Debug, Clone)]
pub struct ProviderModelMatch {
    pub source: CatalogSource,
    pub provider_id: String,
    pub host: String,
    pub model_id: String,
    pub context_window: Option<i64>,
    pub max_input_tokens: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities_json: Value,
    pub modalities: Option<Value>,
    pub prices: Prices,
    pub model_type: Option<String>,
    pub metadata: Value,
    pub source_url: Option<String>,
}

impl ProviderModelMatch {
    pub fn provenance(&self) -> &'static str {
        provenance(self.source, CatalogLayerKind::Provider)
    }

    pub fn reference(&self) -> String {
        format!(
            "{}:{}/{}",
            self.provenance(),
            self.provider_id,
            self.model_id
        )
    }
}

#[derive(Debug, Clone)]
pub struct CatalogResolution {
    pub identity: CanonicalIdentity,
    pub canonical: Option<CanonicalModelMatch>,
    pub provider: Option<ProviderModelMatch>,
    pub fallback_canonical: Option<CanonicalModelMatch>,
    pub fallback_provider: Option<ProviderModelMatch>,
    pub catalog_provenance: Option<Value>,
}

pub struct CatalogLayer<'a> {
    pub source: CatalogSource,
    pub kind: CatalogLayerKind,
    pub context_window: Option<i64>,
    pub max_input_tokens: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities_json: &'a Value,
    pub modalities: Option<&'a Value>,
    pub prices: Option<&'a Prices>,
    pub model_type: Option<&'a str>,
}

impl CatalogLayer<'_> {
    pub fn provenance(&self) -> &'static str {
        provenance(self.source, self.kind)
    }
}

impl CatalogResolution {
    pub fn unresolved(upstream_model_id: &str) -> Self {
        Self {
            identity: CanonicalIdentity::unresolved(upstream_model_id),
            canonical: None,
            provider: None,
            fallback_canonical: None,
            fallback_provider: None,
            catalog_provenance: None,
        }
    }

    /// Catalog layers from lowest to highest authority.
    ///
    /// Bundled data is only a verified fallback. models.dev canonical metadata
    /// beats bundled facts, while a models.dev current-provider offering beats
    /// the canonical default.
    pub fn layers(&self) -> Vec<CatalogLayer<'_>> {
        let mut layers = Vec::new();

        if let Some(canonical) = self.fallback_canonical.as_ref() {
            layers.push(CatalogLayer {
                source: canonical.source,
                kind: CatalogLayerKind::Canonical,
                context_window: canonical.context_window,
                max_input_tokens: canonical.max_input_tokens,
                max_output_tokens: canonical.max_output_tokens,
                capabilities_json: &canonical.capabilities_json,
                modalities: canonical.modalities.as_ref(),
                prices: None,
                model_type: canonical.model_type.as_deref(),
            });
        }
        if let Some(provider) = self.fallback_provider.as_ref() {
            layers.push(CatalogLayer {
                source: provider.source,
                kind: CatalogLayerKind::Provider,
                context_window: provider.context_window,
                max_input_tokens: provider.max_input_tokens,
                max_output_tokens: provider.max_output_tokens,
                capabilities_json: &provider.capabilities_json,
                modalities: provider.modalities.as_ref(),
                prices: Some(&provider.prices),
                model_type: provider.model_type.as_deref(),
            });
        }

        if let Some(canonical) = self
            .canonical
            .as_ref()
            .filter(|entry| entry.source == CatalogSource::BundledCatalog)
        {
            layers.push(CatalogLayer {
                source: canonical.source,
                kind: CatalogLayerKind::Canonical,
                context_window: canonical.context_window,
                max_input_tokens: canonical.max_input_tokens,
                max_output_tokens: canonical.max_output_tokens,
                capabilities_json: &canonical.capabilities_json,
                modalities: canonical.modalities.as_ref(),
                prices: None,
                model_type: canonical.model_type.as_deref(),
            });
        }
        if let Some(provider) = self
            .provider
            .as_ref()
            .filter(|entry| entry.source == CatalogSource::BundledCatalog)
        {
            layers.push(CatalogLayer {
                source: provider.source,
                kind: CatalogLayerKind::Provider,
                context_window: provider.context_window,
                max_input_tokens: provider.max_input_tokens,
                max_output_tokens: provider.max_output_tokens,
                capabilities_json: &provider.capabilities_json,
                modalities: provider.modalities.as_ref(),
                prices: Some(&provider.prices),
                model_type: provider.model_type.as_deref(),
            });
        }
        if let Some(canonical) = self
            .canonical
            .as_ref()
            .filter(|entry| entry.source == CatalogSource::ModelsDev)
        {
            layers.push(CatalogLayer {
                source: canonical.source,
                kind: CatalogLayerKind::Canonical,
                context_window: canonical.context_window,
                max_input_tokens: canonical.max_input_tokens,
                max_output_tokens: canonical.max_output_tokens,
                capabilities_json: &canonical.capabilities_json,
                modalities: canonical.modalities.as_ref(),
                prices: None,
                model_type: canonical.model_type.as_deref(),
            });
        }
        if let Some(provider) = self
            .provider
            .as_ref()
            .filter(|entry| entry.source == CatalogSource::ModelsDev)
        {
            layers.push(CatalogLayer {
                source: provider.source,
                kind: CatalogLayerKind::Provider,
                context_window: provider.context_window,
                max_input_tokens: provider.max_input_tokens,
                max_output_tokens: provider.max_output_tokens,
                capabilities_json: &provider.capabilities_json,
                modalities: provider.modalities.as_ref(),
                prices: Some(&provider.prices),
                model_type: provider.model_type.as_deref(),
            });
        }

        layers
    }

    pub fn catalog_json(&self) -> Value {
        fn canonical_json(entry: &CanonicalModelMatch) -> Value {
            json!({
                "source": entry.provenance(),
                "reference": entry.reference(),
                "canonical_model_id": entry.canonical_id,
                "url": entry.source_url,
                "model_type": entry.model_type,
                "max_input_tokens": entry.max_input_tokens,
                "metadata": entry.metadata,
            })
        }

        fn provider_json(entry: &ProviderModelMatch) -> Value {
            json!({
                "source": entry.provenance(),
                "reference": entry.reference(),
                "provider_id": entry.provider_id,
                "model_id": entry.model_id,
                "url": entry.source_url,
                "model_type": entry.model_type,
                "max_input_tokens": entry.max_input_tokens,
                "metadata": entry.metadata,
            })
        }

        json!({
            "canonical": self.canonical.as_ref().map(canonical_json),
            "provider": self.provider.as_ref().map(provider_json),
            "fallback": {
                "canonical": self.fallback_canonical.as_ref().map(canonical_json),
                "provider": self.fallback_provider.as_ref().map(provider_json),
            },
            "source_state": self.catalog_provenance,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct CanonicalModelIndex {
    exact_model_ids: HashMap<String, Vec<String>>,
    case_insensitive_model_ids: HashMap<String, Vec<String>>,
}

impl CanonicalModelIndex {
    fn from_models(models: &Map<String, Value>) -> Self {
        let mut out = Self::default();
        for canonical_id in models.keys() {
            let Some(model_id) = canonical_local_id(canonical_id) else {
                continue;
            };
            out.exact_model_ids
                .entry(model_id.to_string())
                .or_default()
                .push(canonical_id.clone());
            out.case_insensitive_model_ids
                .entry(model_id.to_ascii_lowercase())
                .or_default()
                .push(canonical_id.clone());
        }
        out
    }

    fn exact(&self, model_id: &str) -> Vec<String> {
        self.exact_model_ids
            .get(model_id)
            .cloned()
            .unwrap_or_default()
    }

    fn case_insensitive(&self, model_id: &str) -> Vec<String> {
        self.case_insensitive_model_ids
            .get(&model_id.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct ModelsDevCatalog {
    models: Value,
    providers: Value,
    canonical_index: CanonicalModelIndex,
    provenance: Option<Value>,
}

#[derive(Debug, Clone)]
struct ModelsDevCacheEntry {
    catalog: ModelsDevCatalog,
    etag: Option<String>,
    last_modified: Option<String>,
    fetched_at: Instant,
}

impl ModelsDevCacheEntry {
    fn mark_validated(mut self) -> Self {
        self.fetched_at = Instant::now();
        self.catalog = self.catalog.with_freshness("fresh");
        self
    }
}

const MODELS_DEV_FRESH_TTL: Duration = Duration::from_secs(15 * 60);
const MODELS_DEV_STALE_IF_ERROR_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

fn models_dev_cache() -> &'static tokio::sync::RwLock<Option<ModelsDevCacheEntry>> {
    static CACHE: OnceLock<tokio::sync::RwLock<Option<ModelsDevCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::RwLock::new(None))
}

impl ModelsDevCatalog {
    fn from_slice(bytes: &[u8]) -> Option<Self> {
        let root: Value = serde_json::from_slice(bytes).ok()?;
        Self::from_catalog_value(root)
    }

    fn from_catalog_value(root: Value) -> Option<Self> {
        let models = root.get("models")?.clone();
        let providers = root.get("providers")?.clone();
        let models_object = models.as_object()?;
        providers.as_object()?;
        Some(Self {
            canonical_index: CanonicalModelIndex::from_models(models_object),
            models,
            providers,
            provenance: None,
        })
    }

    #[cfg(test)]
    fn from_parts(models: Value, providers: Value) -> Option<Self> {
        Self::from_catalog_value(json!({
            "models": models,
            "providers": providers,
        }))
    }

    fn with_source_state(
        mut self,
        retrieved_at: String,
        freshness: &str,
        etag: Option<String>,
        last_modified: Option<String>,
    ) -> Self {
        self.provenance = Some(json!({
            "source": "models.dev",
            "retrieved_at": retrieved_at,
            "freshness": freshness,
            "etag": etag,
            "last_modified": last_modified,
        }));
        self
    }

    fn with_freshness(mut self, freshness: &str) -> Self {
        if let Some(state) = self.provenance.as_mut().and_then(Value::as_object_mut) {
            state.insert("freshness".into(), Value::String(freshness.to_string()));
        }
        self
    }

    pub async fn fetch(client: &reqwest::Client, _base_url: &str) -> Option<Self> {
        // The catalog destination is fixed and never derived from provider input.
        // Apply the existing SSRF/private-network guard to the actual outbound
        // destination rather than suppressing enrichment for private gateways.
        if should_skip_external_lookup(MODELS_DEV_CATALOG_URL) {
            return None;
        }

        let cached = models_dev_cache().read().await.clone();
        if let Some(entry) = cached.as_ref() {
            if entry.fetched_at.elapsed() <= MODELS_DEV_FRESH_TTL {
                return Some(entry.catalog.clone().with_freshness("fresh"));
            }
        }

        let mut request = client
            .get(MODELS_DEV_CATALOG_URL)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(Duration::from_secs(3));
        if let Some(entry) = cached.as_ref() {
            if let Some(etag) = entry.etag.as_deref() {
                request = request.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            if let Some(last_modified) = entry.last_modified.as_deref() {
                request = request.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
            }
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(_) => {
                return cached
                    .filter(|entry| entry.fetched_at.elapsed() <= MODELS_DEV_STALE_IF_ERROR_TTL)
                    .map(|entry| entry.catalog.with_freshness("stale"));
            }
        };

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            if let Some(entry) = cached {
                let entry = entry.mark_validated();
                let catalog = entry.catalog.clone();
                *models_dev_cache().write().await = Some(entry);
                return Some(catalog);
            }
            return None;
        }

        if !response.status().is_success() {
            return cached
                .filter(|entry| entry.fetched_at.elapsed() <= MODELS_DEV_STALE_IF_ERROR_TTL)
                .map(|entry| entry.catalog.with_freshness("stale"));
        }

        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let last_modified = response
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = match response.bytes().await {
            Ok(bytes) if bytes.len() <= MAX_MODELS_DEV_BYTES => bytes,
            _ => {
                return cached
                    .filter(|entry| entry.fetched_at.elapsed() <= MODELS_DEV_STALE_IF_ERROR_TTL)
                    .map(|entry| entry.catalog.with_freshness("stale"));
            }
        };
        let catalog = match Self::from_slice(&bytes) {
            Some(catalog) => catalog,
            None => {
                return cached
                    .filter(|entry| entry.fetched_at.elapsed() <= MODELS_DEV_STALE_IF_ERROR_TTL)
                    .map(|entry| entry.catalog.with_freshness("stale"));
            }
        };
        let catalog = catalog.with_source_state(
            chrono::Utc::now().to_rfc3339(),
            "fresh",
            etag.clone(),
            last_modified.clone(),
        );
        *models_dev_cache().write().await = Some(ModelsDevCacheEntry {
            catalog: catalog.clone(),
            etag,
            last_modified,
            fetched_at: Instant::now(),
        });
        Some(catalog)
    }

    fn provider_id_for_base(&self, base_url: &str) -> Option<String> {
        let normalized_base = normalize_base_url(base_url)?;
        let providers = self.providers.as_object()?;

        if let Some(provider_id) = MODELS_DEV_PROVIDER_BASES
            .iter()
            .find_map(|(base, provider_id)| {
                (normalized_base == *base).then_some((*provider_id).to_string())
            })
            .filter(|provider_id| providers.contains_key(provider_id))
        {
            return Some(provider_id);
        }

        providers.iter().find_map(|(provider_id, provider)| {
            let api = provider.get("api")?.as_str()?;
            (normalize_base_url(api).as_deref() == Some(normalized_base.as_str()))
                .then_some(provider_id.clone())
        })
    }

    fn contains_canonical(&self, canonical_id: &str) -> bool {
        self.models
            .as_object()
            .is_some_and(|models| models.contains_key(canonical_id))
    }

    fn canonical_match(&self, canonical_id: &str) -> Option<CanonicalModelMatch> {
        let model = self.models.as_object()?.get(canonical_id)?;
        models_dev_canonical_match(canonical_id, model)
    }

    fn provider_match(&self, base_url: &str, model_id: &str) -> Option<ProviderModelMatch> {
        let providers = self.providers.as_object()?;
        let provider_id = self.provider_id_for_base(base_url)?;
        let provider = providers.get(&provider_id)?;
        let model = provider.get("models")?.as_object()?.get(model_id)?;
        models_dev_provider_match(base_url, &provider_id, model_id, model)
    }
}

fn normalize_base_url(base_url: &str) -> Option<String> {
    let mut url = url::Url::parse(base_url).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    let trimmed = url.path().trim_end_matches('/').to_string();
    url.set_path(&trimmed);
    Some(url.to_string().trim_end_matches('/').to_string())
}

fn should_skip_external_lookup(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url) else {
        return true;
    };
    let Some(host) = url.host_str() else {
        return true;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if matches!(host.as_str(), "localhost" | "localhost.localdomain")
        || host.ends_with(".local")
        || host.ends_with(".test")
        || host.ends_with(".invalid")
    {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .ok()
        .is_some_and(|ip| match ip {
            std::net::IpAddr::V4(ip) => {
                ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
            }
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local()
            }
        })
}

fn canonical_local_id(canonical_id: &str) -> Option<&str> {
    let (_, local) = canonical_id.split_once('/')?;
    (!local.is_empty()).then_some(local)
}

fn valid_price(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn normalized_modalities(model: &Value) -> Option<Value> {
    fn direction(model: &Value, pointer: &str) -> Option<Vec<String>> {
        let values = model.pointer(pointer)?.as_array()?;
        let mut out = Vec::new();
        for value in values {
            let Some(value) = value.as_str() else {
                continue;
            };
            let normalized = value.trim().to_ascii_lowercase();
            if matches!(
                normalized.as_str(),
                "text" | "image" | "audio" | "video" | "pdf"
            ) && !out.iter().any(|existing| existing == &normalized)
            {
                out.push(normalized);
            }
        }
        Some(out)
    }

    let input = direction(model, "/modalities/input");
    let output = direction(model, "/modalities/output");
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(json!({
        "input": input,
        "output": output,
    }))
}

fn base_capabilities(model: &Value, provider_specific: bool) -> Value {
    let reasoning = model.get("reasoning").and_then(Value::as_bool);
    let mut reasoning_json = reasoning.map(|supported| json!({"supported": supported}));

    if provider_specific && reasoning == Some(true) {
        let mut levels = Vec::new();
        let mut can_disable = false;
        let mut has_toggle = false;
        if let Some(options) = model.get("reasoning_options").and_then(Value::as_array) {
            for option in options {
                match option.get("type").and_then(Value::as_str) {
                    Some("toggle") => {
                        has_toggle = true;
                        can_disable = true;
                    }
                    Some("effort") => {
                        if let Some(values) = option.get("values").and_then(Value::as_array) {
                            for value in values {
                                if value.is_null()
                                    || value.as_str().is_some_and(|value| value == "none")
                                {
                                    can_disable = true;
                                    continue;
                                }
                                let Some(value) = value.as_str() else {
                                    continue;
                                };
                                if matches!(
                                    value,
                                    "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                                ) && !levels.iter().any(|level| level == value)
                                {
                                    levels.push(value.to_string());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        reasoning_json = Some(if !levels.is_empty() {
            json!({
                "supported": true,
                "mode": "level",
                "levels": levels,
                "can_disable": can_disable,
            })
        } else if has_toggle {
            json!({
                "supported": true,
                "mode": "toggle",
                "can_disable": true,
            })
        } else {
            json!({
                "supported": true,
                "can_disable": can_disable,
            })
        });
    }

    let modalities = normalized_modalities(model);
    let input_modalities = modalities
        .as_ref()
        .and_then(|value| value.get("input"))
        .and_then(Value::as_array);
    let output_modalities = modalities
        .as_ref()
        .and_then(|value| value.get("output"))
        .and_then(Value::as_array);

    let mut capabilities = Map::new();
    capabilities.insert("schema_version".to_string(), json!(1));
    if let (Some(inputs), Some(outputs)) = (input_modalities, output_modalities) {
        let text_input = inputs.iter().any(|input| input.as_str() == Some("text"));
        let text_output = outputs.iter().any(|output| output.as_str() == Some("text"));
        capabilities.insert(
            "text".to_string(),
            json!({"supported": text_input && text_output}),
        );
    }
    if let Some(reasoning) = reasoning_json {
        capabilities.insert("reasoning".to_string(), reasoning);
    }
    if let Some(tool_call) = model.get("tool_call").and_then(Value::as_bool) {
        capabilities.insert("tools".to_string(), json!({"supported": tool_call}));
    }
    if let Some(structured_output) = model.get("structured_output").and_then(Value::as_bool) {
        capabilities.insert(
            "structured_output".to_string(),
            json!({"supported": structured_output}),
        );
    }
    if let Some(inputs) = input_modalities {
        capabilities.insert(
            "vision".to_string(),
            json!({
                "input": inputs.iter().any(|input| input.as_str() == Some("image"))
            }),
        );
    }
    Value::Object(capabilities)
}

fn metadata_snapshot(model: &Value, provider_specific: bool) -> Value {
    let mut out = Map::new();
    let mut keys = vec![
        "name",
        "description",
        "type",
        "family",
        "attachment",
        "temperature",
        "knowledge",
        "open_weights",
        "release_date",
        "last_updated",
    ];
    if provider_specific {
        keys.extend([
            "status",
            "reasoning_options",
            "interleaved",
            "provider",
            "experimental",
        ]);
    }
    for key in keys {
        if let Some(value) = model.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(out)
}

fn models_dev_canonical_match(canonical_id: &str, model: &Value) -> Option<CanonicalModelMatch> {
    model.as_object()?;
    Some(CanonicalModelMatch {
        source: CatalogSource::ModelsDev,
        canonical_id: canonical_id.to_string(),
        context_window: model.pointer("/limit/context").and_then(Value::as_i64),
        max_input_tokens: model.pointer("/limit/input").and_then(Value::as_i64),
        max_output_tokens: model.pointer("/limit/output").and_then(Value::as_i64),
        capabilities_json: base_capabilities(model, false),
        modalities: normalized_modalities(model),
        model_type: model
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
        metadata: metadata_snapshot(model, false),
        source_url: Some(MODELS_DEV_CATALOG_URL.to_string()),
    })
}

fn models_dev_provider_match(
    base_url: &str,
    provider_id: &str,
    model_id: &str,
    model: &Value,
) -> Option<ProviderModelMatch> {
    let url = url::Url::parse(base_url).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    model.as_object()?;

    let prices = Prices {
        input_per_1m: valid_price(model.pointer("/cost/input")),
        output_per_1m: valid_price(model.pointer("/cost/output")),
        cached_per_1m: valid_price(model.pointer("/cost/cache_read")),
        cache_write_per_1m: valid_price(model.pointer("/cost/cache_write")),
        thinking_per_1m: valid_price(model.pointer("/cost/reasoning")),
    };

    Some(ProviderModelMatch {
        source: CatalogSource::ModelsDev,
        provider_id: provider_id.to_string(),
        host,
        model_id: model_id.to_string(),
        context_window: model.pointer("/limit/context").and_then(Value::as_i64),
        max_input_tokens: model.pointer("/limit/input").and_then(Value::as_i64),
        max_output_tokens: model.pointer("/limit/output").and_then(Value::as_i64),
        capabilities_json: base_capabilities(model, true),
        modalities: normalized_modalities(model),
        prices,
        model_type: model
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
        metadata: metadata_snapshot(model, true),
        source_url: Some(MODELS_DEV_CATALOG_URL.to_string()),
    })
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct BundledCanonicalModel {
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_input_tokens: Option<i64>,
    #[serde(default)]
    max_output_tokens: Option<i64>,
    #[serde(default = "empty_capabilities")]
    capabilities_json: Value,
    #[serde(default)]
    modalities: Option<Value>,
    #[serde(default)]
    model_type: Option<String>,
    #[serde(default)]
    metadata: Value,
    #[serde(default)]
    source_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledAlias {
    id: String,
    canonical_model_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledProviderCatalog {
    #[serde(default)]
    provider_id: Option<String>,
    base_urls: Vec<String>,
    models: Vec<BundledProviderModel>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledProviderModel {
    id: String,
    #[serde(default)]
    canonical_model_id: Option<String>,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_input_tokens: Option<i64>,
    #[serde(default)]
    max_output_tokens: Option<i64>,
    #[serde(default = "empty_capabilities")]
    capabilities_json: Value,
    #[serde(default)]
    modalities: Option<Value>,
    #[serde(default)]
    prices: Prices,
    #[serde(default)]
    model_type: Option<String>,
    #[serde(default)]
    metadata: Value,
    #[serde(default)]
    source_url: Option<String>,
}

fn empty_capabilities() -> Value {
    json!({"schema_version": 1})
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledCatalogV2 {
    schema_version: u32,
    #[serde(default)]
    models: BTreeMap<String, BundledCanonicalModel>,
    #[serde(default)]
    aliases: Vec<BundledAlias>,
    #[serde(default)]
    provider_overrides: Vec<BundledProviderCatalog>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledCatalogV1 {
    schema_version: u32,
    providers: Vec<BundledProviderCatalogV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledProviderCatalogV1 {
    base_urls: Vec<String>,
    models: Vec<BundledProviderModelV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledProviderModelV1 {
    id: String,
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    capabilities_json: Value,
    #[serde(default)]
    modalities: Option<Value>,
    #[serde(default)]
    prices: Prices,
    source_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct BundledCatalog {
    models: BTreeMap<String, BundledCanonicalModel>,
    aliases: Vec<BundledAlias>,
    provider_overrides: Vec<BundledProviderCatalog>,
}

fn parse_bundled(catalog_json: &str) -> Option<BundledCatalog> {
    let raw: Value = serde_json::from_str(catalog_json).ok()?;
    match raw.get("schema_version").and_then(Value::as_u64)? {
        2 => {
            let parsed: BundledCatalogV2 = serde_json::from_value(raw).ok()?;
            if parsed.schema_version != 2 {
                return None;
            }
            validate_bundled_aliases(&parsed.models, &parsed.aliases)?;
            Some(BundledCatalog {
                models: parsed.models,
                aliases: parsed.aliases,
                provider_overrides: parsed.provider_overrides,
            })
        }
        1 => {
            let parsed: BundledCatalogV1 = serde_json::from_value(raw).ok()?;
            if parsed.schema_version != 1 {
                return None;
            }
            let provider_overrides = parsed
                .providers
                .into_iter()
                .map(|provider| BundledProviderCatalog {
                    provider_id: None,
                    base_urls: provider.base_urls,
                    models: provider
                        .models
                        .into_iter()
                        .map(|model| BundledProviderModel {
                            id: model.id,
                            canonical_model_id: None,
                            context_window: model.context_window,
                            max_input_tokens: None,
                            max_output_tokens: model.max_output_tokens,
                            capabilities_json: model.capabilities_json,
                            modalities: model.modalities,
                            prices: model.prices,
                            model_type: None,
                            metadata: Value::Null,
                            source_url: model.source_url,
                        })
                        .collect(),
                })
                .collect();
            Some(BundledCatalog {
                provider_overrides,
                ..Default::default()
            })
        }
        _ => None,
    }
}

fn validate_bundled_aliases(
    models: &BTreeMap<String, BundledCanonicalModel>,
    aliases: &[BundledAlias],
) -> Option<()> {
    let mut seen = HashSet::new();
    for alias in aliases {
        if alias.id.trim().is_empty()
            || alias.canonical_model_id.trim().is_empty()
            || !models.contains_key(&alias.canonical_model_id)
            || !seen.insert(alias.id.clone())
        {
            return None;
        }

        if models.contains_key(&alias.id) && alias.id != alias.canonical_model_id {
            return None;
        }

        let local_collisions: Vec<&str> = models
            .keys()
            .filter_map(|canonical_id| {
                (canonical_local_id(canonical_id) == Some(alias.id.as_str()))
                    .then_some(canonical_id.as_str())
            })
            .collect();
        if !local_collisions.is_empty()
            && !local_collisions
                .iter()
                .all(|canonical_id| *canonical_id == alias.canonical_model_id)
        {
            return None;
        }
    }
    Some(())
}

fn bundled_canonical_match(
    canonical_id: &str,
    model: &BundledCanonicalModel,
) -> CanonicalModelMatch {
    CanonicalModelMatch {
        source: CatalogSource::BundledCatalog,
        canonical_id: canonical_id.to_string(),
        context_window: model.context_window,
        max_input_tokens: model.max_input_tokens,
        max_output_tokens: model.max_output_tokens,
        capabilities_json: model.capabilities_json.clone(),
        modalities: model.modalities.clone(),
        model_type: model.model_type.clone(),
        metadata: model.metadata.clone(),
        source_url: model.source_url.clone(),
    }
}

fn lookup_bundled_provider(
    catalog: &BundledCatalog,
    base_url: &str,
    model_id: &str,
) -> Option<(ProviderModelMatch, Option<String>)> {
    let url = url::Url::parse(base_url).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let normalized_base = normalize_base_url(base_url)?;

    for provider in &catalog.provider_overrides {
        if !provider.base_urls.iter().any(|candidate| {
            normalize_base_url(candidate).as_deref() == Some(normalized_base.as_str())
        }) {
            continue;
        }
        let model = provider.models.iter().find(|model| model.id == model_id)?;
        return Some((
            ProviderModelMatch {
                source: CatalogSource::BundledCatalog,
                provider_id: provider.provider_id.clone().unwrap_or_else(|| host.clone()),
                host,
                model_id: model.id.clone(),
                context_window: model.context_window,
                max_input_tokens: model.max_input_tokens,
                max_output_tokens: model.max_output_tokens,
                capabilities_json: model.capabilities_json.clone(),
                modalities: model.modalities.clone(),
                prices: model.prices.clone(),
                model_type: model.model_type.clone(),
                metadata: model.metadata.clone(),
                source_url: model.source_url.clone(),
            },
            model.canonical_model_id.clone(),
        ));
    }
    None
}

fn dedupe_sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn canonical_exists(
    canonical_id: &str,
    models_dev: Option<&ModelsDevCatalog>,
    bundled: &BundledCatalog,
) -> bool {
    models_dev.is_some_and(|catalog| catalog.contains_canonical(canonical_id))
        || bundled.models.contains_key(canonical_id)
}

fn exact_model_candidates(
    model_id: &str,
    models_dev: Option<&ModelsDevCatalog>,
    bundled: &BundledCatalog,
) -> Vec<String> {
    let mut candidates = models_dev
        .map(|catalog| catalog.canonical_index.exact(model_id))
        .unwrap_or_default();
    candidates.extend(bundled.models.keys().filter_map(|canonical_id| {
        (canonical_local_id(canonical_id) == Some(model_id)).then_some(canonical_id.clone())
    }));
    dedupe_sorted(candidates)
}

fn case_insensitive_model_candidates(
    model_id: &str,
    models_dev: Option<&ModelsDevCatalog>,
    bundled: &BundledCatalog,
) -> Vec<String> {
    let normalized = model_id.to_ascii_lowercase();
    let mut candidates = models_dev
        .map(|catalog| catalog.canonical_index.case_insensitive(model_id))
        .unwrap_or_default();
    candidates.extend(bundled.models.keys().filter_map(|canonical_id| {
        canonical_local_id(canonical_id)
            .filter(|local| local.to_ascii_lowercase() == normalized)
            .map(|_| canonical_id.clone())
    }));
    dedupe_sorted(candidates)
}

fn identity_for(
    upstream_model_id: &str,
    explicit_hint: Option<&str>,
    models_dev: Option<&ModelsDevCatalog>,
    bundled: &BundledCatalog,
) -> CanonicalIdentity {
    let source_for = |canonical_id: &str| {
        if models_dev.is_some_and(|catalog| catalog.contains_canonical(canonical_id)) {
            Some(CatalogSource::ModelsDev)
        } else if bundled.models.contains_key(canonical_id) {
            Some(CatalogSource::BundledCatalog)
        } else {
            None
        }
    };

    let resolved = |canonical_id: String, match_kind: CanonicalMatchKind| CanonicalIdentity {
        status: CanonicalIdentityStatus::Resolved,
        upstream_model_id: upstream_model_id.to_string(),
        source: source_for(&canonical_id),
        canonical_model_id: Some(canonical_id),
        match_kind: Some(match_kind),
        candidates: Vec::new(),
    };

    if let Some(hint) = explicit_hint.filter(|hint| canonical_exists(hint, models_dev, bundled)) {
        return resolved(hint.to_string(), CanonicalMatchKind::ExplicitHint);
    }

    if canonical_exists(upstream_model_id, models_dev, bundled) {
        return resolved(
            upstream_model_id.to_string(),
            CanonicalMatchKind::ExactCanonicalId,
        );
    }

    let exact = exact_model_candidates(upstream_model_id, models_dev, bundled);
    if exact.len() == 1 {
        return resolved(exact[0].clone(), CanonicalMatchKind::ExactModelId);
    }
    if exact.len() > 1 {
        return CanonicalIdentity {
            status: CanonicalIdentityStatus::Ambiguous,
            upstream_model_id: upstream_model_id.to_string(),
            canonical_model_id: None,
            match_kind: None,
            candidates: exact,
            source: None,
        };
    }

    let case_insensitive =
        case_insensitive_model_candidates(upstream_model_id, models_dev, bundled);
    if case_insensitive.len() == 1 {
        return resolved(
            case_insensitive[0].clone(),
            CanonicalMatchKind::CaseInsensitiveModelId,
        );
    }
    if case_insensitive.len() > 1 {
        return CanonicalIdentity {
            status: CanonicalIdentityStatus::Ambiguous,
            upstream_model_id: upstream_model_id.to_string(),
            canonical_model_id: None,
            match_kind: None,
            candidates: case_insensitive,
            source: None,
        };
    }

    if let Some(alias) = bundled
        .aliases
        .iter()
        .find(|alias| alias.id == upstream_model_id)
    {
        return resolved(
            alias.canonical_model_id.clone(),
            CanonicalMatchKind::ExplicitAlias,
        );
    }

    CanonicalIdentity::unresolved(upstream_model_id)
}

pub fn resolve_canonical(
    upstream_model_id: &str,
    explicit_hint: Option<&str>,
    models_dev: Option<&ModelsDevCatalog>,
) -> CanonicalIdentity {
    let bundled = parse_bundled(BUNDLED_CATALOG_JSON).unwrap_or_default();
    identity_for(upstream_model_id, explicit_hint, models_dev, &bundled)
}

pub fn resolve(
    base_url: &str,
    model_id: &str,
    models_dev: Option<&ModelsDevCatalog>,
) -> CatalogResolution {
    resolve_with_bundled(base_url, model_id, None, models_dev, BUNDLED_CATALOG_JSON)
}

pub fn resolve_with_hint(
    base_url: &str,
    model_id: &str,
    explicit_hint: Option<&str>,
    models_dev: Option<&ModelsDevCatalog>,
) -> CatalogResolution {
    resolve_with_bundled(
        base_url,
        model_id,
        explicit_hint,
        models_dev,
        BUNDLED_CATALOG_JSON,
    )
}

fn resolve_with_bundled(
    base_url: &str,
    model_id: &str,
    explicit_hint: Option<&str>,
    models_dev: Option<&ModelsDevCatalog>,
    bundled_json: &str,
) -> CatalogResolution {
    let bundled = parse_bundled(bundled_json).unwrap_or_default();

    let (bundled_provider, bundled_hint) = lookup_bundled_provider(&bundled, base_url, model_id)
        .map(|(provider, hint)| (Some(provider), hint))
        .unwrap_or((None, None));
    let identity = identity_for(
        model_id,
        explicit_hint.or(bundled_hint.as_deref()),
        models_dev,
        &bundled,
    );

    let bundled_canonical = identity
        .canonical_model_id
        .as_deref()
        .and_then(|canonical_id| {
            bundled
                .models
                .get(canonical_id)
                .map(|model| bundled_canonical_match(canonical_id, model))
        });
    let models_dev_canonical = identity
        .canonical_model_id
        .as_deref()
        .and_then(|canonical_id| {
            models_dev.and_then(|catalog| catalog.canonical_match(canonical_id))
        });
    let models_dev_provider =
        models_dev.and_then(|catalog| catalog.provider_match(base_url, model_id));

    let fallback_canonical = models_dev_canonical.as_ref().and(bundled_canonical.clone());
    let fallback_provider = models_dev_provider.as_ref().and(bundled_provider.clone());
    let canonical = models_dev_canonical.or(bundled_canonical);
    let provider = models_dev_provider.or(bundled_provider);

    CatalogResolution {
        identity,
        canonical,
        provider,
        fallback_canonical,
        fallback_provider,
        catalog_provenance: models_dev.and_then(|catalog| catalog.provenance.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models_dev_fixture() -> ModelsDevCatalog {
        ModelsDevCatalog::from_parts(
            json!({
                "google/gemini-3.8-flash": {
                    "id": "google/gemini-3.8-flash",
                    "name": "Gemini 3.8 Flash",
                    "description": "fixture",
                    "family": "gemini",
                    "reasoning": true,
                    "tool_call": true,
                    "structured_output": true,
                    "temperature": true,
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "limit": {"context": 1048576, "input": 1048576, "output": 65536}
                },
                "deepseek/deepseek-v4.1-flash": {
                    "id": "deepseek/deepseek-v4.1-flash",
                    "name": "DeepSeek V4.1 Flash",
                    "description": "fixture",
                    "reasoning": true,
                    "tool_call": true,
                    "structured_output": true,
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "limit": {"context": 1000000, "output": 384000}
                },
                "anthropic/claude-opus-4-6": {
                    "id": "anthropic/claude-opus-4-6",
                    "name": "Claude Opus 4.6",
                    "description": "fixture",
                    "reasoning": true,
                    "tool_call": true,
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "limit": {"context": 200000, "output": 64000}
                },
                "xiaomi/mimo-v2.6-flash": {
                    "id": "xiaomi/mimo-v2.6-flash",
                    "name": "MiMo V2.6 Flash",
                    "description": "fixture",
                    "reasoning": true,
                    "tool_call": true,
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "limit": {"context": 1048576, "output": 65536}
                },
                "typesafe/jev-latest": {
                    "id": "typesafe/jev-latest",
                    "type": "decision",
                    "name": "JEV",
                    "description": "fixture",
                    "reasoning": false,
                    "tool_call": false,
                    "structured_output": true,
                    "temperature": false,
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "limit": {"context": 64000, "output": 0}
                }
            }),
            json!({
                "openai": {
                    "id": "openai",
                    "name": "OpenAI",
                    "models": {}
                },
                "anthropic": {
                    "id": "anthropic",
                    "name": "Anthropic",
                    "models": {}
                },
                "google": {
                    "id": "google",
                    "name": "Google",
                    "models": {
                        "gemini-3.8-flash": {
                            "id": "gemini-3.8-flash",
                            "reasoning": true,
                            "reasoning_options": [
                                {"type": "effort", "values": ["low", "medium", "high"]}
                            ],
                            "tool_call": true,
                            "structured_output": true,
                            "modalities": {"input": ["text", "image"], "output": ["text"]},
                            "cost": {
                                "input": 0.75,
                                "output": 3.75,
                                "cache_read": 0.075
                            },
                            "limit": {"context": 200000, "output": 32000}
                        }
                    }
                },
                "openrouter": {
                    "id": "openrouter",
                    "api": "https://openrouter.ai/api/v1",
                    "models": {
                        "shared-model": {
                            "id": "shared-model",
                            "reasoning": false,
                            "tool_call": false,
                            "structured_output": false,
                            "modalities": {"input": ["text"], "output": ["text"]},
                            "limit": {"context": 32000, "output": 4096}
                        }
                    }
                }
            }),
        )
        .unwrap()
    }

    fn bundled_v2() -> &'static str {
        r#"{
          "schema_version": 2,
          "models": {
            "anthropic/claude-opus-4-6": {},
            "xiaomi/mimo-v2.6-flash": {},
            "deepseek/deepseek-v4.1-flash": {
              "context_window": 1000000,
              "max_output_tokens": 384000,
              "capabilities_json": {
                "schema_version": 1,
                "reasoning": {"supported": true},
                "tools": {"supported": true},
                "vision": {"input": true},
                "structured_output": {"supported": true}
              }
            }
          },
          "aliases": [
            {
              "id": "claude-opus-4-6-thinking",
              "canonical_model_id": "anthropic/claude-opus-4-6"
            },
            {
              "id": "mimo-v2.6-flash-free",
              "canonical_model_id": "xiaomi/mimo-v2.6-flash"
            }
          ],
          "provider_overrides": [{
            "provider_id": "b-ai",
            "base_urls": ["https://api.b.ai/v1"],
            "models": [{
              "id": "DeepSeek-V4.1-Flash",
              "canonical_model_id": "deepseek/deepseek-v4.1-flash",
              "capabilities_json": {
                "schema_version": 1,
                "reasoning": {
                  "supported": true,
                  "mode": "level",
                  "levels": ["low", "high", "max"]
                }
              }
            }]
          }]
        }"#
    }

    #[test]
    fn catalog_endpoint_requests_specialized_models() {
        assert_eq!(
            MODELS_DEV_CATALOG_URL,
            "https://models.dev/catalog.json?type=all"
        );
    }

    #[test]
    fn resolves_first_party_and_api_declaring_provider_identity() {
        let catalog = models_dev_fixture();
        assert_eq!(
            catalog
                .provider_id_for_base("https://api.openai.com/v1")
                .as_deref(),
            Some("openai")
        );
        assert_eq!(
            catalog
                .provider_id_for_base("https://api.anthropic.com/v1/")
                .as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            catalog
                .provider_id_for_base("https://generativelanguage.googleapis.com/v1beta")
                .as_deref(),
            Some("google")
        );
        assert_eq!(
            catalog
                .provider_id_for_base("https://openrouter.ai/api/v1")
                .as_deref(),
            Some("openrouter")
        );
    }

    #[test]
    fn provider_default_effort_does_not_poison_v1_capabilities() {
        let capabilities = base_capabilities(
            &json!({
                "reasoning": true,
                "reasoning_options": [
                    {"type": "effort", "values": ["none", "default"]}
                ],
                "tool_call": true,
                "structured_output": true,
                "modalities": {"input": ["text"], "output": ["text"]}
            }),
            true,
        );

        assert_eq!(capabilities["reasoning"]["supported"], true);
        assert_eq!(capabilities["reasoning"]["can_disable"], true);
        assert!(capabilities["reasoning"].get("levels").is_none());
        assert!(capabilities["reasoning"].get("mode").is_none());

        let flags = crate::adapters::plugin_capability_flags_v1(&capabilities)
            .expect("models.dev capability envelope should remain valid v1");
        assert_eq!(flags.reasoning, Some(true));
        assert_eq!(flags.tool_calling, Some(true));
        assert_eq!(flags.structured_output, Some(true));
    }

    #[test]
    fn provider_default_effort_is_filtered_beside_executable_levels() {
        let capabilities = base_capabilities(
            &json!({
                "reasoning": true,
                "reasoning_options": [
                    {"type": "effort", "values": ["none", "default", "low", "high"]}
                ]
            }),
            true,
        );

        assert_eq!(capabilities["reasoning"]["mode"], "level");
        assert_eq!(capabilities["reasoning"]["levels"], json!(["low", "high"]));
        assert_eq!(capabilities["reasoning"]["can_disable"], true);

        let reasoning = crate::adapters::normalize_plugin_reasoning_capability_v1(&capabilities)
            .expect("filtered effort list should remain executable");
        assert_eq!(
            reasoning.levels,
            vec!["low".to_string(), "high".to_string()]
        );
    }

    #[test]
    fn canonical_lookup_does_not_require_provider_identity() {
        let catalog = models_dev_fixture();
        let resolved = resolve_with_bundled(
            "https://unknown.example/v1",
            "deepseek-v4.1-flash",
            None,
            Some(&catalog),
            bundled_v2(),
        );
        assert_eq!(
            resolved.identity.canonical_model_id.as_deref(),
            Some("deepseek/deepseek-v4.1-flash")
        );
        assert_eq!(
            resolved.identity.match_kind,
            Some(CanonicalMatchKind::ExactModelId)
        );
        assert!(resolved.canonical.is_some());
        assert!(resolved.provider.is_none());
    }

    #[test]
    fn canonical_lookup_is_case_insensitive_only_when_unique() {
        let catalog = models_dev_fixture();
        let resolved = resolve_with_bundled(
            "https://unknown.example/v1",
            "DeepSeek-V4.1-Flash",
            None,
            Some(&catalog),
            bundled_v2(),
        );
        assert_eq!(
            resolved.identity.canonical_model_id.as_deref(),
            Some("deepseek/deepseek-v4.1-flash")
        );
        assert_eq!(
            resolved.identity.match_kind,
            Some(CanonicalMatchKind::CaseInsensitiveModelId)
        );
    }

    #[test]
    fn explicit_alias_resolves_decorated_provider_id() {
        let catalog = models_dev_fixture();
        let resolved = resolve_with_bundled(
            "https://antigravity.example/v1",
            "claude-opus-4-6-thinking",
            None,
            Some(&catalog),
            bundled_v2(),
        );
        assert_eq!(
            resolved.identity.canonical_model_id.as_deref(),
            Some("anthropic/claude-opus-4-6")
        );
        assert_eq!(
            resolved.identity.match_kind,
            Some(CanonicalMatchKind::ExplicitAlias)
        );
    }

    #[test]
    fn explicit_verified_hint_wins_first() {
        let catalog = models_dev_fixture();
        let identity = identity_for(
            "provider-decorated-name",
            Some("xiaomi/mimo-v2.6-flash"),
            Some(&catalog),
            &parse_bundled(bundled_v2()).unwrap(),
        );
        assert_eq!(
            identity.canonical_model_id.as_deref(),
            Some("xiaomi/mimo-v2.6-flash")
        );
        assert_eq!(identity.match_kind, Some(CanonicalMatchKind::ExplicitHint));
    }

    #[test]
    fn ambiguous_model_id_remains_unresolved() {
        let catalog = ModelsDevCatalog::from_parts(
            json!({
                "vendor-a/foo": {"id": "vendor-a/foo"},
                "vendor-b/foo": {"id": "vendor-b/foo"}
            }),
            json!({}),
        )
        .unwrap();
        let resolved = resolve_with_bundled(
            "https://unknown.example/v1",
            "foo",
            None,
            Some(&catalog),
            r#"{"schema_version":2,"models":{},"aliases":[],"provider_overrides":[]}"#,
        );
        assert_eq!(resolved.identity.status, CanonicalIdentityStatus::Ambiguous);
        assert_eq!(
            resolved.identity.candidates,
            vec!["vendor-a/foo".to_string(), "vendor-b/foo".to_string()]
        );
        assert!(resolved.canonical.is_none());
    }

    #[test]
    fn unknown_model_stays_unknown() {
        let catalog = models_dev_fixture();
        let resolved = resolve_with_bundled(
            "https://unknown.example/v1",
            "some-future-private-model",
            None,
            Some(&catalog),
            bundled_v2(),
        );
        assert_eq!(
            resolved.identity.status,
            CanonicalIdentityStatus::Unresolved
        );
        assert!(resolved.canonical.is_none());
        assert!(resolved.provider.is_none());
    }

    #[test]
    fn models_dev_only_overrides_present_bundled_fields() {
        let catalog = ModelsDevCatalog::from_parts(
            json!({
                "vendor/model": {
                    "id": "vendor/model",
                    "reasoning": true,
                    "limit": {"context": 200000}
                }
            }),
            json!({}),
        )
        .unwrap();
        let bundled = r#"{
          "schema_version": 2,
          "models": {
            "vendor/model": {
              "context_window": 100000,
              "max_output_tokens": 32000,
              "capabilities_json": {
                "schema_version": 1,
                "tools": {"supported": true}
              }
            }
          },
          "aliases": [],
          "provider_overrides": []
        }"#;
        let resolved = resolve_with_bundled(
            "https://unknown.example/v1",
            "model",
            None,
            Some(&catalog),
            bundled,
        );
        let layers = resolved.layers();

        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].provenance(), "bundled_catalog");
        assert_eq!(layers[1].provenance(), "models.dev:canonical");
        assert_eq!(layers[0].max_output_tokens, Some(32_000));
        assert_eq!(layers[1].max_output_tokens, None);
    }

    #[test]
    fn canonical_modalities_preserve_supported_media_types() {
        let catalog = ModelsDevCatalog::from_parts(
            json!({
                "vendor/multimodal": {
                    "id": "vendor/multimodal",
                    "modalities": {
                        "input": ["text", "image", "audio", "video", "pdf"],
                        "output": ["text", "image", "audio", "video"]
                    }
                }
            }),
            json!({}),
        )
        .unwrap();
        let resolved = resolve_with_bundled(
            "https://unknown.example/v1",
            "multimodal",
            None,
            Some(&catalog),
            r#"{"schema_version":2,"models":{},"aliases":[],"provider_overrides":[]}"#,
        );
        let modalities = resolved.canonical.unwrap().modalities.unwrap();

        assert_eq!(
            modalities["input"],
            json!(["text", "image", "audio", "video", "pdf"])
        );
        assert_eq!(
            modalities["output"],
            json!(["text", "image", "audio", "video"])
        );
    }

    #[test]
    fn provider_overlay_is_separate_and_overrides_canonical_limits() {
        let catalog = models_dev_fixture();
        let resolved = resolve_with_bundled(
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-3.8-flash",
            None,
            Some(&catalog),
            bundled_v2(),
        );
        assert_eq!(
            resolved.canonical.as_ref().unwrap().context_window,
            Some(1_048_576)
        );
        assert_eq!(
            resolved.provider.as_ref().unwrap().context_window,
            Some(200_000)
        );
        let layers = resolved.layers();
        assert_eq!(layers.last().unwrap().provenance(), "models.dev:provider");
        assert_eq!(layers.last().unwrap().context_window, Some(200_000));
    }

    #[test]
    fn canonical_metadata_never_carries_provider_pricing_or_reasoning_levels() {
        let catalog = models_dev_fixture();
        let resolved = resolve_with_bundled(
            "https://unknown.example/v1",
            "gemini-3.8-flash",
            None,
            Some(&catalog),
            bundled_v2(),
        );
        let canonical = resolved.canonical.unwrap();
        assert_eq!(canonical.capabilities_json["reasoning"]["supported"], true);
        assert!(canonical.capabilities_json["reasoning"]
            .get("levels")
            .is_none());
        assert!(resolved.provider.is_none());
    }

    #[test]
    fn provider_pricing_only_comes_from_current_provider_offering() {
        let catalog = models_dev_fixture();
        let known = resolve_with_bundled(
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-3.8-flash",
            None,
            Some(&catalog),
            bundled_v2(),
        );
        assert_eq!(
            known.provider.as_ref().unwrap().prices.input_per_1m,
            Some(0.75)
        );

        let unknown = resolve_with_bundled(
            "https://unknown.example/v1",
            "gemini-3.8-flash",
            None,
            Some(&catalog),
            bundled_v2(),
        );
        assert!(unknown.provider.is_none());
    }

    #[test]
    fn recognized_provider_missing_model_still_gets_canonical_enrichment() {
        let catalog = models_dev_fixture();
        let resolved = resolve_with_bundled(
            "https://generativelanguage.googleapis.com/v1beta",
            "deepseek-v4.1-flash",
            None,
            Some(&catalog),
            bundled_v2(),
        );
        assert!(resolved.canonical.is_some());
        assert!(resolved.provider.is_none());
    }

    #[test]
    fn models_dev_provider_maps_prices_and_modalities() {
        let catalog = ModelsDevCatalog::from_parts(
            json!({
                "vendor/priced-model": {
                    "id": "vendor/priced-model",
                    "modalities": {
                        "input": ["text", "audio", "unknown-input"],
                        "output": ["text", "video", "unknown-output"]
                    }
                }
            }),
            json!({
                "openrouter": {
                    "id": "openrouter",
                    "api": "https://openrouter.ai/api/v1",
                    "models": {
                        "priced-model": {
                            "id": "priced-model",
                            "modalities": {
                                "input": ["text", "audio", "unknown-input"],
                                "output": ["text", "video", "unknown-output"]
                            },
                            "cost": {
                                "input": 1.0,
                                "output": 2.0,
                                "cache_read": 0.25,
                                "cache_write": 0.5,
                                "reasoning": 3.0,
                                "cache_storage": 99.0
                            }
                        }
                    }
                }
            }),
        )
        .unwrap();
        let provider = catalog
            .provider_match("https://openrouter.ai/api/v1", "priced-model")
            .unwrap();

        assert_eq!(provider.prices.input_per_1m, Some(1.0));
        assert_eq!(provider.prices.output_per_1m, Some(2.0));
        assert_eq!(provider.prices.cached_per_1m, Some(0.25));
        assert_eq!(provider.prices.cache_write_per_1m, Some(0.5));
        assert_eq!(provider.prices.thinking_per_1m, Some(3.0));
        assert_eq!(
            provider.modalities.as_ref().unwrap()["input"],
            json!(["text", "audio"])
        );
        assert_eq!(
            provider.modalities.as_ref().unwrap()["output"],
            json!(["text", "video"])
        );
    }

    #[test]
    fn specialized_model_type_and_zero_output_are_preserved() {
        let catalog = models_dev_fixture();
        let resolved = resolve_with_bundled(
            "https://unknown.example/v1",
            "jev-latest",
            None,
            Some(&catalog),
            bundled_v2(),
        );
        let canonical = resolved.canonical.unwrap();
        assert_eq!(canonical.canonical_id, "typesafe/jev-latest");
        assert_eq!(canonical.model_type.as_deref(), Some("decision"));
        assert_eq!(canonical.context_window, Some(64_000));
        assert_eq!(canonical.max_output_tokens, Some(0));
        assert_eq!(canonical.capabilities_json["reasoning"]["supported"], false);
        assert_eq!(canonical.capabilities_json["tools"]["supported"], false);
        assert_eq!(
            canonical.capabilities_json["structured_output"]["supported"],
            true
        );
    }

    #[test]
    fn stale_catalog_keeps_original_retrieval_provenance() {
        let catalog = models_dev_fixture().with_source_state(
            "2026-09-20T00:00:00Z".into(),
            "fresh",
            Some("etag-1".into()),
            Some("Sun, 20 Sep 2026 00:00:00 GMT".into()),
        );
        let stale = catalog.with_freshness("stale");
        let state = stale.provenance.as_ref().unwrap();
        assert_eq!(state["retrieved_at"], "2026-09-20T00:00:00Z");
        assert_eq!(state["freshness"], "stale");
        assert_eq!(state["etag"], "etag-1");
    }

    #[test]
    fn not_modified_validation_keeps_price_provenance_stable() {
        let catalog = models_dev_fixture().with_source_state(
            "2026-09-20T00:00:00Z".into(),
            "fresh",
            Some("etag-1".into()),
            Some("Sun, 20 Sep 2026 00:00:00 GMT".into()),
        );
        let expected_provenance = catalog.provenance.clone();
        let entry = ModelsDevCacheEntry {
            catalog,
            etag: Some("etag-1".into()),
            last_modified: Some("Sun, 20 Sep 2026 00:00:00 GMT".into()),
            fetched_at: Instant::now(),
        }
        .mark_validated();

        assert_eq!(entry.catalog.provenance, expected_provenance);
        let state = entry.catalog.provenance.as_ref().unwrap();
        assert_eq!(state["retrieved_at"], "2026-09-20T00:00:00Z");
        assert_eq!(state["freshness"], "fresh");
    }

    #[test]
    fn explicit_plugin_hint_resolves_only_existing_canonical_model() {
        let catalog = models_dev_fixture();
        let resolved = resolve_with_hint(
            "https://daily-cloudcode-pa.googleapis.com",
            "gemini-3.8-flash-high",
            Some("google/gemini-3.8-flash"),
            Some(&catalog),
        );
        assert_eq!(
            resolved.identity.canonical_model_id.as_deref(),
            Some("google/gemini-3.8-flash")
        );
        assert_eq!(
            resolved.identity.match_kind,
            Some(CanonicalMatchKind::ExplicitHint)
        );
        assert!(resolved.canonical.is_some());
    }

    #[test]
    fn unknown_plugin_hint_is_ignored_before_normal_resolution() {
        let catalog = models_dev_fixture();
        let resolved = resolve_with_hint(
            "https://unknown.example/v1",
            "deepseek-v4.1-flash",
            Some("invented/not-real"),
            Some(&catalog),
        );
        assert_eq!(
            resolved.identity.canonical_model_id.as_deref(),
            Some("deepseek/deepseek-v4.1-flash")
        );
        assert_ne!(
            resolved.identity.match_kind,
            Some(CanonicalMatchKind::ExplicitHint)
        );
    }

    #[test]
    fn models_dev_unavailability_uses_canonical_bundled_fallback() {
        let resolved = resolve_with_bundled(
            "https://unknown.example/v1",
            "deepseek-v4.1-flash",
            None,
            None,
            bundled_v2(),
        );
        assert_eq!(
            resolved.identity.canonical_model_id.as_deref(),
            Some("deepseek/deepseek-v4.1-flash")
        );
        assert_eq!(
            resolved.canonical.as_ref().unwrap().source,
            CatalogSource::BundledCatalog
        );
    }

    #[test]
    fn bundled_provider_matching_is_exact() {
        let resolved = resolve_with_bundled(
            "https://api.b.ai/other",
            "DeepSeek-V4.1-Flash",
            None,
            None,
            bundled_v2(),
        );
        assert!(resolved.provider.is_none());
    }

    #[test]
    fn bundled_alias_validation_rejects_duplicate_aliases() {
        assert!(parse_bundled(
            r#"{
              "schema_version":2,
              "models":{"vendor/a":{}},
              "aliases":[
                {"id":"alias","canonical_model_id":"vendor/a"},
                {"id":"alias","canonical_model_id":"vendor/a"}
              ],
              "provider_overrides":[]
            }"#
        )
        .is_none());
    }

    #[test]
    fn bundled_alias_validation_rejects_missing_targets() {
        assert!(parse_bundled(
            r#"{
              "schema_version":2,
              "models":{},
              "aliases":[{"id":"alias","canonical_model_id":"vendor/missing"}],
              "provider_overrides":[]
            }"#
        )
        .is_none());
    }

    #[test]
    fn bundled_alias_validation_rejects_ambiguous_aliases() {
        assert!(parse_bundled(
            r#"{
              "schema_version":2,
              "models":{"vendor-a/foo":{},"vendor-b/alias":{}},
              "aliases":[{"id":"foo","canonical_model_id":"vendor-b/alias"}],
              "provider_overrides":[]
            }"#
        )
        .is_none());
    }

    #[test]
    fn legacy_bundled_schema_remains_parseable_during_migration() {
        let legacy = r#"{
          "schema_version":1,
          "providers":[{
            "base_urls":["https://api.b.ai/v1"],
            "models":[{
              "id":"DeepSeek-V4.1-Flash",
              "context_window":1000000,
              "max_output_tokens":384000,
              "capabilities_json":{"schema_version":1},
              "source_url":"https://example.invalid"
            }]
          }]
        }"#;
        let resolved = resolve_with_bundled(
            "https://api.b.ai/v1",
            "DeepSeek-V4.1-Flash",
            None,
            None,
            legacy,
        );
        assert!(resolved.provider.is_some());
    }
}
