//! Field-disposition contract (see `docs/field-contract.md`).
//!
//! Every client-visible semantic field of every frontend declares, per
//! outbound transport, exactly one disposition in
//! `tests/fixtures/field-contract/*.json`:
//!
//! * `preserved`  - forwarded upstream unchanged at the same path
//! * `translated` - carried upstream in a different shape (`upstream` + `expect`)
//! * `consumed`   - handled inside Kinetix and deliberately not sent upstream
//! * `rejected`   - the request fails closed before dispatch
//!
//! The harness drives the real decode -> translation gate -> upstream body
//! path and compares the observed behavior against the declaration, so a
//! field can never be kept, dropped, or leaked by accident.

use std::collections::BTreeSet;
use std::path::Path;

use kinetix::adapters::{AdapterRegistry, TargetTransport, UpstreamContext};
use kinetix::db::{ModelRow, ProviderRow};
use kinetix::frontends::{self, FrontendFormat};
use kinetix::passthrough;
use kinetix::pipeline::build_upstream_body;
use serde::Deserialize;
use serde_json::{json, Value};

const UPSTREAM_MODEL: &str = "m-upstream";
const CLIENT_MODEL: &str = "m-client";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FieldDisposition {
    Preserved,
    Translated,
    Consumed,
    Rejected,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Declared {
    Short(FieldDisposition),
    Full {
        disposition: FieldDisposition,
        #[serde(default)]
        upstream: Option<String>,
        #[serde(default)]
        expect: Option<Value>,
        /// Why a consumed field is not sent upstream.
        #[serde(default)]
        #[allow(dead_code)]
        note: Option<String>,
    },
}

impl Declared {
    fn disposition(&self) -> FieldDisposition {
        match self {
            Declared::Short(d) => *d,
            Declared::Full { disposition, .. } => *disposition,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Entry {
    field: String,
    #[serde(default)]
    case: Option<String>,
    sample: Value,
    /// The sample has no scalar leaf that identifies it on the wire (for
    /// example a bare `"text"` type tag); skip leaf-based leak detection.
    #[serde(default)]
    untracked: bool,
    outbound: std::collections::BTreeMap<String, Declared>,
}

impl Entry {
    fn label(&self) -> String {
        match &self.case {
            Some(case) => format!("{}[{case}]", self.field),
            None => self.field.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Contract {
    frontend: String,
    base: Value,
    /// Top-level upstream keys an adapter always owns per transport
    /// (model selection, stream policy, ...), independent of any client field.
    adapter_owned: std::collections::BTreeMap<String, Vec<String>>,
    fields: Vec<Entry>,
    unknown_field: Entry,
}

const TRANSPORTS: [&str; 4] = ["openai", "openai-responses", "anthropic", "gemini"];

fn load(name: &str) -> Contract {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/field-contract")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path:?}: {e}"))
}

fn format_of(contract: &Contract) -> FrontendFormat {
    match contract.frontend.as_str() {
        "openai-chat" => FrontendFormat::OpenAi,
        "anthropic-messages" => FrontendFormat::Anthropic,
        "openai-responses" => FrontendFormat::OpenAiResponses,
        other => panic!("unknown frontend {other}"),
    }
}

fn decoded_fields(format: FrontendFormat) -> &'static [&'static str] {
    match format {
        FrontendFormat::OpenAi => frontends::openai::DECODED_FIELDS,
        FrontendFormat::Anthropic => frontends::anthropic::DECODED_FIELDS,
        FrontendFormat::OpenAiResponses => frontends::responses::DECODED_FIELDS,
    }
}

fn transport_of(name: &str) -> TargetTransport {
    TargetTransport::parse(name).unwrap_or_else(|| panic!("transport {name}"))
}

fn provider(wire: &str) -> ProviderRow {
    ProviderRow {
        id: "prov".into(),
        name: "p".into(),
        base_url: "https://api.example.com/v1".into(),
        wire_format: wire.into(),
        auth_scheme: "bearer".into(),
        custom_header_name: None,
        custom_param_name: None,
        extra_headers: "{}".into(),
        timeout_ms: 1000,
        capability_mode: "permissive".into(),
        models_path: None,
        rate_limit_rules: "{}".into(),
        enabled: 1,
        follow_redirects: 0,
        credential_hosts: String::new(),
        allow_insecure_tls: 0,
        created_at: "2026-01-01T00:00:00Z".into(),
        wire_plugin: String::new(),
        credential_plugin: String::new(),
        model_source_plugin: String::new(),
        credential_mode: "manual".into(),
        source_plugin_id: None,
        source_integration_id: None,
        pricing_scope: "direct_api".into(),
        integration_features: None,
        integration_protocols: None,
        connection_parameters: None,
        connection_parameters_attested: None,
    }
}

/// Scalar thinking mapping each transport's adapter understands, so the
/// canonical `thinking` level is observable on the wire.
fn thinking_map(transport: &str) -> Value {
    match transport {
        "openai" => json!({"levels": {"high": {"reasoning_effort": "high"}}}),
        "openai-responses" => json!({
            "mode": "level",
            "levels": {"high": "high"},
            "level_field": "reasoning.effort"
        }),
        "anthropic" => json!({
            "levels": {"high": 4096},
            "budget_field": "thinking.budget_tokens"
        }),
        "gemini" => json!({
            "mode": "level",
            "levels": {"high": "high"},
            "level_field": "thinkingConfig.thinkingLevel"
        }),
        other => panic!("transport {other}"),
    }
}

fn model(transport: &str) -> ModelRow {
    ModelRow {
        id: "m".into(),
        provider_id: "prov".into(),
        upstream_id: UPSTREAM_MODEL.into(),
        display_name: "Up".into(),
        enabled: 1,
        context_window: None,
        max_output_tokens: None,
        capabilities: "{}".into(),
        prices: "{}".into(),
        parameters: "{}".into(),
        thinking_map: thinking_map(transport).to_string(),
        extra_request: "{}".into(),
        discovery: "{}".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        opaque_state_plugin: String::new(),
    }
}

/// What the real outbound path did with a client body.
#[derive(Debug)]
enum Outcome {
    Rejected(String),
    Sent(Value),
}

/// Mirror of the pipeline's per-target body construction: decode, translation
/// gate, then the same `build_upstream_body` the dispatcher uses.
fn run(format: FrontendFormat, transport_name: &str, client_body: &Value) -> Outcome {
    let transport = transport_of(transport_name);
    let mut req = match frontends::decode(format, client_body.clone()) {
        Ok(req) => req,
        Err(error) => return Outcome::Rejected(error.message),
    };
    // The API boundary retains the exact received bytes for passthrough.
    req.raw_body = Some(client_body.to_string());
    let use_passthrough =
        req.raw_body.is_some() && passthrough::is_transport_passthrough(format, &transport);
    if !use_passthrough {
        if let Some(message) = frontends::translation_unsupported(&req.extra) {
            return Outcome::Rejected(message);
        }
    }
    let wire = match &transport {
        TargetTransport::OpenAiChat | TargetTransport::OpenAiResponses => "openai",
        other => other.as_str(),
    };
    let (provider, model) = (provider(wire), model(transport_name));
    let ctx = UpstreamContext {
        provider: &provider,
        model: &model,
        account_id: None,
        session_context: None,
        credential_metadata: None,
        credential: "k".into(),
    };
    let adapter = AdapterRegistry::new()
        .for_transport(&transport)
        .expect("built-in adapter");
    match build_upstream_body(adapter.as_ref(), &ctx, &req, use_passthrough) {
        Ok(body) => Outcome::Sent(body),
        Err(failure) => Outcome::Rejected(format!("{failure:?}")),
    }
}

fn request_for(contract: &Contract, entry: &Entry) -> Value {
    let mut body = contract.base.as_object().cloned().unwrap_or_default();
    body.insert(entry.field.clone(), entry.sample.clone());
    // `max_completion_tokens` and friends are alternates for a base field;
    // keep the sample isolated from its sibling.
    Value::Object(body)
}

/// Distinctive scalar leaves of a sample (strings and numbers; booleans and
/// nulls are too ambiguous to track through an adapter).
fn marker_leaves(sample: &Value, out: &mut Vec<Value>) {
    match sample {
        Value::String(_) | Value::Number(_) => out.push(sample.clone()),
        Value::Array(items) => items.iter().for_each(|v| marker_leaves(v, out)),
        Value::Object(map) => map.values().for_each(|v| marker_leaves(v, out)),
        _ => {}
    }
}

fn contains_leaf(body: &Value, leaf: &Value) -> bool {
    match body {
        Value::Object(map) => map.values().any(|v| contains_leaf(v, leaf)),
        Value::Array(items) => items.iter().any(|v| contains_leaf(v, leaf)),
        other => other == leaf,
    }
}

/// Leaves that identify only this field, not the shared base request.
fn field_markers(contract: &Contract, entry: &Entry) -> Vec<Value> {
    if entry.untracked {
        return Vec::new();
    }
    let mut base = Vec::new();
    if contract.base.get(&entry.field).is_none() {
        marker_leaves(&contract.base, &mut base);
    }
    let mut leaves = Vec::new();
    marker_leaves(&entry.sample, &mut leaves);
    leaves.retain(|leaf| !base.contains(leaf));
    leaves
}

fn upstream_top_level_keys(contract: &Contract, transport: &str) -> BTreeSet<String> {
    let mut keys: BTreeSet<String> = contract
        .adapter_owned
        .get(transport)
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    for entry in contract.fields.iter().chain([&contract.unknown_field]) {
        if let Some(declared) = entry.outbound.get(transport) {
            if !matches!(
                declared.disposition(),
                FieldDisposition::Preserved | FieldDisposition::Translated
            ) {
                continue;
            }
            let upstream = match declared {
                Declared::Full { upstream, .. } => upstream.clone(),
                Declared::Short(_) => None,
            };
            let pointer = upstream.unwrap_or_else(|| format!("/{}", entry.field));
            keys.insert(
                pointer
                    .trim_start_matches('/')
                    .split('/')
                    .next()
                    .unwrap()
                    .into(),
            );
        }
    }
    keys
}

fn check_entry(contract: &Contract, entry: &Entry, transport: &str) -> Vec<String> {
    let format = format_of(contract);
    let label = format!("{}:{}->{transport}", contract.frontend, entry.label());
    let Some(declared) = entry.outbound.get(transport) else {
        return vec![format!("{label}: no disposition declared")];
    };
    let outcome = run(format, transport, &request_for(contract, entry));
    let markers = field_markers(contract, entry);
    let mut problems = Vec::new();

    match (declared.disposition(), &outcome) {
        (FieldDisposition::Rejected, Outcome::Rejected(_)) => {}
        (FieldDisposition::Rejected, Outcome::Sent(body)) => {
            problems.push(format!(
                "{label}: declared rejected but request was sent: {body}"
            ));
        }
        (other, Outcome::Rejected(message)) => {
            problems.push(format!(
                "{label}: declared {other:?} but rejected: {message}"
            ));
        }
        (FieldDisposition::Consumed, Outcome::Sent(body)) => {
            for leaf in &markers {
                if contains_leaf(body, leaf) {
                    problems.push(format!(
                        "{label}: consumed field leaked {leaf} upstream: {body}"
                    ));
                }
            }
        }
        (
            disposition @ (FieldDisposition::Preserved | FieldDisposition::Translated),
            Outcome::Sent(body),
        ) => {
            let (upstream, expect) = match declared {
                Declared::Full {
                    upstream, expect, ..
                } => (upstream.clone(), expect.clone()),
                Declared::Short(_) => (None, None),
            };
            let pointer = upstream.unwrap_or_else(|| format!("/{}", entry.field));
            let expected = match (disposition, expect) {
                (_, Some(expect)) => expect,
                (FieldDisposition::Preserved, None) => entry.sample.clone(),
                _ => {
                    problems.push(format!("{label}: translated requires `expect`"));
                    return problems;
                }
            };
            if disposition == FieldDisposition::Preserved && pointer != format!("/{}", entry.field)
            {
                problems.push(format!(
                    "{label}: preserved must keep its path, got {pointer}"
                ));
            }
            match body.pointer(&pointer) {
                Some(actual) if *actual == expected => {}
                actual => problems.push(format!(
                    "{label}: expected {expected} at {pointer}, found {actual:?} in {body}"
                )),
            }
        }
    }

    if let Outcome::Sent(body) = &outcome {
        let allowed = upstream_top_level_keys(contract, transport);
        for key in body.as_object().expect("object body").keys() {
            if !allowed.contains(key) {
                problems.push(format!(
                    "{label}: adapter emitted undeclared top-level field '{key}': {body}"
                ));
            }
        }
        let serialized = body.to_string();
        if serialized.contains("__kinetix") {
            problems.push(format!(
                "{label}: internal Kinetix state serialized: {body}"
            ));
        }
        if declared.disposition() != FieldDisposition::Preserved
            && !matches!(entry.field.as_str(), "model" | "messages" | "input")
            && serialized.contains(CLIENT_MODEL)
        {
            problems.push(format!(
                "{label}: client model alias leaked upstream: {body}"
            ));
        }
    }
    problems
}

fn contract_files() -> [&'static str; 3] {
    [
        "openai-chat.json",
        "anthropic-messages.json",
        "openai-responses.json",
    ]
}

#[test]
fn every_decoded_field_has_a_disposition() {
    let mut problems = Vec::new();
    for file in contract_files() {
        let contract = load(file);
        let declared: BTreeSet<&str> = contract.fields.iter().map(|e| e.field.as_str()).collect();
        for field in decoded_fields(format_of(&contract)) {
            if !declared.contains(field) {
                problems.push(format!(
                    "{file}: decoded field '{field}' has no disposition"
                ));
            }
        }
        for entry in &contract.fields {
            for transport in TRANSPORTS {
                if !entry.outbound.contains_key(transport) {
                    problems.push(format!("{file}: {} lacks '{transport}'", entry.label()));
                }
            }
        }
        for transport in TRANSPORTS {
            if !contract.unknown_field.outbound.contains_key(transport) {
                problems.push(format!("{file}: unknown_field lacks '{transport}'"));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn observed_behavior_matches_declared_dispositions() {
    let mut problems = Vec::new();
    for file in contract_files() {
        let contract = load(file);
        for entry in contract.fields.iter().chain([&contract.unknown_field]) {
            for transport in TRANSPORTS {
                problems.extend(check_entry(&contract, entry, transport));
            }
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn carried_dispositions_declare_their_wire_shape() {
    for file in contract_files() {
        let contract = load(file);
        for entry in contract.fields.iter().chain([&contract.unknown_field]) {
            for (transport, declared) in &entry.outbound {
                assert!(
                    TRANSPORTS.contains(&transport.as_str()),
                    "{file}: {transport}"
                );
                let (upstream, expect) = match declared {
                    Declared::Full {
                        upstream, expect, ..
                    } => (upstream.is_some(), expect.is_some()),
                    Declared::Short(_) => (false, false),
                };
                match declared.disposition() {
                    FieldDisposition::Translated => assert!(
                        upstream && expect,
                        "{file}: {}->{transport}: translated needs `upstream` and `expect`",
                        entry.label()
                    ),
                    FieldDisposition::Consumed | FieldDisposition::Rejected => assert!(
                        !upstream && !expect,
                        "{file}: {}->{transport}: only carried fields have a wire shape",
                        entry.label()
                    ),
                    FieldDisposition::Preserved => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The harness must fail for each regression the contract exists to catch.
// ---------------------------------------------------------------------------

fn with_declared(file: &str, field: &str, transport: &str, declared: Value) -> Vec<String> {
    let mut contract = load(file);
    let index = contract
        .fields
        .iter()
        .position(|e| e.field == field && e.case.is_none())
        .expect("field");
    contract.fields[index]
        .outbound
        .insert(transport.into(), serde_json::from_value(declared).unwrap());
    check_entry(&contract, &contract.fields[index], transport)
}

#[test]
fn harness_detects_translated_field_that_disappears() {
    // Claim the top_p is translated into a place the adapter never writes.
    let problems = with_declared(
        "openai-chat.json",
        "top_p",
        "gemini",
        json!({"disposition": "translated", "upstream": "/generationConfig/missing", "expect": 0.8123}),
    );
    assert_eq!(problems.len(), 1, "{problems:?}");
}

#[test]
fn harness_detects_rejected_field_that_is_sent() {
    let problems = with_declared(
        "openai-chat.json",
        "temperature",
        "anthropic",
        json!("rejected"),
    );
    assert!(
        problems.iter().any(|p| p.contains("declared rejected")),
        "{problems:?}"
    );
}

#[test]
fn harness_detects_consumed_field_that_leaks() {
    let problems = with_declared(
        "openai-chat.json",
        "temperature",
        "anthropic",
        json!("consumed"),
    );
    assert!(
        problems.iter().any(|p| p.contains("leaked")),
        "{problems:?}"
    );
}

#[test]
fn harness_detects_field_that_is_silently_dropped() {
    let problems = with_declared("openai-chat.json", "seed", "anthropic", json!("preserved"));
    assert!(!problems.is_empty(), "{problems:?}");
}

#[test]
fn harness_detects_adapter_field_without_fixture_coverage() {
    // Forget the adapter-owned allowance: the keys the adapter emits on its
    // own (model/stream/...) are now undeclared and must fail.
    let mut contract = load("openai-chat.json");
    contract.adapter_owned.clear();
    let entry = contract
        .fields
        .iter()
        .find(|e| e.field == "temperature")
        .unwrap();
    let problems = check_entry(&contract, entry, "openai");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("undeclared top-level field")),
        "{problems:?}"
    );
}
