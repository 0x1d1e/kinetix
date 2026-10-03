//! End-to-end plugin lifecycle tests against a real database and Wasmtime host.
//!
//! These exercise the acceptance criteria that do not require a fully-featured
//! guest component: hash verification, API-compatibility rejection,
//! installed-disabled semantics, fail-closed enablement for an incompatible
//! component, all-or-nothing permissions, and removal (which cascades state).

use std::sync::Arc;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use kinetix::crypto::Crypto;
use kinetix::db::{self, Pool};
use kinetix::plugins::{Capability, HostPolicy, PluginManager};

const GOOD_MANIFEST: &str = r#"
manifest_version = 1
id = "dev.example.foo"
name = "Foo Provider Integration"
version = "1.2.0"
plugin_api = "1"

[provides]
model_sources = ["foo-models"]
routing_facts = ["foo-facts"]

[permissions]
network_hosts = ["api.foo.example"]
credential_scopes = ["provider:foo"]

[limits]
memory = "64MiB"
storage = "2MiB"
"#;

const AUTH_MANIFEST: &str = r#"
manifest_version = 1
id = "dev.example.auth"
name = "Auth"
version = "1.0.0"
plugin_api = "1"
[provides]
auth_flows = ["foo-login"]
"#;

const ACCOUNT_MODELS_MANIFEST: &str = r#"
manifest_version = 1
id = "dev.example.account-models"
name = "Account Models"
version = "1.0.0"
plugin_api = "1"
[provides]
account_model_sources = ["foo-models"]
"#;

const ADAPTER_MANIFEST: &str = r#"
manifest_version = 1
id = "dev.example.adapter"
name = "Adapter"
version = "1.0.0"
plugin_api = "1"
[provides]
provider_adapters = ["foo-adapter"]
"#;

/// Frozen API-v1 component fixture. Add new fixtures for later API revisions;
/// do not regenerate this against the current WIT when the contract evolves.
const VALID_COMPONENT: &[u8] = include_bytes!("fixtures/plugin-v1.component.wasm");
/// A valid empty component that deliberately omits every plugin-world export.
const EMPTY_COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";

async fn manager() -> (PluginManager, Pool) {
    let dir = std::env::temp_dir().join(format!("kinetix-plugin-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
    let pool = db::connect(&url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let crypto = Arc::new(Crypto::new(&[9u8; 32]));
    let manager = PluginManager::new(
        pool.clone(),
        crypto,
        HostPolicy::default(),
        dir.join("plugin-packages"),
    )
    .unwrap();
    (manager, pool)
}

async fn manager_with_single_connection() -> (PluginManager, Pool, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "kinetix-plugin-single-connection-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.join("t.db"))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(10))
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    let manager = PluginManager::new(
        pool.clone(),
        Arc::new(Crypto::new(&[9u8; 32])),
        HostPolicy::default(),
        dir.join("plugin-packages"),
    )
    .unwrap();
    (manager, pool, dir)
}

/// Build a `.kxp` archive in memory.
fn build_kxp(manifest: &str, wasm: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in [
        ("plugin.toml", manifest.as_bytes()),
        ("plugin.wasm", wasm),
        ("README.md", b"# Foo"),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, data).unwrap();
    }
    builder.into_inner().unwrap()
}

async fn set_persisted_manifest_version(pool: &Pool, id: &str, version: &str) {
    let stored: String = sqlx::query_scalar("SELECT manifest_json FROM plugins WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    let mut manifest: serde_json::Value = serde_json::from_str(&stored).unwrap();
    manifest["version"] = serde_json::Value::String(version.into());
    sqlx::query("UPDATE plugins SET manifest_json = ? WHERE id = ?")
        .bind(manifest.to_string())
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn plugin_summary_exposes_host_version_bounds() {
    let (m, _pool) = manager().await;
    let manifest = GOOD_MANIFEST.replace(
        "plugin_api = \"1\"",
        "plugin_api = \"1\"\n\n[compatibility]\nmin_host_version = \"0.1.0\"\nmax_host_version = \"99.0.0\"",
    );
    m.install(&build_kxp(&manifest, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();

    let row = m.get("dev.example.foo").await.unwrap().unwrap();
    let summary = kinetix::plugins::manager::manifest_summary(&row);
    assert_eq!(
        summary["compatibility"],
        serde_json::json!({
            "min_host_version": "0.1.0",
            "max_host_version": "99.0.0",
        })
    );
}

#[tokio::test]
async fn install_is_disabled_and_records_provenance() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    let outcome = m.install(&kxp, None, &[], false).await.unwrap();
    assert_eq!(outcome.id, "dev.example.foo");
    assert_eq!(outcome.version, "1.2.0");
    assert_eq!(outcome.provides.len(), 2);

    // Installed, but disabled (install and enable are separate operations).
    let row = m.get("dev.example.foo").await.unwrap().unwrap();
    assert_eq!(row.enabled, 0);
    assert_eq!(row.package_sha256.len(), 64);

    // Installation records requested permissions in the manifest but grants
    // no runtime authority until the operator explicitly approves them (§20).
    let perms = kinetix::plugins::store::permissions(&pool, "dev.example.foo")
        .await
        .unwrap();
    assert!(perms.is_empty());
}

#[tokio::test]
async fn legacy_invalid_manifest_is_rejected_by_validate_and_enable() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();

    set_persisted_manifest_version(&pool, "dev.example.foo", "not-semver").await;

    let validate_error = m.validate("dev.example.foo").await.unwrap_err();
    assert!(validate_error
        .to_string()
        .contains("invalid manifest `version`"));

    let enable_error = m.enable("dev.example.foo").await.unwrap_err();
    assert!(enable_error
        .to_string()
        .contains("invalid manifest `version`"));
}

#[tokio::test]
async fn invalid_enabled_manifest_fails_closed_at_runtime() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();
    m.approve_permissions("dev.example.foo").await.unwrap();
    m.enable("dev.example.foo").await.unwrap();

    set_persisted_manifest_version(&pool, "dev.example.foo", "release-1").await;

    assert!(!m.is_usable("dev.example.foo").await);
    assert!(m
        .resolve_binding("plugin:dev.example.foo/foo-models", Capability::ModelSource)
        .await
        .is_none());
    let error = m
        .model_discover(
            "dev.example.foo",
            "provider-foo",
            "https://api.foo.example",
            "/models",
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("invalid manifest `version`"),
        "{error}"
    );
}

#[tokio::test]
async fn startup_reconciliation_disables_invalid_enabled_manifests() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();
    m.approve_permissions("dev.example.foo").await.unwrap();
    m.enable("dev.example.foo").await.unwrap();
    set_persisted_manifest_version(&pool, "dev.example.foo", "release-1").await;

    m.reconcile_enabled_plugins().await.unwrap();

    let row = m.get("dev.example.foo").await.unwrap().unwrap();
    assert_eq!(row.enabled, 0);
    assert!(!m.is_usable("dev.example.foo").await);
}

#[tokio::test]
async fn startup_reconciliation_disables_plugin_missing_declared_adapter_world() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();
    m.approve_permissions("dev.example.foo").await.unwrap();
    m.enable("dev.example.foo").await.unwrap();

    let healthy_manifest = GOOD_MANIFEST
        .replace("dev.example.foo", "dev.example.bar")
        .replace("foo-models", "bar-models")
        .replace("foo-facts", "bar-facts");
    let healthy_kxp = build_kxp(&healthy_manifest, VALID_COMPONENT);
    m.install(&healthy_kxp, None, &[], false).await.unwrap();
    m.approve_permissions("dev.example.bar").await.unwrap();
    m.enable("dev.example.bar").await.unwrap();

    // Simulate a legacy enabled row: the old manifest and component were
    // accepted because enablement checked only the base plugin world.
    let stored: String = sqlx::query_scalar("SELECT manifest_json FROM plugins WHERE id = ?")
        .bind("dev.example.foo")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut manifest: serde_json::Value = serde_json::from_str(&stored).unwrap();
    manifest["provides"]["provider_adapters"] = serde_json::json!(["foo-adapter"]);
    sqlx::query("UPDATE plugins SET manifest_json = ? WHERE id = ?")
        .bind(manifest.to_string())
        .bind("dev.example.foo")
        .execute(&pool)
        .await
        .unwrap();

    m.reconcile_enabled_plugins().await.unwrap();

    let row = m.get("dev.example.foo").await.unwrap().unwrap();
    assert_eq!(row.enabled, 0);
    assert!(!m.is_usable("dev.example.foo").await);

    let healthy_row = m.get("dev.example.bar").await.unwrap().unwrap();
    assert_eq!(healthy_row.enabled, 1);
    assert!(m.is_usable("dev.example.bar").await);
}

#[tokio::test]
async fn install_preserves_exact_package_and_provenance() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    let outcome = m.install(&kxp, None, &[], false).await.unwrap();

    let packages = kinetix::plugins::store::list_packages(&pool, "dev.example.foo")
        .await
        .unwrap();
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0].version, "1.2.0");
    assert_eq!(packages[0].package_sha256, outcome.package_sha256);
    assert_eq!(packages[0].source, "local");

    let stored = std::fs::read(m.package_root().join(&packages[0].package_path)).unwrap();
    assert_eq!(stored, kxp);
}

#[tokio::test]
async fn compiled_component_cache_warms_lazily_after_restart() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();

    let restarted = PluginManager::new(
        pool,
        Arc::new(Crypto::new(&[9u8; 32])),
        HostPolicy::default(),
        m.package_root().to_path_buf(),
    )
    .unwrap();

    let before = restarted.counters();
    assert_eq!(before.component_cache_hits, 0);
    assert_eq!(before.component_cache_misses, 0);

    // Validation succeeds after restart and warms the compiled component cache.
    restarted.validate("dev.example.foo").await.unwrap();
    let after_first = restarted.counters();
    assert_eq!(after_first.component_cache_hits, 0);
    assert_eq!(after_first.component_cache_misses, 1);

    restarted.validate("dev.example.foo").await.unwrap();
    let after_second = restarted.counters();
    assert_eq!(after_second.component_cache_hits, 1);
    assert_eq!(after_second.component_cache_misses, 1);
}

#[tokio::test]
async fn hash_mismatch_is_rejected() {
    let (m, _pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    let err = m
        .install(&kxp, Some(&"0".repeat(64)), &[], false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("hash mismatch"), "{err}");
}

#[tokio::test]
async fn enable_requires_explicit_permission_approval() {
    let (m, _pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();

    let err = m.enable("dev.example.foo").await.unwrap_err();
    assert!(
        err.to_string().contains("permissions are not approved"),
        "{err}"
    );
    assert_eq!(m.get("dev.example.foo").await.unwrap().unwrap().enabled, 0);
}

#[tokio::test]
async fn non_expanding_upgrade_preserves_valid_approval_and_enabled_state() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();
    m.approve_permissions("dev.example.foo").await.unwrap();
    kinetix::plugins::store::set_enabled(&pool, "dev.example.foo", true)
        .await
        .unwrap();

    let upgraded = GOOD_MANIFEST.replace("version = \"1.2.0\"", "version = \"1.3.0\"");
    let upgraded_kxp = build_kxp(&upgraded, VALID_COMPONENT);
    let outcome = m.install(&upgraded_kxp, None, &[], false).await.unwrap();

    let row = m.get("dev.example.foo").await.unwrap().unwrap();
    assert_eq!(row.version, "1.3.0");
    assert_eq!(row.enabled, 1);
    assert!(outcome.approval_preserved);
    assert!(outcome.enabled);
    assert!(!outcome.permission_diff.increased);
    assert_eq!(
        kinetix::plugins::store::permissions(&pool, "dev.example.foo")
            .await
            .unwrap()
            .len(),
        3
    );

    let packages = kinetix::plugins::store::list_packages(&pool, "dev.example.foo")
        .await
        .unwrap();
    assert_eq!(packages.len(), 2);
    assert!(packages.iter().any(|p| p.version == "1.2.0"));
    assert!(packages.iter().any(|p| p.version == "1.3.0"));
}

#[tokio::test]
async fn update_does_not_restore_grants_revoked_after_approval_snapshot() {
    let (m, pool) = manager().await;
    m.install(&build_kxp(GOOD_MANIFEST, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();
    m.approve_permissions("dev.example.foo").await.unwrap();
    m.enable("dev.example.foo").await.unwrap();

    let old_row = m.get("dev.example.foo").await.unwrap().unwrap();
    let stale_approval: Vec<_> = kinetix::plugins::store::permissions(&pool, "dev.example.foo")
        .await
        .unwrap()
        .into_iter()
        .map(|grant| kinetix::plugins::store::PermissionGrant {
            permission: grant.permission,
            value_json: grant.value_json,
        })
        .collect();
    let upgraded = GOOD_MANIFEST.replace("version = \"1.2.0\"", "version = \"1.3.0\"");
    let bytes = build_kxp(&upgraded, VALID_COMPONENT);
    let package = kinetix::plugins::package::read_package(&bytes).unwrap();
    let validated =
        kinetix::plugins::package::validate_manifest(&package, HostPolicy::default()).unwrap();
    let target_grants = kinetix::plugins::manager::permission_grants(&validated.manifest);
    let signature = kinetix::plugins::package::verify_signature(&package, &[]).unwrap();

    // Model revocation after the update captured its old grants but before its
    // package upsert transaction starts.
    m.revoke_permission("dev.example.foo", "network_hosts")
        .await
        .unwrap();
    let outcome = kinetix::plugins::store::upsert_plugin(
        &pool,
        &validated,
        &package.package_sha256,
        &package.component,
        signature.as_str(),
        "dev.example.foo/update.kxp",
        "test",
        Some(&old_row.package_sha256),
        Some((&stale_approval, &target_grants)),
    )
    .await
    .unwrap();

    assert!(!outcome.approval_preserved);
    assert!(!outcome.enabled);
    assert_eq!(m.get("dev.example.foo").await.unwrap().unwrap().enabled, 0);
    assert!(
        kinetix::plugins::store::permissions(&pool, "dev.example.foo")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn expanding_upgrade_clears_approval_and_disables_plugin() {
    let (m, pool) = manager().await;
    m.install(&build_kxp(GOOD_MANIFEST, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();
    m.approve_permissions("dev.example.foo").await.unwrap();
    kinetix::plugins::store::set_enabled(&pool, "dev.example.foo", true)
        .await
        .unwrap();

    let expanded = GOOD_MANIFEST
        .replace("version = \"1.2.0\"", "version = \"1.3.0\"")
        .replace("memory = \"64MiB\"", "memory = \"128MiB\"")
        .replace(
            "network_hosts = [\"api.foo.example\"]",
            "network_hosts = [\"api.foo.example\", \"new.foo.example\"]",
        );
    let outcome = m
        .install(&build_kxp(&expanded, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();

    let row = m.get("dev.example.foo").await.unwrap().unwrap();
    assert_eq!(row.enabled, 0);
    assert!(!outcome.approval_preserved);
    assert!(!outcome.enabled);
    assert!(outcome.permission_diff.increased);
    assert_eq!(
        outcome.permission_diff.network_hosts.added,
        vec!["new.foo.example".to_string()]
    );
    assert!(outcome.permission_diff.limits.memory.increased);
    assert!(
        kinetix::plugins::store::permissions(&pool, "dev.example.foo")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn limit_only_expansion_requires_approval_before_enable_and_runtime_use() {
    let (m, pool) = manager().await;
    let manifest = GOOD_MANIFEST.replace(
        "[permissions]\nnetwork_hosts = [\"api.foo.example\"]\ncredential_scopes = [\"provider:foo\"]\n\n",
        "",
    );
    m.install(&build_kxp(&manifest, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();
    let approved = m.approve_permissions("dev.example.foo").await.unwrap();
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0].permission, "limits");
    m.enable("dev.example.foo").await.unwrap();

    let expanded = manifest
        .replace("version = \"1.2.0\"", "version = \"1.3.0\"")
        .replace("memory = \"64MiB\"", "memory = \"128MiB\"");
    let outcome = m
        .install(&build_kxp(&expanded, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();
    assert!(outcome.permission_diff.increased);
    assert!(outcome.permission_diff.limits.memory.increased);
    assert!(!outcome.approval_preserved);
    assert!(!outcome.enabled);
    assert!(
        kinetix::plugins::store::permissions(&pool, "dev.example.foo")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(m.enable("dev.example.foo").await.is_err());

    kinetix::plugins::store::set_enabled(&pool, "dev.example.foo", true)
        .await
        .unwrap();
    assert!(!m.is_usable("dev.example.foo").await);
}

#[tokio::test]
async fn pinned_plugin_rejects_different_version_until_unpinned() {
    let (m, _pool) = manager().await;
    m.install(&build_kxp(GOOD_MANIFEST, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();
    assert_eq!(
        m.set_version_pin("dev.example.foo", true).await.unwrap(),
        "1.2.0"
    );

    let upgraded = GOOD_MANIFEST.replace("version = \"1.2.0\"", "version = \"1.3.0\"");
    let err = m
        .install(&build_kxp(&upgraded, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("pinned to version 1.2.0"), "{err}");
    assert_eq!(
        m.get("dev.example.foo")
            .await
            .unwrap()
            .unwrap()
            .pinned_version
            .as_deref(),
        Some("1.2.0")
    );

    m.set_version_pin("dev.example.foo", false).await.unwrap();
    assert_eq!(
        m.install(&build_kxp(&upgraded, VALID_COMPONENT), None, &[], false)
            .await
            .unwrap()
            .version,
        "1.3.0"
    );
}

#[tokio::test]
async fn dependency_impact_requires_current_acknowledgement_for_disable_and_remove() {
    let (m, pool) = manager().await;
    m.install(&build_kxp(GOOD_MANIFEST, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();
    m.approve_permissions("dev.example.foo").await.unwrap();
    m.enable("dev.example.foo").await.unwrap();

    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "Plugin-backed",
            base_url: "https://foo.example",
            wire_format: kinetix::types::WireFormat::Openai,
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
            credential_plugin: "",
            model_source_plugin: "plugin:dev.example.foo/foo-models",
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .unwrap();
    let model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &provider_id,
            upstream_id: "foo-model",
            display_name: "Foo model",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: serde_json::json!({}),
            prices: serde_json::json!({}),
            parameters: serde_json::json!({}),
            thinking_map: serde_json::json!({}),
            extra_request: serde_json::json!({}),
            discovery: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    let route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "Foo Route",
            description: "",
            strategy: "priority",
            fallback_triggers: serde_json::json!([]),
            portability_policy: "strict",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: None,
            max_concurrent_requests: None,
        },
    )
    .await
    .unwrap();
    db::insert_route_target(&pool, &route_id, None, &model_id, 0, 1, "{}", "{}")
        .await
        .unwrap();

    let impact = m.dependency_impact("dev.example.foo").await.unwrap();
    assert_eq!(impact.providers.len(), 1);
    assert_eq!(impact.providers[0].id, provider_id);
    assert_eq!(impact.routes.len(), 1);
    assert_eq!(impact.routes[0].id, route_id);
    assert_eq!(impact.routes[0].model_ids, vec![model_id]);

    // A preview fingerprint cannot authorize a mutation after bindings change.
    db::clear_route_targets(&pool, &route_id).await.unwrap();
    assert!(m
        .disable("dev.example.foo", Some(&impact.fingerprint))
        .await
        .is_err());
    let refreshed = m.dependency_impact("dev.example.foo").await.unwrap();
    assert!(refreshed.routes.is_empty());
    assert_ne!(refreshed.fingerprint, impact.fingerprint);
    assert!(m.disable("dev.example.foo", None).await.is_err());
    assert_eq!(m.get("dev.example.foo").await.unwrap().unwrap().enabled, 1);

    m.disable("dev.example.foo", Some(&refreshed.fingerprint))
        .await
        .unwrap();
    assert_eq!(m.get("dev.example.foo").await.unwrap().unwrap().enabled, 0);
    assert!(m.remove("dev.example.foo", None).await.is_err());
    m.remove("dev.example.foo", Some(&refreshed.fingerprint))
        .await
        .unwrap();
    assert!(m.get("dev.example.foo").await.unwrap().is_none());
}

#[tokio::test]
async fn lifecycle_impact_reads_work_with_a_single_connection_pool() {
    let (m, pool, dir) = manager_with_single_connection().await;
    m.install(&build_kxp(GOOD_MANIFEST, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), m.disable("dev.example.foo", None))
        .await
        .expect("disable must use its transaction connection for impact reads")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), m.remove("dev.example.foo", None))
        .await
        .expect("remove must use its transaction connection for impact reads")
        .unwrap();

    pool.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn concurrent_provider_and_route_bindings_block_disable_and_remove() {
    let (m, pool) = manager().await;
    m.install(&build_kxp(GOOD_MANIFEST, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();
    m.approve_permissions("dev.example.foo").await.unwrap();
    m.enable("dev.example.foo").await.unwrap();

    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "Racing provider",
            base_url: "https://foo.example",
            wire_format: kinetix::types::WireFormat::Openai,
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
            credential_plugin: "",
            model_source_plugin: "",
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .unwrap();
    let model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &provider_id,
            upstream_id: "racing-model",
            display_name: "Racing model",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: serde_json::json!({}),
            prices: serde_json::json!({}),
            parameters: serde_json::json!({}),
            thinking_map: serde_json::json!({}),
            extra_request: serde_json::json!({}),
            discovery: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    let route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "Racing route",
            description: "",
            strategy: "priority",
            fallback_triggers: serde_json::json!([]),
            portability_policy: "strict",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: None,
            max_concurrent_requests: None,
        },
    )
    .await
    .unwrap();

    // The lifecycle operation must acquire the SQLite writer lock before
    // enumerating impact. A binding and Route target committed while it waits
    // must be visible to both disable and removal checks.
    let mut writer = pool.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *writer)
        .await
        .unwrap();
    let disabling = {
        let manager = m.clone();
        tokio::spawn(async move { manager.disable("dev.example.foo", None).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    sqlx::query("UPDATE providers SET model_source_plugin = ? WHERE id = ?")
        .bind("plugin:dev.example.foo/foo-models")
        .bind(&provider_id)
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO route_targets (id, route_id, account_id, model_id, priority, weight, param_overrides, predicate)
         VALUES ('racing-target-1', ?, NULL, ?, 0, 1, '{}', '{}')",
    )
    .bind(&route_id)
    .bind(&model_id)
    .execute(&mut *writer)
    .await
    .unwrap();
    sqlx::query("COMMIT").execute(&mut *writer).await.unwrap();
    assert!(disabling.await.unwrap().is_err());
    assert_eq!(m.get("dev.example.foo").await.unwrap().unwrap().enabled, 1);

    sqlx::query("UPDATE providers SET model_source_plugin = '' WHERE id = ?")
        .bind(&provider_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM route_targets WHERE id = 'racing-target-1'")
        .execute(&pool)
        .await
        .unwrap();

    let mut writer = pool.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *writer)
        .await
        .unwrap();
    let removing = {
        let manager = m.clone();
        tokio::spawn(async move { manager.remove("dev.example.foo", None).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    sqlx::query("UPDATE providers SET model_source_plugin = ? WHERE id = ?")
        .bind("plugin:dev.example.foo/foo-models")
        .bind(&provider_id)
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO route_targets (id, route_id, account_id, model_id, priority, weight, param_overrides, predicate)
         VALUES ('racing-target-2', ?, NULL, ?, 0, 1, '{}', '{}')",
    )
    .bind(&route_id)
    .bind(&model_id)
    .execute(&mut *writer)
    .await
    .unwrap();
    sqlx::query("COMMIT").execute(&mut *writer).await.unwrap();
    assert!(removing.await.unwrap().is_err());
    assert!(m.get("dev.example.foo").await.unwrap().is_some());
    let impact = m.dependency_impact("dev.example.foo").await.unwrap();
    assert_eq!(impact.providers.len(), 1);
    assert_eq!(impact.routes.len(), 1);
}

#[tokio::test]
async fn rollback_revalidates_retained_package_and_clears_authority() {
    let (m, pool) = manager().await;
    let original = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    let original_outcome = m.install(&original, None, &[], false).await.unwrap();

    let upgraded = GOOD_MANIFEST.replace("version = \"1.2.0\"", "version = \"1.3.0\"");
    let upgraded_kxp = build_kxp(&upgraded, VALID_COMPONENT);
    m.install(&upgraded_kxp, None, &[], false).await.unwrap();
    m.approve_permissions("dev.example.foo").await.unwrap();
    kinetix::plugins::store::set_enabled(&pool, "dev.example.foo", true)
        .await
        .unwrap();

    let rolled_back = m
        .rollback("dev.example.foo", &original_outcome.package_sha256)
        .await
        .unwrap();
    assert_eq!(rolled_back.version, "1.2.0");

    let row = m.get("dev.example.foo").await.unwrap().unwrap();
    assert_eq!(row.version, "1.2.0");
    assert_eq!(row.package_sha256, original_outcome.package_sha256);
    assert_eq!(row.enabled, 0);
    assert!(
        kinetix::plugins::store::permissions(&pool, "dev.example.foo")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn rollback_preview_reports_semantic_permission_diff() {
    let (m, _pool) = manager().await;
    let original = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    let original_outcome = m.install(&original, None, &[], false).await.unwrap();

    let upgraded = GOOD_MANIFEST
        .replace("version = \"1.2.0\"", "version = \"1.3.0\"")
        .replace(
            "network_hosts = [\"api.foo.example\"]",
            "network_hosts = [\"api.foo.example\", \"api.new.example\"]",
        )
        .replace(
            "credential_scopes = [\"provider:foo\"]",
            "credential_scopes = [\"provider:foo\", \"provider:bar\"]\ncredential_read = true",
        );
    m.install(&build_kxp(&upgraded, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();

    let preview = m
        .rollback_preview("dev.example.foo", &original_outcome.package_sha256)
        .await
        .unwrap();

    assert_eq!(preview.current_version, "1.3.0");
    assert_eq!(preview.target_version, "1.2.0");
    assert!(preview.permission_diff.network_hosts.added.is_empty());
    assert_eq!(
        preview.permission_diff.network_hosts.removed,
        vec!["api.new.example".to_string()]
    );
    assert!(preview.permission_diff.credential_scopes.added.is_empty());
    assert_eq!(
        preview.permission_diff.credential_scopes.removed,
        vec!["provider:bar".to_string()]
    );
    assert!(preview.permission_diff.credential_read.changed);
    assert!(preview.permission_diff.credential_read.from);
    assert!(!preview.permission_diff.credential_read.to);
}

#[tokio::test]
async fn rollback_rejects_tampered_retained_package() {
    let (m, pool) = manager().await;
    let original = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    let original_outcome = m.install(&original, None, &[], false).await.unwrap();

    let upgraded = GOOD_MANIFEST.replace("version = \"1.2.0\"", "version = \"1.3.0\"");
    m.install(&build_kxp(&upgraded, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();

    let package = kinetix::plugins::store::get_package(
        &pool,
        "dev.example.foo",
        &original_outcome.package_sha256,
    )
    .await
    .unwrap()
    .unwrap();
    std::fs::write(m.package_root().join(&package.package_path), b"tampered").unwrap();

    let err = m
        .rollback("dev.example.foo", &original_outcome.package_sha256)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("hash mismatch"), "{err}");
}

#[tokio::test]
async fn reinstall_from_retained_package_recovers_after_removal() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    let outcome = m.install(&kxp, None, &[], false).await.unwrap();
    let sha = outcome.package_sha256.clone();

    // Remove the plugin: the active row (and cascaded permissions) go away, but
    // the content-addressed package provenance is retained.
    m.remove("dev.example.foo", None).await.unwrap();
    assert!(m.get("dev.example.foo").await.unwrap().is_none());
    assert_eq!(
        kinetix::plugins::store::list_packages(&pool, "dev.example.foo")
            .await
            .unwrap()
            .len(),
        1
    );

    // Reinstall from the retained bytes: re-hashed, re-validated, installed
    // disabled with no granted authority.
    let reinstalled = m.install_retained("dev.example.foo", &sha).await.unwrap();
    assert_eq!(reinstalled.package_sha256, sha);
    assert_eq!(reinstalled.version, "1.2.0");
    let row = m.get("dev.example.foo").await.unwrap().unwrap();
    assert_eq!(row.enabled, 0);
    assert!(
        kinetix::plugins::store::permissions(&pool, "dev.example.foo")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn scoped_permission_approval_grants_only_the_requested_subset() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();

    // Approve a strict subset of the declared network hosts.
    let grants = m
        .approve_permissions_scoped(
            "dev.example.foo",
            Some(vec!["api.foo.example".to_string()]),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(grants.len(), 2);
    assert!(grants.iter().any(|grant| grant.permission == "limits"));
    let perms = kinetix::plugins::store::permissions(&pool, "dev.example.foo")
        .await
        .unwrap();
    assert_eq!(perms.len(), 2);
    let network_hosts = perms
        .iter()
        .find(|grant| grant.permission == "network_hosts")
        .unwrap();
    assert_eq!(network_hosts.value_json, "[\"api.foo.example\"]");
    assert!(perms.iter().any(|grant| grant.permission == "limits"));

    // A host not declared by the manifest cannot be approved.
    let err = m
        .approve_permissions_scoped(
            "dev.example.foo",
            Some(vec!["evil.example".to_string()]),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not declared"), "{err}");
}

#[tokio::test]
async fn full_explicit_scoped_approval_includes_limits_and_allows_runtime_use() {
    let (m, pool) = manager().await;
    let manifest = GOOD_MANIFEST.replace(
        "credential_scopes = [\"provider:foo\"]",
        "credential_scopes = [\"provider:foo\"]\ncredential_read = true",
    );
    m.install(&build_kxp(&manifest, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap();

    let grants = m
        .approve_permissions_scoped(
            "dev.example.foo",
            Some(vec!["api.foo.example".to_string()]),
            Some(vec!["provider:foo".to_string()]),
            Some(true),
        )
        .await
        .unwrap();
    assert_eq!(grants.len(), 4);
    assert!(grants.iter().any(|grant| grant.permission == "limits"));
    m.enable("dev.example.foo").await.unwrap();
    assert!(m.is_usable("dev.example.foo").await);

    pool.close().await;
}

#[tokio::test]
async fn revoking_a_permission_disables_the_plugin() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();
    m.approve_permissions("dev.example.foo").await.unwrap();
    kinetix::plugins::store::set_enabled(&pool, "dev.example.foo", true)
        .await
        .unwrap();

    m.revoke_permission("dev.example.foo", "network_hosts")
        .await
        .unwrap();

    assert_eq!(m.get("dev.example.foo").await.unwrap().unwrap().enabled, 0);
    let perms = kinetix::plugins::store::permissions(&pool, "dev.example.foo")
        .await
        .unwrap();
    assert!(!perms.iter().any(|p| p.permission == "network_hosts"));
}

#[tokio::test]
async fn api_v1_v2_and_v3_manifests_are_accepted_and_v4_is_rejected() {
    let (m, _pool) = manager().await;
    for major in [1, 2, 3] {
        let manifest = GOOD_MANIFEST
            .replace("dev.example.foo", &format!("dev.example.foo.v{major}"))
            .replace("plugin_api = \"1\"", &format!("plugin_api = \"{major}\""));
        let outcome = m
            .install(&build_kxp(&manifest, VALID_COMPONENT), None, &[], false)
            .await
            .unwrap();
        assert_eq!(outcome.id, format!("dev.example.foo.v{major}"));
        assert_eq!(
            m.get(&outcome.id)
                .await
                .unwrap()
                .unwrap()
                .manifest()
                .unwrap()
                .api_major(),
            Some(major)
        );
    }

    let bad = GOOD_MANIFEST.replace("plugin_api = \"1\"", "plugin_api = \"4\"");
    let err = m
        .install(&build_kxp(&bad, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("incompatible plugin_api"), "{err}");
}

#[tokio::test]
async fn install_rejects_host_version_outside_manifest_range() {
    let (m, _pool) = manager().await;
    let host_major = env!("CARGO_PKG_VERSION")
        .split('.')
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let required_version = format!("{}.0.0", host_major.saturating_add(1));
    let manifest = GOOD_MANIFEST.replace(
        "plugin_api = \"1\"",
        &format!(
            "plugin_api = \"1\"\n\n[compatibility]\nmin_host_version = \"{required_version}\""
        ),
    );

    let err = m
        .install(&build_kxp(&manifest, VALID_COMPONENT), None, &[], false)
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains(&format!("plugin requires Kinetix >= {required_version}")),
        "{err}"
    );
    assert!(m.get("dev.example.foo").await.unwrap().is_none());
}

#[tokio::test]
async fn undeclared_capabilities_are_rejected() {
    let (m, _pool) = manager().await;
    let bad = GOOD_MANIFEST
        .replace("model_sources = [\"foo-models\"]", "")
        .replace("routing_facts = [\"foo-facts\"]", "");
    let kxp = build_kxp(&bad, VALID_COMPONENT);
    let err = m.install(&kxp, None, &[], false).await.unwrap_err();
    assert!(
        err.to_string().contains("provides no capabilities"),
        "{err}"
    );
}

#[tokio::test]
async fn component_missing_required_exports_is_rejected_during_install() {
    let (m, _pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, EMPTY_COMPONENT);
    let err = m.install(&kxp, None, &[], false).await.unwrap_err();
    assert!(err.to_string().contains("plugin-world validation"), "{err}");
    assert!(m.get("dev.example.foo").await.unwrap().is_none());
}

#[tokio::test]
async fn install_validates_all_declared_optional_worlds() {
    let (m, _pool) = manager().await;
    for (manifest, component) in [
        (
            AUTH_MANIFEST,
            include_bytes!("fixtures/plugin-auth.component.wasm").as_slice(),
        ),
        (
            ACCOUNT_MODELS_MANIFEST,
            include_bytes!("fixtures/plugin-model-source.component.wasm").as_slice(),
        ),
        (
            ADAPTER_MANIFEST,
            include_bytes!("fixtures/plugin-adapter.component.wasm").as_slice(),
        ),
    ] {
        m.install(&build_kxp(manifest, component), None, &[], false)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn install_rejects_missing_declared_optional_worlds() {
    let (m, _pool) = manager().await;
    for (manifest, world) in [
        (AUTH_MANIFEST, "auth-world"),
        (ACCOUNT_MODELS_MANIFEST, "account model-source world"),
        (ADAPTER_MANIFEST, "provider-adapter world"),
    ] {
        let err = m
            .install(&build_kxp(manifest, EMPTY_COMPONENT), None, &[], false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains(world), "{err}");
    }
}

#[tokio::test]
async fn removing_a_plugin_cascades_stored_state() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();
    let crypto = Crypto::new(&[9u8; 32]);
    kinetix::plugins::store::kv_put(&pool, &crypto, "dev.example.foo", "lease:h1", b"secret")
        .await
        .unwrap();
    assert!(
        kinetix::plugins::store::kv_bytes(&pool, "dev.example.foo")
            .await
            .unwrap()
            > 0
    );
    m.remove("dev.example.foo", None).await.unwrap();
    assert!(m.get("dev.example.foo").await.unwrap().is_none());
    // KV rows are gone via ON DELETE CASCADE.
    assert_eq!(
        kinetix::plugins::store::kv_bytes(&pool, "dev.example.foo")
            .await
            .unwrap(),
        0
    );
    // Immutable package provenance is intentionally independent of active
    // plugin state and survives uninstall.
    let packages = kinetix::plugins::store::list_packages(&pool, "dev.example.foo")
        .await
        .unwrap();
    assert_eq!(packages.len(), 1);
}

#[tokio::test]
async fn a_reference_to_a_disabled_plugin_does_not_resolve() {
    let (m, _pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();
    // Installed-disabled: a config binding must not resolve (fail closed, §6.0).
    let resolved = m
        .resolve_binding("plugin:dev.example.foo/foo-models", Capability::ModelSource)
        .await;
    assert!(resolved.is_none());
}

#[tokio::test]
async fn kv_is_encrypted_and_namespaced_by_plugin() {
    let (m, pool) = manager().await;
    let kxp = build_kxp(GOOD_MANIFEST, VALID_COMPONENT);
    m.install(&kxp, None, &[], false).await.unwrap();
    let crypto = Crypto::new(&[9u8; 32]);
    kinetix::plugins::store::kv_put(&pool, &crypto, "dev.example.foo", "k", b"v1")
        .await
        .unwrap();
    // A different plugin id cannot read the first plugin's key.
    assert!(
        kinetix::plugins::store::kv_get(&pool, &crypto, "other.plugin", "k")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        kinetix::plugins::store::kv_get(&pool, &crypto, "dev.example.foo", "k")
            .await
            .unwrap()
            .unwrap(),
        b"v1"
    );
    // Stored ciphertext is not the plaintext.
    let raw: Vec<u8> = sqlx::query_scalar("SELECT value FROM plugin_kv WHERE key='k'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(raw, b"v1");
}

#[tokio::test]
async fn catalog_filtering_and_loading_works() {
    let catalog = kinetix::plugins::catalog::embedded_catalog().unwrap();
    assert!(!catalog.plugins.is_empty());

    // Search query filter
    let res =
        kinetix::plugins::catalog::filter_catalog(&catalog.plugins, Some("antigravity"), None);
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id, "dev.kinetix.antigravity-oauth");

    // Capability filter
    let res =
        kinetix::plugins::catalog::filter_catalog(&catalog.plugins, None, Some("model_source"));
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id, "dev.kinetix.opencode-free");

    // Both filters
    let res = kinetix::plugins::catalog::filter_catalog(
        &catalog.plugins,
        Some("Google"),
        Some("provider_adapter"),
    );
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id, "dev.kinetix.antigravity-oauth");

    // Empty match
    let res = kinetix::plugins::catalog::filter_catalog(
        &catalog.plugins,
        Some("nonexistent_keyword"),
        None,
    );
    assert!(res.is_empty());

    // Cache file round trip
    let dir = std::env::temp_dir().join(format!(
        "kinetix-catalog-cache-test-{}",
        uuid::Uuid::new_v4()
    ));
    let cache_file = dir.join("catalog.cache.json");
    kinetix::plugins::catalog::write_cached_catalog(&cache_file, &catalog).unwrap();
    assert!(cache_file.exists());

    let loaded = kinetix::plugins::catalog::read_cached_catalog(&cache_file).unwrap();
    assert_eq!(loaded.plugins.len(), catalog.plugins.len());
}
