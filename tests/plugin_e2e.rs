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

use kinetix::adapters::{AdapterRegistry, UpstreamContext};
use kinetix::crypto::Crypto;
use kinetix::db::{self, Pool};
use kinetix::plugins::{
    adapter::register_declared_adapters, Capability, HostPolicy, PluginManager,
};
use kinetix::types::{
    InternalRequest, Message, Part, Role, SamplingParams, ThinkingLevel, WireFormat,
};

/// Path to an externally built `.kxp` used for host/guest conformance.
fn package_path() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("KINETIX_PLUGIN_E2E_PACKAGE").map(std::path::PathBuf::from)?;
    path.is_file().then_some(path)
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

    let outcome = m.install(&bytes, None, &[], false).await.unwrap();
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
    m.install(&bytes, None, &[], false).await.unwrap();
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
    m.install(&bytes, None, &[], false).await.unwrap();
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
async fn a_real_guest_reports_usable_after_enable() {
    let Some(path) = package_path() else {
        eprintln!(
            "skipping: set KINETIX_PLUGIN_E2E_PACKAGE to a built .kxp from PrightCord/kinetix-plugins"
        );
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let (m, _pool) = manager().await;
    m.install(&bytes, None, &[], false).await.unwrap();
    m.approve_permissions("dev.kinetix.antigravity-oauth")
        .await
        .unwrap();
    m.enable("dev.kinetix.antigravity-oauth").await.unwrap();

    // The plugin does not provide health-probe, so the host reports an unknown
    // capability rather than invoking a missing export. `credential_strategy`
    // is provided; asking for health through the credential plugin path is
    // covered by the plugin's own `health` export below.
    let usable = m.is_usable("dev.kinetix.antigravity-oauth").await;
    assert!(usable);
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
    m.install(&bytes, None, &[], false).await.unwrap();
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

    let provider = r#"{"base_url":"https://daily-cloudcode-pa.googleapis.com","extra_headers":"{\"x-antigravity-project\":\"test-project\"}"}"#;
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
    let arr = ev.as_array().unwrap();
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
    let Some(api2_path) = package_path() else {
        eprintln!("skipping: set KINETIX_PLUGIN_E2E_PACKAGE to an API-v2 .kxp");
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

/// Error classification maps Antigravity's 429 + reset hint onto the host's
/// typed failure vocabulary.
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
    m.install(&bytes, None, &[], false).await.unwrap();
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
