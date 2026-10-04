//! Thinking translation contract (see `docs/thinking-contract.md`).
//!
//! Both sides of the translation are pinned:
//!
//! ```text
//! client intent -> canonical intent -> exact provider body
//! ```
//!
//! * `tests/fixtures/thinking-translation/client-intents.json` pins what each
//!   frontend decodes into the canonical `ThinkingLevel`.
//! * `tests/fixtures/thinking-translation/<transport>.json` pins, per model
//!   capability, the canonical level a client intent decodes to and either the
//!   complete provider-facing JSON body or the fail-closed rejection.
//!
//! The harness drives the real path: decode, execution-profile resolution,
//! the thinking translation gate, then the same `build_upstream_body` the
//! dispatcher uses. A fixture change is therefore a deliberate compatibility
//! change, never a side effect.

use std::path::Path;

use kinetix::adapters::{
    resolve_execution_profile_for_target, AdapterRegistry, TargetTransport, UpstreamContext,
};
use kinetix::db::{ModelRow, ProviderRow};
use kinetix::frontends::{self, FrontendFormat};
use kinetix::passthrough;
use kinetix::pipeline::{build_upstream_body, check_resolved_thinking_translation};
use serde::Deserialize;
use serde_json::{json, Value};

const UPSTREAM_MODEL: &str = "m-upstream";
const CLIENT_MODEL: &str = "m-client";

fn fixture<T: for<'de> Deserialize<'de>>(name: &str) -> T {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/thinking-translation")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path:?}: {e}"))
}

fn format_of(frontend: &str) -> FrontendFormat {
    match frontend {
        "openai-chat" => FrontendFormat::OpenAi,
        "anthropic-messages" => FrontendFormat::Anthropic,
        "openai-responses" => FrontendFormat::OpenAiResponses,
        other => panic!("unknown frontend {other}"),
    }
}

/// Smallest valid streaming client request per frontend.
fn base_client_body(frontend: &str) -> Value {
    match frontend {
        "openai-chat" => json!({
            "model": CLIENT_MODEL,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }),
        "anthropic-messages" => json!({
            "model": CLIENT_MODEL,
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }),
        "openai-responses" => json!({
            "model": CLIENT_MODEL,
            "input": "hi",
            "stream": true
        }),
        other => panic!("unknown frontend {other}"),
    }
}

/// Merge a client intent into the base body. A `null` value removes the key.
fn client_body(frontend: &str, intent: &Value) -> Value {
    let mut body = base_client_body(frontend);
    for (key, value) in intent.as_object().expect("intent is an object") {
        if value.is_null() {
            body.as_object_mut().unwrap().remove(key);
        } else {
            body[key] = value.clone();
        }
    }
    body
}

fn level_key(req: &kinetix::types::InternalRequest) -> Option<&'static str> {
    req.thinking.map(|level| level.as_key())
}

// ---------------------------------------------------------------------------
// Side 1: client intent -> canonical intent
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct IntentCase {
    name: String,
    frontend: String,
    intent: Value,
    /// Canonical `ThinkingLevel` key; `null` when the client expressed no
    /// thinking intent.
    canonical: Option<String>,
}

#[test]
fn client_intents_decode_to_pinned_canonical_levels() {
    let cases: Vec<IntentCase> = fixture("client-intents.json");
    assert!(!cases.is_empty());
    let mut problems = Vec::new();
    for case in &cases {
        let body = client_body(&case.frontend, &case.intent);
        let req = frontends::decode(format_of(&case.frontend), body)
            .unwrap_or_else(|e| panic!("{}: decode failed: {}", case.name, e.message));
        if level_key(&req) != case.canonical.as_deref() {
            problems.push(format!(
                "{}: expected canonical {:?}, decoded {:?}",
                case.name,
                case.canonical,
                level_key(&req)
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn unsupported_client_effort_is_rejected_at_decode() {
    let body = client_body(
        "anthropic-messages",
        &json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "minimal"}}),
    );
    let error = frontends::decode(FrontendFormat::Anthropic, body).unwrap_err();
    assert!(
        error.message.contains("output_config.effort"),
        "{}",
        error.message
    );
}

// ---------------------------------------------------------------------------
// Side 2: canonical intent -> exact provider body
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Contract {
    transport: String,
    /// Model capability profiles keyed by name.
    models: std::collections::BTreeMap<String, ModelProfile>,
    cases: Vec<Case>,
}

/// One model capability: an operator `thinking_map`, or discovery metadata the
/// execution profile derives a map from.
#[derive(Debug, Deserialize)]
struct ModelProfile {
    #[serde(default)]
    thinking_map: Option<Value>,
    #[serde(default)]
    discovery: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    model: String,
    frontend: String,
    intent: Value,
    canonical: Option<String>,
    expect: Expect,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Expect {
    /// The complete provider-facing JSON body.
    Body(Value),
    /// Fail closed; the error message contains this text.
    Rejected(String),
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

fn model(transport: &str, profile: &ModelProfile) -> ModelRow {
    let mut discovery = profile.discovery.clone().unwrap_or_else(|| json!({}));
    discovery["configured_transport"] = json!(transport);
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
        thinking_map: profile
            .thinking_map
            .as_ref()
            .map_or_else(|| "{}".to_string(), Value::to_string),
        extra_request: "{}".into(),
        discovery: discovery.to_string(),
        created_at: "2026-01-01T00:00:00Z".into(),
        opaque_state_plugin: String::new(),
    }
}

#[derive(Debug)]
enum Outcome {
    Rejected(String),
    Sent(Value),
}

/// Mirror of the dispatcher's per-target body construction: decode, execution
/// profile, translation gates, `build_upstream_body`.
fn run(
    format: FrontendFormat,
    transport_name: &str,
    profile: &ModelProfile,
    client: &Value,
) -> (Option<&'static str>, Outcome) {
    let transport = TargetTransport::parse(transport_name)
        .unwrap_or_else(|| panic!("transport {transport_name}"));
    let mut req = frontends::decode(format, client.clone()).expect("client body decodes");
    let canonical = level_key(&req);
    // The API boundary retains the exact received bytes for passthrough.
    req.raw_body = Some(client.to_string());

    let wire = match &transport {
        TargetTransport::OpenAiChat | TargetTransport::OpenAiResponses => "openai",
        other => other.as_str(),
    };
    let provider = provider(wire);
    let model = model(transport_name, profile);
    let resolved = resolve_execution_profile_for_target(&provider, &model, None)
        .expect("execution profile resolves");
    assert_eq!(resolved.transport, transport);

    // The dispatcher builds the body from the model with the resolved map.
    let mut execution_model = model.clone();
    execution_model.thinking_map =
        serde_json::to_string(&resolved.thinking_map).expect("thinking map serializes");
    let ctx = UpstreamContext {
        provider: &provider,
        model: &execution_model,
        account_id: None,
        session_context: None,
        credential_metadata: None,
        credential: "k".into(),
    };
    let adapter = AdapterRegistry::new()
        .for_transport(&transport)
        .expect("built-in adapter");

    let use_passthrough =
        req.raw_body.is_some() && passthrough::is_transport_passthrough(format, &transport);
    if !use_passthrough {
        if let Err(error) = check_resolved_thinking_translation(
            adapter.as_ref(),
            &model.display_name,
            &resolved,
            &req,
        ) {
            return (canonical, Outcome::Rejected(error.message));
        }
    }
    let outcome = match build_upstream_body(adapter.as_ref(), &ctx, &req, use_passthrough) {
        Ok(body) => Outcome::Sent(body),
        Err(failure) => Outcome::Rejected(failure.message),
    };
    (canonical, outcome)
}

fn check_contract(file: &str) {
    let contract: Contract = fixture(file);
    assert!(!contract.cases.is_empty(), "{file}: no cases");
    let mut names = std::collections::BTreeSet::new();
    let mut problems = Vec::new();

    for case in &contract.cases {
        let label = format!("{file}:{}", case.name);
        assert!(names.insert(case.name.clone()), "{label}: duplicate case");
        let profile = contract
            .models
            .get(&case.model)
            .unwrap_or_else(|| panic!("{label}: unknown model profile {}", case.model));
        let client = client_body(&case.frontend, &case.intent);
        let (canonical, outcome) = run(
            format_of(&case.frontend),
            &contract.transport,
            profile,
            &client,
        );

        if canonical != case.canonical.as_deref() {
            problems.push(format!(
                "{label}: expected canonical {:?}, decoded {canonical:?}",
                case.canonical
            ));
        }
        match (&case.expect, outcome) {
            (Expect::Body(expected), Outcome::Sent(actual)) => {
                if *expected != actual {
                    problems.push(format!(
                        "{label}: provider body changed\n  expected: {expected}\n  actual:   {actual}"
                    ));
                }
            }
            (Expect::Body(expected), Outcome::Rejected(message)) => problems.push(format!(
                "{label}: expected body {expected} but rejected: {message}"
            )),
            (Expect::Rejected(fragment), Outcome::Rejected(message)) => {
                if !message.contains(fragment.as_str()) {
                    problems.push(format!(
                        "{label}: rejection {message:?} does not mention {fragment:?}"
                    ));
                }
            }
            (Expect::Rejected(fragment), Outcome::Sent(body)) => problems.push(format!(
                "{label}: expected rejection ({fragment}) but sent {body}"
            )),
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn openai_chat_thinking_bodies_are_pinned() {
    check_contract("openai-chat.json");
}

#[test]
fn openai_responses_thinking_bodies_are_pinned() {
    check_contract("openai-responses.json");
}

#[test]
fn anthropic_thinking_bodies_are_pinned() {
    check_contract("anthropic.json");
}

#[test]
fn gemini_thinking_bodies_are_pinned() {
    check_contract("gemini.json");
}
