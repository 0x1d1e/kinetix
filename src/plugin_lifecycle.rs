//! Plugin lifecycle transitions for the running gateway.
//!
//! Every transition that can change what a plugin serves (install, reinstall,
//! rollback, enable, disable, remove, permission revocation) goes through
//! [`PluginLifecycle`], which persists it via the [`PluginManager`] and then
//! re-syncs the runtime: registered credential strategies, provider adapters,
//! routing-fact work scopes, and the Providers its integrations own. After any
//! transition the runtime mirrors the persisted plugin row.

use std::sync::Arc;

use anyhow::Result;

use crate::app::AppState;
use crate::plugins::manager::{InstallOutcome, RollbackOutcome};
use crate::plugins::PluginManager;

pub struct PluginLifecycle<'a> {
    state: &'a AppState,
    manager: &'a Arc<PluginManager>,
}

impl<'a> PluginLifecycle<'a> {
    /// `None` when the plugin host is unavailable.
    pub fn new(state: &'a AppState) -> Option<Self> {
        let manager = state.plugin_manager()?;
        Some(Self { state, manager })
    }

    pub fn manager(&self) -> &PluginManager {
        self.manager
    }

    pub async fn install(
        &self,
        bytes: &[u8],
        expected_sha256: Option<&str>,
        trusted_keys: &[[u8; 32]],
        allow_untrusted_signature: bool,
        source: &str,
    ) -> Result<InstallOutcome> {
        let outcome = self
            .manager
            .install_from_source(
                bytes,
                expected_sha256,
                trusted_keys,
                allow_untrusted_signature,
                source,
            )
            .await?;
        self.sync(&outcome.id).await;
        Ok(outcome)
    }

    /// Reinstall a retained package.
    pub async fn reinstall(&self, id: &str, sha256: &str) -> Result<InstallOutcome> {
        let outcome = self.manager.install_retained(id, sha256).await?;
        self.sync(&outcome.id).await;
        Ok(outcome)
    }

    /// Reactivate a retained package; it is always activated disabled.
    pub async fn rollback(&self, id: &str, sha256: &str) -> Result<RollbackOutcome> {
        let outcome = self.manager.rollback(id, sha256).await?;
        self.sync(id).await;
        Ok(outcome)
    }

    pub async fn enable(&self, id: &str) -> Result<()> {
        self.manager.enable(id).await?;
        self.sync(id).await;
        Ok(())
    }

    pub async fn disable(&self, id: &str, acknowledged_impact: Option<&str>) -> Result<()> {
        self.manager.disable(id, acknowledged_impact).await?;
        self.sync(id).await;
        Ok(())
    }

    pub async fn remove(&self, id: &str, acknowledged_impact: Option<&str>) -> Result<()> {
        self.manager.remove(id, acknowledged_impact).await?;
        self.sync(id).await;
        Ok(())
    }

    /// Revocation disables the plugin when its requested set is no longer
    /// fully granted.
    pub async fn revoke_permission(&self, id: &str, permission: &str) -> Result<()> {
        self.manager.revoke_permission(id, permission).await?;
        self.sync(id).await;
        Ok(())
    }

    /// Startup: disable persisted plugins that fail the current manifest or
    /// permission contract, then activate every plugin that remains enabled.
    pub async fn activate_persisted(&self) -> Result<()> {
        self.manager.reconcile_enabled_plugins().await?;
        for row in self.manager.list().await? {
            if row.status().is_enabled() {
                self.sync(&row.id).await;
            }
        }
        Ok(())
    }

    /// Make the runtime mirror the persisted row: drop everything the plugin
    /// registered, then re-register it if it is enabled. Idempotent.
    pub async fn sync(&self, id: &str) {
        self.state.unregister_plugin_capabilities(id);
        register_enabled(self.state, self.manager, id).await;
    }
}

/// Register the runtime capability objects an enabled plugin provides (§6.0).
///
/// A plugin that declares a `credential_strategies` capability gets a
/// `PluginCredentialStrategy` so a provider bound to it resolves through the
/// plugin; a plugin that declares `provider_adapters` gets a plugin-backed
/// adapter for each declared name.
async fn register_enabled(state: &AppState, manager: &Arc<PluginManager>, id: &str) {
    let crypto = state.crypto.clone();
    let pool = state.pool.clone();
    let (provides, cached_routing_facts) = match manager.get(id).await {
        Ok(Some(row)) if row.status().is_enabled() => {
            let Some(manifest) = row.manifest() else {
                return;
            };
            let cached_routing_facts = manifest.routing_facts_mode == "cached"
                && !manifest.provides.routing_facts.is_empty();
            (manifest.provides, cached_routing_facts)
        }
        _ => return,
    };
    if cached_routing_facts {
        state
            .provider_work
            .activate_auxiliary_scope(&format!("plugin:{id}"));
    }
    if !provides.credential_strategies.is_empty() {
        let strategy: Arc<dyn crate::credentials::CredentialStrategy> =
            Arc::new(crate::plugins::credential::PluginCredentialStrategy::new(
                Arc::clone(manager),
                pool,
                crypto,
                id,
            ));
        state.register_plugin_credential_strategy(id, strategy);
    }
    // ProviderAdapter registration (§6.3, §7.1): a plugin adapter is a pure
    // translation library — core still owns the outbound streaming send. The
    // `plugin-adapter` world imports no network capability, so registering it
    // does not widen the plugin's authority.
    if !provides.provider_adapters.is_empty() {
        if let Err(e) = crate::plugins::adapter::register_declared_adapters(
            &state.adapters,
            manager.as_ref().clone(),
            id,
            &provides,
        )
        .await
        {
            tracing::warn!(
                plugin = %id,
                error = %e,
                "plugin declares provider_adapters but its adapter world could not be loaded; bound providers will fail closed"
            );
        }
    }

    crate::admin::auto_provision_plugin_providers(state, id).await;
}
