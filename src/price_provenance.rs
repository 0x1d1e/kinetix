//! Per-field model price provenance.
//!
//! Owns which source supplies each observed price field (provider metadata >
//! plugin capability document > catalog layers), which source owns each
//! effective price field (operator edits, pins, and accepts versus automatic
//! observations), and the provenance persisted as `price_source`,
//! `price_metadata`, and `discovery.effective_pricing`. Callers decide when a
//! transition happens; this module decides what it does to prices and
//! provenance.

use serde_json::{json, Map, Value};

use crate::model_catalog::is_external_catalog_price_source;
use crate::types::Prices;

/// Observation source for prices reported by the upstream model listing.
pub(crate) const PROVIDER_METADATA: &str = "provider_metadata";
/// Observation source for prices declared by a plugin capability document.
pub(crate) const PLUGIN_DOCUMENT: &str = "plugin_capabilities_json";

/// The persisted provenance of a model's effective prices.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PriceProvenance {
    /// One field source, `mixed`, `untracked`, or the transition fallback.
    pub source: String,
    /// `{"fields": {<field>: {"source", "metadata"}}, ...}`.
    pub metadata: Value,
}

impl PriceProvenance {
    fn new(fields: Map<String, Value>, prices: &Prices, fallback: &str) -> Self {
        let source = if prices.is_configured() {
            effective_price_source(&fields, prices)
        } else {
            fallback.to_string()
        };
        Self {
            source,
            metadata: json!({ "fields": fields }),
        }
    }

    pub(crate) fn untracked() -> Self {
        Self {
            source: "untracked".into(),
            metadata: json!({ "fields": {} }),
        }
    }
}

// ---------------------------------------------------------------------------
// Observation: what discovery saw and where each field came from.
// ---------------------------------------------------------------------------

fn valid_discovery_price(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// Prices reported in a metadata document. Negative or non-finite values are
/// treated as unknown.
pub(crate) fn discovery_prices(metadata: &Value) -> Prices {
    let mut prices = Prices::default();
    if let Some(reported) = metadata.get("prices") {
        for field in Prices::FIELDS {
            prices.set_field(field, valid_discovery_price(reported.get(field)));
        }
    }
    prices
}

/// Observed prices built by applying sources from lowest to highest
/// precedence; a later layer wins each field it reports.
#[derive(Debug, Default)]
pub(crate) struct LayeredPrices {
    prices: Prices,
    sources: Map<String, Value>,
}

impl LayeredPrices {
    pub(crate) fn layer(&mut self, prices: &Prices, source: &str) {
        for field in Prices::FIELDS {
            if let Some(value) = prices.field(field) {
                self.prices.set_field(field, Some(value));
                self.sources
                    .insert(field.into(), Value::String(source.into()));
            }
        }
    }

    /// The observed prices and the `price_sources` document, with an explicit
    /// null for every field no source reported.
    pub(crate) fn finish(mut self) -> (Prices, Value) {
        let sources = Prices::FIELDS
            .into_iter()
            .map(|field| {
                let source = self.sources.remove(field).unwrap_or(Value::Null);
                (field.to_string(), source)
            })
            .collect();
        (self.prices, Value::Object(sources))
    }
}

fn observed_field_source(observation: &Value, field: &str) -> String {
    observation
        .pointer(&format!("/price_sources/{field}"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| automatic_price_source(observation))
}

fn automatic_price_source(observation: &Value) -> String {
    let sources: std::collections::BTreeSet<&str> = observation
        .get("price_sources")
        .and_then(Value::as_object)
        .map(|values| values.values().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    match sources.len() {
        0 => "discovery".into(),
        1 => sources.into_iter().next().unwrap_or("discovery").into(),
        _ => "mixed".into(),
    }
}

fn is_automatic_price_source(source: &str) -> bool {
    matches!(
        source,
        "provider_metadata"
            | "plugin_capabilities_json"
            | "models.dev"
            | "bundled_catalog"
            | "mixed"
            | "discovery"
    ) || source.starts_with("models.dev:")
        || source.starts_with("bundled_catalog:")
}

/// Whether an observation carries any automatic price signal, including an
/// authoritative absence (`price_sources.<field> = null`).
pub(crate) fn has_automatic_price_observation(observed: &Prices, observation: &Value) -> bool {
    observed.is_configured()
        || Prices::FIELDS.iter().any(|field| {
            observation
                .pointer(&format!("/price_sources/{field}"))
                .is_some_and(Value::is_null)
        })
}

/// Drop externally catalogued prices from an observation for an
/// integration-scoped Provider, whose own pricing must not be inferred from a
/// public catalog.
pub(crate) fn automatic_prices_for_provider_scope(
    prices: &Prices,
    observation: &Value,
    pricing_scope: &str,
) -> Prices {
    let mut effective = prices.clone();
    if pricing_scope != "integration" {
        return effective;
    }
    for field in Prices::FIELDS {
        if effective.field(field).is_some()
            && is_external_catalog_price_source(&observed_field_source(observation, field))
        {
            effective.set_field(field, None);
        }
    }
    effective
}

// ---------------------------------------------------------------------------
// Effective prices: who owns each field and how transitions change it.
// ---------------------------------------------------------------------------

/// Per-field provenance stored on `discovery.effective_pricing`. Configured
/// prices without per-field provenance inherit the legacy whole-row source.
fn effective_price_fields(discovery: &Value, current: &Prices) -> Map<String, Value> {
    let mut fields = discovery
        .pointer("/effective_pricing/fields")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let legacy_source = discovery
        .pointer("/effective_pricing/source")
        .and_then(Value::as_str)
        .unwrap_or("operator");
    let legacy_metadata = discovery
        .pointer("/effective_pricing/metadata")
        .cloned()
        .unwrap_or_else(|| json!({}));
    for field in Prices::FIELDS {
        if current.field(field).is_some() && !fields.contains_key(field) {
            set_field_provenance(&mut fields, field, legacy_source, legacy_metadata.clone());
        }
    }
    fields
}

fn set_field_provenance(
    fields: &mut Map<String, Value>,
    field: &str,
    source: &str,
    metadata: Value,
) {
    fields.insert(
        field.to_string(),
        json!({ "source": source, "metadata": metadata }),
    );
}

fn field_source<'a>(fields: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    fields
        .get(field)
        .and_then(|value| value.get("source"))
        .and_then(Value::as_str)
}

fn catalog_contributes(fields: &Map<String, Value>) -> bool {
    fields
        .keys()
        .any(|field| field_source(fields, field).is_some_and(is_external_catalog_price_source))
}

fn price_field_operator_owned(fields: &Map<String, Value>, current: &Prices, field: &str) -> bool {
    if let Some(source) = field_source(fields, field) {
        return !is_automatic_price_source(source);
    }
    // Legacy configured prices predate per-field provenance and are operator
    // owned. An absent value without provenance remains probe/sync-refinable.
    current.field(field).is_some()
}

fn effective_price_source(fields: &Map<String, Value>, prices: &Prices) -> String {
    let sources: std::collections::BTreeSet<&str> = Prices::FIELDS
        .iter()
        .filter(|field| prices.field(field).is_some())
        .filter_map(|field| field_source(fields, field))
        .collect();
    match sources.len() {
        0 => "untracked".into(),
        1 => sources.into_iter().next().unwrap_or("untracked").into(),
        _ => "mixed".into(),
    }
}

fn catalog_provider_price_identity(observation: &Value) -> Value {
    let Some(provider) = observation
        .pointer("/catalog/provider")
        .and_then(Value::as_object)
    else {
        return Value::Null;
    };
    let identity: Map<String, Value> = ["reference", "provider_id", "model_id"]
        .into_iter()
        .filter_map(|field| Some((field.to_string(), provider.get(field)?.clone())))
        .collect();
    if identity.is_empty() {
        Value::Null
    } else {
        Value::Object(identity)
    }
}

/// Record that `field` now holds the automatic value observed in `observation`.
fn set_automatic_field_provenance(
    fields: &mut Map<String, Value>,
    field: &str,
    observation: &Value,
) {
    let source = observed_field_source(observation, field);
    let metadata = if is_external_catalog_price_source(&source) {
        let source_state = observation
            .pointer("/catalog/source_state")
            .cloned()
            .unwrap_or(Value::Null);
        json!({
            "observed_at": source_state.get("retrieved_at").cloned().unwrap_or(Value::Null),
            "catalog_source_state": source_state,
            "catalog_provider": catalog_provider_price_identity(observation),
        })
    } else {
        json!({
            "observed_at": observation.get("last_seen").cloned().unwrap_or(Value::Null),
            "catalog_source_state": null,
            "catalog_provider": null,
        })
    };
    set_field_provenance(fields, field, &source, metadata);
}

fn automatic_provenance(
    fields: Map<String, Value>,
    prices: &Prices,
    observation: &Value,
) -> PriceProvenance {
    let source = effective_price_source(&fields, prices);
    let catalog_source_state = catalog_contributes(&fields).then(|| {
        observation
            .pointer("/catalog/source_state")
            .cloned()
            .unwrap_or(Value::Null)
    });
    let mut metadata = json!({ "fields": fields });
    if let Some(state) = catalog_source_state {
        metadata["catalog_source_state"] = state;
    }
    PriceProvenance { source, metadata }
}

/// Provenance for prices taken wholesale from an automatic observation.
pub(crate) fn automatic_price_provenance(prices: &Prices, observation: &Value) -> PriceProvenance {
    let mut fields = Map::new();
    for field in Prices::FIELDS {
        if prices.field(field).is_some() {
            set_automatic_field_provenance(&mut fields, field, observation);
        }
    }
    automatic_provenance(fields, prices, observation)
}

/// Provenance for prices an operator configured wholesale.
pub(crate) fn operator_price_provenance(prices: &Prices) -> PriceProvenance {
    let mut fields = Map::new();
    for field in Prices::FIELDS {
        if prices.field(field).is_some() {
            set_field_provenance(
                &mut fields,
                field,
                "operator",
                json!({ "configured_by": "admin" }),
            );
        }
    }
    PriceProvenance {
        source: effective_price_source(&fields, prices),
        metadata: json!({ "fields": fields }),
    }
}

/// An operator edit from `previous` to `next`: changed fields become operator
/// owned, cleared fields lose provenance, unchanged fields keep theirs.
pub(crate) fn operator_price_edit(
    discovery: &Value,
    previous: &Prices,
    next: &Prices,
    configured_by: &str,
) -> PriceProvenance {
    let mut fields = effective_price_fields(discovery, previous);
    for field in Prices::FIELDS {
        let value = next.field(field);
        if previous.field(field) == value {
            continue;
        }
        if value.is_some() {
            set_field_provenance(
                &mut fields,
                field,
                "operator",
                json!({ "configured_by": configured_by }),
            );
        } else {
            fields.remove(field);
        }
    }
    PriceProvenance::new(fields, next, "operator")
}

/// Pin the selected `prices.<field>` entries at their current values so later
/// automatic observations cannot replace them.
pub(crate) fn pin_price_fields(
    discovery: &Value,
    current: &Prices,
    selected: &[String],
) -> PriceProvenance {
    let mut fields = effective_price_fields(discovery, current);
    let pinned_at = crate::db::now_iso();
    for field in selected
        .iter()
        .filter_map(|field| field.strip_prefix("prices."))
    {
        let previous_source = field_source(&fields, field)
            .unwrap_or("untracked")
            .to_string();
        set_field_provenance(
            &mut fields,
            field,
            "operator_pin",
            json!({ "pinned_at": pinned_at, "previous_source": previous_source }),
        );
    }
    PriceProvenance::new(fields, current, "operator_pin")
}

/// Accept the selected `prices.<field>` entries from a reconciliation
/// observation, making them operator owned.
pub(crate) fn accept_price_fields(
    discovery: &Value,
    previous: &Prices,
    accepted: &Prices,
    observation: &Value,
    selected: &[String],
) -> PriceProvenance {
    let mut fields = effective_price_fields(discovery, previous);
    for field in selected
        .iter()
        .filter_map(|field| field.strip_prefix("prices."))
    {
        let accepted_from = observation
            .pointer(&format!("/price_sources/{field}"))
            .cloned()
            .unwrap_or(Value::Null);
        set_field_provenance(
            &mut fields,
            field,
            "operator_accept",
            json!({ "accepted_from": accepted_from }),
        );
    }
    PriceProvenance::new(fields, accepted, "operator_accept")
}

#[derive(Debug)]
pub(crate) struct AutomaticPriceMerge {
    pub prices: Prices,
    pub provenance: PriceProvenance,
    /// At least one observed field was withheld because an operator owns it.
    pub preserved_manual: bool,
}

/// Merge an automatic observation into the current effective prices.
/// Operator-owned fields never change; automatic fields take the observed
/// value, and an authoritative absence clears them.
///
/// `ownership` is the model's top-level discovery envelope while fresh values
/// live in `observation`, so reconciliation snapshots cannot hide operator
/// pins.
pub(crate) fn merge_automatic_price_observation(
    current: &Prices,
    observed: &Prices,
    observation: &Value,
    ownership: &Value,
) -> AutomaticPriceMerge {
    let mut prices = current.clone();
    let mut fields = effective_price_fields(ownership, current);
    let mut preserved_manual = false;
    for field in Prices::FIELDS {
        let observed_value = observed.field(field);
        let authoritative_absence = observed_value.is_none()
            && observation
                .pointer(&format!("/price_sources/{field}"))
                .is_some_and(Value::is_null);
        if observed_value.is_none() && !authoritative_absence {
            continue;
        }
        if price_field_operator_owned(&fields, current, field) {
            preserved_manual = true;
            continue;
        }
        prices.set_field(field, observed_value);
        if observed_value.is_some() {
            set_automatic_field_provenance(&mut fields, field, observation);
        } else {
            fields.remove(field);
        }
    }
    let provenance = automatic_provenance(fields, &prices, observation);
    AutomaticPriceMerge {
        prices,
        provenance,
        preserved_manual,
    }
}

// ---------------------------------------------------------------------------
// Integration pricing scope: external catalog prices are not authoritative.
// ---------------------------------------------------------------------------

/// Remove externally catalogued fields from imported effective prices. Fields
/// without per-field provenance fall back to the whole-row source. Returns the
/// remaining prices, their provenance, and the suppressed field names.
pub(crate) fn without_catalog_prices(
    prices: &Prices,
    provenance: &PriceProvenance,
) -> (Prices, PriceProvenance, Vec<String>) {
    let mut effective = prices.clone();
    let mut metadata = provenance.metadata.clone();
    let mut fields = metadata
        .get("fields")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut suppressed = Vec::new();
    for field in Prices::FIELDS {
        if effective.field(field).is_none() {
            continue;
        }
        let source = field_source(&fields, field).unwrap_or(&provenance.source);
        if is_external_catalog_price_source(source) {
            effective.set_field(field, None);
            fields.remove(field);
            suppressed.push(field.to_string());
        }
    }
    let catalog_still_contributes = catalog_contributes(&fields);
    metadata["fields"] = Value::Object(fields.clone());
    if !catalog_still_contributes {
        if let Some(metadata) = metadata.as_object_mut() {
            metadata.remove("catalog_source_state");
            metadata.remove("catalog_provider");
        }
    }
    let source = effective_price_source(&fields, &effective);
    (effective, PriceProvenance { source, metadata }, suppressed)
}

/// Whether persisted effective pricing still carries external catalog prices
/// that an integration pricing scope forbids.
pub(crate) fn effective_pricing_needs_scope_repair(effective: Option<&Map<String, Value>>) -> bool {
    let previous_source = effective
        .and_then(|value| value.get("source"))
        .and_then(Value::as_str)
        .unwrap_or("untracked");
    let has_external_catalog_field = effective
        .and_then(|value| value.get("fields"))
        .and_then(Value::as_object)
        .is_some_and(catalog_contributes);
    // A legacy whole-row catalog snapshot has no per-field provenance.
    has_external_catalog_field || is_external_catalog_price_source(previous_source)
}

/// Revoke external catalog prices from persisted effective pricing. A legacy
/// whole-row catalog snapshot clears every field.
pub(crate) fn revoke_catalog_pricing(
    prices: &Prices,
    effective: Option<&Map<String, Value>>,
) -> (Prices, PriceProvenance) {
    let mut prices = prices.clone();
    let previous_source = effective
        .and_then(|value| value.get("source"))
        .and_then(Value::as_str)
        .unwrap_or("untracked");
    let mut fields = effective
        .and_then(|value| value.get("fields"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let external_catalog_fields: Vec<String> = fields
        .keys()
        .filter(|field| field_source(&fields, field).is_some_and(is_external_catalog_price_source))
        .cloned()
        .collect();
    if external_catalog_fields.is_empty() && is_external_catalog_price_source(previous_source) {
        prices = Prices::default();
        fields.clear();
    } else {
        for field in external_catalog_fields {
            prices.set_field(&field, None);
            fields.remove(&field);
        }
    }

    let mut metadata = effective
        .and_then(|value| value.get("metadata"))
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    metadata["fields"] = Value::Object(fields.clone());
    if !catalog_contributes(&fields) {
        if let Some(object) = metadata.as_object_mut() {
            object.remove("catalog_source_state");
        }
    }
    let field_sources: std::collections::BTreeSet<&str> = fields
        .keys()
        .filter_map(|field| field_source(&fields, field))
        .collect();
    let source = match field_sources.len() {
        1 => field_sources
            .into_iter()
            .next()
            .unwrap_or("untracked")
            .to_string(),
        n if n > 1 => "mixed".to_string(),
        _ if prices.is_configured() && !is_external_catalog_price_source(previous_source) => {
            previous_source.to_string()
        }
        _ => "untracked".to_string(),
    };
    (prices, PriceProvenance { source, metadata })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_catalog_observation_preserves_effective_automatic_price() {
        let current = Prices {
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let observed = Prices {
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let discovery = json!({
            "price_sources": {
                "output_per_1m": "models.dev:provider"
            },
            "effective_pricing": {
                "source": "models.dev:provider",
                "fields": {
                    "output_per_1m": {"source": "models.dev:provider", "metadata": {}}
                }
            },
            "catalog": {
                "source_state": {
                    "source": "models.dev",
                    "retrieved_at": "2026-09-26T00:00:00Z",
                    "freshness": "stale"
                }
            }
        });

        let AutomaticPriceMerge {
            prices: effective,
            provenance,
            preserved_manual,
        } = merge_automatic_price_observation(&current, &observed, &discovery, &discovery);
        let fields = &provenance.metadata["fields"];
        assert_eq!(effective.output_per_1m, Some(5.0));
        assert!(!preserved_manual);
        assert_eq!(fields["output_per_1m"]["source"], "models.dev:provider");
    }

    #[test]
    fn pinned_price_field_survives_later_automatic_observation() {
        let current = Prices {
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let mut discovery = json!({
            "price_sources": {
                "output_per_1m": "models.dev:provider"
            },
            "effective_pricing": {
                "source": "models.dev:provider",
                "fields": {
                    "output_per_1m": {
                        "source": "models.dev:provider",
                        "metadata": {}
                    }
                }
            }
        });
        let selected = vec!["prices.output_per_1m".to_string()];
        let pinned = pin_price_fields(&discovery, &current, &selected).metadata["fields"].clone();
        discovery.as_object_mut().unwrap().insert(
            "effective_pricing".into(),
            json!({
                "source": "operator_pin",
                "fields": pinned
            }),
        );
        let observed = Prices {
            output_per_1m: Some(6.0),
            ..Default::default()
        };

        let AutomaticPriceMerge {
            prices: effective,
            provenance,
            preserved_manual,
        } = merge_automatic_price_observation(&current, &observed, &discovery, &discovery);
        let fields = &provenance.metadata["fields"];
        assert_eq!(effective.output_per_1m, Some(5.0));
        assert!(preserved_manual);
        assert_eq!(fields["output_per_1m"]["source"], "operator_pin");
    }

    #[test]
    fn mixed_price_ownership_preserves_manual_input_and_updates_automatic_output() {
        let current = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(4.0),
            ..Default::default()
        };
        let observed = Prices {
            input_per_1m: Some(1.5),
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let discovery = json!({
            "last_seen": "2026-09-27T00:00:00Z",
            "price_sources": {
                "input_per_1m": "models.dev:provider",
                "output_per_1m": "models.dev:provider"
            },
            "effective_pricing": {
                "source": "mixed",
                "fields": {
                    "input_per_1m": {"source": "operator", "metadata": {}},
                    "output_per_1m": {"source": "models.dev:provider", "metadata": {}}
                }
            },
            "catalog": {
                "source_state": {
                    "source": "models.dev",
                    "retrieved_at": "2026-09-27T00:00:00Z",
                    "freshness": "fresh"
                }
            }
        });
        let AutomaticPriceMerge {
            prices: effective,
            provenance,
            preserved_manual,
        } = merge_automatic_price_observation(&current, &observed, &discovery, &discovery);
        let fields = &provenance.metadata["fields"];
        assert_eq!(effective.input_per_1m, Some(1.0));
        assert_eq!(effective.output_per_1m, Some(5.0));
        assert!(preserved_manual);
        assert_eq!(fields["input_per_1m"]["source"], "operator");
        assert_eq!(fields["output_per_1m"]["source"], "models.dev:provider");
    }

    #[test]
    fn authoritative_price_absence_clears_automatic_field() {
        let current = Prices {
            output_per_1m: Some(5.0),
            ..Default::default()
        };
        let observed = Prices::default();
        let discovery = json!({
            "price_sources": {
                "output_per_1m": null
            },
            "effective_pricing": {
                "source": "models.dev:provider",
                "fields": {
                    "output_per_1m": {"source": "models.dev:provider", "metadata": {}}
                }
            },
            "catalog": {
                "source_state": {
                    "source": "models.dev",
                    "retrieved_at": "2026-09-27T01:00:00Z",
                    "freshness": "fresh"
                }
            }
        });

        assert!(has_automatic_price_observation(&observed, &discovery));
        let AutomaticPriceMerge {
            prices: effective,
            provenance,
            preserved_manual,
        } = merge_automatic_price_observation(&current, &observed, &discovery, &discovery);
        let fields = &provenance.metadata["fields"];

        assert_eq!(effective.output_per_1m, None);
        assert!(!preserved_manual);
        assert!(fields.get("output_per_1m").is_none());
    }

    #[test]
    fn authoritative_price_absence_preserves_operator_field() {
        let current = Prices {
            output_per_1m: Some(7.0),
            ..Default::default()
        };
        let observed = Prices::default();
        let discovery = json!({
            "price_sources": {
                "output_per_1m": null
            },
            "effective_pricing": {
                "source": "operator",
                "fields": {
                    "output_per_1m": {
                        "source": "operator",
                        "metadata": {"configured_by": "admin"}
                    }
                }
            }
        });

        let AutomaticPriceMerge {
            prices: effective,
            provenance,
            preserved_manual,
        } = merge_automatic_price_observation(&current, &observed, &discovery, &discovery);
        let fields = &provenance.metadata["fields"];

        assert_eq!(effective.output_per_1m, Some(7.0));
        assert!(preserved_manual);
        assert_eq!(fields["output_per_1m"]["source"], "operator");
    }

    fn prices(input: Option<f64>, output: Option<f64>) -> Prices {
        Prices {
            input_per_1m: input,
            output_per_1m: output,
            ..Default::default()
        }
    }

    #[test]
    fn observation_layers_apply_provider_over_plugin_over_catalog() {
        let mut layered = LayeredPrices::default();
        layered.layer(&prices(Some(1.0), Some(2.0)), "models.dev:canonical");
        layered.layer(&prices(Some(1.5), None), "models.dev:provider");
        layered.layer(&prices(None, Some(3.0)), PLUGIN_DOCUMENT);
        layered.layer(
            &discovery_prices(&json!({"prices": {"input_per_1m": -1.0, "output_per_1m": 4.0}})),
            PROVIDER_METADATA,
        );
        let (observed, sources) = layered.finish();
        assert_eq!(observed.input_per_1m, Some(1.5));
        assert_eq!(observed.output_per_1m, Some(4.0));
        assert_eq!(
            sources,
            json!({
                "input_per_1m": "models.dev:provider",
                "output_per_1m": "provider_metadata",
                "cached_per_1m": null,
                "cache_write_per_1m": null,
                "thinking_per_1m": null,
            })
        );
        assert_eq!(
            sources.as_object().unwrap().keys().collect::<Vec<_>>(),
            Prices::FIELDS.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn operator_edit_owns_changed_fields_and_keeps_unchanged_provenance() {
        let discovery = json!({
            "effective_pricing": {
                "source": "models.dev:provider",
                "fields": {
                    "input_per_1m": {"source": "models.dev:provider", "metadata": {}},
                    "output_per_1m": {"source": "models.dev:provider", "metadata": {}}
                }
            }
        });
        let previous = prices(Some(1.0), Some(2.0));
        let edited = operator_price_edit(
            &discovery,
            &previous,
            &prices(Some(1.0), Some(9.0)),
            "admin",
        );
        assert_eq!(edited.source, "mixed");
        assert_eq!(
            edited.metadata["fields"]["input_per_1m"]["source"],
            "models.dev:provider"
        );
        assert_eq!(
            edited.metadata["fields"]["output_per_1m"],
            json!({"source": "operator", "metadata": {"configured_by": "admin"}})
        );

        let cleared =
            operator_price_edit(&discovery, &previous, &Prices::default(), "config_import");
        assert_eq!(
            cleared,
            PriceProvenance {
                source: "operator".into(),
                metadata: json!({"fields": {}})
            }
        );
    }

    #[test]
    fn legacy_whole_row_source_seeds_unprovenanced_fields() {
        let discovery = json!({"effective_pricing": {"source": "operator", "metadata": {"configured_by": "admin"}}});
        let current = prices(Some(1.0), None);
        let pinned = pin_price_fields(&discovery, &current, &["prices.input_per_1m".into()]);
        assert_eq!(pinned.source, "operator_pin");
        assert_eq!(
            pinned.metadata["fields"]["input_per_1m"]["metadata"]["previous_source"],
            "operator"
        );
        let observed = prices(Some(2.0), Some(3.0));
        let merge = merge_automatic_price_observation(&current, &observed, &json!({}), &discovery);
        assert!(merge.preserved_manual);
        assert_eq!(merge.prices.input_per_1m, Some(1.0));
        assert_eq!(merge.prices.output_per_1m, Some(3.0));
        assert_eq!(
            merge.provenance.metadata["fields"]["output_per_1m"]["source"],
            "discovery"
        );
        assert_eq!(merge.provenance.source, "mixed");
    }

    #[test]
    fn accept_records_the_observed_source() {
        let observation = json!({"price_sources": {"output_per_1m": "models.dev:provider"}});
        let accepted = accept_price_fields(
            &json!({}),
            &Prices::default(),
            &prices(None, Some(4.0)),
            &observation,
            &["prices.output_per_1m".into()],
        );
        assert_eq!(accepted.source, "operator_accept");
        assert_eq!(
            accepted.metadata["fields"]["output_per_1m"],
            json!({"source": "operator_accept", "metadata": {"accepted_from": "models.dev:provider"}})
        );
    }

    #[test]
    fn integration_scope_drops_catalog_prices_from_observations_and_imports() {
        let observation = json!({"price_sources": {"input_per_1m": "provider_metadata", "output_per_1m": "models.dev:provider"}});
        let observed = prices(Some(1.0), Some(2.0));
        let scoped = automatic_prices_for_provider_scope(&observed, &observation, "integration");
        assert_eq!(
            (scoped.input_per_1m, scoped.output_per_1m),
            (Some(1.0), None)
        );
        let unscoped = automatic_prices_for_provider_scope(&observed, &observation, "provider");
        assert_eq!(unscoped.output_per_1m, Some(2.0));

        let imported = automatic_price_provenance(&observed, &observation);
        let (kept, provenance, suppressed) = without_catalog_prices(&observed, &imported);
        assert_eq!((kept.input_per_1m, kept.output_per_1m), (Some(1.0), None));
        assert_eq!(suppressed, vec!["output_per_1m".to_string()]);
        assert_eq!(provenance.source, "provider_metadata");
        assert!(provenance.metadata.get("catalog_source_state").is_none());
    }

    #[test]
    fn scope_repair_revokes_catalog_fields_and_legacy_catalog_snapshots() {
        let per_field = json!({
            "source": "mixed",
            "metadata": {"catalog_source_state": {"source": "models.dev"}},
            "fields": {
                "input_per_1m": {"source": "operator", "metadata": {}},
                "output_per_1m": {"source": "models.dev:provider", "metadata": {}}
            }
        });
        let per_field = per_field.as_object();
        assert!(effective_pricing_needs_scope_repair(per_field));
        let (repaired, provenance) =
            revoke_catalog_pricing(&prices(Some(1.0), Some(2.0)), per_field);
        assert_eq!(
            (repaired.input_per_1m, repaired.output_per_1m),
            (Some(1.0), None)
        );
        assert_eq!(provenance.source, "operator");
        assert!(provenance.metadata.get("catalog_source_state").is_none());

        let legacy = json!({"source": "models.dev:provider"});
        assert!(effective_pricing_needs_scope_repair(legacy.as_object()));
        let (repaired, provenance) =
            revoke_catalog_pricing(&prices(Some(1.0), Some(2.0)), legacy.as_object());
        assert!(!repaired.is_configured());
        assert_eq!(provenance.source, "untracked");

        let operator = json!({"source": "operator"});
        assert!(!effective_pricing_needs_scope_repair(operator.as_object()));
    }
}
