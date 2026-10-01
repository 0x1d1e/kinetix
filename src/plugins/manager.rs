//! Plugin lifecycle and invocation (§11–§16).
//!
//! The manager owns install/enable/disable/remove, the per-plugin circuit
//! breaker, and every host→guest invocation. It is the only place that talks to
//! Wasmtime for request-path work, and it always maps guest results into typed
//! evidence that core policy consumes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use dashmap::DashMap;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::crypto::Crypto;
use crate::db::Pool;

use super::manifest::{self, HostPolicy};
use super::package::{self, Package, SignatureStatus};
use super::runtime::{
    adapter_bindings, adapter_v2_bindings, bindings, health_v2_bindings, health_v2_wit, wit,
    DeadlineGuard, HostBacking, HostCapabilityMode, HostCtx, PluginFault, PluginRuntime,
    CONFIG_PREFIX,
};
use super::store::{self, PermissionGrant, PluginRow};
use super::types::{Capability, Manifest, Permissions, Provided};

/// Bounds total concurrent guest invocations across the process (§14).
const MAX_CONCURRENT_INVOCATIONS: usize = 16;
/// Prevent one plugin from monopolizing the global guest-execution budget.
const MAX_CONCURRENT_INVOCATIONS_PER_PLUGIN: usize = 4;
/// Consecutive runtime faults before a plugin's circuit opens (§15).
const CIRCUIT_FAULT_THRESHOLD: i64 = 5;
/// Cooldown before a half-open probe (§15).
const CIRCUIT_OPEN_SECS: i64 = 60;

#[derive(Default)]
struct PluginMetricCell {
    invocations: std::sync::atomic::AtomicU64,
    successes: std::sync::atomic::AtomicU64,
    faults: std::sync::atomic::AtomicU64,
    timeouts: std::sync::atomic::AtomicU64,
    cancellations: std::sync::atomic::AtomicU64,
    http_requests: std::sync::atomic::AtomicU64,
    duration_micros: std::sync::atomic::AtomicU64,
}

type PluginMetricRegistry = DashMap<(String, String), Arc<PluginMetricCell>>;

fn metric_cell(
    registry: &PluginMetricRegistry,
    plugin_id: &str,
    capability: &str,
) -> Arc<PluginMetricCell> {
    registry
        .entry((plugin_id.to_string(), capability.to_string()))
        .or_insert_with(|| Arc::new(PluginMetricCell::default()))
        .clone()
}

/// Backing implementation for host effects (storage, logs, credentials).
pub struct Backing {
    pool: Pool,
    crypto: Arc<Crypto>,
    metrics: Arc<PluginMetricRegistry>,
}

#[async_trait::async_trait]
impl HostBacking for Backing {
    async fn kv_get(&self, plugin_id: &str, key: &str) -> Result<Option<Vec<u8>>> {
        store::kv_get(&self.pool, &self.crypto, plugin_id, key).await
    }
    async fn kv_put_limited(
        &self,
        plugin_id: &str,
        key: &str,
        value: &[u8],
        quota: u64,
    ) -> Result<()> {
        store::kv_put_limited(&self.pool, &self.crypto, plugin_id, key, value, quota).await
    }
    async fn kv_delete(&self, plugin_id: &str, key: &str) -> Result<()> {
        store::kv_delete(&self.pool, plugin_id, key).await
    }
    fn record_http_request(&self, plugin_id: &str, capability: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        metric_cell(&self.metrics, plugin_id, capability)
            .http_requests
            .fetch_add(1, Relaxed);
    }
    async fn credential_scope_allows(
        &self,
        plugin_id: &str,
        provider_id: &str,
        scopes: &[String],
    ) -> Result<bool> {
        let exact = format!("provider:{provider_id}");
        if scopes.iter().any(|scope| scope == "*" || scope == &exact) {
            return Ok(true);
        }

        let Some(provider) = crate::db::get_provider(&self.pool, provider_id).await? else {
            return Ok(false);
        };
        for scope in scopes {
            let Some(strategy) = scope.strip_prefix("credential_strategy:") else {
                continue;
            };
            let expected = format!("plugin:{plugin_id}/{strategy}");
            if provider.credential_plugin == expected {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn log(&self, plugin_id: &str, level: &str, message: &str) {
        // §18: plugin logs are namespaced and redacted like core logs.
        match level {
            "error" => tracing::error!(plugin = %plugin_id, "plugin: {message}"),
            "warn" => tracing::warn!(plugin = %plugin_id, "plugin: {message}"),
            "info" => tracing::info!(plugin = %plugin_id, "plugin: {message}"),
            "debug" => tracing::debug!(plugin = %plugin_id, "plugin: {message}"),
            _ => tracing::trace!(plugin = %plugin_id, "plugin: {message}"),
        }
    }
    async fn resolve_secret(
        &self,
        _plugin_id: &str,
        provider_id: &str,
        account_id: &str,
    ) -> Result<String> {
        let account = crate::db::get_account(&self.pool, account_id)
            .await?
            .ok_or_else(|| anyhow!("account not found"))?;
        if account.provider_id != provider_id {
            bail!(
                "account '{}' does not belong to provider '{}'",
                account_id,
                provider_id
            );
        }
        self.crypto.decrypt(&account.secret_enc)
    }
    async fn credential_egress_hosts(&self, provider_id: &str) -> Result<Vec<String>> {
        let provider = crate::db::get_provider(&self.pool, provider_id)
            .await?
            .ok_or_else(|| anyhow!("provider '{provider_id}' not found"))?;
        let mut hosts = Vec::new();
        if let Some(base) = provider.base_host() {
            hosts.push(base);
        }
        hosts.extend(provider.credential_hosts());
        hosts.sort_unstable();
        hosts.dedup();
        Ok(hosts)
    }
}

/// Validation gets an isolated in-memory KV namespace and no credential or
/// network authority, so a guest start function cannot mutate installed state.
#[derive(Default)]
struct ValidationBacking {
    entries: Mutex<HashMap<(String, String), Vec<u8>>>,
}

#[async_trait::async_trait]
impl HostBacking for ValidationBacking {
    async fn kv_get(&self, plugin_id: &str, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(plugin_id.to_string(), key.to_string()))
            .cloned())
    }
    async fn kv_put_limited(
        &self,
        plugin_id: &str,
        key: &str,
        value: &[u8],
        quota: u64,
    ) -> Result<()> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let map_key = (plugin_id.to_string(), key.to_string());
        let old_size = entries
            .get(&map_key)
            .map(|bytes| bytes.len() as u64)
            .unwrap_or(0);
        let used = entries
            .iter()
            .filter(|((owner, _), _)| owner == plugin_id)
            .map(|(_, bytes)| bytes.len() as u64)
            .sum::<u64>();
        let new_used = used
            .saturating_sub(old_size)
            .saturating_add(value.len() as u64);
        if new_used > quota {
            bail!("plugin validation storage quota exceeded");
        }
        entries.insert(map_key, value.to_vec());
        Ok(())
    }
    async fn kv_delete(&self, plugin_id: &str, key: &str) -> Result<()> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(plugin_id.to_string(), key.to_string()));
        Ok(())
    }
    fn record_http_request(&self, plugin_id: &str, capability: &str) {
        tracing::debug!(plugin = %plugin_id, capability, "plugin validation attempted outbound HTTP");
    }
    async fn credential_scope_allows(
        &self,
        _plugin_id: &str,
        _provider_id: &str,
        _scopes: &[String],
    ) -> Result<bool> {
        Ok(false)
    }
    fn log(&self, plugin_id: &str, level: &str, message: &str) {
        match level {
            "error" => tracing::error!(plugin = %plugin_id, "plugin validation: {message}"),
            "warn" => tracing::warn!(plugin = %plugin_id, "plugin validation: {message}"),
            "info" => tracing::info!(plugin = %plugin_id, "plugin validation: {message}"),
            "debug" => tracing::debug!(plugin = %plugin_id, "plugin validation: {message}"),
            _ => tracing::trace!(plugin = %plugin_id, "plugin validation: {message}"),
        }
    }
    async fn resolve_secret(
        &self,
        _plugin_id: &str,
        _provider_id: &str,
        _account_id: &str,
    ) -> Result<String> {
        bail!("plugin validation cannot access credentials")
    }
    async fn credential_egress_hosts(&self, _provider_id: &str) -> Result<Vec<String>> {
        bail!("plugin validation cannot access credential egress hosts")
    }
}

#[derive(Clone, Copy)]
enum ValidationWorld {
    Plugin,
    Auth,
    AccountModelSource,
    ProviderAdapter,
    ProviderAdapterV2,
    HealthV2,
}

/// The plugin manager. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct PluginManager {
    inner: Arc<Inner>,
}

struct CachedComponent {
    package_sha256: String,
    component: Arc<wasmtime::component::Component>,
}

struct Inner {
    runtime: PluginRuntime,
    pool: Pool,
    crypto: Arc<Crypto>,
    backing: Arc<Backing>,
    policy: HostPolicy,
    package_root: PathBuf,
    /// One compiled component per installed plugin, tagged by package SHA.
    /// Stores/instances are never cached because they hold invocation-specific
    /// authority, quotas, deadlines, and counters.
    component_cache: DashMap<String, CachedComponent>,
    semaphore: Arc<Semaphore>,
    plugin_semaphores: Mutex<HashMap<String, Arc<Semaphore>>>,
    metrics: Arc<PluginMetricRegistry>,
    /// Simple counters for the admin metrics surface (§18).
    invocations: std::sync::atomic::AtomicU64,
    faults: std::sync::atomic::AtomicU64,
    timeouts: std::sync::atomic::AtomicU64,
    cancellations: std::sync::atomic::AtomicU64,
    component_cache_hits: std::sync::atomic::AtomicU64,
    component_cache_misses: std::sync::atomic::AtomicU64,
}

impl PluginManager {
    pub fn new(
        pool: Pool,
        crypto: Arc<Crypto>,
        policy: HostPolicy,
        package_root: PathBuf,
    ) -> Result<Self> {
        std::fs::create_dir_all(&package_root).map_err(|e| {
            anyhow!(
                "creating plugin package store {}: {e}",
                package_root.display()
            )
        })?;
        let runtime = PluginRuntime::new()?;
        let metrics = Arc::new(PluginMetricRegistry::new());
        let backing = Arc::new(Backing {
            pool: pool.clone(),
            crypto: crypto.clone(),
            metrics: metrics.clone(),
        });
        Ok(PluginManager {
            inner: Arc::new(Inner {
                runtime,
                pool,
                crypto,
                backing,
                policy,
                package_root,
                component_cache: Default::default(),
                semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_INVOCATIONS)),
                plugin_semaphores: Mutex::new(HashMap::new()),
                metrics,
                invocations: Default::default(),
                faults: Default::default(),
                timeouts: Default::default(),
                cancellations: Default::default(),
                component_cache_hits: Default::default(),
                component_cache_misses: Default::default(),
            }),
        })
    }

    pub fn policy(&self) -> HostPolicy {
        self.inner.policy
    }

    pub fn counters(&self) -> PluginCounters {
        use std::sync::atomic::Ordering::Relaxed;
        PluginCounters {
            invocations: self.inner.invocations.load(Relaxed),
            faults: self.inner.faults.load(Relaxed),
            timeouts: self.inner.timeouts.load(Relaxed),
            cancellations: self.inner.cancellations.load(Relaxed),
            http_requests: self
                .inner
                .metrics
                .iter()
                .map(|entry| entry.value().http_requests.load(Relaxed))
                .sum(),
            component_cache_hits: self.inner.component_cache_hits.load(Relaxed),
            component_cache_misses: self.inner.component_cache_misses.load(Relaxed),
        }
    }

    pub fn metrics_for_plugin(&self, id: &str) -> PluginMetricsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;

        let mut totals = PluginCapabilityCounters::default();
        let mut by_capability = BTreeMap::new();
        for entry in self.inner.metrics.iter() {
            let (plugin_id, capability) = entry.key();
            if plugin_id != id {
                continue;
            }
            let cell = entry.value();
            let counters = PluginCapabilityCounters {
                invocations: cell.invocations.load(Relaxed),
                successes: cell.successes.load(Relaxed),
                faults: cell.faults.load(Relaxed),
                timeouts: cell.timeouts.load(Relaxed),
                cancellations: cell.cancellations.load(Relaxed),
                http_requests: cell.http_requests.load(Relaxed),
                duration_micros: cell.duration_micros.load(Relaxed),
            };
            totals.add_assign(&counters);
            by_capability.insert(capability.clone(), counters);
        }
        PluginMetricsSnapshot {
            totals,
            by_capability,
        }
    }

    fn remember_component(
        &self,
        plugin_id: &str,
        package_sha256: &str,
        component: Arc<wasmtime::component::Component>,
    ) {
        self.inner.component_cache.insert(
            plugin_id.to_string(),
            CachedComponent {
                package_sha256: package_sha256.to_string(),
                component,
            },
        );
    }

    /// Return compiled code for the active package. Fresh Stores and instances
    /// are still created for every invocation so runtime authority is never
    /// retained in the cache.
    fn compiled_component(&self, row: &PluginRow) -> Result<Arc<wasmtime::component::Component>> {
        use std::sync::atomic::Ordering::Relaxed;

        if let Some(cached) = self.inner.component_cache.get(&row.id) {
            if cached.package_sha256 == row.package_sha256 {
                self.inner.component_cache_hits.fetch_add(1, Relaxed);
                return Ok(cached.component.clone());
            }
        }

        self.inner.component_cache_misses.fetch_add(1, Relaxed);
        let component = Arc::new(self.inner.runtime.compile(&row.component)?);
        self.remember_component(&row.id, &row.package_sha256, component.clone());
        Ok(component)
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// Install (or upgrade) a plugin from package bytes. Verifies hash, parses
    /// and validates the manifest, checks signature, compiles the component,
    /// and stores it **installed-disabled** (§11).
    pub async fn install(
        &self,
        bytes: &[u8],
        expected_sha256: Option<&str>,
        trusted_keys: &[[u8; 32]],
        allow_untrusted_signature: bool,
    ) -> Result<InstallOutcome> {
        self.install_from_source(
            bytes,
            expected_sha256,
            trusted_keys,
            allow_untrusted_signature,
            "local",
        )
        .await
    }

    /// Install a package whose bytes were previously accepted (a retained
    /// `plugin_packages` row) — the recovery path after `remove`. The retained
    /// bytes are re-read from disk and re-hashed against the provenance row, so
    /// historical acceptance is not sufficient on its own. The signature was
    /// verified when the package was first accepted and the bytes are identical
    /// (hash-checked), so re-verification against trusted keys is unnecessary.
    pub async fn install_retained(&self, id: &str, sha256: &str) -> Result<InstallOutcome> {
        let (retained, _pkg, _validated) = self.load_retained_package(id, sha256).await?;
        let bytes = self.read_retained_bytes(&retained).await?;
        self.install_from_source(
            &bytes,
            Some(sha256),
            &[],
            true,
            &format!("retained:{}", retained.package_sha256),
        )
        .await
    }

    /// Install package bytes while retaining an operator-safe provenance label.
    pub async fn install_from_source(
        &self,
        bytes: &[u8],
        expected_sha256: Option<&str>,
        trusted_keys: &[[u8; 32]],
        allow_untrusted_signature: bool,
        source: &str,
    ) -> Result<InstallOutcome> {
        let pkg = package::read_package(bytes)?;
        if let Some(expected) = expected_sha256 {
            if !expected.eq_ignore_ascii_case(&pkg.package_sha256) {
                bail!(
                    "package hash mismatch: expected {expected}, computed {}",
                    pkg.package_sha256
                );
            }
        }
        let validated = package::validate_manifest(&pkg, self.inner.policy)?;
        let sig = package::verify_signature(&pkg, trusted_keys)?;
        if sig == SignatureStatus::Untrusted && !allow_untrusted_signature {
            bail!("package signature is present but not from a trusted publisher key");
        }
        // Compile now so a broken component is rejected before it is stored,
        // and retain the immutable compiled artifact after publication.
        let compiled = Arc::new(
            self.inner
                .runtime
                .compile(&pkg.component)
                .map_err(|e| anyhow!("{e}"))?,
        );
        self.validate_component_contract(
            &validated.manifest,
            &validated.effective,
            compiled.as_ref(),
        )
        .await?;

        // Preserve the exact accepted package before publishing its active
        // metadata. The filename is content-addressed so the version string
        // never becomes a filesystem path component.
        let package_path = self
            .persist_package(&validated.manifest.id, &pkg.package_sha256, bytes)
            .await?;

        // Installation and upgrade never grant authority. The operator must
        // explicitly approve the declared permission set before enablement.
        store::upsert_plugin(
            &self.inner.pool,
            &validated,
            &pkg.package_sha256,
            &pkg.component,
            sig.as_str(),
            &package_path,
            source,
        )
        .await?;

        self.remember_component(&validated.manifest.id, &pkg.package_sha256, compiled);

        Ok(InstallOutcome {
            id: validated.manifest.id.clone(),
            version: validated.manifest.version.clone(),
            package_sha256: pkg.package_sha256,
            signature: sig,
            provides: validated.manifest.provides.provided(),
        })
    }

    async fn persist_package(&self, plugin_id: &str, sha256: &str, bytes: &[u8]) -> Result<String> {
        let relative = PathBuf::from(plugin_id).join(format!("{sha256}.kxp"));
        let target = self.inner.package_root.join(&relative);
        let parent = target
            .parent()
            .ok_or_else(|| anyhow!("plugin package path has no parent"))?;

        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            anyhow!(
                "creating plugin package directory {}: {e}",
                parent.display()
            )
        })?;

        match tokio::fs::read(&target).await {
            Ok(existing) => {
                if existing != bytes {
                    bail!(
                        "plugin package store collision at {} for SHA-256 {}",
                        target.display(),
                        sha256
                    );
                }
                return Ok(relative.to_string_lossy().into_owned());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow!(
                    "reading existing plugin package {}: {e}",
                    target.display()
                ));
            }
        }

        let temp = target.with_extension(format!("kxp.tmp-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::write(&temp, bytes)
            .await
            .map_err(|e| anyhow!("writing plugin package {}: {e}", temp.display()))?;

        if let Err(rename_err) = tokio::fs::rename(&temp, &target).await {
            match tokio::fs::read(&target).await {
                Ok(existing) if existing == bytes => {
                    let _ = tokio::fs::remove_file(&temp).await;
                }
                _ => {
                    let _ = tokio::fs::remove_file(&temp).await;
                    return Err(anyhow!(
                        "publishing plugin package {}: {rename_err}",
                        target.display()
                    ));
                }
            }
        }

        Ok(relative.to_string_lossy().into_owned())
    }

    async fn load_retained_package(
        &self,
        id: &str,
        sha256: &str,
    ) -> Result<(store::PackageRow, Package, manifest::ValidatedManifest)> {
        let retained = store::get_package(&self.inner.pool, id, sha256)
            .await?
            .ok_or_else(|| anyhow!("retained package '{sha256}' not found for plugin '{id}'"))?;

        let bytes = self.read_retained_bytes(&retained).await?;
        let computed = package::sha256_hex(&bytes);
        if !computed.eq_ignore_ascii_case(&retained.package_sha256)
            || !computed.eq_ignore_ascii_case(sha256)
        {
            bail!(
                "retained package hash mismatch: expected {}, computed {}",
                retained.package_sha256,
                computed
            );
        }

        let pkg = package::read_package(&bytes)?;
        let validated = package::validate_manifest(&pkg, self.inner.policy)?;
        if validated.manifest.id != id {
            bail!(
                "retained package id mismatch: expected '{id}', package declares '{}'",
                validated.manifest.id
            );
        }
        if validated.manifest.version != retained.version {
            bail!(
                "retained package version mismatch: provenance says '{}', package declares '{}'",
                retained.version,
                validated.manifest.version
            );
        }

        Ok((retained, pkg, validated))
    }

    /// Read the on-disk bytes of a retained package row, rejecting any path
    /// that is not a plain relative file below the package root.
    async fn read_retained_bytes(&self, retained: &store::PackageRow) -> Result<Vec<u8>> {
        let relative = Path::new(&retained.package_path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            bail!("retained package path is invalid");
        }
        let path = self.inner.package_root.join(relative);
        tokio::fs::read(&path)
            .await
            .map_err(|e| anyhow!("reading retained package {}: {e}", path.display()))
    }

    /// Preview a retained package and the authority delta relative to the
    /// currently active manifest without changing runtime state.
    pub async fn rollback_preview(&self, id: &str, sha256: &str) -> Result<RollbackPreview> {
        let current = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        let current_manifest = current
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let (retained, pkg, validated) = self.load_retained_package(id, sha256).await?;
        let component = self.inner.runtime.compile(&pkg.component)?;
        self.validate_component_contract(&validated.manifest, &validated.effective, &component)
            .await?;

        Ok(RollbackPreview {
            id: id.to_string(),
            current_version: current.version,
            target_version: retained.version,
            package_sha256: retained.package_sha256,
            signature: retained.signature,
            source: retained.source,
            permissions: validated.manifest.permissions.clone(),
            permission_diff: permission_diff(
                &current_manifest.permissions,
                &validated.manifest.permissions,
            ),
            provides: validated.manifest.provides.provided(),
        })
    }

    /// Reactivate an exact retained package as the active plugin version.
    ///
    /// Historical acceptance is not enough by itself: the retained bytes are
    /// re-hashed, the manifest identity/version are checked against provenance,
    /// and the component is recompiled. Activation goes through `upsert_plugin`,
    /// which disables the plugin and clears all permission grants.
    pub async fn rollback(&self, id: &str, sha256: &str) -> Result<RollbackOutcome> {
        let current = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        if current.package_sha256.eq_ignore_ascii_case(sha256) {
            bail!("plugin '{id}' is already using package {sha256}");
        }

        let (retained, pkg, validated) = self.load_retained_package(id, sha256).await?;

        let compiled = Arc::new(
            self.inner
                .runtime
                .compile(&pkg.component)
                .map_err(|e| anyhow!("{e}"))?,
        );
        self.validate_component_contract(
            &validated.manifest,
            &validated.effective,
            compiled.as_ref(),
        )
        .await?;

        let source = format!("rollback:{}", retained.package_sha256);
        store::upsert_plugin(
            &self.inner.pool,
            &validated,
            &retained.package_sha256,
            &pkg.component,
            &retained.signature,
            &retained.package_path,
            &source,
        )
        .await?;

        self.remember_component(id, &retained.package_sha256, compiled);

        Ok(RollbackOutcome {
            id: id.to_string(),
            version: retained.version,
            package_sha256: retained.package_sha256,
            signature: retained.signature,
            provides: validated.manifest.provides.provided(),
        })
    }

    /// Root of the immutable package cache.
    pub fn package_root(&self) -> &Path {
        &self.inner.package_root
    }

    /// Install from a local file path. The computed SHA-256 is recorded (§11).
    pub async fn install_file(
        &self,
        path: &std::path::Path,
        trusted_keys: &[[u8; 32]],
        allow_untrusted_signature: bool,
    ) -> Result<InstallOutcome> {
        let bytes = std::fs::read(path).map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
        self.install(&bytes, None, trusted_keys, allow_untrusted_signature)
            .await
    }

    /// Enable an installed plugin, instantiating it once to prove it loads.
    pub async fn enable(&self, id: &str) -> Result<()> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let (validated, _) = self
            .validated_manifest_with_approved_permissions(id, manifest)
            .await?;
        let component = self.compiled_component(&row)?;
        self.validate_component_contract(
            &validated.manifest,
            &validated.effective,
            component.as_ref(),
        )
        .await?;
        store::set_enabled(&self.inner.pool, id, true).await?;
        store::clear_plugin_failures(&self.inner.pool, id).await?;
        Ok(())
    }

    pub async fn disable(&self, id: &str) -> Result<()> {
        store::set_enabled(&self.inner.pool, id, false).await?;
        Ok(())
    }

    /// Disable enabled plugins whose persisted manifest, approved grants, or
    /// active component no longer satisfy the current host contract.
    pub async fn reconcile_enabled_plugins(&self) -> Result<()> {
        let rows = store::list_plugins(&self.inner.pool).await?;
        for row in rows.into_iter().filter(|row| row.status().is_enabled()) {
            let validated = match row.manifest() {
                Some(manifest) => match self.validate_persisted_manifest(manifest) {
                    Ok(validated) => validated,
                    Err(error) => {
                        tracing::warn!(
                            plugin = %row.id,
                            error = %error,
                            "disabling plugin that fails startup validation"
                        );
                        store::set_enabled(&self.inner.pool, &row.id, false).await?;
                        continue;
                    }
                },
                None => {
                    tracing::warn!(plugin = %row.id, "disabling plugin with an unreadable manifest");
                    store::set_enabled(&self.inner.pool, &row.id, false).await?;
                    continue;
                }
            };
            let approved = self.approved_permissions(&row.id).await?;
            if !permission_grants_match(&permission_grants(&validated.manifest), &approved) {
                tracing::warn!(
                    plugin = %row.id,
                    "disabling plugin whose approved permissions no longer match its manifest"
                );
                store::set_enabled(&self.inner.pool, &row.id, false).await?;
                continue;
            }

            let contract_result = match self.compiled_component(&row) {
                Ok(component) => {
                    self.validate_component_contract(
                        &validated.manifest,
                        &validated.effective,
                        component.as_ref(),
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = contract_result {
                tracing::warn!(
                    plugin = %row.id,
                    error = %error,
                    "disabling plugin that fails startup component validation"
                );
                store::set_enabled(&self.inner.pool, &row.id, false).await?;
            }
        }
        Ok(())
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        store::delete_plugin(&self.inner.pool, id).await?;
        self.inner
            .plugin_semaphores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        self.inner
            .metrics
            .retain(|(plugin_id, _), _| plugin_id != id);
        self.inner.component_cache.remove(id);
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<PluginRow>> {
        store::list_plugins(&self.inner.pool).await
    }

    pub async fn get(&self, id: &str) -> Result<Option<PluginRow>> {
        store::get_plugin(&self.inner.pool, id).await
    }

    /// Return host-owned dashboard settings without revealing secret values.
    pub async fn ui_settings(&self, id: &str) -> Result<serde_json::Value> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;

        let mut fields = Vec::new();
        for setting in &manifest.ui.settings {
            let storage_key = format!("{CONFIG_PREFIX}{}", setting.key);
            let stored =
                store::kv_get(&self.inner.pool, &self.inner.crypto, id, &storage_key).await?;
            let configured = stored.is_some();
            let value = if setting.kind == "secret" {
                serde_json::Value::Null
            } else if let Some(bytes) = stored {
                let text = String::from_utf8(bytes)
                    .map_err(|_| anyhow!("stored setting '{}' is not utf-8", setting.key))?;
                if setting.kind == "boolean" {
                    serde_json::Value::Bool(text == "true")
                } else {
                    serde_json::Value::String(text)
                }
            } else if let Some(default) = &setting.default {
                if setting.kind == "boolean" {
                    serde_json::Value::Bool(default == "true")
                } else {
                    serde_json::Value::String(default.clone())
                }
            } else {
                serde_json::Value::Null
            };

            fields.push(serde_json::json!({
                "key": setting.key,
                "label": setting.label,
                "kind": setting.kind,
                "description": setting.description,
                "required": setting.required,
                "options": setting.options,
                "configured": configured,
                "value": value,
            }));
        }

        Ok(serde_json::json!({ "id": id, "settings": fields }))
    }

    /// Partially update host-owned plugin settings. Omitted keys are unchanged;
    /// null deletes an optional value. Secret values are never echoed back.
    pub async fn update_ui_settings(
        &self,
        id: &str,
        values: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let storage_quota = manifest::effective_limits(&manifest, self.inner.policy)?.storage;

        for (key, value) in values {
            let setting = manifest
                .ui
                .settings
                .iter()
                .find(|setting| setting.key == *key)
                .ok_or_else(|| anyhow!("unknown plugin setting '{key}'"))?;
            let storage_key = format!("{CONFIG_PREFIX}{key}");

            if value.is_null() {
                if setting.required {
                    bail!("required plugin setting '{key}' cannot be cleared");
                }
                store::kv_delete(&self.inner.pool, id, &storage_key).await?;
                continue;
            }

            let encoded = match setting.kind.as_str() {
                "boolean" => value
                    .as_bool()
                    .ok_or_else(|| anyhow!("plugin setting '{key}' must be boolean"))?
                    .to_string(),
                "text" | "secret" | "select" => {
                    let text = value
                        .as_str()
                        .ok_or_else(|| anyhow!("plugin setting '{key}' must be a string"))?;
                    if setting.required && text.trim().is_empty() {
                        bail!("required plugin setting '{key}' must not be empty");
                    }
                    if setting.kind == "select" && !setting.options.iter().any(|v| v == text) {
                        bail!("plugin setting '{key}' has an unsupported option");
                    }
                    text.to_string()
                }
                other => bail!("unsupported plugin setting kind '{other}'"),
            };

            if encoded.len() > 64 * 1024 {
                bail!("plugin setting '{key}' exceeds 64 KiB");
            }
            store::kv_put_limited(
                &self.inner.pool,
                &self.inner.crypto,
                id,
                &storage_key,
                encoded.as_bytes(),
                storage_quota,
            )
            .await?;
        }

        self.ui_settings(id).await
    }

    /// Explicitly approve the plugin's currently declared permission set.
    pub async fn approve_permissions(&self, id: &str) -> Result<Vec<PermissionGrant>> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let grants = permission_grants(&manifest);
        store::replace_permissions(&self.inner.pool, id, &grants).await?;
        Ok(grants)
    }

    /// Approve an explicit subset of the plugin's declared permissions. Each
    /// requested key must be declared by the manifest; the grant is written
    /// with exactly the requested scope (e.g. a subset of `network_hosts`), so
    /// the operator is not forced to rubber-stamp the full declaration.
    /// Enablement and runtime use still require the effective grant set to
    /// match the validated manifest exactly.
    pub async fn approve_permissions_scoped(
        &self,
        id: &str,
        network_hosts: Option<Vec<String>>,
        credential_scopes: Option<Vec<String>>,
        credential_read: Option<bool>,
    ) -> Result<Vec<PermissionGrant>> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let declared = &manifest.permissions;

        let mut grants = Vec::new();
        if let Some(hosts) = network_hosts {
            if hosts.is_empty() {
                bail!("network_hosts approval must not be empty when provided");
            }
            for host in &hosts {
                if !declared.network_hosts.iter().any(|d| d == host) {
                    bail!("network host '{host}' is not declared by the plugin manifest");
                }
            }
            grants.push(PermissionGrant {
                permission: "network_hosts".into(),
                value_json: serde_json::to_string(&hosts).unwrap_or_else(|_| "[]".into()),
            });
        }
        if let Some(scopes) = credential_scopes {
            if scopes.is_empty() {
                bail!("credential_scopes approval must not be empty when provided");
            }
            for scope in &scopes {
                if !declared.credential_scopes.iter().any(|d| d == scope) {
                    bail!("credential scope '{scope}' is not declared by the plugin manifest");
                }
            }
            grants.push(PermissionGrant {
                permission: "credential_scopes".into(),
                value_json: serde_json::to_string(&scopes).unwrap_or_else(|_| "[]".into()),
            });
        }
        if let Some(read) = credential_read {
            if read && !declared.credential_read {
                bail!("plugin manifest does not request credential_read");
            }
            if read {
                grants.push(PermissionGrant {
                    permission: "credential_read".into(),
                    value_json: "true".into(),
                });
            }
        }

        store::replace_permissions(&self.inner.pool, id, &grants).await?;
        Ok(grants)
    }

    /// Revoke one permission and immediately disable the plugin.
    pub async fn revoke_permission(&self, id: &str, permission: &str) -> Result<()> {
        if self.get(id).await?.is_none() {
            bail!("plugin '{id}' is not installed");
        }
        store::revoke_permission(&self.inner.pool, id, permission).await?;
        self.disable(id).await
    }

    /// Validate a persisted manifest against the current host contract.
    fn validate_persisted_manifest(
        &self,
        manifest: Manifest,
    ) -> Result<manifest::ValidatedManifest> {
        manifest::validate(manifest, self.inner.policy)
    }

    /// Validate a persisted manifest and return grants only when they exactly
    /// match its current permission declarations.
    async fn validated_manifest_with_approved_permissions(
        &self,
        id: &str,
        manifest: Manifest,
    ) -> Result<(manifest::ValidatedManifest, Vec<PermissionGrant>)> {
        let validated = self.validate_persisted_manifest(manifest)?;
        let requested = permission_grants(&validated.manifest);
        let approved = self.approved_permissions(id).await?;

        if !permission_grants_match(&requested, &approved) {
            bail!(
                "plugin '{id}' permissions are not approved for the current manifest; run kinetix plugin approve {id}"
            );
        }
        Ok((validated, approved))
    }

    async fn approved_permissions(&self, id: &str) -> Result<Vec<PermissionGrant>> {
        Ok(store::permissions(&self.inner.pool, id)
            .await?
            .into_iter()
            .map(|row| PermissionGrant {
                permission: row.permission,
                value_json: row.value_json,
            })
            .collect())
    }

    /// Read the plugin's host-stamped cached routing facts (§6.4) for the
    /// request path. Returns `(name, value_json, observed_at, max_age_ms)`.
    /// This never invokes the guest, so a `cached` fact provider cannot add
    /// network or wall-time cost to routing.
    pub async fn cached_facts(
        &self,
        id: &str,
    ) -> Result<Vec<(String, serde_json::Value, Option<String>, Option<u64>)>> {
        let entries = store::kv_list_prefix(
            &self.inner.pool,
            &self.inner.crypto,
            id,
            super::runtime::CACHE_PREFIX,
        )
        .await?;
        let mut out = Vec::new();
        for (key, bytes) in entries {
            let Ok(env) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let name = key
                .strip_prefix(super::runtime::CACHE_PREFIX)
                .unwrap_or(&key)
                .to_string();
            let value = env.get("value").cloned().unwrap_or(serde_json::Value::Null);
            let observed_at = env
                .get("observed_at")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let max_age_ms = env.get("max_age_ms").and_then(|v| v.as_u64());
            out.push((name, value, observed_at, max_age_ms));
        }
        Ok(out)
    }

    /// Whether a plugin is installed, enabled, and its circuit is not open.
    pub async fn is_usable(&self, id: &str) -> bool {
        let row = match self.get(id).await {
            Ok(Some(row)) if row.status().is_enabled() => row,
            _ => return false,
        };
        let Some(manifest) = row.manifest() else {
            return false;
        };
        if self
            .validated_manifest_with_approved_permissions(id, manifest)
            .await
            .is_err()
        {
            return false;
        }
        store::circuit_ready(&self.inner.pool, id)
            .await
            .unwrap_or(false)
    }

    /// Whether the plugin provides the named capability.
    pub async fn provides(&self, id: &str, capability: Capability, name: &str) -> bool {
        match self.get(id).await {
            Ok(Some(row)) => row
                .manifest()
                .map(|m| {
                    m.provides
                        .provided()
                        .iter()
                        .any(|p| p.capability == capability && p.name == name)
                })
                .unwrap_or(false),
            _ => false,
        }
    }

    /// Enabled plugins that declare the named read-only hook, newest id order.
    /// A hook is a side observer: if the plugin is disabled, faulted, or its
    /// circuit is open it simply does not run (core never fails a request on a
    /// hook's behalf, §6.6).
    pub async fn plugins_with_hook(&self, hook: &str) -> Vec<String> {
        let Ok(rows) = self.list().await else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        for row in rows {
            let declares = row
                .manifest()
                .map(|m| m.provides.hooks.iter().any(|h| h == hook))
                .unwrap_or(false);
            if declares && self.is_usable(&row.id).await {
                ids.push(row.id);
            }
        }
        ids
    }

    // -----------------------------------------------------------------------
    // Invocation plumbing
    // -----------------------------------------------------------------------

    fn plugin_semaphore(&self, id: &str) -> Arc<Semaphore> {
        let mut semaphores = self
            .inner
            .plugin_semaphores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        semaphores
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_CONCURRENT_INVOCATIONS_PER_PLUGIN)))
            .clone()
    }

    async fn acquire_invocation_permits(&self, id: &str) -> InvocationPermits {
        // Acquire the plugin-local slot first. A noisy plugin waiting on its
        // own limit must not reserve a global slot that another plugin could use.
        let plugin = self
            .plugin_semaphore(id)
            .acquire_owned()
            .await
            .expect("plugin invocation semaphore is never closed");
        let global = self
            .inner
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("global plugin invocation semaphore is never closed");
        InvocationPermits {
            _plugin: plugin,
            _global: global,
        }
    }

    async fn ensure_circuit_ready(&self, id: &str) -> Result<()> {
        if !store::circuit_ready(&self.inner.pool, id).await? {
            bail!("plugin '{id}' circuit is open or already half-open");
        }
        Ok(())
    }

    async fn claim_circuit_probe(&self, id: &str) -> Result<()> {
        if !store::claim_circuit_probe(&self.inner.pool, id).await? {
            bail!("plugin '{id}' circuit is open or another half-open probe is in flight");
        }
        Ok(())
    }

    fn new_store(
        &self,
        row: &PluginRow,
        limits: &manifest::EffectiveLimits,
        grants: &[PermissionGrant],
        adapter: bool,
        buffered_http_allowed: bool,
        capability: &str,
    ) -> wasmtime::Store<HostCtx> {
        // Runtime authority is derived only from approved grant rows.
        let mut network_hosts = Vec::new();
        let mut credential_scopes = Vec::new();
        let mut credential_read = false;
        for grant in grants {
            match grant.permission.as_str() {
                "network_hosts" => {
                    network_hosts = serde_json::from_str(&grant.value_json).unwrap_or_default();
                }
                "credential_scopes" => {
                    credential_scopes = serde_json::from_str(&grant.value_json).unwrap_or_default();
                }
                "credential_read" => {
                    credential_read = serde_json::from_str(&grant.value_json).unwrap_or(false);
                }
                _ => {}
            }
        }

        let ctx = HostCtx {
            plugin_id: row.id.clone(),
            capability: capability.to_string(),
            host_capability_mode: HostCapabilityMode::Normal,
            network_hosts,
            credential_read,
            credential_sign: !credential_scopes.is_empty(),
            credential_scopes,
            storage_quota: limits.storage,
            pending_cache: Default::default(),
            max_outbound_requests: limits.max_outbound_requests,
            max_http_body: limits.max_http_body,
            adapter_stream: adapter,
            buffered_http_allowed,
            allow_private_network: self.inner.policy.allow_private_network,
            http_timeout: Duration::from_millis(limits.wall_time_ms.max(1)),
            outbound_count: 0,
            backing: self.inner.backing.clone(),
            limits: wasmtime::StoreLimitsBuilder::new().build(),
        };
        // `new_store` replaces the placeholder store-limits field.
        self.inner.runtime.new_store(ctx, limits.memory)
    }

    fn new_validation_store(
        &self,
        plugin_id: &str,
        limits: &manifest::EffectiveLimits,
        capability: &str,
    ) -> wasmtime::Store<HostCtx> {
        let ctx = HostCtx {
            plugin_id: plugin_id.to_string(),
            capability: capability.to_string(),
            host_capability_mode: HostCapabilityMode::Normal,
            network_hosts: Vec::new(),
            credential_read: false,
            credential_sign: false,
            credential_scopes: Vec::new(),
            storage_quota: limits.storage,
            pending_cache: BTreeMap::new(),
            max_outbound_requests: 0,
            max_http_body: 0,
            adapter_stream: false,
            buffered_http_allowed: false,
            allow_private_network: false,
            http_timeout: Duration::from_millis(limits.wall_time_ms.max(1)),
            outbound_count: 0,
            backing: Arc::new(ValidationBacking::default()),
            limits: wasmtime::StoreLimitsBuilder::new().build(),
        };
        self.inner.runtime.new_store(ctx, limits.memory)
    }

    async fn validate_component_world(
        &self,
        plugin_id: &str,
        limits: &manifest::EffectiveLimits,
        component: &wasmtime::component::Component,
        linker: &wasmtime::component::Linker<HostCtx>,
        world: ValidationWorld,
    ) -> Result<()> {
        let world_name = match world {
            ValidationWorld::Plugin => "plugin",
            ValidationWorld::Auth => "plugin-auth",
            ValidationWorld::AccountModelSource => "plugin-model-source",
            ValidationWorld::ProviderAdapter => "plugin-adapter",
            ValidationWorld::ProviderAdapterV2 => "plugin-adapter-v2",
            ValidationWorld::HealthV2 => "plugin-health-v2",
        };
        let mut store = self.new_validation_store(plugin_id, limits, "validation");
        let _deadline = self.inner.runtime.arm_deadline(
            &mut store,
            Duration::from_millis(limits.wall_time_ms.max(1)),
        );
        match world {
            ValidationWorld::Plugin => {
                self.inner
                    .runtime
                    .instantiate(linker, &mut store, component)
                    .await?;
            }
            ValidationWorld::Auth => {
                self.inner
                    .runtime
                    .instantiate_auth(linker, &mut store, component)
                    .await?;
            }
            ValidationWorld::AccountModelSource => {
                self.inner
                    .runtime
                    .instantiate_model_source(linker, &mut store, component)
                    .await?;
            }
            ValidationWorld::ProviderAdapter => {
                self.inner
                    .runtime
                    .instantiate_adapter(linker, &mut store, component)
                    .await?;
            }
            ValidationWorld::ProviderAdapterV2 => {
                self.inner
                    .runtime
                    .instantiate_adapter_v2(linker, &mut store, component)
                    .await?;
            }
            ValidationWorld::HealthV2 => {
                self.inner
                    .runtime
                    .instantiate_health_v2(linker, &mut store, component)
                    .await?;
            }
        }
        tracing::debug!(plugin = %plugin_id, world = world_name, "plugin component world validated");
        Ok(())
    }

    async fn validate_component_contract(
        &self,
        manifest: &Manifest,
        limits: &manifest::EffectiveLimits,
        component: &wasmtime::component::Component,
    ) -> Result<()> {
        let provides = &manifest.provides;
        let has_plugin_world = !provides.credential_strategies.is_empty()
            || !provides.model_sources.is_empty()
            || !provides.health_probes.is_empty()
            || !provides.routing_facts.is_empty()
            || !provides.hooks.is_empty();
        let has_other_capability = provides
            .provided()
            .iter()
            .any(|provided| provided.capability != Capability::ProviderAdapter);
        let linker = self.inner.runtime.linker()?;

        if has_plugin_world {
            self.validate_component_world(
                &manifest.id,
                limits,
                component,
                &linker,
                ValidationWorld::Plugin,
            )
            .await
            .map_err(|e| {
                anyhow!(
                    "plugin '{}' failed plugin-world validation: {e}",
                    manifest.id
                )
            })?;
        }
        if !provides.auth_flows.is_empty() {
            self.validate_component_world(
                &manifest.id,
                limits,
                component,
                &linker,
                ValidationWorld::Auth,
            )
            .await
            .map_err(|e| anyhow!("plugin '{}' failed auth-world validation: {e}", manifest.id))?;
        }
        if !provides.account_model_sources.is_empty() {
            self.validate_component_world(
                &manifest.id,
                limits,
                component,
                &linker,
                ValidationWorld::AccountModelSource,
            )
            .await
            .map_err(|e| {
                anyhow!(
                    "plugin '{}' failed account model-source world validation: {e}",
                    manifest.id
                )
            })?;
        }
        if !provides.provider_adapters.is_empty() {
            let (world, world_name) = match manifest.api_major() {
                Some(1) => (ValidationWorld::ProviderAdapter, "provider-adapter"),
                Some(2) => (ValidationWorld::ProviderAdapterV2, "provider-adapter-v2"),
                Some(major) => bail!("unsupported plugin API major {major}"),
                None => bail!("plugin '{}' has invalid plugin_api", manifest.id),
            };
            let result = if manifest.api_major() == Some(2) && !has_other_capability {
                let adapter_linker = self.inner.runtime.adapter_v2_linker();
                self.validate_component_world(
                    &manifest.id,
                    limits,
                    component,
                    &adapter_linker,
                    world,
                )
                .await
            } else {
                self.validate_component_world(&manifest.id, limits, component, &linker, world)
                    .await
            };
            result.map_err(|e| {
                anyhow!(
                    "plugin '{}' failed {world_name} world validation: {e}",
                    manifest.id
                )
            })?;
        }
        if self.inner.runtime.has_health_probe_v2(component) {
            self.validate_component_world(
                &manifest.id,
                limits,
                component,
                &linker,
                ValidationWorld::HealthV2,
            )
            .await
            .map_err(|e| {
                anyhow!(
                    "plugin '{}' failed plugin-health-v2 world validation: {e}",
                    manifest.id
                )
            })?;
        }
        Ok(())
    }

    /// Prepare the optional v2 health world when exported, otherwise the legacy plugin world.
    async fn prepare_health_probe(&self, id: &str) -> Result<HealthPrepared> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        if !row.status().is_enabled() {
            bail!("plugin '{id}' is not enabled");
        }
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let (validated, grants) = self
            .validated_manifest_with_approved_permissions(id, manifest)
            .await?;
        self.ensure_circuit_ready(id).await?;
        let limits = validated.effective;
        let component = self.compiled_component(&row)?;
        let linker = self.inner.runtime.linker()?;
        let mut store = self.new_store(&row, &limits, &grants, false, true, "health_probe");
        let plugin = if self.inner.runtime.has_health_probe_v2(component.as_ref()) {
            HealthProbePlugin::V2(
                self.inner
                    .runtime
                    .instantiate_health_v2(&linker, &mut store, component.as_ref())
                    .await?,
            )
        } else {
            HealthProbePlugin::Legacy(
                self.inner
                    .runtime
                    .instantiate(&linker, &mut store, component.as_ref())
                    .await?,
            )
        };
        self.claim_circuit_probe(id).await?;
        Ok(HealthPrepared { store, plugin })
    }

    /// Prepare a ready-to-call instance for a plugin.
    async fn prepare(
        &self,
        id: &str,
        buffered_http_allowed: bool,
        capability: &str,
    ) -> Result<Prepared> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        if !row.status().is_enabled() {
            bail!("plugin '{id}' is not enabled");
        }
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let (validated, grants) = self
            .validated_manifest_with_approved_permissions(id, manifest)
            .await?;
        self.ensure_circuit_ready(id).await?;
        let limits = validated.effective;
        let component = self.compiled_component(&row)?;
        let linker = self.inner.runtime.linker()?;
        let mut store = self.new_store(
            &row,
            &limits,
            &grants,
            false,
            buffered_http_allowed,
            capability,
        );
        let plugin = self
            .inner
            .runtime
            .instantiate(&linker, &mut store, component.as_ref())
            .await?;
        self.claim_circuit_probe(id).await?;
        Ok(Prepared {
            store,
            plugin,
            wall_time: Duration::from_millis(limits.wall_time_ms),
        })
    }

    /// Instantiate the optional account-aware model discovery world.
    async fn prepare_model_source(&self, id: &str) -> Result<ModelSourcePrepared> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        if !row.status().is_enabled() {
            bail!("plugin '{id}' is not enabled");
        }
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let (validated, grants) = self
            .validated_manifest_with_approved_permissions(id, manifest)
            .await?;
        let limits = validated.effective;
        let component = self.inner.runtime.compile(&row.component)?;
        let linker = self.inner.runtime.linker()?;
        let mut store = self.new_store(&row, &limits, &grants, false, true, "model_source");
        let plugin = self
            .inner
            .runtime
            .instantiate_model_source(&linker, &mut store, &component)
            .await?;
        Ok(ModelSourcePrepared {
            store,
            plugin,
            wall_time: Duration::from_millis(limits.wall_time_ms),
        })
    }

    /// Instantiate the optional account-authorization world for one call.
    async fn prepare_auth(&self, id: &str) -> Result<AuthPrepared> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        if !row.status().is_enabled() {
            bail!("plugin '{id}' is not enabled");
        }
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let (validated, grants) = self
            .validated_manifest_with_approved_permissions(id, manifest)
            .await?;
        self.ensure_circuit_ready(id).await?;
        let limits = validated.effective;
        let component = self.compiled_component(&row)?;
        let linker = self.inner.runtime.linker()?;
        let mut store = self.new_store(&row, &limits, &grants, false, true, "auth_flow");
        let plugin = self
            .inner
            .runtime
            .instantiate_auth(&linker, &mut store, component.as_ref())
            .await?;
        self.claim_circuit_probe(id).await?;
        Ok(AuthPrepared {
            store,
            plugin,
            wall_time: Duration::from_millis(limits.wall_time_ms),
        })
    }

    /// Start a named plugin-provided account authorization flow.
    pub async fn auth_begin(
        &self,
        id: &str,
        flow_name: &str,
        redirect_uri: &str,
        state: &str,
        pkce_challenge: Option<&str>,
    ) -> Result<String, PluginFault> {
        if !self.provides(id, Capability::AuthFlow, flow_name).await {
            return Err(PluginFault::InvalidResult(format!(
                "plugin '{id}' does not provide auth flow '{flow_name}'"
            )));
        }
        let started = self.bump_invocation(id, "auth_flow");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut prepared = self
            .prepare_auth(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = prepared.plugin;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut prepared.store, prepared.wall_time);
        let result = plugin
            .auth_flow()
            .call_begin(
                &mut prepared.store,
                flow_name,
                redirect_uri,
                state,
                pkce_challenge,
            )
            .await
            .map_err(map_call_error)
            .and_then(map_auth_result);
        self.settle(id, "auth_flow", started, result).await
    }

    /// Exchange a browser callback code for host-persistable credential JSON.
    pub async fn auth_exchange(
        &self,
        id: &str,
        flow_name: &str,
        code: &str,
        redirect_uri: &str,
        pkce_verifier: Option<&str>,
    ) -> Result<
        crate::plugins::runtime::auth_bindings::kinetix::plugin::types::AuthResult,
        PluginFault,
    > {
        if !self.provides(id, Capability::AuthFlow, flow_name).await {
            return Err(PluginFault::InvalidResult(format!(
                "plugin '{id}' does not provide auth flow '{flow_name}'"
            )));
        }
        let started = self.bump_invocation(id, "auth_flow");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut prepared = self
            .prepare_auth(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = prepared.plugin;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut prepared.store, prepared.wall_time);
        let result = plugin
            .auth_flow()
            .call_exchange(
                &mut prepared.store,
                flow_name,
                code,
                redirect_uri,
                pkce_verifier,
            )
            .await
            .map_err(map_call_error)
            .and_then(map_auth_result);
        self.settle(id, "auth_flow", started, result).await
    }

    // -----------------------------------------------------------------------
    // ProviderAdapter invocation (§6.3, §7.1)
    //
    // The adapter world is bound separately so the buffered host-http import can
    // never serve as the adapter transport. Adapters are synchronous translation
    // functions; the manager exposes async methods and `PluginAdapter` (the sync
    // `Adapter` impl) bridges them via `block_in_place`.
    // -----------------------------------------------------------------------

    /// Instantiate the adapter world for a plugin.
    async fn prepare_adapter(&self, id: &str) -> Result<AdapterPrepared> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        if !row.status().is_enabled() {
            bail!("plugin '{id}' is not enabled");
        }
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let api_major = manifest.api_major();
        let has_other_capability = manifest
            .provides
            .provided()
            .iter()
            .any(|provided| provided.capability != Capability::ProviderAdapter);
        let (validated, grants) = self
            .validated_manifest_with_approved_permissions(id, manifest)
            .await?;
        self.ensure_circuit_ready(id).await?;
        let limits = validated.effective;
        let component = self.compiled_component(&row)?;
        let linker = if api_major == Some(2) && !has_other_capability {
            self.inner.runtime.adapter_v2_linker()
        } else {
            self.inner.runtime.linker()?
        };
        let mut store = self.new_store(&row, &limits, &grants, true, false, "provider_adapter");
        if api_major == Some(2) {
            store.data_mut().host_capability_mode = HostCapabilityMode::ApiV2Adapter;
        }
        let plugin = match api_major {
            Some(1) => AdapterGuest::Api1(
                self.inner
                    .runtime
                    .instantiate_adapter(&linker, &mut store, component.as_ref())
                    .await?,
            ),
            Some(2) => AdapterGuest::Api2(
                self.inner
                    .runtime
                    .instantiate_adapter_v2(&linker, &mut store, component.as_ref())
                    .await?,
            ),
            Some(major) => bail!("unsupported plugin API major {major}"),
            None => bail!("plugin '{id}' has invalid plugin_api"),
        };
        self.claim_circuit_probe(id).await?;
        Ok(AdapterPrepared {
            store,
            plugin,
            wall_time: Duration::from_millis(limits.wall_time_ms),
        })
    }

    /// Replace raw client session values with stable host-keyed identities
    /// before passing session context across a plugin ABI boundary.
    fn opaque_plugin_session_context(&self, raw_session: Option<&str>) -> Option<String> {
        raw_session.map(|session| self.inner.crypto.opaque_plugin_session_identity(session))
    }

    pub async fn adapter_wire_format(&self, id: &str) -> Result<String, PluginFault> {
        let started = self.bump_invocation(id, "provider_adapter");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare_adapter(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let res = plugin.call_wire_format(&mut p.store).await;
        self.settle_cancellable(id, "provider_adapter", started, &guard, res)
            .await
    }

    pub async fn adapter_build_url(
        &self,
        id: &str,
        provider_json: &str,
        model_json: &str,
    ) -> Result<String, PluginFault> {
        let started = self.bump_invocation(id, "provider_adapter");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare_adapter(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let res = plugin
            .call_build_url(&mut p.store, provider_json, model_json)
            .await;
        self.settle_cancellable(id, "provider_adapter", started, &guard, res)
            .await
    }

    pub async fn adapter_apply_auth(
        &self,
        id: &str,
        provider_json: &str,
        credential: &str,
        raw_session_context: Option<&str>,
    ) -> Result<String, PluginFault> {
        let started = self.bump_invocation(id, "provider_adapter");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare_adapter(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let opaque_session_context = self.opaque_plugin_session_context(raw_session_context);
        let res = plugin
            .call_apply_auth(
                &mut p.store,
                provider_json,
                credential,
                opaque_session_context.as_deref(),
            )
            .await;
        self.settle_cancellable(id, "provider_adapter", started, &guard, res)
            .await
    }

    pub async fn adapter_build_body(
        &self,
        id: &str,
        request_json: &str,
        provider_json: &str,
        model_json: &str,
        raw_session_context: Option<&str>,
    ) -> Result<String, PluginFault> {
        let started = self.bump_invocation(id, "provider_adapter");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare_adapter(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let opaque_session_context = self.opaque_plugin_session_context(raw_session_context);
        let res = plugin
            .call_build_body(
                &mut p.store,
                request_json,
                provider_json,
                model_json,
                opaque_session_context.as_deref(),
            )
            .await;
        self.settle_cancellable(id, "provider_adapter", started, &guard, res)
            .await
    }

    pub async fn adapter_classify_error(
        &self,
        id: &str,
        status: u16,
        body: &str,
        headers_json: &str,
    ) -> Result<String, PluginFault> {
        let started = self.bump_invocation(id, "provider_adapter");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare_adapter(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let res = plugin
            .call_classify_error(&mut p.store, status, body, headers_json)
            .await;
        self.settle_cancellable(id, "provider_adapter", started, &guard, res)
            .await
    }

    pub async fn adapter_parse_stream_chunk(
        &self,
        id: &str,
        data: &str,
    ) -> Result<String, PluginFault> {
        let started = self.bump_invocation(id, "provider_adapter");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare_adapter(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let res = plugin.call_parse_stream_chunk(&mut p.store, data).await;
        self.settle_cancellable(id, "provider_adapter", started, &guard, res)
            .await
    }

    pub async fn adapter_parse_full_response(
        &self,
        id: &str,
        body_json: &str,
    ) -> Result<String, PluginFault> {
        let started = self.bump_invocation(id, "provider_adapter");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare_adapter(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let res = plugin
            .call_parse_full_response(&mut p.store, body_json)
            .await;
        self.settle_cancellable(id, "provider_adapter", started, &guard, res)
            .await
    }

    fn observe_duration(
        &self,
        id: &str,
        capability: &str,
        started: Instant,
    ) -> Arc<PluginMetricCell> {
        use std::sync::atomic::Ordering::Relaxed;
        let cell = metric_cell(&self.inner.metrics, id, capability);
        let micros = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        cell.duration_micros.fetch_add(micros, Relaxed);
        cell
    }

    /// Record a successful invocation, closing the breaker.
    async fn record_success(&self, id: &str, capability: &str, started: Instant) {
        use std::sync::atomic::Ordering::Relaxed;
        self.observe_duration(id, capability, started)
            .successes
            .fetch_add(1, Relaxed);
        let _ = store::clear_plugin_failures(&self.inner.pool, id).await;
    }

    /// Record a fault and trip the breaker when the threshold is reached (§15).
    async fn record_fault(
        &self,
        id: &str,
        capability: &str,
        started: Instant,
        fault: &PluginFault,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        self.inner.faults.fetch_add(1, Relaxed);
        let cell = self.observe_duration(id, capability, started);
        cell.faults.fetch_add(1, Relaxed);
        if matches!(fault, PluginFault::Timeout) {
            self.inner.timeouts.fetch_add(1, Relaxed);
            cell.timeouts.fetch_add(1, Relaxed);
        }
        if !fault.counts_against_circuit() {
            let _ = store::clear_plugin_failures(&self.inner.pool, id).await;
            return;
        }
        let _ = store::record_plugin_failure(
            &self.inner.pool,
            id,
            CIRCUIT_FAULT_THRESHOLD,
            CIRCUIT_OPEN_SECS,
            fault.code(),
        )
        .await;
    }

    fn bump_invocation(&self, id: &str, capability: &str) -> Instant {
        use std::sync::atomic::Ordering::Relaxed;
        self.inner.invocations.fetch_add(1, Relaxed);
        metric_cell(&self.inner.metrics, id, capability)
            .invocations
            .fetch_add(1, Relaxed);
        Instant::now()
    }

    // -----------------------------------------------------------------------
    // Capability invocations
    // -----------------------------------------------------------------------

    /// CredentialStrategy::resolve (§6.1). Returns the opaque lease only; the
    /// secret never crosses back to core from a plugin in this path.
    pub async fn credential_resolve(
        &self,
        id: &str,
        provider_id: &str,
        account_id: &str,
        account_label: &str,
    ) -> Result<wit::types::CredentialLease, PluginFault> {
        let started = self.bump_invocation(id, "credential_strategy");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare(id, true, "credential_strategy")
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let res = plugin
            .credential_strategy()
            .call_resolve(&mut p.store, provider_id, account_id, account_label)
            .await
            .map_err(map_call_error)
            .and_then(map_plugin_result);
        self.settle(id, "credential_strategy", started, res).await
    }

    /// CredentialStrategy::rotate (§6.1). Forces the guest to renew the
    /// account credential before core resolves a fresh lease.
    pub async fn credential_rotate(
        &self,
        id: &str,
        provider_id: &str,
        account_id: &str,
    ) -> Result<(), PluginFault> {
        let started = self.bump_invocation(id, "credential_strategy");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare(id, true, "credential_strategy")
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let res = plugin
            .credential_strategy()
            .call_rotate(&mut p.store, provider_id, account_id)
            .await
            .map_err(map_call_error)
            .and_then(map_plugin_result);
        self.settle(id, "credential_strategy", started, res).await
    }

    /// ModelSource::discover (§6.2).
    pub async fn model_discover(
        &self,
        id: &str,
        provider_id: &str,
        base_url: &str,
        models_path: &str,
    ) -> Result<Vec<wit::types::DiscoveredModel>, PluginFault> {
        let started = self.bump_invocation(id, "model_source");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare(id, true, "model_source")
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut p.store, Duration::from_secs(30));
        let res = plugin
            .model_source()
            .call_discover(&mut p.store, provider_id, base_url, models_path)
            .await
            .map_err(map_call_error)
            .and_then(map_plugin_result);
        self.settle(id, "model_source", started, res).await
    }

    /// Account-aware model discovery. The explicit account reference lets the
    /// host enforce credential scope before any secret reaches the guest.
    pub async fn account_model_discover(
        &self,
        id: &str,
        provider_id: &str,
        account_id: &str,
        base_url: &str,
        models_path: &str,
    ) -> Result<Vec<wit::types::DiscoveredModel>, PluginFault> {
        let started = self.bump_invocation(id, "account_model_source");
        let _permit = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare_model_source(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let account =
            crate::plugins::runtime::model_source_bindings::kinetix::plugin::types::AccountRef {
                provider_id: provider_id.to_string(),
                account_id: account_id.to_string(),
            };
        let result = plugin
            .account_model_source()
            .call_discover(&mut p.store, provider_id, &account, base_url, models_path)
            .await
            .map_err(map_call_error)
            .and_then(map_account_model_result)
            .map(|models| {
                models
                    .into_iter()
                    .map(|model| wit::types::DiscoveredModel {
                        id: model.id,
                        display_name: model.display_name,
                        context_window: model.context_window,
                        max_output_tokens: model.max_output_tokens,
                        capabilities_json: model.capabilities_json,
                        raw_metadata: model.raw_metadata,
                    })
                    .collect()
            });
        self.settle(id, "account_model_source", started, result)
            .await
    }

    /// HealthProbe::probe (§6.5), preferring structured v2 snapshots when exported.
    pub async fn health_probe_with_snapshots(
        &self,
        id: &str,
        provider_id: &str,
        account_id: &str,
    ) -> Result<HealthProbeObservation, PluginFault> {
        let started = self.bump_invocation(id, "health_probe");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare_health_probe(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut p.store, Duration::from_secs(10));
        let result = match p.plugin {
            HealthProbePlugin::Legacy(plugin) => plugin
                .health_probe()
                .call_probe(&mut p.store, provider_id, account_id)
                .await
                .map_err(map_call_error)
                .and_then(map_plugin_result)
                .map(|observation| HealthProbeObservation {
                    observation,
                    quota_snapshots: None,
                }),
            HealthProbePlugin::V2(plugin) => plugin
                .health_probe_v2()
                .call_probe(&mut p.store, provider_id, account_id)
                .await
                .map_err(map_call_error)
                .and_then(map_health_v2_result)
                .map(|observation| {
                    let health_v2_wit::types::HealthObservationV2 {
                        state,
                        quota_state,
                        reset_at,
                        retry_after,
                        detail_code,
                        quota_snapshots,
                    } = observation;
                    HealthProbeObservation {
                        observation: wit::types::HealthObservation {
                            state,
                            quota_state,
                            reset_at,
                            retry_after,
                            detail_code,
                        },
                        quota_snapshots: Some(quota_snapshots),
                    }
                }),
        };
        self.settle(id, "health_probe", started, result).await
    }

    /// Legacy health projection retained for credential-strategy callers.
    pub async fn health_probe(
        &self,
        id: &str,
        provider_id: &str,
        account_id: &str,
    ) -> Result<wit::types::HealthObservation, PluginFault> {
        self.health_probe_with_snapshots(id, provider_id, account_id)
            .await
            .map(|result| result.observation)
    }

    /// RoutingFacts::facts (§6.4). `request_json` must carry only request/config
    /// facts core already knows. `cancelled` is polled while the guest runs; if
    /// the client disconnects the guest is epoch-interrupted and the outcome is
    /// reported as [`PluginFault::Cancelled`], never a fault (§7.2).
    pub async fn routing_facts_cancellable(
        &self,
        id: &str,
        request_json: &str,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Vec<wit::types::RoutingFact>, PluginFault> {
        let started = self.bump_invocation(id, "routing_facts");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare(id, false, "routing_facts")
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let guard = rt.arm_deadline(&mut p.store, Duration::from_millis(25));
        let watchdog = spawn_cancel_watchdog(guard.cancelled.clone(), cancelled);
        let res = plugin
            .routing_facts()
            .call_facts(&mut p.store, request_json)
            .await
            .map_err(map_call_error)
            .and_then(map_plugin_result);
        watchdog.abort();
        self.settle_cancellable(id, "routing_facts", started, &guard, res)
            .await
    }

    /// RoutingFacts::facts (§6.4). `request_json` must carry only request/config
    /// facts core already knows.
    pub async fn routing_facts(
        &self,
        id: &str,
        request_json: &str,
    ) -> Result<Vec<wit::types::RoutingFact>, PluginFault> {
        let started = self.bump_invocation(id, "routing_facts");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare(id, false, "routing_facts")
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut p.store, Duration::from_millis(25));
        let res = plugin
            .routing_facts()
            .call_facts(&mut p.store, request_json)
            .await
            .map_err(map_call_error)
            .and_then(map_plugin_result);
        self.settle(id, "routing_facts", started, res).await
    }

    /// Refresh the complete cached routing-fact snapshot off the request path.
    ///
    /// Cached mode may use approved buffered HTTP here. Facts returned from the
    /// guest and facts published through `cache_set` are merged into one
    /// validated, host-stamped snapshot and committed atomically.
    pub async fn refresh_cached_routing_facts(&self, id: &str) -> Result<usize, PluginFault> {
        let row = self
            .get(id)
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?
            .ok_or_else(|| PluginFault::Internal(format!("plugin '{id}' is not installed")))?;
        let manifest = row.manifest().ok_or_else(|| {
            PluginFault::Internal(format!("plugin '{id}' has an unreadable manifest"))
        })?;
        if manifest.routing_facts_mode != "cached" || manifest.provides.routing_facts.is_empty() {
            return Err(PluginFault::InvalidResult(
                "cached routing-fact refresh requested for a plugin that does not declare cached routing facts"
                    .into(),
            ));
        }
        let limits = manifest::effective_limits(&manifest, self.inner.policy)
            .map_err(|e| PluginFault::Internal(e.to_string()))?;

        let started = self.bump_invocation(id, "routing_facts.refresh");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare(id, true, "routing_facts.refresh")
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let call = plugin
            .routing_facts()
            .call_facts(&mut p.store, r#"{"kind":"background_refresh"}"#)
            .await
            .map_err(map_call_error)
            .and_then(map_plugin_result);
        let pending = std::mem::take(&mut p.store.data_mut().pending_cache);
        drop(guard);

        let result = match call {
            Ok(facts) => match build_cached_fact_snapshot(facts, pending, &crate::db::now_iso()) {
                Ok(snapshot) => match store::kv_replace_prefix_limited(
                    &self.inner.pool,
                    &self.inner.crypto,
                    id,
                    super::runtime::CACHE_PREFIX,
                    &snapshot,
                    limits.storage,
                )
                .await
                {
                    Ok(()) => Ok(snapshot.len()),
                    Err(error) => Err(PluginFault::Internal(format!(
                        "persisting cached routing fact snapshot: {error}"
                    ))),
                },
                Err(fault) => Err(fault),
            },
            Err(fault) => Err(fault),
        };

        self.settle(id, "routing_facts.refresh", started, result)
            .await
    }

    /// Read-only hook: on_request_normalized (§6.6).
    pub async fn hook_request_normalized(
        &self,
        id: &str,
        request_json: &str,
    ) -> Result<(), PluginFault> {
        let started = self.bump_invocation(id, "hook.on_request_normalized");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare(id, true, "hook.on_request_normalized")
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut p.store, Duration::from_millis(25));
        let res = plugin
            .hooks()
            .call_on_request_normalized(&mut p.store, request_json)
            .await
            .map_err(map_call_error)
            .and_then(map_plugin_result);
        self.settle(id, "hook.on_request_normalized", started, res)
            .await
    }

    /// Read-only hook: on_target_candidate (§6.6).
    pub async fn hook_target_candidate(
        &self,
        id: &str,
        target_json: &str,
    ) -> Result<(), PluginFault> {
        let started = self.bump_invocation(id, "hook.on_target_candidate");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare(id, true, "hook.on_target_candidate")
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut p.store, Duration::from_millis(25));
        let res = plugin
            .hooks()
            .call_on_target_candidate(&mut p.store, target_json)
            .await
            .map_err(map_call_error)
            .and_then(map_plugin_result);
        self.settle(id, "hook.on_target_candidate", started, res)
            .await
    }

    /// Fire-and-forget hook: on_usage_finalized (§6.6). Runs off the request
    /// path; the caller must never await it inline.
    pub async fn hook_usage_finalized(
        &self,
        id: &str,
        usage_json: &str,
    ) -> Result<(), PluginFault> {
        let started = self.bump_invocation(id, "hook.on_usage_finalized");
        let _permits = self.acquire_invocation_permits(id).await;
        let mut p = self
            .prepare(id, true, "hook.on_usage_finalized")
            .await
            .map_err(|e| PluginFault::Internal(e.to_string()))?;
        let plugin = p.plugin;
        let rt = self.inner.runtime.clone();
        let _guard = rt.arm_deadline(&mut p.store, p.wall_time);
        let res = plugin
            .hooks()
            .call_on_usage_finalized(&mut p.store, usage_json)
            .await
            .map_err(map_call_error)
            .and_then(map_plugin_result);
        self.settle(id, "hook.on_usage_finalized", started, res)
            .await
    }

    /// Validate an installed plugin by instantiating it (§11 self-check).
    pub async fn validate(&self, id: &str) -> Result<Vec<Provided>> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("plugin '{id}' is not installed"))?;
        let manifest = row
            .manifest()
            .ok_or_else(|| anyhow!("plugin '{id}' has an unreadable manifest"))?;
        let validated = self.validate_persisted_manifest(manifest)?;
        let component = self.compiled_component(&row)?;
        self.validate_component_contract(
            &validated.manifest,
            &validated.effective,
            component.as_ref(),
        )
        .await?;
        Ok(validated.manifest.provides.provided())
    }

    /// Resolve a `plugin:<id>/<capability>` reference to an enabled plugin that
    /// provides it. `None` means the binding is unsatisfied (fail closed, §6.0).
    pub async fn resolve_binding(&self, reference: &str, capability: Capability) -> Option<String> {
        let r = super::types::PluginRef::parse(reference)?;
        if self.provides(&r.plugin_id, capability, &r.capability).await
            && self.is_usable(&r.plugin_id).await
        {
            Some(r.plugin_id)
        } else {
            None
        }
    }

    async fn settle<T>(
        &self,
        id: &str,
        capability: &str,
        started: Instant,
        res: Result<T, PluginFault>,
    ) -> Result<T, PluginFault> {
        match res {
            Ok(v) => {
                self.record_success(id, capability, started).await;
                Ok(v)
            }
            Err(fault) => {
                self.record_fault(id, capability, started, &fault).await;
                Err(fault)
            }
        }
    }

    /// Cancellation-aware settlement for request-path plugin calls.
    async fn settle_cancellable<T>(
        &self,
        id: &str,
        capability: &str,
        started: Instant,
        guard: &DeadlineGuard,
        res: Result<T, PluginFault>,
    ) -> Result<T, PluginFault> {
        match res {
            Ok(v) => {
                self.record_success(id, capability, started).await;
                Ok(v)
            }
            Err(fault) => {
                if guard.is_cancelled() {
                    use std::sync::atomic::Ordering::Relaxed;
                    self.inner.cancellations.fetch_add(1, Relaxed);
                    self.observe_duration(id, capability, started)
                        .cancellations
                        .fetch_add(1, Relaxed);
                    let _ =
                        store::reopen_plugin_circuit(&self.inner.pool, id, CIRCUIT_OPEN_SECS).await;
                    return Err(PluginFault::Cancelled);
                }
                self.record_fault(id, capability, started, &fault).await;
                Err(fault)
            }
        }
    }
}

fn build_cached_fact_snapshot(
    facts: Vec<wit::types::RoutingFact>,
    pending: std::collections::BTreeMap<String, (String, u64)>,
    observed_at: &str,
) -> Result<Vec<(String, Vec<u8>)>, PluginFault> {
    if facts.len().saturating_add(pending.len()) > 256 {
        return Err(PluginFault::InvalidResult(
            "cached routing fact count exceeds 256".into(),
        ));
    }
    let mut names = std::collections::HashSet::new();
    let mut snapshot = Vec::with_capacity(facts.len() + pending.len());

    let mut add = |name: String, value_json: String, max_age_ms: u64| -> Result<(), PluginFault> {
        if name.is_empty()
            || name.len() > 128
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err(PluginFault::InvalidResult(format!(
                "invalid cached routing fact name '{name}'"
            )));
        }
        if !names.insert(name.clone()) {
            return Err(PluginFault::InvalidResult(format!(
                "duplicate cached routing fact '{name}'"
            )));
        }
        let value: serde_json::Value = serde_json::from_str(&value_json).map_err(|error| {
            PluginFault::InvalidResult(format!(
                "cached routing fact '{name}' has invalid JSON: {error}"
            ))
        })?;
        if max_age_ms == 0 || max_age_ms > 24 * 60 * 60 * 1000 {
            return Err(PluginFault::InvalidResult(format!(
                "cached routing fact '{name}' max_age_ms is out of range"
            )));
        }
        let envelope = serde_json::json!({
            "value": value,
            "observed_at": observed_at,
            "max_age_ms": max_age_ms,
        })
        .to_string();
        snapshot.push((
            format!("{}{}", super::runtime::CACHE_PREFIX, name),
            envelope.into_bytes(),
        ));
        Ok(())
    };

    for fact in facts {
        let max_age_ms = fact.max_age_ms.ok_or_else(|| {
            PluginFault::InvalidResult(format!(
                "cached routing fact '{}' must declare max_age_ms",
                fact.name
            ))
        })?;
        add(fact.name, fact.value_json, max_age_ms)?;
    }
    for (name, (value_json, max_age_ms)) in pending {
        add(name, value_json, max_age_ms)?;
    }

    Ok(snapshot)
}

/// Poll an external cancellation flag while a guest call runs and, once set,
/// flip the guard's cancelled flag. The guard's epoch ticker then interrupts the
/// guest on its next tick (§7.2). Returns the watchdog task to abort after the
/// call completes.
fn spawn_cancel_watchdog(
    guard_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    external: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if external.load(std::sync::atomic::Ordering::SeqCst) {
                guard_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
}

struct InvocationPermits {
    _plugin: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
}

pub struct HealthProbeObservation {
    pub observation: wit::types::HealthObservation,
    pub quota_snapshots: Option<Vec<health_v2_wit::types::QuotaSnapshotV1>>,
}

enum HealthProbePlugin {
    Legacy(bindings::Plugin),
    V2(health_v2_bindings::PluginHealthV2),
}

struct HealthPrepared {
    store: wasmtime::Store<HostCtx>,
    plugin: HealthProbePlugin,
}

struct Prepared {
    store: wasmtime::Store<HostCtx>,
    plugin: bindings::Plugin,
    wall_time: Duration,
}

struct AuthPrepared {
    store: wasmtime::Store<HostCtx>,
    plugin: crate::plugins::runtime::auth_bindings::PluginAuth,
    wall_time: Duration,
}

struct ModelSourcePrepared {
    store: wasmtime::Store<HostCtx>,
    plugin: crate::plugins::runtime::model_source_bindings::PluginModelSource,
    wall_time: Duration,
}

struct AdapterPrepared {
    store: wasmtime::Store<HostCtx>,
    plugin: AdapterGuest,
    wall_time: Duration,
}

enum AdapterGuest {
    Api1(adapter_bindings::PluginAdapter),
    Api2(adapter_v2_bindings::PluginAdapterV2),
}

impl AdapterGuest {
    async fn call_wire_format(
        &self,
        store: &mut wasmtime::Store<HostCtx>,
    ) -> Result<String, PluginFault> {
        match self {
            Self::Api1(plugin) => plugin
                .provider_adapter()
                .call_wire_format(store)
                .await
                .map_err(map_call_error),
            Self::Api2(plugin) => plugin
                .kinetix_plugin2_0_0_provider_adapter()
                .call_wire_format(store)
                .await
                .map_err(map_call_error),
        }
    }

    async fn call_build_url(
        &self,
        store: &mut wasmtime::Store<HostCtx>,
        provider_json: &str,
        model_json: &str,
    ) -> Result<String, PluginFault> {
        match self {
            Self::Api1(plugin) => plugin
                .provider_adapter()
                .call_build_url(store, provider_json, model_json)
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
            Self::Api2(plugin) => plugin
                .kinetix_plugin2_0_0_provider_adapter()
                .call_build_url(store, provider_json, model_json)
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
        }
    }

    async fn call_apply_auth(
        &self,
        store: &mut wasmtime::Store<HostCtx>,
        provider_json: &str,
        credential: &str,
        opaque_session_context: Option<&str>,
    ) -> Result<String, PluginFault> {
        let session = opaque_session_context.map(|id| {
            adapter_v2_bindings::kinetix::plugin2_0_0::types::SessionContext { id: id.into() }
        });
        match self {
            Self::Api1(plugin) => plugin
                .provider_adapter()
                .call_apply_auth(store, provider_json, credential)
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
            Self::Api2(plugin) => plugin
                .kinetix_plugin2_0_0_provider_adapter()
                .call_apply_auth(store, provider_json, credential, session.as_ref())
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
        }
    }

    async fn call_build_body(
        &self,
        store: &mut wasmtime::Store<HostCtx>,
        request_json: &str,
        provider_json: &str,
        model_json: &str,
        opaque_session_context: Option<&str>,
    ) -> Result<String, PluginFault> {
        let session = opaque_session_context.map(|id| {
            adapter_v2_bindings::kinetix::plugin2_0_0::types::SessionContext { id: id.into() }
        });
        match self {
            Self::Api1(plugin) => plugin
                .provider_adapter()
                .call_build_body(store, request_json, provider_json, model_json)
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
            Self::Api2(plugin) => plugin
                .kinetix_plugin2_0_0_provider_adapter()
                .call_build_body(
                    store,
                    request_json,
                    provider_json,
                    model_json,
                    session.as_ref(),
                )
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
        }
    }

    async fn call_classify_error(
        &self,
        store: &mut wasmtime::Store<HostCtx>,
        status: u16,
        body: &str,
        headers_json: &str,
    ) -> Result<String, PluginFault> {
        match self {
            Self::Api1(plugin) => plugin
                .provider_adapter()
                .call_classify_error(store, status, body, headers_json)
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
            Self::Api2(plugin) => plugin
                .kinetix_plugin2_0_0_provider_adapter()
                .call_classify_error(store, status, body, headers_json)
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
        }
    }

    async fn call_parse_stream_chunk(
        &self,
        store: &mut wasmtime::Store<HostCtx>,
        data: &str,
    ) -> Result<String, PluginFault> {
        match self {
            Self::Api1(plugin) => plugin
                .provider_adapter()
                .call_parse_stream_chunk(store, data)
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
            Self::Api2(plugin) => plugin
                .kinetix_plugin2_0_0_provider_adapter()
                .call_parse_stream_chunk(store, data)
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
        }
    }

    async fn call_parse_full_response(
        &self,
        store: &mut wasmtime::Store<HostCtx>,
        body_json: &str,
    ) -> Result<String, PluginFault> {
        match self {
            Self::Api1(plugin) => plugin
                .provider_adapter()
                .call_parse_full_response(store, body_json)
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
            Self::Api2(plugin) => plugin
                .kinetix_plugin2_0_0_provider_adapter()
                .call_parse_full_response(store, body_json)
                .await
                .map_err(map_call_error)
                .and_then(map_adapter_result),
        }
    }
}

/// Map a `wasmtime::Result` error from a guest call into a [`PluginFault`].
fn map_call_error(e: wasmtime::Error) -> PluginFault {
    let msg = e.to_string();
    if msg.contains("epoch") || msg.contains("interrupt") || msg.contains("deadline") {
        PluginFault::Timeout
    } else {
        PluginFault::Trap(msg)
    }
}

/// Map the guest's `Result<T, PluginError>` into a [`PluginFault`].
fn map_plugin_result<T>(r: Result<T, wit::types::PluginError>) -> Result<T, PluginFault> {
    r.map_err(|e| PluginFault::PluginError {
        code: e.code,
        message: e.message,
        retryable: e.retryable,
        retry_after: e.retry_after,
    })
}

fn map_health_v2_result<T>(
    result: Result<T, health_v2_wit::types::PluginError>,
) -> Result<T, PluginFault> {
    result.map_err(|e| PluginFault::PluginError {
        code: e.code,
        message: e.message,
        retryable: e.retryable,
        retry_after: e.retry_after,
    })
}

fn map_account_model_result<T>(
    result: Result<
        T,
        crate::plugins::runtime::model_source_bindings::kinetix::plugin::types::PluginError,
    >,
) -> Result<T, PluginFault> {
    result.map_err(|e| PluginFault::PluginError {
        code: e.code,
        message: e.message,
        retryable: e.retryable,
        retry_after: e.retry_after,
    })
}

/// Like [`map_plugin_result`] but for the separately-bound auth world.
fn map_auth_result<T>(
    result: Result<T, crate::plugins::runtime::auth_bindings::kinetix::plugin::types::PluginError>,
) -> Result<T, PluginFault> {
    result.map_err(|e| PluginFault::PluginError {
        code: e.code,
        message: e.message,
        retryable: e.retryable,
        retry_after: e.retry_after,
    })
}

/// Error shapes shared by the separately generated adapter API bindings.
trait AdapterPluginError {
    fn into_fault(self) -> PluginFault;
}

impl AdapterPluginError for adapter_bindings::kinetix::plugin::types::PluginError {
    fn into_fault(self) -> PluginFault {
        PluginFault::PluginError {
            code: self.code,
            message: self.message,
            retryable: self.retryable,
            retry_after: self.retry_after,
        }
    }
}

impl AdapterPluginError for adapter_v2_bindings::kinetix::plugin1_0_0::types::PluginError {
    fn into_fault(self) -> PluginFault {
        PluginFault::PluginError {
            code: self.code,
            message: self.message,
            retryable: self.retryable,
            retry_after: self.retry_after,
        }
    }
}

fn map_adapter_result<T, E: AdapterPluginError>(result: Result<T, E>) -> Result<T, PluginFault> {
    result.map_err(AdapterPluginError::into_fault)
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PluginCapabilityCounters {
    pub invocations: u64,
    pub successes: u64,
    pub faults: u64,
    pub timeouts: u64,
    pub cancellations: u64,
    pub http_requests: u64,
    pub duration_micros: u64,
}

impl PluginCapabilityCounters {
    fn add_assign(&mut self, other: &Self) {
        self.invocations = self.invocations.saturating_add(other.invocations);
        self.successes = self.successes.saturating_add(other.successes);
        self.faults = self.faults.saturating_add(other.faults);
        self.timeouts = self.timeouts.saturating_add(other.timeouts);
        self.cancellations = self.cancellations.saturating_add(other.cancellations);
        self.http_requests = self.http_requests.saturating_add(other.http_requests);
        self.duration_micros = self.duration_micros.saturating_add(other.duration_micros);
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PluginMetricsSnapshot {
    pub totals: PluginCapabilityCounters,
    pub by_capability: BTreeMap<String, PluginCapabilityCounters>,
}

/// Host-side global counters surfaced by the admin metrics endpoint (§18).
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct PluginCounters {
    pub invocations: u64,
    pub faults: u64,
    pub timeouts: u64,
    pub cancellations: u64,
    pub http_requests: u64,
    pub component_cache_hits: u64,
    pub component_cache_misses: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PermissionListDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PermissionBoolDiff {
    pub from: bool,
    pub to: bool,
    pub changed: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PermissionDiff {
    pub network_hosts: PermissionListDiff,
    pub credential_scopes: PermissionListDiff,
    pub credential_read: PermissionBoolDiff,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RollbackPreview {
    pub id: String,
    pub current_version: String,
    pub target_version: String,
    pub package_sha256: String,
    pub signature: String,
    pub source: String,
    pub permissions: Permissions,
    pub permission_diff: PermissionDiff,
    pub provides: Vec<Provided>,
}

fn list_permission_diff(from: &[String], to: &[String]) -> PermissionListDiff {
    let from: BTreeSet<&str> = from.iter().map(String::as_str).collect();
    let to: BTreeSet<&str> = to.iter().map(String::as_str).collect();
    PermissionListDiff {
        added: to
            .difference(&from)
            .map(|value| (*value).to_string())
            .collect(),
        removed: from
            .difference(&to)
            .map(|value| (*value).to_string())
            .collect(),
    }
}

pub(crate) fn permission_diff(from: &Permissions, to: &Permissions) -> PermissionDiff {
    PermissionDiff {
        network_hosts: list_permission_diff(&from.network_hosts, &to.network_hosts),
        credential_scopes: list_permission_diff(&from.credential_scopes, &to.credential_scopes),
        credential_read: PermissionBoolDiff {
            from: from.credential_read,
            to: to.credential_read,
            changed: from.credential_read != to.credential_read,
        },
    }
}

/// The outcome of reactivating a retained package.
#[derive(Debug, Clone)]
pub struct RollbackOutcome {
    pub id: String,
    pub version: String,
    pub package_sha256: String,
    pub signature: String,
    pub provides: Vec<Provided>,
}

/// The outcome of a successful install.
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub id: String,
    pub version: String,
    pub package_sha256: String,
    pub signature: SignatureStatus,
    pub provides: Vec<Provided>,
}

/// Turn a validated manifest into the all-or-nothing permission grant set (§20).
pub fn permission_grants(manifest: &Manifest) -> Vec<PermissionGrant> {
    let mut grants = Vec::new();
    if !manifest.permissions.network_hosts.is_empty() {
        grants.push(PermissionGrant {
            permission: "network_hosts".into(),
            value_json: serde_json::to_string(&manifest.permissions.network_hosts)
                .unwrap_or_else(|_| "[]".into()),
        });
    }
    if !manifest.permissions.credential_scopes.is_empty() {
        grants.push(PermissionGrant {
            permission: "credential_scopes".into(),
            value_json: serde_json::to_string(&manifest.permissions.credential_scopes)
                .unwrap_or_else(|_| "[]".into()),
        });
    }
    if manifest.permissions.credential_read {
        grants.push(PermissionGrant {
            permission: "credential_read".into(),
            value_json: "true".into(),
        });
    }
    grants
}

fn permission_grants_match(requested: &[PermissionGrant], approved: &[PermissionGrant]) -> bool {
    let requested_set: std::collections::BTreeSet<_> = requested
        .iter()
        .map(|grant| (grant.permission.as_str(), grant.value_json.as_str()))
        .collect();
    let approved_set: std::collections::BTreeSet<_> = approved
        .iter()
        .map(|grant| (grant.permission.as_str(), grant.value_json.as_str()))
        .collect();
    requested_set == approved_set
}

/// The set of capabilities an enabled plugin provides, for the dashboard.
pub fn manifest_summary(row: &PluginRow) -> serde_json::Value {
    let manifest = row.manifest();
    serde_json::json!({
        "id": row.id,
        "name": manifest.as_ref().map(|m| m.name.clone()).unwrap_or_else(|| row.id.clone()),
        "version": row.version,
        "plugin_api_major": row.plugin_api_major,
        "compatibility": manifest
            .as_ref()
            .map(|m| m.compatibility.clone())
            .unwrap_or_default(),
        "sha256": row.package_sha256,
        "signature": row.signature,
        "status": row.status().as_str(),
        "provides": manifest.as_ref().map(|m| m.provides.provided()).unwrap_or_default(),
        "integrations": manifest.as_ref().map(|m| m.integrations.clone()).unwrap_or_default(),
        "ui": manifest.as_ref().map(|m| m.ui.clone()).unwrap_or_default(),
        "permissions": manifest.as_ref().map(|m| m.permissions.clone()).unwrap_or_default(),
        "limits": manifest.as_ref().map(|m| m.limits.clone()).unwrap_or_default(),
        "routing_facts_mode": manifest.as_ref().map(|m| m.routing_facts_mode.clone()).unwrap_or_else(|| "pure".into()),
        "routing_facts_refresh_ms": manifest.as_ref().map(|m| m.routing_facts_refresh_ms).unwrap_or(30_000),
    })
}

/// Package-level helper used by the CLI and admin API.
pub fn read_package(path: &std::path::Path) -> Result<Package> {
    package::read_package_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn concurrency_test_manager() -> (PluginManager, Pool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "kinetix-plugin-concurrency-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        let manager = PluginManager::new(
            pool.clone(),
            Arc::new(Crypto::new(&[29_u8; 32])),
            HostPolicy::default(),
            dir.join("packages"),
        )
        .unwrap();
        (manager, pool, dir)
    }

    #[tokio::test]
    async fn malformed_health_v2_export_fails_component_validation() {
        let (manager, pool, dir) = concurrency_test_manager().await;
        let component_bytes = wat::parse_str(
            r#"
            (component
              (core module $m
                (func (export "wrong-health")))
              (core instance $i (instantiate $m))
              (alias core export $i "wrong-health" (core func $f))
              (type $func-type (func))
              (func $lifted (type $func-type) (canon lift (core func $f)))
              (export "health-probe-v2" (func $lifted)))
            "#,
        )
        .unwrap();
        let component = manager.inner.runtime.compile(&component_bytes).unwrap();
        let manifest: Manifest = toml::from_str(
            r#"
            manifest_version = 1
            id = "dev.example.malformed-health"
            name = "Malformed health export"
            version = "0.1.0"
            plugin_api = "1"
            "#,
        )
        .unwrap();
        let limits = manifest::EffectiveLimits {
            memory: 1024 * 1024,
            wall_time_ms: 1000,
            max_outbound_requests: 0,
            max_http_body: 0,
            storage: 0,
        };

        let error = manager
            .validate_component_contract(&manifest, &limits, &component)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("plugin-health-v2 world validation"),
            "unexpected validation error: {error}"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn api_v2_adapter_contract_rejects_every_host_import() {
        let (manager, pool, dir) = concurrency_test_manager().await;
        let manifest: Manifest = toml::from_str(
            r#"
            manifest_version = 1
            id = "dev.example.v2-adapter"
            name = "V2 adapter"
            version = "0.1.0"
            plugin_api = "2"

            [provides]
            provider_adapters = ["test-adapter"]
            "#,
        )
        .unwrap();
        let limits = manifest::EffectiveLimits {
            memory: 1024 * 1024,
            wall_time_ms: 1000,
            max_outbound_requests: 0,
            max_http_body: 0,
            storage: 0,
        };

        for import in [
            "kinetix:plugin/host-http@1.0.0",
            "kinetix:plugin/host-storage@1.0.0",
            "kinetix:plugin/host-log@1.0.0",
            "kinetix:plugin/host-credential@1.0.0",
            "kinetix:plugin/host-clock@1.0.0",
        ] {
            let component_bytes = wat::parse_str(format!(
                r#"(component
                    (type $required-func (func (param "input" string) (result string)))
                    (type $host (instance
                        (export "required" (func (type $required-func)))
                    ))
                    (import "{import}" (instance (type $host)))
                )"#
            ))
            .unwrap();
            let component = manager.inner.runtime.compile(&component_bytes).unwrap();
            let error = manager
                .validate_component_contract(&manifest, &limits, &component)
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("provider-adapter-v2 world validation")
                    && error.to_string().contains(import),
                "expected API-v2 contract to reject {import}, got: {error}"
            );
        }

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn one_plugin_cannot_monopolize_global_invocation_capacity() {
        let (manager, pool, dir) = concurrency_test_manager().await;

        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_INVOCATIONS_PER_PLUGIN {
            held.push(manager.acquire_invocation_permits("plugin-a").await);
        }

        let waiting_manager = manager.clone();
        let blocked =
            tokio::spawn(
                async move { waiting_manager.acquire_invocation_permits("plugin-a").await },
            );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !blocked.is_finished(),
            "fifth invocation for the same plugin must wait"
        );

        let other = tokio::time::timeout(
            Duration::from_millis(100),
            manager.acquire_invocation_permits("plugin-b"),
        )
        .await
        .expect("another plugin should retain access to global capacity");
        drop(other);

        drop(held.pop());
        let released = tokio::time::timeout(Duration::from_millis(100), blocked)
            .await
            .expect("waiting same-plugin invocation should resume after a slot is released")
            .unwrap();
        drop(released);
        drop(held);

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn plugin_metrics_are_isolated_by_plugin_and_capability() {
        let (manager, pool, dir) = concurrency_test_manager().await;

        let a = metric_cell(&manager.inner.metrics, "plugin-a", "model_source");
        a.invocations.store(3, std::sync::atomic::Ordering::Relaxed);
        a.successes.store(2, std::sync::atomic::Ordering::Relaxed);
        a.http_requests
            .store(5, std::sync::atomic::Ordering::Relaxed);

        let b = metric_cell(&manager.inner.metrics, "plugin-b", "health_probe");
        b.invocations.store(7, std::sync::atomic::Ordering::Relaxed);

        let a_snapshot = manager.metrics_for_plugin("plugin-a");
        assert_eq!(a_snapshot.totals.invocations, 3);
        assert_eq!(a_snapshot.totals.successes, 2);
        assert_eq!(a_snapshot.totals.http_requests, 5);
        assert_eq!(a_snapshot.by_capability.len(), 1);
        assert!(a_snapshot.by_capability.contains_key("model_source"));

        let b_snapshot = manager.metrics_for_plugin("plugin-b");
        assert_eq!(b_snapshot.totals.invocations, 7);
        assert_eq!(b_snapshot.by_capability.len(), 1);
        assert!(b_snapshot.by_capability.contains_key("health_probe"));

        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cached_fact_snapshot_rejects_duplicate_and_invalid_values() {
        let fact =
            |name: &str, value_json: &str, max_age_ms: Option<u64>| wit::types::RoutingFact {
                name: name.into(),
                value_json: value_json.into(),
                observed_at: Some("guest-controlled".into()),
                max_age_ms,
            };

        let duplicate = build_cached_fact_snapshot(
            vec![
                fact("capacity", "\"burst\"", Some(30_000)),
                fact("capacity", "\"steady\"", Some(30_000)),
            ],
            Default::default(),
            "2026-09-20T00:00:00Z",
        )
        .unwrap_err();
        assert!(duplicate
            .message()
            .contains("duplicate cached routing fact"));

        let invalid = build_cached_fact_snapshot(
            vec![fact("capacity", "{bad", Some(30_000))],
            Default::default(),
            "2026-09-20T00:00:00Z",
        )
        .unwrap_err();
        assert!(invalid.message().contains("invalid JSON"));

        let missing_age = build_cached_fact_snapshot(
            vec![fact("capacity", "true", None)],
            Default::default(),
            "2026-09-20T00:00:00Z",
        )
        .unwrap_err();
        assert!(missing_age.message().contains("must declare max_age_ms"));
    }

    #[test]
    fn cached_fact_snapshot_host_stamps_guest_observations() {
        let fact = wit::types::RoutingFact {
            name: "capacity".into(),
            value_json: "\"burst\"".into(),
            observed_at: Some("1900-01-01T00:00:00Z".into()),
            max_age_ms: Some(30_000),
        };
        let snapshot =
            build_cached_fact_snapshot(vec![fact], Default::default(), "2026-09-20T00:00:00Z")
                .unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&snapshot[0].1).unwrap();
        assert_eq!(
            envelope["observed_at"],
            serde_json::Value::String("2026-09-20T00:00:00Z".into())
        );
    }

    #[tokio::test]
    async fn backing_allows_credentials_only_for_matching_strategy_binding() {
        let dir = std::env::temp_dir().join(format!(
            "kinetix-plugin-scope-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let crypto = Arc::new(Crypto::new(&[13u8; 32]));

        let provider_id = crate::db::insert_provider(
            &pool,
            &crate::db::NewProvider {
                name: "Bound",
                base_url: "https://example.com",
                wire_format: crate::types::WireFormat::Plugin,
                auth_scheme: crate::types::AuthScheme::Bearer,
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
                wire_plugin: "plugin:dev.example.plugin/adapter",
                credential_plugin: "plugin:dev.example.plugin/oauth",
                model_source_plugin: "",
                credential_mode: "manual",
                source_plugin_id: None,
                source_integration_id: None,
            },
        )
        .await
        .unwrap();

        let backing = Backing {
            pool,
            crypto,
            metrics: Arc::new(PluginMetricRegistry::new()),
        };
        assert!(backing
            .credential_scope_allows(
                "dev.example.plugin",
                &provider_id,
                &["credential_strategy:oauth".into()],
            )
            .await
            .unwrap());
        assert!(!backing
            .credential_scope_allows(
                "dev.example.plugin",
                &provider_id,
                &["credential_strategy:other".into()],
            )
            .await
            .unwrap());
        assert!(!backing
            .credential_scope_allows(
                "dev.other.plugin",
                &provider_id,
                &["credential_strategy:oauth".into()],
            )
            .await
            .unwrap());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn backing_rejects_cross_provider_account_lookup() {
        let dir = std::env::temp_dir().join(format!(
            "kinetix-plugin-credential-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let crypto = Arc::new(Crypto::new(&[11u8; 32]));

        fn new_provider(name: &str) -> crate::db::NewProvider<'_> {
            crate::db::NewProvider {
                name,
                base_url: "https://example.com",
                wire_format: crate::types::WireFormat::Openai,
                auth_scheme: crate::types::AuthScheme::Bearer,
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
            }
        }
        let provider_a = crate::db::insert_provider(&pool, &new_provider("A"))
            .await
            .unwrap();
        let provider_b = crate::db::insert_provider(&pool, &new_provider("B"))
            .await
            .unwrap();
        let secret_enc = crypto.encrypt("provider-b-secret").unwrap();
        let account_id = crate::db::insert_account(
            &pool,
            &provider_b,
            "B account",
            &secret_enc,
            "****",
            1,
            1,
            None,
            "unknown",
        )
        .await
        .unwrap();

        let backing = Backing {
            pool,
            crypto,
            metrics: Arc::new(PluginMetricRegistry::new()),
        };
        let err = backing
            .resolve_secret("dev.example.plugin", &provider_a, &account_id)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not belong"), "{err}");
    }

    #[test]
    fn permission_grants_are_all_or_nothing() {
        let m: Manifest = toml::from_str(
            r#"
manifest_version = 1
id = "x"
name = "X"
version = "1"
plugin_api = "1"
[provides]
model_sources = ["m"]
[permissions]
network_hosts = ["a.example"]
credential_scopes = ["provider:p"]
credential_read = true
"#,
        )
        .unwrap();
        let grants = permission_grants(&m);
        assert_eq!(grants.len(), 3);
        assert!(grants.iter().any(|g| g.permission == "credential_read"));
    }
}
