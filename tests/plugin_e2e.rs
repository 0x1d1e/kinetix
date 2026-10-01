//! End-to-end test: install and run a real compiled plugin component.
//!
//! This exercises the full path that unit tests cannot: a genuine `.kxp`
//! package containing a WebAssembly component built by the external
//! `PrightCord/kinetix-plugins` repository, installed through the manager,
//! enabled (which instantiates it), and invoked through a capability. It proves
//! the WIT host boundary works against a real guest.
//!
//! Set `KINETIX_PLUGIN_E2E_PACKAGE` to a built `.kxp`. The test is skipped when
//! that package is unavailable so normal core CI stays hermetic and independent
//! from the plugin repository/toolchain.

use std::io::{Cursor, Read};
use std::sync::Arc;

use axum::{
    extract::State,
    http::{header::AUTHORIZATION, StatusCode},
    response::IntoResponse,
    routing::any,
    Json, Router,
};
use kinetix::plugins::{
    adapter::{register_declared_adapters, request_to_json, PluginAdapter},
    credential::PluginCredentialStrategy,
    Capability, HostPolicy, PluginManager,
};
use kinetix::{
    adapters::{Adapter, AdapterRegistry, UpstreamContext},
    app::AppState,
    config::Config,
    credentials::CredentialStrategy,
    crypto::{self, Crypto},
    db::{self, NewProvider, Pool},
    logqueue::UsageLogQueue,
    paths::Paths,
    registry::Registry,
    types::{
        AuthScheme, FailureKind, FinishReason, InternalRequest, Message, Part, ProxyError, Role,
        SamplingParams, StreamEvent, ThinkingLevel, UpstreamFailure, WireFormat,
    },
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// These tests exercise the host/guest boundary, not publisher trust; local
/// builds may be signed with a development key unknown to the test manager.
const ALLOW_UNTRUSTED_TEST_PACKAGE: bool = true;

/// Path to an externally built `.kxp` used for host/guest conformance.
fn package_path() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("KINETIX_PLUGIN_E2E_PACKAGE").map(std::path::PathBuf::from)?;
    path.is_file().then_some(path)
}

/// Build an unsigned package containing one of the checked-in ABI fixtures.
fn fixture_package(id: &str, name: &str, api_major: u8, component: &[u8]) -> Vec<u8> {
    fixture_package_with_adapter(id, name, api_major, "session-echo", false, component)
}

fn fixture_package_with_adapter(
    id: &str,
    name: &str,
    api_major: u8,
    adapter: &str,
    thinking_translation: bool,
    component: &[u8],
) -> Vec<u8> {
    let manifest = format!(
        "manifest_version = 1\nid = {id:?}\nname = {name:?}\nversion = \"0.1.0\"\nplugin_api = \"{api_major}\"\n\n[provides]\nprovider_adapters = [{adapter:?}]\nthinking_translation = {thinking_translation}\n"
    );
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in [
        ("plugin.toml", manifest.as_bytes()),
        ("plugin.wasm", component),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, data).unwrap();
    }
    builder.into_inner().unwrap()
}

/// Repackage an unsigned compatibility fixture under a distinct ID so API-v1
/// and API-v2 adapters can be installed in the same manager. Signature entries
/// are dropped because changing the manifest invalidates the package signature.
fn repackage_with_id(package: &[u8], id: &str) -> Vec<u8> {
    let mut archive = tar::Archive::new(Cursor::new(package));
    let mut builder = tar::Builder::new(Vec::new());
    let mut changed_manifest = false;

    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        if path == "signature.ed25519" {
            continue;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        if path == "plugin.toml" {
            let manifest = String::from_utf8(data).unwrap();
            let lines = manifest
                .lines()
                .map(|line| {
                    if !changed_manifest && line.starts_with("id = ") {
                        changed_manifest = true;
                        format!("id = {id:?}")
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            data = lines.into_bytes();
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, data.as_slice())
            .unwrap();
    }

    assert!(changed_manifest, "fixture manifest must contain an id");
    builder.into_inner().unwrap()
}

async fn manager() -> (PluginManager, Pool) {
    let dir = std::env::temp_dir().join(format!("kinetix-ag-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
    let pool = db::connect(&url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let crypto = Arc::new(Crypto::new(&[7u8; 32]));
    let manager = PluginManager::new(
        pool.clone(),
        crypto,
        HostPolicy::default(),
        dir.join("plugin-packages"),
    )
    .unwrap();
    (manager, pool)
}

#[derive(Clone, Default)]
struct AnthropicPluginMock(Arc<Mutex<Vec<Value>>>);

async fn anthropic_plugin_upstream(
    State(mock): State<AnthropicPluginMock>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    mock.0.lock().await.push(body);
    Json(json!({
        "schema": "kinetix.plugin.response",
        "schema_version": 2,
        "events": [
            {"type": "text_delta", "text": "plugin accepted the continuation"},
            {"type": "finish", "reason": "stop"}
        ]
    }))
}

struct AnthropicTranslationPluginAdapter;

impl Adapter for AnthropicTranslationPluginAdapter {
    fn wire_format(&self) -> &'static str {
        "anthropic-translation-plugin-fixture"
    }

    fn handles_thinking_translation(&self) -> bool {
        true
    }

    fn build_url(&self, ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
        Ok(ctx.provider.base_url.clone())
    }

    fn apply_auth(
        &self,
        ctx: &UpstreamContext<'_>,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, UpstreamFailure> {
        Ok(request.bearer_auth(&ctx.credential))
    }

    fn build_body(
        &self,
        _ctx: &UpstreamContext<'_>,
        request: &InternalRequest,
    ) -> Result<Value, UpstreamFailure> {
        serde_json::from_str(&request_to_json(request)).map_err(|error| UpstreamFailure {
            kind: FailureKind::PluginFailure,
            status: None,
            retry_after_secs: None,
            message: format!("invalid plugin request contract JSON: {error}"),
            quota_reset_at: None,
        })
    }

    fn classify_error(
        &self,
        status: u16,
        _body: &str,
        _headers: &reqwest::header::HeaderMap,
    ) -> UpstreamFailure {
        UpstreamFailure {
            kind: FailureKind::ServerError,
            status: Some(status),
            retry_after_secs: None,
            message: "synthetic plugin upstream error".into(),
            quota_reset_at: None,
        }
    }

    fn parse_stream_chunk(&self, _data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        Ok(Vec::new())
    }

    fn parse_full_response(&self, body: &Value) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        let text = body["events"]
            .as_array()
            .and_then(|events| {
                events.iter().find_map(|event| {
                    (event["type"] == "text_delta")
                        .then(|| event["text"].as_str())
                        .flatten()
                })
            })
            .ok_or_else(|| UpstreamFailure {
                kind: FailureKind::MalformedUpstream,
                status: None,
                retry_after_secs: None,
                message: "plugin response omitted text_delta".into(),
                quota_reset_at: None,
            })?;
        Ok(vec![
            StreamEvent::TextDelta(text.to_owned()),
            StreamEvent::Finish(FinishReason::Stop),
        ])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_messages_frontend_passes_thinking_and_tool_continuation_to_plugin() {
    const CLIENT_KEY: &str = "sk-kinetix-anthropic-plugin-test";
    const PLUGIN_ID: &str = "dev.kinetix.anthropic-echo-fixture";
    let mock = AnthropicPluginMock::default();
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_mock = mock.clone();
    let upstream_server = tokio::spawn(async move {
        axum::serve(
            upstream_listener,
            Router::new()
                .fallback(any(anthropic_plugin_upstream))
                .with_state(upstream_mock),
        )
        .await
        .unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-anthropic-plugin-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let paths = Paths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        state_dir: root.join("state"),
    };
    paths.ensure_dirs().unwrap();
    let database_url = paths.database_url();
    let pool = db::connect(&database_url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let crypto = Arc::new(Crypto::new(&[37u8; 32]));
    let base_url = format!("http://{upstream_addr}");
    let wire_plugin = format!("plugin:{PLUGIN_ID}/anthropic-echo");
    let provider_id = db::insert_provider(
        &pool,
        &NewProvider {
            name: "anthropic-plugin-fixture",
            base_url: &base_url,
            wire_format: WireFormat::Plugin,
            auth_scheme: AuthScheme::Bearer,
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: json!({}),
            timeout_ms: 2_000,
            capability_mode: "permissive",
            models_path: None,
            rate_limit_rules: json!({}),
            follow_redirects: false,
            credential_hosts: "",
            allow_insecure_tls: true,
            wire_plugin: &wire_plugin,
            credential_plugin: "",
            model_source_plugin: "",
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .unwrap();
    let secret = crypto.encrypt("plugin-test-key").unwrap();
    let _account_id = db::insert_account(
        &pool,
        &provider_id,
        "anthropic-plugin-account",
        &secret,
        "plugin-test-key",
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
            upstream_id: "fixture-anthropic-model",
            display_name: "Anthropic plugin fixture",
            enabled: true,
            context_window: Some(8_192),
            max_output_tokens: Some(1_024),
            capabilities: json!({"text": true, "reasoning": true, "tools": true}),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({}),
        },
    )
    .await
    .unwrap();
    let route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "anthropic-plugin-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "reject",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(1),
            max_concurrent_requests: Some(1),
        },
    )
    .await
    .unwrap();
    db::insert_route_target(&pool, &route_id, None, &model_id, 1, 1, "{}", "{}")
        .await
        .unwrap();
    db::insert_virtual_key(
        &pool,
        &db::VirtualKeyRow {
            id: "anthropic-plugin-test-key".into(),
            key_hash: crypto::hash_virtual_key(CLIENT_KEY),
            name: "Anthropic plugin test".into(),
            owner: "test".into(),
            tag: String::new(),
            allowed_models: json!(["*"]).to_string(),
            allowed_providers: json!([]).to_string(),
            rpm_limit: None,
            tpm_limit: None,
            max_concurrent_requests: None,
            daily_budget: None,
            monthly_budget: None,
            expires_at: None,
            status: "active".into(),
            allowed_ips: json!([]).to_string(),
            body_logging: 0,
            created_at: db::now_iso(),
            revoked_at: None,
        },
    )
    .await
    .unwrap();

    let registry = Arc::new(Registry::new());
    registry.reload(&pool).await.unwrap();
    let state = AppState::new(
        Arc::new(Config {
            bind: "127.0.0.1:0".into(),
            public_base_url: "http://127.0.0.1".into(),
            database_url,
            master_key: [37u8; 32],
            admin_token: "test-admin".into(),
            cf_access_aud: None,
            cf_access_team_domain: None,
            log_json: false,
            bootstrap_file: None,
            allow_private_upstreams: true,
            max_inflight_inferences: 4,
            allow_insecure_tls: true,
            data_dir: paths.data_dir.clone(),
            shutdown_grace_secs: 1,
            alert_webhook_url: None,
            alert_fallback_rate: 1.0,
            alert_error_rate: 1.0,
            alert_min_requests: 1,
            alert_interval_secs: 60,
            alert_p95_latency_ms: 1_000,
            ip_rate_limit_per_min: 0,
            session_ttl_minutes: 60,
            export_retention_days: 1,
            paths,
            generated_admin_password: None,
        }),
        pool.clone(),
        registry,
        crypto,
        reqwest::Client::new(),
        UsageLogQueue::new(pool, 16),
        0,
    );
    state.register_plugin_adapter(wire_plugin, Arc::new(AnthropicTranslationPluginAdapter));

    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = gateway_listener.local_addr().unwrap();
    let gateway = kinetix::router::build(state);
    let gateway_server = tokio::spawn(async move {
        axum::serve(
            kinetix::server::DisconnectAwareListener::new(gateway_listener),
            gateway.into_make_service_with_connect_info::<kinetix::server::ClientConnectionInfo>(),
        )
        .await
        .unwrap();
    });
    let request = json!({
        "model": "anthropic-plugin-route",
        "max_tokens": 128,
        "stream": false,
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "tools": [{
            "name": "lookup",
            "description": "Look up a city",
            "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
        }],
        "messages": [
            {"role": "user", "content": "continue the tool turn"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_prior", "name": "lookup", "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_prior", "content": "sunny"},
                {"type": "text", "text": "continue"}
            ]}
        ]
    });
    let response = reqwest::Client::new()
        .post(format!("http://{gateway_addr}/v1/messages"))
        .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
        .json(&request)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let response_body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{response_body}");
    let response_json: Value = serde_json::from_str(&response_body).unwrap();
    assert_eq!(
        response_json["content"],
        json!([{"type": "text", "text": "plugin accepted the continuation"}])
    );

    let mut nonportable_continuation = request.clone();
    nonportable_continuation["messages"][1]["content"] = json!([
        {
            "type": "thinking",
            "thinking": "historical private reasoning",
            "signature": "opaque-anthropic-signature"
        },
        {"type": "redacted_thinking", "data": "opaque-redacted-thinking"},
        {
            "type": "tool_use",
            "id": "toolu_prior",
            "name": "lookup",
            "input": {"city": "Paris"}
        }
    ]);
    let response = reqwest::Client::new()
        .post(format!("http://{gateway_addr}/v1/messages"))
        .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
        .json(&nonportable_continuation)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let response_body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response_body}");
    assert!(response_body.contains("non-portable provider continuation state"));
    assert_eq!(
        mock.0.lock().await.len(),
        1,
        "the reject policy must stop non-portable thinking before plugin egress"
    );

    let received = mock.0.lock().await;
    assert_eq!(received.len(), 1);
    let canonical = &received[0];
    assert_eq!(canonical["schema"], "kinetix.plugin.request");
    assert_eq!(canonical["thinking"], json!({"level": "low"}));
    assert_eq!(canonical["stream"], false);
    let assistant_parts = canonical["messages"][1]["parts"].as_array().unwrap();
    assert!(
        assistant_parts.contains(&json!({
            "type": "tool_call",
            "id": "toolu_prior",
            "name": "lookup",
            "arguments": "{\"city\":\"Paris\"}",
            "signature": null
        })),
        "canonical request: {canonical}"
    );
    let user_parts = canonical["messages"][2]["parts"].as_array().unwrap();
    assert!(user_parts.contains(&json!({
        "type": "tool_result",
        "tool_call_id": "toolu_prior",
        "name": "lookup",
        "content": "sunny",
        "is_error": false
    })));
    assert_eq!(canonical["tools"][0]["name"], "lookup");

    gateway_server.abort();
    upstream_server.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn installs_enables_and_instantiates_a_real_component() {
    let Some(path) = package_path() else {
        eprintln!(
            "skipping: set KINETIX_PLUGIN_E2E_PACKAGE to a built .kxp from PrightCord/kinetix-plugins"
        );
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let (m, _pool) = manager().await;

    let outcome = m
        .install(&bytes, None, &[], ALLOW_UNTRUSTED_TEST_PACKAGE)
        .await
        .unwrap();
    assert_eq!(outcome.id, "dev.kinetix.antigravity-oauth");
    assert!(
        outcome
            .provides
            .iter()
            .any(|p| p.capability == Capability::CredentialStrategy),
        "plugin should provide a credential strategy"
    );

    // Enable instantiates the component: this is the real proof the guest links
    // against the host's WIT world.
    m.approve_permissions("dev.kinetix.antigravity-oauth")
        .await
        .unwrap();
    m.enable("dev.kinetix.antigravity-oauth")
        .await
        .expect("a real component should enable against the host world");

    // The binding resolves once enabled (fail-closed before that, §6.0).
    let resolved = m
        .resolve_binding(
            "plugin:dev.kinetix.antigravity-oauth/antigravity-oauth",
            Capability::CredentialStrategy,
        )
        .await;
    assert_eq!(resolved.as_deref(), Some("dev.kinetix.antigravity-oauth"));

    assert!(
        outcome
            .provides
            .iter()
            .any(|p| p.capability == Capability::AuthFlow),
        "plugin should provide an account auth flow"
    );
    let authorize_url = m
        .auth_begin(
            "dev.kinetix.antigravity-oauth",
            "antigravity",
            "http://127.0.0.1:8080/admin/api/plugins/auth/callback",
            "state-123",
            Some("challenge-123"),
        )
        .await
        .expect("plugin-auth world should bind and build an authorization URL");
    assert!(authorize_url.starts_with("https://accounts.google.com/"));
    assert!(authorize_url.contains("state=state-123"));
    assert!(authorize_url.contains("code_challenge=challenge-123"));
}

#[tokio::test]
async fn invokes_a_real_guest_capability_through_the_host_boundary() {
    let Some(path) = package_path() else {
        eprintln!(
            "skipping: set KINETIX_PLUGIN_E2E_PACKAGE to a built .kxp from PrightCord/kinetix-plugins"
        );
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let (m, _pool) = manager().await;
    m.install(&bytes, None, &[], ALLOW_UNTRUSTED_TEST_PACKAGE)
        .await
        .unwrap();
    m.approve_permissions("dev.kinetix.antigravity-oauth")
        .await
        .unwrap();
    m.enable("dev.kinetix.antigravity-oauth").await.unwrap();

    // Invoke the real `credential-strategy.resolve` export. With no matching
    // account the guest calls `host-credential.read`, which fails, and the guest
    // returns a *structured* PluginError (not a trap): this proves the guest ran
    // and the host WIT boundary carried typed results both ways.
    let result = m
        .credential_resolve(
            "dev.kinetix.antigravity-oauth",
            "antigravity",
            "acc_missing",
            "missing",
        )
        .await;
    match result {
        Err(fault) => {
            assert_eq!(fault.code(), "credential_expired", "got {fault:?}");
            // A structured plugin error must not be counted as a runtime fault.
            assert!(!fault.counts_against_circuit());
        }
        Ok(lease) => panic!("unexpectedly resolved a lease: {lease:?}"),
    }
}

#[tokio::test]
async fn invokes_real_guest_credential_rotation_through_host_boundary() {
    let Some(path) = package_path() else {
        eprintln!(
            "skipping: set KINETIX_PLUGIN_E2E_PACKAGE to a built .kxp from PrightCord/kinetix-plugins"
        );
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let (m, _pool) = manager().await;
    m.install(&bytes, None, &[], ALLOW_UNTRUSTED_TEST_PACKAGE)
        .await
        .unwrap();
    m.approve_permissions("dev.kinetix.antigravity-oauth")
        .await
        .unwrap();
    m.enable("dev.kinetix.antigravity-oauth").await.unwrap();

    let result = m
        .credential_rotate(
            "dev.kinetix.antigravity-oauth",
            "antigravity",
            "acc_missing",
        )
        .await;
    match result {
        Err(fault) => assert_eq!(fault.code(), "credential_expired", "got {fault:?}"),
        Ok(()) => panic!("unexpectedly rotated a missing account"),
    }
}

#[tokio::test]
async fn a_real_guest_health_probe_is_declared_resolvable_and_invocable() {
    let Some(path) = package_path() else {
        eprintln!(
            "skipping: set KINETIX_PLUGIN_E2E_PACKAGE to a built .kxp from PrightCord/kinetix-plugins"
        );
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let (m, _pool) = manager().await;
    let id = "dev.kinetix.antigravity-oauth";
    let outcome = m
        .install(&bytes, None, &[], ALLOW_UNTRUSTED_TEST_PACKAGE)
        .await
        .unwrap();
    assert!(outcome.provides.iter().any(|provided| {
        provided.capability == Capability::HealthProbe && provided.name == "antigravity-oauth"
    }));
    m.approve_permissions(id).await.unwrap();
    m.enable(id).await.unwrap();
    assert!(m.is_usable(id).await);

    let provider_id = db::insert_provider(
        &_pool,
        &db::NewProvider {
            name: "Antigravity health probe test",
            base_url: "https://daily-cloudcode-pa.googleapis.com",
            wire_format: WireFormat::Plugin,
            auth_scheme: kinetix::types::AuthScheme::Bearer,
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: serde_json::json!({}),
            timeout_ms: 30_000,
            capability_mode: "permissive",
            models_path: None,
            rate_limit_rules: serde_json::json!({}),
            follow_redirects: false,
            credential_hosts: "",
            allow_insecure_tls: false,
            wire_plugin: "",
            credential_plugin: "plugin:dev.kinetix.antigravity-oauth/antigravity-oauth",
            model_source_plugin: "",
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .unwrap();
    let secret = Crypto::new(&[7u8; 32])
        .encrypt(r#"{"access_token":"fixture-token","expiry":"2999-01-01T00:00:00Z"}"#)
        .unwrap();
    let account_id = db::insert_account(
        &_pool,
        &provider_id,
        "health-probe-fixture",
        &secret,
        "fixture",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();

    let reference = format!("plugin:{id}/antigravity-oauth");
    assert_eq!(
        m.resolve_binding(&reference, Capability::HealthProbe)
            .await
            .as_deref(),
        Some(id),
        "the installed .kxp manifest must resolve the health-probe capability"
    );

    // With a valid token but no cached project id, the real guest returns an
    // empty structured observation without onboarding or making network calls.
    // `Some([])` proves the manager invoked and decoded health-probe-v2 rather
    // than falling back to the legacy projection.
    let result = m
        .health_probe_with_snapshots(id, &provider_id, &account_id)
        .await
        .unwrap();
    assert_eq!(result.observation.state, "unknown");
    let snapshots = result.quota_snapshots.expect("v2 snapshots are present");
    assert!(snapshots.is_empty());
}

/// The second world (`plugin-adapter`, §6.3) is bound from the same component
/// and translates the Antigravity `v1internal` wire format end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adapter_world_translates_the_antigravity_wire_format() {
    let Some(path) = package_path() else {
        eprintln!(
            "skipping: set KINETIX_PLUGIN_E2E_PACKAGE to a built .kxp from PrightCord/kinetix-plugins"
        );
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let (m, _pool) = manager().await;
    m.install(&bytes, None, &[], ALLOW_UNTRUSTED_TEST_PACKAGE)
        .await
        .unwrap();
    m.approve_permissions("dev.kinetix.antigravity-oauth")
        .await
        .unwrap();
    m.enable("dev.kinetix.antigravity-oauth").await.unwrap();
    let id = "dev.kinetix.antigravity-oauth";

    let wf = m
        .adapter_wire_format(id)
        .await
        .expect("adapter world binds");
    assert_eq!(wf, "antigravity");

    let provider = r#"{"base_url":"https://daily-cloudcode-pa.googleapis.com","extra_headers":"{\"x-antigravity-project\":\"test-project\"}","_kinetix":{"account_id":"test-account","project_id":"account-project","now_unix_millis":1700000000123}}"#;
    let model = r#"{"upstream_id":"gemini-3-flash"}"#;

    let url = m.adapter_build_url(id, provider, model).await.unwrap();
    assert!(
        url.ends_with("/v1internal:streamGenerateContent?alt=sse"),
        "got {url}"
    );

    let host_session = "stable-session-for-e2e";
    let headers = m
        .adapter_apply_auth(id, provider, "tok123", Some(host_session))
        .await
        .unwrap();
    assert!(headers.contains("Bearer tok123"), "got {headers}");
    assert!(headers.contains("antigravity/ide/"));

    let request = r#"{"requested_model":"gemini-3-flash","system":["be nice"],"messages":[{"role":"user","parts":[{"type":"text","text":"hi"}]}],"tools":[{"name":"my-tool!","description":"d","parameters":{"type":"object"}}],"thinking":{"level":"high"},"stream":true}"#;
    let body = m
        .adapter_build_body(id, request, provider, model, Some(host_session))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["model"], "gemini-3-flash");
    assert_eq!(v["project"], "test-project");
    assert_eq!(v["userAgent"], "antigravity");
    assert_eq!(v["request"]["contents"][0]["parts"][0]["text"], "hi");
    assert_eq!(v["request"]["contents"][0]["role"], "user");
    assert_eq!(
        v["request"]["systemInstruction"]["parts"][0]["text"],
        "be nice"
    );
    // Function names are sanitized to the Gemini rule.
    assert_eq!(
        v["request"]["tools"][0]["functionDeclarations"][0]["name"],
        "my-tool_"
    );
    assert_eq!(
        v["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
    let native_session = v["request"]["sessionId"].as_str().unwrap();
    assert_ne!(native_session, host_session);
    let repeated_body = m
        .adapter_build_body(id, request, provider, model, Some(host_session))
        .await
        .unwrap();
    let repeated: serde_json::Value = serde_json::from_str(&repeated_body).unwrap();
    assert_eq!(repeated["request"]["sessionId"], native_session);

    // The actual installed manifest drives the exact production adapter
    // registration path. Missing the Antigravity opt-in must fail this before
    // any request reaches the guest.
    let row = m.get(id).await.unwrap().unwrap();
    let manifest = row.manifest().expect("installed manifest should parse");
    assert!(
        manifest.provides.thinking_translation,
        "Antigravity must explicitly opt in to canonical thinking translation"
    );
    let registry = AdapterRegistry::new();
    register_declared_adapters(&registry, m.clone(), id, &manifest.provides)
        .await
        .expect("real Antigravity adapter should register");
    let provider_row = db::ProviderRow {
        id: "provider_antigravity".into(),
        name: "Antigravity".into(),
        base_url: "https://daily-cloudcode-pa.googleapis.com".into(),
        wire_format: "plugin".into(),
        auth_scheme: "bearer".into(),
        custom_header_name: None,
        custom_param_name: None,
        extra_headers: serde_json::json!({
            "x-antigravity-project": "test-project"
        })
        .to_string(),
        timeout_ms: 120_000,
        capability_mode: "permissive".into(),
        models_path: None,
        rate_limit_rules: "{}".into(),
        enabled: 1,
        follow_redirects: 0,
        credential_hosts: String::new(),
        allow_insecure_tls: 0,
        created_at: "2026-01-01T00:00:00Z".into(),
        wire_plugin: format!("plugin:{id}/antigravity"),
        credential_plugin: String::new(),
        model_source_plugin: String::new(),
        credential_mode: "auth_flow".into(),
        source_plugin_id: Some(id.into()),
        source_integration_id: Some("antigravity".into()),
        pricing_scope: "integration".into(),
        integration_features: None,
        integration_protocols: None,
        connection_parameters: None,
        connection_parameters_attested: None,
    };
    assert_eq!(provider_row.wire(), WireFormat::Plugin);
    let registered = registry.for_provider(&provider_row);
    assert!(
        registered.handles_thinking_translation(),
        "manifest opt-in must survive real adapter registration"
    );

    let model_row = db::ModelRow {
        id: "model_antigravity".into(),
        provider_id: provider_row.id.clone(),
        upstream_id: "gemini-3-flash".into(),
        display_name: "Gemini 3 Flash".into(),
        enabled: 1,
        context_window: None,
        max_output_tokens: None,
        capabilities: "{}".into(),
        prices: "{}".into(),
        parameters: "{}".into(),
        thinking_map: "{}".into(),
        extra_request: "{}".into(),
        discovery: "{}".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        opaque_state_plugin: String::new(),
    };
    let canonical = InternalRequest {
        requested_model: "gemini-3-flash".into(),
        system: vec![],
        messages: vec![Message {
            role: Role::User,
            parts: vec![Part::Text("hi".into())],
        }],
        tools: vec![],
        tool_choice: None,
        tool_choice_name: None,
        params: SamplingParams::default(),
        stream: true,
        include_usage: false,
        thinking: Some(ThinkingLevel::High),
        extra: Default::default(),
        raw_body: None,
    };
    let ctx = UpstreamContext {
        provider: &provider_row,
        model: &model_row,
        account_id: Some("account_test"),
        session_context: Some(host_session),
        credential_metadata: None,
        credential: "tok123".into(),
    };
    let registered_body = registered
        .build_body(&ctx, &canonical)
        .expect("registered Antigravity adapter should translate canonical thinking");
    assert_eq!(
        registered_body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
    let registered_session = registered_body["request"]["sessionId"]
        .as_str()
        .expect("session-aware adapter should produce a native session ID");
    assert_ne!(registered_session, host_session);
    let repeated_registered_body = registered
        .build_body(&ctx, &canonical)
        .expect("registered Antigravity adapter should build repeated requests");
    assert_eq!(
        repeated_registered_body["request"]["sessionId"],
        registered_session
    );

    // A real Antigravity SSE chunk parses to canonical events.
    let chunk = r#"{"response":{"responseId":"resp_1","candidates":[{"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2}}}"#;
    let events = m.adapter_parse_stream_chunk(id, chunk).await.unwrap();
    let ev: serde_json::Value = serde_json::from_str(&events).unwrap();
    assert_eq!(ev["schema"], "kinetix.plugin.response");
    let arr = ev["events"].as_array().unwrap();
    assert!(arr
        .iter()
        .any(|e| e["type"] == "text_delta" && e["text"] == "hello"));
    assert!(arr
        .iter()
        .any(|e| e["type"] == "finish" && e["reason"] == "stop"));
    assert!(arr
        .iter()
        .any(|e| e["type"] == "usage" && e["input"] == 5 && e["output"] == 2));
    assert!(arr
        .iter()
        .any(|e| e["type"] == "start" && e["upstream_request_id"] == "resp_1"));
}

/// A released API-v1 component and a session-aware API-v2 component can both
/// load and execute through the same host manager. The legacy adapter retains
/// its original call shape; only API v2 receives the session identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_v1_and_session_aware_api_v2_adapters_load_together() {
    let Some(api2_path) = std::env::var_os("KINETIX_PLUGIN_API_V2_E2E_PACKAGE")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_file())
    else {
        eprintln!("skipping: set KINETIX_PLUGIN_API_V2_E2E_PACKAGE to an API-v2 .kxp");
        return;
    };
    let Some(api1_path) = std::env::var_os("KINETIX_PLUGIN_API_V1_E2E_PACKAGE")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_file())
    else {
        eprintln!("skipping: set KINETIX_PLUGIN_API_V1_E2E_PACKAGE to a released API-v1 .kxp");
        return;
    };

    let api1_id = "dev.kinetix.antigravity-oauth.v1-compat";
    let api1 = repackage_with_id(&std::fs::read(api1_path).unwrap(), api1_id);
    let api2 = std::fs::read(api2_path).unwrap();
    let (m, _pool) = manager().await;

    assert_eq!(
        m.install(&api1, None, &[], false).await.unwrap().id,
        api1_id
    );
    let api2_id = m.install(&api2, None, &[], false).await.unwrap().id;
    assert_ne!(api1_id, api2_id);
    assert_eq!(
        m.get(api1_id)
            .await
            .unwrap()
            .unwrap()
            .manifest()
            .unwrap()
            .api_major(),
        Some(1)
    );
    assert_eq!(
        m.get(&api2_id)
            .await
            .unwrap()
            .unwrap()
            .manifest()
            .unwrap()
            .api_major(),
        Some(2)
    );

    for id in [api1_id, api2_id.as_str()] {
        m.approve_permissions(id).await.unwrap();
        m.enable(id).await.unwrap();
        assert_eq!(m.adapter_wire_format(id).await.unwrap(), "antigravity");
    }

    let provider = r#"{"base_url":"https://daily-cloudcode-pa.googleapis.com","extra_headers":"{\"x-antigravity-project\":\"compat-test\"}"}"#;
    let model = r#"{"upstream_id":"gemini-3-flash"}"#;
    let request = r#"{"requested_model":"gemini-3-flash","system":[],"messages":[{"role":"user","parts":[{"type":"text","text":"hello"}]}],"stream":true}"#;
    let host_session = "compat-session-identity";

    let legacy_headers = m
        .adapter_apply_auth(api1_id, provider, "legacy-token", Some(host_session))
        .await
        .unwrap();
    assert!(legacy_headers.contains("Bearer legacy-token"));
    let legacy_body = m
        .adapter_build_body(api1_id, request, provider, model, Some(host_session))
        .await
        .unwrap();
    let legacy_body: serde_json::Value = serde_json::from_str(&legacy_body).unwrap();
    let legacy_session = legacy_body["request"]["sessionId"].as_str().unwrap();
    assert_ne!(legacy_session, host_session);
    let legacy_without_context = m
        .adapter_build_body(api1_id, request, provider, model, None)
        .await
        .unwrap();
    let legacy_without_context: serde_json::Value =
        serde_json::from_str(&legacy_without_context).unwrap();
    assert_eq!(
        legacy_without_context["request"]["sessionId"],
        legacy_session
    );

    let current_body = m
        .adapter_build_body(&api2_id, request, provider, model, Some(host_session))
        .await
        .unwrap();
    let current_body: serde_json::Value = serde_json::from_str(&current_body).unwrap();
    let native_session = current_body["request"]["sessionId"].as_str().unwrap();
    assert_ne!(native_session, host_session);
}

/// API-v1 and API-v2 fixtures always exercise cross-version runtime behavior,
/// independent of optional externally built release packages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_v1_and_api_v2_legacy_host_imports_coexist_with_opaque_session_context() {
    const API1_ID: &str = "dev.kinetix.test.api1-adapter";
    const API2_ID: &str = "dev.kinetix.test.api2-session-echo";
    const API2_FALLBACK_ID: &str = "dev.kinetix.test.api2-fallback-session-echo";
    const RAW_SESSION: &str = "customer@example.com";
    const SECOND_RAW_SESSION: &str = "internal-ticket-123";

    let api1 = fixture_package(
        API1_ID,
        "API v1 adapter fixture",
        1,
        include_bytes!("fixtures/plugin-api-v1-adapter-fixture.component.wasm"),
    );
    let api2_component = include_bytes!("fixtures/plugin-api-v2-session-echo.component.wasm");
    let api2 = fixture_package(API2_ID, "API v2 session echo fixture", 2, api2_component);
    let api2_fallback = fixture_package(
        API2_FALLBACK_ID,
        "API v2 fallback session echo fixture",
        2,
        api2_component,
    );
    let (manager, pool) = manager().await;
    assert_eq!(
        manager.install(&api1, None, &[], false).await.unwrap().id,
        API1_ID
    );
    assert_eq!(
        manager.install(&api2, None, &[], false).await.unwrap().id,
        API2_ID
    );
    assert_eq!(
        manager
            .install(&api2_fallback, None, &[], false)
            .await
            .unwrap()
            .id,
        API2_FALLBACK_ID
    );

    for id in [API1_ID, API2_ID, API2_FALLBACK_ID] {
        manager.approve_permissions(id).await.unwrap();
        manager.enable(id).await.unwrap();
    }
    // Seed host storage so the v2 fixture must successfully call the imported
    // host-storage interface during build-body, not merely instantiate it.
    kinetix::plugins::store::kv_put(
        &pool,
        &Crypto::new(&[7u8; 32]),
        API2_ID,
        "_config:login_hint",
        b"legacy-storage-read",
    )
    .await
    .unwrap();
    kinetix::plugins::store::kv_put(
        &pool,
        &Crypto::new(&[7u8; 32]),
        API2_ID,
        "project:account_fixture",
        b"legacy-project-read",
    )
    .await
    .unwrap();

    // API-v1 executes through its unchanged session-unaware exports. The
    // host-side context must not alter what the API-v1 guest receives.
    let provider_json = r#"{"base_url":"https://fixture.invalid"}"#;
    let model_json = r#"{"upstream_id":"fixture-model"}"#;
    let request_json = r#"{"requested_model":"fixture-model","messages":[],"stream":false}"#;
    let api1_with_context = manager
        .adapter_build_body(
            API1_ID,
            request_json,
            provider_json,
            model_json,
            Some(RAW_SESSION),
        )
        .await
        .unwrap();
    let api1_without_context = manager
        .adapter_build_body(API1_ID, request_json, provider_json, model_json, None)
        .await
        .unwrap();
    assert_eq!(api1_with_context, api1_without_context);
    let api1_auth_with_context = manager
        .adapter_apply_auth(
            API1_ID,
            provider_json,
            "fixture-credential",
            Some(RAW_SESSION),
        )
        .await
        .unwrap();
    let api1_auth_without_context = manager
        .adapter_apply_auth(API1_ID, provider_json, "fixture-credential", None)
        .await
        .unwrap();
    assert_eq!(api1_auth_with_context, api1_auth_without_context);

    // Exercise API-v2 through the same Adapter boundary used by the pipeline.
    let adapter = PluginAdapter::new(manager.clone(), API2_ID.into(), false)
        .await
        .unwrap();
    let provider = db::ProviderRow {
        id: "provider_fixture".into(),
        name: "Fixture".into(),
        base_url: "https://fixture.invalid".into(),
        wire_format: "plugin".into(),
        auth_scheme: "bearer".into(),
        custom_header_name: None,
        custom_param_name: None,
        extra_headers: "{}".into(),
        timeout_ms: 30_000,
        capability_mode: "permissive".into(),
        models_path: None,
        rate_limit_rules: "{}".into(),
        enabled: 1,
        follow_redirects: 0,
        credential_hosts: String::new(),
        allow_insecure_tls: 0,
        created_at: "2026-01-01T00:00:00Z".into(),
        wire_plugin: format!("plugin:{API2_ID}/session-echo"),
        credential_plugin: String::new(),
        model_source_plugin: String::new(),
        credential_mode: "static".into(),
        source_plugin_id: Some(API2_ID.into()),
        source_integration_id: Some("session-echo".into()),
        pricing_scope: "integration".into(),
        integration_features: None,
        integration_protocols: None,
        connection_parameters: None,
        connection_parameters_attested: None,
    };
    let model = db::ModelRow {
        id: "model_fixture".into(),
        provider_id: provider.id.clone(),
        upstream_id: "fixture-model".into(),
        display_name: "Fixture model".into(),
        enabled: 1,
        context_window: None,
        max_output_tokens: None,
        capabilities: "{}".into(),
        prices: "{}".into(),
        parameters: "{}".into(),
        thinking_map: "{}".into(),
        extra_request: "{}".into(),
        discovery: "{}".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        opaque_state_plugin: String::new(),
    };
    let request = InternalRequest {
        requested_model: "fixture-model".into(),
        system: Vec::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        tool_choice: None,
        tool_choice_name: None,
        params: SamplingParams::default(),
        stream: false,
        include_usage: false,
        thinking: None,
        extra: Default::default(),
        raw_body: None,
    };
    let crypto = Crypto::new(&[7u8; 32]);
    let expected_identity = crypto.opaque_plugin_session_identity(RAW_SESSION);
    let context = UpstreamContext {
        provider: &provider,
        model: &model,
        account_id: Some("account_fixture"),
        session_context: Some(RAW_SESSION),
        credential_metadata: None,
        credential: "fixture-credential".into(),
    };
    let first = adapter.build_body(&context, &request).unwrap();
    assert_eq!(first["session"], expected_identity);
    assert_eq!(first["account_id"], "account_fixture");
    assert_eq!(first["project_id"], "legacy-project-read");
    assert_eq!(first["login_hint"], "legacy-storage-read");
    assert_ne!(first["session"], RAW_SESSION);
    assert!(!first["session"].as_str().unwrap().contains(RAW_SESSION));

    // Repeated attempts keep one identity; another raw session maps elsewhere.
    let retry = adapter.build_body(&context, &request).unwrap();
    assert_eq!(retry["session"], first["session"]);
    let second_context = UpstreamContext {
        provider: &provider,
        model: &model,
        account_id: Some("account_fixture"),
        session_context: Some(SECOND_RAW_SESSION),
        credential_metadata: None,
        credential: "fixture-credential".into(),
    };
    let second = adapter.build_body(&second_context, &request).unwrap();
    assert_ne!(second["session"], first["session"]);

    // A different API-v2 adapter used as a fallback target receives the same
    // opaque identity for the same raw client session.
    let fallback_adapter = PluginAdapter::new(manager.clone(), API2_FALLBACK_ID.into(), false)
        .await
        .unwrap();
    let mut fallback_provider = provider.clone();
    fallback_provider.id = "provider_fallback".into();
    fallback_provider.wire_plugin = format!("plugin:{API2_FALLBACK_ID}/session-echo");
    fallback_provider.source_plugin_id = Some(API2_FALLBACK_ID.into());
    let mut fallback_model = model.clone();
    fallback_model.id = "model_fallback".into();
    fallback_model.provider_id = fallback_provider.id.clone();
    let fallback_context = UpstreamContext {
        provider: &fallback_provider,
        model: &fallback_model,
        account_id: Some("account_fallback"),
        session_context: Some(RAW_SESSION),
        credential_metadata: None,
        credential: "fixture-credential".into(),
    };
    let fallback = fallback_adapter
        .build_body(&fallback_context, &request)
        .unwrap();
    assert_eq!(fallback["session"], first["session"]);

    let auth_request = reqwest::Client::new().get("https://fixture.invalid");
    let authed = adapter
        .apply_auth(&context, auth_request)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(
        authed.headers()["x-plugin-session"],
        expected_identity.as_str()
    );

    let no_session_context = UpstreamContext {
        provider: &provider,
        model: &model,
        account_id: Some("account_fixture"),
        session_context: None,
        credential_metadata: None,
        credential: "fixture-credential".into(),
    };
    let without_session = adapter.build_body(&no_session_context, &request).unwrap();
    assert!(without_session["session"].is_null());
    let no_session_auth = adapter
        .apply_auth(
            &no_session_context,
            reqwest::Client::new().get("https://fixture.invalid"),
        )
        .unwrap()
        .build()
        .unwrap();
    assert!(!no_session_auth.headers().contains_key("x-plugin-session"));
}

/// Error classification maps Antigravity's 429 + reset hint onto the host's
/// typed failure vocabulary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn antigravity_lease_metadata_reaches_the_v3_adapter_without_parsing_the_secret() {
    let Some(path) = package_path() else {
        eprintln!(
            "skipping: set KINETIX_PLUGIN_E2E_PACKAGE to a built .kxp from PrightCord/kinetix-plugins"
        );
        return;
    };
    const PLUGIN_ID: &str = "dev.kinetix.antigravity-oauth";
    const ACCOUNT_LABEL: &str = "lease metadata fixture";
    const ACCESS_TOKEN: &str = "e2e-access-token";
    const PROJECT_ID: &str = "e2e-cloud-project";

    let bytes = std::fs::read(&path).unwrap();
    let (manager, pool) = manager().await;
    let crypto = Arc::new(Crypto::new(&[7u8; 32]));
    manager
        .install(&bytes, None, &[], ALLOW_UNTRUSTED_TEST_PACKAGE)
        .await
        .unwrap();
    manager.approve_permissions(PLUGIN_ID).await.unwrap();
    manager.enable(PLUGIN_ID).await.unwrap();

    let plugin_ref = format!("plugin:{PLUGIN_ID}/antigravity-oauth");
    let provider_id = db::insert_provider(
        &pool,
        &NewProvider {
            name: "Antigravity lease metadata test",
            base_url: "https://daily-cloudcode-pa.googleapis.com",
            wire_format: WireFormat::Plugin,
            auth_scheme: AuthScheme::Bearer,
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: json!({}),
            timeout_ms: 30_000,
            capability_mode: "permissive",
            models_path: None,
            rate_limit_rules: json!({}),
            follow_redirects: false,
            credential_hosts: "",
            allow_insecure_tls: false,
            wire_plugin: &plugin_ref,
            credential_plugin: &plugin_ref,
            model_source_plugin: "",
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .unwrap();

    let imported = json!({
        "access_token": ACCESS_TOKEN,
        "expiry": "2999-01-01T00:00:00Z",
        "project_id": PROJECT_ID,
    });
    let encrypted = crypto.encrypt(&imported.to_string()).unwrap();
    let account_id = db::insert_account(
        &pool,
        &provider_id,
        ACCOUNT_LABEL,
        &encrypted,
        "fixture",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();
    let account = db::get_account(&pool, &account_id).await.unwrap().unwrap();

    // This invokes the built Antigravity component's real credential resolver.
    // Its lease contains only the access token; project_id travels in the
    // separate non-secret metadata KV entry read by core.
    let strategy =
        PluginCredentialStrategy::new(Arc::new(manager.clone()), pool.clone(), crypto, PLUGIN_ID);
    let resolved = strategy.resolve(&account).await.unwrap();
    assert_eq!(resolved.secret, ACCESS_TOKEN);
    assert_eq!(resolved.metadata.project_id.as_deref(), Some(PROJECT_ID));

    let provider = db::get_provider(&pool, &provider_id)
        .await
        .unwrap()
        .unwrap();
    let model = db::ModelRow {
        id: "antigravity-lease-model".into(),
        provider_id: provider_id.clone(),
        upstream_id: "gemini-3-flash".into(),
        display_name: "Gemini 3 Flash".into(),
        enabled: 1,
        context_window: None,
        max_output_tokens: None,
        capabilities: "{}".into(),
        prices: "{}".into(),
        parameters: "{}".into(),
        thinking_map: "{}".into(),
        extra_request: "{}".into(),
        discovery: "{}".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        opaque_state_plugin: String::new(),
    };
    let request = InternalRequest {
        requested_model: model.upstream_id.clone(),
        system: Vec::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        tool_choice: None,
        tool_choice_name: None,
        params: SamplingParams::default(),
        stream: false,
        include_usage: false,
        thinking: None,
        extra: Default::default(),
        raw_body: None,
    };
    let adapter = PluginAdapter::new(manager, PLUGIN_ID.into(), false)
        .await
        .unwrap();
    let context = UpstreamContext {
        provider: &provider,
        model: &model,
        account_id: Some(&account_id),
        session_context: None,
        credential: resolved.secret,
        credential_metadata: Some(&resolved.metadata),
    };
    let body = adapter.build_body(&context, &request).unwrap();
    assert_eq!(body["project"], PROJECT_ID);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adapter_classifies_quota_exhaustion() {
    let Some(path) = package_path() else {
        eprintln!(
            "skipping: set KINETIX_PLUGIN_E2E_PACKAGE to a built .kxp from PrightCord/kinetix-plugins"
        );
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let (m, _pool) = manager().await;
    m.install(&bytes, None, &[], ALLOW_UNTRUSTED_TEST_PACKAGE)
        .await
        .unwrap();
    m.approve_permissions("dev.kinetix.antigravity-oauth")
        .await
        .unwrap();
    m.enable("dev.kinetix.antigravity-oauth").await.unwrap();
    let id = "dev.kinetix.antigravity-oauth";

    let body = r#"{"error":{"message":"Quota exhausted. Your quota will reset after 2h7m23s"}}"#;
    let ev = m.adapter_classify_error(id, 429, body, "{}").await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&ev).unwrap();
    assert_eq!(v["kind"], "quota_exhausted");
    assert_eq!(v["retry_after_secs"], 2 * 3600 + 7 * 60 + 23);
}
