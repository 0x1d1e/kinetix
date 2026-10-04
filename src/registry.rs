//! Runtime registry: an in-memory snapshot of the admin-configured providers,
//! accounts, models, aliases, and routes. Reloaded from the database whenever
//! configuration changes so edits take effect without a restart (FR-10.13).
//!
//! In-flight requests keep the `Arc<Registry>` snapshot they started with.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;

use crate::db::{
    self, AccountRow, AliasRow, ModelRow, Pool, ProviderRow, RouteRow, RouteTargetRow,
};

/// An immutable runtime configuration snapshot (FR-10.13, NFR-2.10).
///
/// The data plane holds an `Arc<Snapshot>` for the lifetime of a request, so a
/// configuration change (which swaps in a new snapshot) never affects in-flight
/// work. There are no interior locks on this type; it is read-only once built.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<RwLock<Arc<Snapshot>>>,
    publication: Arc<tokio::sync::Mutex<()>>,
    config_imports: Arc<tokio::sync::Mutex<()>>,
    revision: Arc<AtomicU64>,
    /// `registry_revision.revision` observed before building the active
    /// database-backed snapshot; `-1` until the first successful reload.
    loaded_db_revision: Arc<AtomicI64>,
    lifecycle: Arc<parking_lot::Mutex<()>>,
}

/// Holds the registry publication lock across a control-plane database update.
/// Publish the matching snapshot before dropping the guard.
pub(crate) struct RegistryPublication<'a> {
    registry: &'a Registry,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub providers: HashMap<String, ProviderRow>,
    pub provider_order: Vec<String>,
    pub accounts: HashMap<String, AccountRow>,
    pub models: HashMap<String, ModelRow>,
    /// (provider_id, upstream_id) -> model_id
    pub model_by_upstream: HashMap<(String, String), String>,
    pub aliases: HashMap<String, AliasRow>,
    pub routes: HashMap<String, RouteRow>,
    pub route_targets: HashMap<String, Vec<RouteTargetRow>>,
    pub(crate) provider_work_generations: HashMap<String, Arc<AtomicBool>>,
    pub(crate) account_work_generations: HashMap<String, Arc<AtomicBool>>,
}

/// Opaque lifecycle identity captured from the active registry snapshot. Work
/// admitted with an older identity is rejected after provider/account removal.
#[derive(Clone)]
pub struct ProviderWorkIdentity {
    pub(crate) provider_id: String,
    pub(crate) provider_generation: Arc<AtomicBool>,
    pub(crate) account_generation: Option<Arc<AtomicBool>>,
    #[cfg(test)]
    pub(crate) test_provider_id_only: bool,
}

impl ProviderWorkIdentity {
    pub(crate) fn is_current(&self) -> bool {
        self.provider_generation.load(Ordering::Acquire)
            && self
                .account_generation
                .as_ref()
                .is_none_or(|generation| generation.load(Ordering::Acquire))
    }

    #[cfg(test)]
    pub(crate) fn for_test(provider_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            provider_generation: Arc::new(AtomicBool::new(true)),
            account_generation: None,
            test_provider_id_only: false,
        }
    }
}

#[cfg(test)]
impl From<&str> for ProviderWorkIdentity {
    fn from(provider_id: &str) -> Self {
        let mut identity = Self::for_test(provider_id);
        identity.test_provider_id_only = true;
        identity
    }
}

#[cfg(test)]
impl From<String> for ProviderWorkIdentity {
    fn from(provider_id: String) -> Self {
        let mut identity = Self::for_test(provider_id);
        identity.test_provider_id_only = true;
        identity
    }
}

#[cfg(test)]
impl From<&String> for ProviderWorkIdentity {
    fn from(provider_id: &String) -> Self {
        let mut identity = Self::for_test(provider_id.clone());
        identity.test_provider_id_only = true;
        identity
    }
}

/// A resolved routing decision for a client-requested model name.
#[derive(Debug, Clone)]
pub enum Resolved {
    /// A single (provider, model) target.
    Single {
        provider_id: String,
        model_id: String,
    },
    /// A route with an ordered list of targets.
    Route {
        route: RouteRow,
        targets: Vec<ResolvedTarget>,
    },
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub account: AccountRow,
    pub model: ModelRow,
    pub provider: ProviderRow,
    /// Stable logical route-target identity. Multiple account candidates for an
    /// unpinned route target share this id so route strategy/weight is applied
    /// once to the logical target, not once per account.
    pub route_target_id: Option<String>,
    pub priority: i64,
    pub weight: i64,
    /// Optional typed eligibility predicate (FR-12.3).
    pub predicate: crate::predicate::TargetPredicate,
    /// Optional per-target parameter overrides (FR-12.2).
    pub param_overrides: serde_json::Value,
}

impl Registry {
    pub fn new() -> Self {
        Registry {
            inner: Arc::new(RwLock::new(Arc::new(Snapshot::default()))),
            publication: Arc::new(tokio::sync::Mutex::new(())),
            config_imports: Arc::new(tokio::sync::Mutex::new(())),
            revision: Arc::new(AtomicU64::new(0)),
            loaded_db_revision: Arc::new(AtomicI64::new(-1)),
            lifecycle: Arc::new(parking_lot::Mutex::new(())),
        }
    }

    /// Serialize apply imports before they read control-plane state to plan.
    pub(crate) async fn config_import_lock(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.config_imports.clone().lock_owned().await
    }

    pub(crate) async fn publication(&self) -> RegistryPublication<'_> {
        RegistryPublication {
            registry: self,
            _guard: self.publication.lock().await,
        }
    }

    pub async fn reload(&self, pool: &Pool) -> Result<()> {
        self.publication().await.reload(pool).await
    }

    /// Rebuild the snapshot only when the control-plane revision changed since
    /// the last database-backed reload. Returns whether a reload ran. This is
    /// the steady-state poll: an unchanged registry costs one single-row read.
    pub async fn reload_if_changed(&self, pool: &Pool) -> Result<bool> {
        let publication = self.publication().await;
        let db_revision = db::registry_revision(pool).await?;
        if db_revision == self.loaded_db_revision.load(Ordering::Acquire) {
            return Ok(false);
        }
        publication.reload_at(pool, db_revision).await?;
        Ok(true)
    }

    /// Build a registry snapshot without making it active.
    pub async fn build_snapshot(pool: &Pool) -> Result<Snapshot> {
        let providers = db::list_providers(pool).await?;
        let accounts = db::list_accounts(pool).await?;
        let models = db::list_models(pool).await?;
        let aliases = db::list_aliases(pool).await?;
        let routes = db::list_routes(pool).await?;
        let mut route_targets = HashMap::new();
        for route in &routes {
            route_targets.insert(route.id.clone(), db::route_targets(pool, &route.id).await?);
        }
        Ok(Self::snapshot_from_rows(
            providers,
            accounts,
            models,
            aliases,
            routes,
            route_targets,
        ))
    }

    /// Build a snapshot from uncommitted configuration for atomic activation
    /// after the transaction commits.
    pub async fn build_snapshot_in_transaction(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<Snapshot> {
        let providers =
            sqlx::query_as::<_, ProviderRow>("SELECT * FROM providers ORDER BY created_at")
                .fetch_all(&mut **tx)
                .await?;
        let accounts =
            sqlx::query_as::<_, AccountRow>("SELECT * FROM accounts ORDER BY priority, created_at")
                .fetch_all(&mut **tx)
                .await?;
        let models = sqlx::query_as::<_, ModelRow>("SELECT * FROM models ORDER BY created_at")
            .fetch_all(&mut **tx)
            .await?;
        let aliases = sqlx::query_as::<_, AliasRow>("SELECT * FROM aliases ORDER BY alias")
            .fetch_all(&mut **tx)
            .await?;
        let routes = sqlx::query_as::<_, RouteRow>("SELECT * FROM routes ORDER BY created_at")
            .fetch_all(&mut **tx)
            .await?;
        let mut route_targets = HashMap::new();
        for route in &routes {
            let targets = sqlx::query_as::<_, RouteTargetRow>(
                "SELECT * FROM route_targets WHERE route_id = ? ORDER BY priority, weight DESC",
            )
            .bind(&route.id)
            .fetch_all(&mut **tx)
            .await?;
            route_targets.insert(route.id.clone(), targets);
        }
        Ok(Self::snapshot_from_rows(
            providers,
            accounts,
            models,
            aliases,
            routes,
            route_targets,
        ))
    }

    /// Atomically activate a fully built immutable snapshot while publication
    /// is serialized by `RegistryPublication`.
    fn activate(&self, mut snapshot: Snapshot, expected_revision: Option<u64>) -> bool {
        let _lifecycle = self.lifecycle.lock();
        let mut current = self.inner.write();
        if expected_revision
            .is_some_and(|revision| self.revision.load(Ordering::Acquire) != revision)
        {
            return false;
        }
        if expected_revision.is_none() {
            self.revision.fetch_add(1, Ordering::AcqRel);
        }

        let previous = current.clone();
        for provider_id in snapshot.providers.keys() {
            let generation = previous
                .provider_work_generations
                .get(provider_id)
                .filter(|generation| generation.load(Ordering::Acquire))
                .cloned()
                .unwrap_or_else(|| Arc::new(AtomicBool::new(true)));
            snapshot
                .provider_work_generations
                .insert(provider_id.clone(), generation);
        }
        for account_id in snapshot.accounts.keys() {
            let is_active = snapshot
                .accounts
                .get(account_id)
                .is_some_and(|account| account.status != "disabled");
            if !is_active {
                continue;
            }
            let same_provider = previous
                .accounts
                .get(account_id)
                .zip(snapshot.accounts.get(account_id))
                .is_some_and(|(old, new)| {
                    old.provider_id == new.provider_id && old.status != "disabled"
                });
            let generation = same_provider
                .then(|| previous.account_work_generations.get(account_id))
                .flatten()
                .filter(|generation| generation.load(Ordering::Acquire))
                .cloned()
                .unwrap_or_else(|| Arc::new(AtomicBool::new(true)));
            snapshot
                .account_work_generations
                .insert(account_id.clone(), generation);
        }

        for (provider_id, generation) in &previous.provider_work_generations {
            if !snapshot.providers.contains_key(provider_id) {
                generation.store(false, Ordering::Release);
            }
        }
        for (account_id, generation) in &previous.account_work_generations {
            if !snapshot.account_work_generations.contains_key(account_id) {
                generation.store(false, Ordering::Release);
            }
        }

        *current = Arc::new(snapshot);
        true
    }

    fn snapshot_from_rows(
        providers: Vec<ProviderRow>,
        accounts: Vec<AccountRow>,
        models: Vec<ModelRow>,
        aliases: Vec<AliasRow>,
        routes: Vec<RouteRow>,
        route_targets: HashMap<String, Vec<RouteTargetRow>>,
    ) -> Snapshot {
        let mut snap = Snapshot::default();
        for provider in providers {
            if provider.enabled == 0 {
                continue;
            }
            snap.provider_order.push(provider.id.clone());
            snap.providers.insert(provider.id.clone(), provider);
        }
        for account in accounts {
            let allowed = snap
                .providers
                .get(&account.provider_id)
                .is_some_and(|provider| match provider.credential_mode.as_str() {
                    "none" => account.label == "__kinetix_noauth__",
                    _ => account.label != "__kinetix_noauth__",
                });
            if allowed {
                snap.accounts.insert(account.id.clone(), account);
            }
        }
        for model in models {
            snap.model_by_upstream.insert(
                (model.provider_id.clone(), model.upstream_id.clone()),
                model.id.clone(),
            );
            snap.models.insert(model.id.clone(), model);
        }
        for alias in aliases {
            snap.aliases.insert(alias.alias.clone(), alias);
        }
        for route in routes {
            if let Some(targets) = route_targets.get(&route.id) {
                snap.route_targets.insert(route.id.clone(), targets.clone());
            }
            snap.routes.insert(route.id.clone(), route);
        }
        snap
    }

    pub(crate) fn lifecycle_lock(&self) -> Arc<parking_lot::Mutex<()>> {
        self.lifecycle.clone()
    }

    /// Take a reference to the current immutable snapshot.
    ///
    /// A request should call this once at the start and use the returned Arc
    /// throughout, so it is unaffected by concurrent configuration changes.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner.read().clone()
    }

    /// Whether an account still belongs to a provider in the active runtime
    /// snapshot. Credential resolution uses this to reject stale request
    /// snapshots after account/provider deletion without retaining tombstones.
    pub fn contains_provider_account(&self, provider_id: &str, account_id: &str) -> bool {
        let snapshot = self.snapshot();
        snapshot.providers.contains_key(provider_id)
            && snapshot.accounts.get(account_id).is_some_and(|account| {
                account.provider_id == provider_id && account.status != "disabled"
            })
    }

    /// Capture the lifecycle generation to use for provider-scoped work. When
    /// an account is supplied, its identity is fenced as well.
    pub fn provider_work_identity(
        &self,
        provider_id: &str,
        account_id: Option<&str>,
    ) -> Option<ProviderWorkIdentity> {
        let snapshot = self.inner.read();
        if !snapshot.providers.contains_key(provider_id) {
            return None;
        }
        let provider_generation = snapshot.provider_work_generations.get(provider_id)?.clone();
        let account_generation = if let Some(account_id) = account_id {
            let account = snapshot.accounts.get(account_id)?;
            if account.provider_id != provider_id || account.status == "disabled" {
                return None;
            }
            Some(snapshot.account_work_generations.get(account_id)?.clone())
        } else {
            None
        };
        let identity = ProviderWorkIdentity {
            provider_id: provider_id.to_string(),
            provider_generation,
            account_generation,
            #[cfg(test)]
            test_provider_id_only: false,
        };
        identity.is_current().then_some(identity)
    }

    /// Bind account work to an already captured provider generation. This
    /// prevents a queued operation from attaching itself to a recreated
    /// provider that happens to reuse the same ID.
    pub fn account_work_identity(
        &self,
        provider_identity: &ProviderWorkIdentity,
        account_id: &str,
    ) -> Option<ProviderWorkIdentity> {
        if !provider_identity.is_current() {
            return None;
        }
        let snapshot = self.inner.read();
        let current_generation = snapshot
            .provider_work_generations
            .get(&provider_identity.provider_id)?;
        if !Arc::ptr_eq(current_generation, &provider_identity.provider_generation) {
            return None;
        }
        let account = snapshot.accounts.get(account_id)?;
        if account.provider_id != provider_identity.provider_id || account.status == "disabled" {
            return None;
        }
        let identity = ProviderWorkIdentity {
            provider_id: provider_identity.provider_id.clone(),
            provider_generation: provider_identity.provider_generation.clone(),
            account_generation: Some(snapshot.account_work_generations.get(account_id)?.clone()),
            #[cfg(test)]
            test_provider_id_only: false,
        };
        identity.is_current().then_some(identity)
    }

    /// Remove a deleted account from the active snapshot immediately. The next
    /// database-backed reload will publish the complete canonical snapshot.
    pub fn forget_account(&self, account_id: &str) {
        let _lifecycle = self.lifecycle.lock();
        self.forget_account_locked(account_id);
    }

    pub(crate) fn forget_account_locked(&self, account_id: &str) {
        let mut current = self.inner.write();
        self.revision.fetch_add(1, Ordering::AcqRel);
        let mut snapshot = (**current).clone();
        snapshot.accounts.remove(account_id);
        if let Some(generation) = snapshot.account_work_generations.remove(account_id) {
            generation.store(false, Ordering::Release);
        }
        *current = Arc::new(snapshot);
    }

    /// Remove a deleted provider and its runtime-owned children immediately.
    pub fn forget_provider(&self, provider_id: &str) {
        let _lifecycle = self.lifecycle.lock();
        self.forget_provider_locked(provider_id);
    }

    pub(crate) fn forget_provider_locked(&self, provider_id: &str) {
        let mut current = self.inner.write();
        self.revision.fetch_add(1, Ordering::AcqRel);
        let mut snapshot = (**current).clone();
        snapshot.providers.remove(provider_id);
        if let Some(generation) = snapshot.provider_work_generations.remove(provider_id) {
            generation.store(false, Ordering::Release);
        }
        let removed_accounts = snapshot
            .accounts
            .values()
            .filter(|account| account.provider_id == provider_id)
            .map(|account| account.id.clone())
            .collect::<Vec<_>>();
        for account_id in removed_accounts {
            snapshot.accounts.remove(&account_id);
            if let Some(generation) = snapshot.account_work_generations.remove(&account_id) {
                generation.store(false, Ordering::Release);
            }
        }
        snapshot.provider_order.retain(|id| id != provider_id);
        snapshot
            .accounts
            .retain(|_, account| account.provider_id != provider_id);
        snapshot
            .models
            .retain(|_, model| model.provider_id != provider_id);
        snapshot
            .model_by_upstream
            .retain(|(id, _), _| id != provider_id);
        *current = Arc::new(snapshot);
    }

    /// The number of providers currently in the active snapshot.
    pub fn provider_count(&self) -> usize {
        self.inner.read().providers.len()
    }

    /// Resolve a client-facing model name to a route against the active snapshot.
    pub fn resolve(&self, requested: &str) -> Option<Resolved> {
        Self::resolve_in(&self.snapshot(), requested)
    }

    /// Resolve against a caller-held snapshot (used by the request pipeline so
    /// the whole request sees one consistent view, NFR-2.10).
    pub fn resolve_in(snap: &Snapshot, requested: &str) -> Option<Resolved> {
        // 1. Alias table.
        if let Some(alias) = snap.aliases.get(requested) {
            if alias.target_type == "route" {
                if let Some(route) = Self::build_route(snap, &alias.target_id) {
                    return Some(route);
                }
            } else if let Some(m) = snap.models.get(&alias.target_id) {
                if m.enabled != 0 {
                    return Some(Resolved::Single {
                        provider_id: m.provider_id.clone(),
                        model_id: m.id.clone(),
                    });
                }
            }
        }

        // 2. Route by name.
        if let Some(route) = snap.routes.values().find(|c| c.name == requested) {
            if let Some(route) = Self::build_route(snap, &route.id) {
                return Some(route);
            }
        }

        // 3. `provider/model-id` (provider matched by name or id).
        if let Some((prov_part, model_part)) = requested.split_once('/') {
            let provider = snap
                .providers
                .values()
                .find(|p| p.name == prov_part || p.id == prov_part);
            if let Some(p) = provider {
                if let Some(mid) = snap
                    .model_by_upstream
                    .get(&(p.id.clone(), model_part.to_string()))
                {
                    if let Some(m) = snap.models.get(mid) {
                        if m.enabled != 0 {
                            return Some(Resolved::Single {
                                provider_id: m.provider_id.clone(),
                                model_id: m.id.clone(),
                            });
                        }
                    }
                }
            }
        }

        // 4. Bare upstream model id.
        if let Some(m) = snap
            .models
            .values()
            .find(|m| m.upstream_id == requested && m.enabled != 0)
        {
            return Some(Resolved::Single {
                provider_id: m.provider_id.clone(),
                model_id: m.id.clone(),
            });
        }

        None
    }

    fn build_route(snap: &Snapshot, route_id: &str) -> Option<Resolved> {
        let route = snap.routes.get(route_id)?.clone();
        if route.enabled == 0 {
            return None;
        }
        let mut targets = Vec::new();
        for t in snap.route_targets.get(route_id).into_iter().flatten() {
            let Some(model) = snap.models.get(&t.model_id).cloned() else {
                continue;
            };
            // A disabled model is never a routable target (FR-10.2).
            if model.enabled == 0 {
                continue;
            }
            let Some(provider) = snap.providers.get(&model.provider_id).cloned() else {
                continue;
            };
            // An explicit account remains pinned. An unpinned target expands
            // to the provider's full account pool; the pipeline later orders
            // these siblings without multiplying this logical target's route
            // priority/weight.
            let accounts: Vec<AccountRow> = match &t.account_id {
                Some(aid) => snap
                    .accounts
                    .get(aid)
                    .filter(|a| a.provider_id == model.provider_id)
                    .cloned()
                    .into_iter()
                    .collect(),
                None => snap
                    .accounts
                    .values()
                    .filter(|a| a.provider_id == model.provider_id)
                    .cloned()
                    .collect(),
            };
            for account in accounts {
                targets.push(ResolvedTarget {
                    account,
                    model: model.clone(),
                    provider: provider.clone(),
                    route_target_id: Some(t.id.clone()),
                    priority: t.priority,
                    weight: t.weight,
                    predicate: crate::predicate::TargetPredicate::parse(&t.predicate),
                    param_overrides: serde_json::from_str(&t.param_overrides)
                        .unwrap_or(serde_json::Value::Null),
                });
            }
        }
        if targets.is_empty() {
            return None;
        }
        Some(Resolved::Route { route, targets })
    }

    /// All enabled models the registry knows, for `/v1/models`.
    pub fn enabled_models(&self) -> Vec<ModelRow> {
        let snap = self.snapshot();
        snap.models
            .values()
            .filter(|m| m.enabled != 0)
            .cloned()
            .collect()
    }

    /// Provider by id.
    pub fn provider(&self, id: &str) -> Option<ProviderRow> {
        self.snapshot().providers.get(id).cloned()
    }

    pub fn model(&self, id: &str) -> Option<ModelRow> {
        self.snapshot().models.get(id).cloned()
    }

    pub fn account(&self, id: &str) -> Option<AccountRow> {
        self.snapshot().accounts.get(id).cloned()
    }

    pub fn route_name(&self, id: &str) -> Option<String> {
        self.snapshot().routes.get(id).map(|c| c.name.clone())
    }

    /// The full route row (used for cache-affinity / portability policy).
    pub fn route_row(&self, id: &str) -> Option<RouteRow> {
        self.snapshot().routes.get(id).cloned()
    }

    pub fn aliases(&self) -> Vec<AliasRow> {
        self.snapshot().aliases.values().cloned().collect()
    }

    /// Enabled Routes whose target list is currently empty, i.e. no eligible
    /// target (disabled model, missing account, or no targets configured).
    /// Used by the startup diagnostic and alerting; it only reads the snapshot.
    pub fn routes_with_no_targets(&self) -> Vec<String> {
        let snap = self.snapshot();
        snap.routes
            .values()
            .filter(|r| r.enabled != 0)
            .filter(|r| {
                snap.route_targets
                    .get(&r.id)
                    .map(|ts| ts.is_empty())
                    .unwrap_or(true)
            })
            .map(|r| r.name.clone())
            .collect()
    }
}

impl RegistryPublication<'_> {
    /// Build and publish a fresh snapshot while holding the publication lock.
    pub(crate) async fn reload(self, pool: &Pool) -> Result<()> {
        let db_revision = db::registry_revision(pool).await?;
        self.reload_at(pool, db_revision).await
    }

    /// Build and publish a snapshot read after observing `db_revision`, so a
    /// concurrent write is at worst reloaded once more, never missed.
    async fn reload_at(self, pool: &Pool, db_revision: i64) -> Result<()> {
        let revision = self.registry.revision.fetch_add(1, Ordering::AcqRel) + 1;
        let snapshot = Registry::build_snapshot(pool).await?;
        if self.registry.activate(snapshot, Some(revision)) {
            self.registry
                .loaded_db_revision
                .store(db_revision, Ordering::Release);
        }
        Ok(())
    }

    /// Publish a snapshot staged from a transaction after that transaction has
    /// committed. The lock excludes concurrent reloads until activation.
    pub(crate) fn activate(self, snapshot: Snapshot) {
        let _ = self.registry.activate(snapshot, None);
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
