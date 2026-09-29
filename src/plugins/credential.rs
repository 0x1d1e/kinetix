//! Plugin-backed credential strategy (§6.1, §8.1).
//!
//! A credential plugin *produces or refreshes* an account credential. To keep
//! the secret out of the WIT return value, the plugin writes the credential to
//! its encrypted KV namespace under `lease:<handle>` and returns the opaque
//! `handle`; the host reads it back (decrypting on the host side) and injects it
//! upstream. Core never lets the plugin choose a Route or account.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;

use crate::credentials::{
    CredentialHealth, CredentialRotationError, CredentialStrategy, ResolvedCredential,
};
use crate::crypto::Crypto;
use crate::db::{AccountRow, Pool};

use super::manager::PluginManager;

/// The KV key prefix under which a plugin stores a leased secret.
const LEASE_PREFIX: &str = "lease:";

fn credential_error(fault: super::runtime::PluginFault) -> CredentialRotationError {
    match fault {
        super::runtime::PluginFault::PluginError {
            code,
            message,
            retryable,
            retry_after,
        } => CredentialRotationError::new(code, message, retryable, retry_after),
        // Runtime/host faults are not evidence that the account credential was
        // revoked. Treat them as transient so core never permanently poisons
        // an account because the plugin host failed.
        other => CredentialRotationError::new(other.code(), other.message(), true, None),
    }
}

#[derive(Debug, Clone)]
struct PluginCredentialLease {
    handle: String,
    expires_at: Option<String>,
    refresh_after: Option<String>,
}

struct PluginHealthObservation {
    state: String,
    reset_at: Option<String>,
}

#[async_trait]
trait CredentialPluginHost: Send + Sync {
    async fn resolve_lease(
        &self,
        plugin_id: &str,
        provider_id: &str,
        account_id: &str,
        account_label: &str,
    ) -> Result<PluginCredentialLease, super::runtime::PluginFault>;

    async fn rotate_lease(
        &self,
        plugin_id: &str,
        provider_id: &str,
        account_id: &str,
    ) -> Result<(), super::runtime::PluginFault>;

    async fn health_state(
        &self,
        plugin_id: &str,
        provider_id: &str,
        account_id: &str,
    ) -> Result<PluginHealthObservation, super::runtime::PluginFault>;
}

#[async_trait]
impl CredentialPluginHost for PluginManager {
    async fn resolve_lease(
        &self,
        plugin_id: &str,
        provider_id: &str,
        account_id: &str,
        account_label: &str,
    ) -> Result<PluginCredentialLease, super::runtime::PluginFault> {
        let lease = self
            .credential_resolve(plugin_id, provider_id, account_id, account_label)
            .await?;
        Ok(PluginCredentialLease {
            handle: lease.handle,
            expires_at: lease.expires_at,
            refresh_after: lease.refresh_after,
        })
    }

    async fn rotate_lease(
        &self,
        plugin_id: &str,
        provider_id: &str,
        account_id: &str,
    ) -> Result<(), super::runtime::PluginFault> {
        self.credential_rotate(plugin_id, provider_id, account_id)
            .await
    }

    async fn health_state(
        &self,
        plugin_id: &str,
        provider_id: &str,
        account_id: &str,
    ) -> Result<PluginHealthObservation, super::runtime::PluginFault> {
        self.health_probe(plugin_id, provider_id, account_id)
            .await
            .map(|observation| PluginHealthObservation {
                state: observation.state,
                reset_at: observation.reset_at,
            })
    }
}

pub struct PluginCredentialStrategy {
    manager: Arc<dyn CredentialPluginHost>,
    pool: Pool,
    crypto: Arc<Crypto>,
    plugin_id: String,
    leases: DashMap<(String, String), PluginCredentialLease>,
}

impl PluginCredentialStrategy {
    pub fn new(
        manager: Arc<PluginManager>,
        pool: Pool,
        crypto: Arc<Crypto>,
        plugin_id: impl Into<String>,
    ) -> Self {
        Self::with_host(manager, pool, crypto, plugin_id)
    }

    fn with_host(
        manager: Arc<dyn CredentialPluginHost>,
        pool: Pool,
        crypto: Arc<Crypto>,
        plugin_id: impl Into<String>,
    ) -> Self {
        PluginCredentialStrategy {
            manager,
            pool,
            crypto,
            plugin_id: plugin_id.into(),
            leases: DashMap::new(),
        }
    }

    async fn lease_secret(
        &self,
        handle: &str,
    ) -> std::result::Result<String, CredentialRotationError> {
        let key = format!("{LEASE_PREFIX}{handle}");
        let bytes = super::store::kv_get(&self.pool, &self.crypto, &self.plugin_id, &key)
            .await
            .map_err(|error| {
                CredentialRotationError::new(
                    "plugin_internal",
                    format!("reading plugin credential lease: {error}"),
                    true,
                    None,
                )
            })?
            .ok_or_else(|| {
                CredentialRotationError::new(
                    "plugin_internal",
                    format!(
                        "plugin '{}' returned lease '{handle}' but stored no secret for it",
                        self.plugin_id
                    ),
                    false,
                    None,
                )
            })?;
        String::from_utf8(bytes).map_err(|_| {
            CredentialRotationError::new(
                "plugin_internal",
                "leased credential is not utf-8",
                false,
                None,
            )
        })
    }
}

#[async_trait]
impl CredentialStrategy for PluginCredentialStrategy {
    fn name(&self) -> &'static str {
        "plugin_credential_strategy"
    }

    async fn resolve_cached(
        &self,
        account: &AccountRow,
    ) -> std::result::Result<Option<ResolvedCredential>, CredentialRotationError> {
        let key = (account.provider_id.clone(), account.id.clone());
        let Some(lease) = self.leases.get(&key).map(|entry| entry.value().clone()) else {
            return Ok(None);
        };
        let now = chrono::Utc::now();
        let expires_at = lease.expires_at.as_deref().and_then(crate::db::parse_dt);
        // refresh_after controls proactive renewal, not access-token validity.
        // Keep the cached lease available until explicit expiry; when expiry is
        // unknown, keep using it until auth failure triggers reactive rotation.
        if lease.expires_at.is_some() && expires_at.is_none_or(|deadline| deadline <= now) {
            return Ok(None);
        }

        let secret = self.lease_secret(&lease.handle).await?;
        Ok(Some(ResolvedCredential {
            secret,
            expires_at: lease.expires_at,
            refresh_after: lease.refresh_after,
            rotated: false,
        }))
    }

    async fn resolve(
        &self,
        account: &AccountRow,
    ) -> std::result::Result<ResolvedCredential, CredentialRotationError> {
        let lease = self
            .manager
            .resolve_lease(
                &self.plugin_id,
                &account.provider_id,
                &account.id,
                &account.label,
            )
            .await
            .map_err(credential_error)?;
        let secret = self.lease_secret(&lease.handle).await?;
        self.leases.insert(
            (account.provider_id.clone(), account.id.clone()),
            lease.clone(),
        );
        Ok(ResolvedCredential {
            secret,
            expires_at: lease.expires_at,
            refresh_after: lease.refresh_after,
            // API v1 exposes the handle as opaque lookup data, not as a stable
            // generation signal. Core compares resolved secret and timing
            // identity instead of inferring rotation from handle churn.
            rotated: false,
        })
    }

    async fn rotate(
        &self,
        account: &AccountRow,
    ) -> std::result::Result<(), CredentialRotationError> {
        self.manager
            .rotate_lease(&self.plugin_id, &account.provider_id, &account.id)
            .await
            .map_err(credential_error)
    }

    async fn health(&self, account: &AccountRow) -> CredentialHealth {
        match self
            .manager
            .health_state(&self.plugin_id, &account.provider_id, &account.id)
            .await
        {
            Ok(obs) => match obs.state.as_str() {
                "healthy" => CredentialHealth::Healthy,
                "degraded" => {
                    CredentialHealth::ExpiringSoon(obs.reset_at.unwrap_or_else(|| "unknown".into()))
                }
                _ => CredentialHealth::Unusable(format!("plugin reports state '{}'", obs.state)),
            },
            Err(f) => CredentialHealth::Unusable(f.message()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::db;
    use crate::plugins::runtime::PluginFault;

    #[test]
    fn rotation_error_preserves_retryable_plugin_evidence() {
        let error = credential_error(PluginFault::PluginError {
            code: "upstream_unavailable".into(),
            message: "refresh endpoint unavailable".into(),
            retryable: true,
            retry_after: Some(5),
        });

        assert_eq!(error.code, "upstream_unavailable");
        assert_eq!(error.message, "refresh endpoint unavailable");
        assert!(error.retryable);
        assert_eq!(error.retry_after_secs, Some(5));
        assert!(!error.invalid_credential());
    }

    #[test]
    fn non_retryable_expired_credential_is_terminal() {
        let error = credential_error(PluginFault::PluginError {
            code: "credential_expired".into(),
            message: "refresh token revoked".into(),
            retryable: false,
            retry_after: None,
        });

        assert!(error.invalid_credential());
    }

    struct ChangingLeaseHost {
        pool: Pool,
        crypto: Arc<Crypto>,
        resolutions: AtomicUsize,
        rotations: AtomicUsize,
        expires_at: String,
        refresh_after: String,
        change_secret_on_resolve: bool,
        advance_expiry_on_resolve: bool,
        omit_timing_metadata: bool,
    }

    #[async_trait]
    impl CredentialPluginHost for ChangingLeaseHost {
        async fn resolve_lease(
            &self,
            plugin_id: &str,
            _provider_id: &str,
            _account_id: &str,
            _account_label: &str,
        ) -> Result<PluginCredentialLease, PluginFault> {
            let generation = self.resolutions.fetch_add(1, Ordering::Relaxed);
            let handle = format!("lease-{generation}");
            let secret = if self.change_secret_on_resolve {
                format!("access-token-{generation}")
            } else {
                "stable-access-token".to_owned()
            };
            crate::plugins::store::kv_put(
                &self.pool,
                &self.crypto,
                plugin_id,
                &format!("{LEASE_PREFIX}{handle}"),
                secret.as_bytes(),
            )
            .await
            .map_err(|error| PluginFault::Internal(error.to_string()))?;
            let expires_at = if self.advance_expiry_on_resolve && generation > 0 {
                (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339()
            } else {
                self.expires_at.clone()
            };
            Ok(PluginCredentialLease {
                handle,
                expires_at: (!self.omit_timing_metadata).then_some(expires_at),
                refresh_after: (!self.omit_timing_metadata).then(|| self.refresh_after.clone()),
            })
        }

        async fn rotate_lease(
            &self,
            _plugin_id: &str,
            _provider_id: &str,
            _account_id: &str,
        ) -> Result<(), PluginFault> {
            self.rotations.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn health_state(
            &self,
            _plugin_id: &str,
            _provider_id: &str,
            _account_id: &str,
        ) -> Result<PluginHealthObservation, PluginFault> {
            Ok(PluginHealthObservation {
                state: "healthy".into(),
                reset_at: None,
            })
        }
    }

    async fn test_store(name: &str) -> (std::path::PathBuf, Pool, Arc<Crypto>) {
        let root = std::env::temp_dir().join(format!(
            "kinetix-plugin-credential-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let database_url = format!("sqlite://{}?mode=rwc", root.join("state.db").display());
        let pool = db::connect(&database_url).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let installed_at = db::now_iso();
        sqlx::query(
            r#"INSERT INTO plugins
               (id, version, plugin_api_major, package_sha256, enabled, signature, manifest_json, component, installed_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind("test.plugin")
        .bind("0.0.1")
        .bind(1_i64)
        .bind("test-package-hash")
        .bind(1_i64)
        .bind("unsigned")
        .bind("{}")
        .bind(Vec::<u8>::new())
        .bind(&installed_at)
        .bind(&installed_at)
        .execute(&pool)
        .await
        .unwrap();
        (root, pool, Arc::new(Crypto::new(&[31_u8; 32])))
    }

    fn test_account(id: &str, provider_id: &str) -> AccountRow {
        AccountRow {
            id: id.into(),
            provider_id: provider_id.into(),
            label: "plugin-account".into(),
            secret_enc: String::new(),
            key_mask: String::new(),
            status: "healthy".into(),
            status_reason: "healthy".into(),
            status_changed_at: None,
            account_state_version: 0,
            cooldown_until: None,
            quota_reset_at: None,
            quota_type: "none".into(),
            quota_window_s: None,
            soft_quota_usd: None,
            priority: 1,
            weight: 1,
            last_error: None,
            last_probe_at: None,
            circuit_open_until: None,
            consecutive_failures: 0,
            created_at: db::now_iso(),
        }
    }

    struct MissingSecretOnceHost {
        pool: Pool,
        crypto: Arc<Crypto>,
        resolutions: AtomicUsize,
    }

    #[async_trait]
    impl CredentialPluginHost for MissingSecretOnceHost {
        async fn resolve_lease(
            &self,
            plugin_id: &str,
            _provider_id: &str,
            _account_id: &str,
            _account_label: &str,
        ) -> Result<PluginCredentialLease, PluginFault> {
            let generation = self.resolutions.fetch_add(1, Ordering::Relaxed);
            let handle = format!("materialized-{generation}");
            if generation > 0 {
                crate::plugins::store::kv_put(
                    &self.pool,
                    &self.crypto,
                    plugin_id,
                    &format!("{LEASE_PREFIX}{handle}"),
                    b"valid-access-token",
                )
                .await
                .map_err(|error| PluginFault::Internal(error.to_string()))?;
            }
            let now = chrono::Utc::now();
            Ok(PluginCredentialLease {
                handle,
                expires_at: Some((now + chrono::Duration::hours(1)).to_rfc3339()),
                refresh_after: Some((now + chrono::Duration::minutes(30)).to_rfc3339()),
            })
        }

        async fn rotate_lease(
            &self,
            _plugin_id: &str,
            _provider_id: &str,
            _account_id: &str,
        ) -> Result<(), PluginFault> {
            Ok(())
        }

        async fn health_state(
            &self,
            _plugin_id: &str,
            _provider_id: &str,
            _account_id: &str,
        ) -> Result<PluginHealthObservation, PluginFault> {
            Ok(PluginHealthObservation {
                state: "healthy".into(),
                reset_at: None,
            })
        }
    }

    #[tokio::test]
    async fn cached_lease_remains_available_after_refresh_deadline_during_backoff() {
        let (root, pool, crypto) = test_store("cached-before-expiry").await;
        let now = chrono::Utc::now();
        let host = Arc::new(ChangingLeaseHost {
            pool: pool.clone(),
            crypto: crypto.clone(),
            resolutions: AtomicUsize::new(0),
            rotations: AtomicUsize::new(0),
            expires_at: (now + chrono::Duration::hours(1)).to_rfc3339(),
            refresh_after: (now - chrono::Duration::seconds(1)).to_rfc3339(),
            change_secret_on_resolve: false,
            advance_expiry_on_resolve: false,
            omit_timing_metadata: false,
        });
        let concrete_strategy = Arc::new(PluginCredentialStrategy::with_host(
            host.clone(),
            pool.clone(),
            crypto.clone(),
            "test.plugin",
        ));
        let strategy: Arc<dyn CredentialStrategy> = concrete_strategy.clone();
        let account = test_account("cached-account", "cached-provider");
        let original = strategy.resolve(&account).await.unwrap();

        let provider_work = crate::provider_work::ProviderWorkCoordinator::default();
        provider_work
            .acquire(
                &account.provider_id,
                crate::provider_work::ProviderWorkClass::ModelDiscovery,
            )
            .await
            .unwrap()
            .finish_failure(Some(
                crate::provider_work::ProviderBackoffEvidence::Transient {
                    retry_after_secs: None,
                },
            ))
            .await;

        let refresh = crate::credential_refresh::RefreshCoordinator::default();
        let cached = refresh
            .resolve_cached(&account.provider_id, strategy, &account)
            .await
            .unwrap()
            .expect("valid cached lease must remain usable until expiry");
        assert_eq!(cached.secret, original.secret);
        assert_eq!(host.resolutions.load(Ordering::Relaxed), 1);
        assert_eq!(host.rotations.load(Ordering::Relaxed), 0);

        let no_expiry_account = test_account("no-expiry-account", "no-expiry-provider");
        crate::plugins::store::kv_put(
            &pool,
            &crypto,
            "test.plugin",
            "lease:no-expiry",
            b"unexpired-by-policy",
        )
        .await
        .unwrap();
        concrete_strategy.leases.insert(
            (
                no_expiry_account.provider_id.clone(),
                no_expiry_account.id.clone(),
            ),
            PluginCredentialLease {
                handle: "no-expiry".into(),
                expires_at: None,
                refresh_after: Some((now - chrono::Duration::seconds(1)).to_rfc3339()),
            },
        );
        assert_eq!(
            concrete_strategy
                .resolve_cached(&no_expiry_account)
                .await
                .unwrap()
                .unwrap()
                .secret,
            "unexpired-by-policy"
        );

        let expired_account = test_account("expired-account", "expired-provider");
        concrete_strategy.leases.insert(
            (
                expired_account.provider_id.clone(),
                expired_account.id.clone(),
            ),
            PluginCredentialLease {
                handle: "already-expired".into(),
                expires_at: Some((now - chrono::Duration::seconds(1)).to_rfc3339()),
                refresh_after: None,
            },
        );
        assert!(concrete_strategy
            .resolve_cached(&expired_account)
            .await
            .unwrap()
            .is_none());

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_lease_materialization_is_not_cached() {
        let (root, pool, crypto) = test_store("missing-lease-secret").await;
        let host = Arc::new(MissingSecretOnceHost {
            pool: pool.clone(),
            crypto: crypto.clone(),
            resolutions: AtomicUsize::new(0),
        });
        let strategy: Arc<dyn CredentialStrategy> = Arc::new(PluginCredentialStrategy::with_host(
            host.clone(),
            pool.clone(),
            crypto,
            "test.plugin",
        ));
        let account = test_account("materialization-account", "materialization-provider");
        let refresh = crate::credential_refresh::RefreshCoordinator::default();

        assert!(refresh
            .resolve(&account.provider_id, strategy.clone(), &account)
            .await
            .is_err());
        let recovered = refresh
            .resolve(&account.provider_id, strategy, &account)
            .await
            .expect("a failed lease must not prevent fresh resolution");
        assert_eq!(recovered.secret, "valid-access-token");
        assert_eq!(host.resolutions.load(Ordering::Relaxed), 2);

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn plugin_strategy_ignores_handle_churn_but_detects_secret_changes() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-plugin-credential-resolve-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let database_url = format!("sqlite://{}?mode=rwc", root.join("state.db").display());
        let pool = db::connect(&database_url).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let installed_at = db::now_iso();
        sqlx::query(
            "INSERT INTO plugins \
             (id, version, plugin_api_major, package_sha256, enabled, signature, manifest_json, component, installed_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("test.plugin")
        .bind("0.0.1")
        .bind(1_i64)
        .bind("test-package-hash")
        .bind(1_i64)
        .bind("unsigned")
        .bind("{}")
        .bind(Vec::<u8>::new())
        .bind(&installed_at)
        .bind(&installed_at)
        .execute(&pool)
        .await
        .unwrap();
        let crypto = Arc::new(Crypto::new(&[31_u8; 32]));
        let now = chrono::Utc::now();
        let host = Arc::new(ChangingLeaseHost {
            pool: pool.clone(),
            crypto: crypto.clone(),
            resolutions: AtomicUsize::new(0),
            rotations: AtomicUsize::new(0),
            expires_at: (now + chrono::Duration::hours(1)).to_rfc3339(),
            refresh_after: (now - chrono::Duration::seconds(1)).to_rfc3339(),
            change_secret_on_resolve: false,
            advance_expiry_on_resolve: false,
            omit_timing_metadata: false,
        });
        let strategy = Arc::new(PluginCredentialStrategy::with_host(
            host.clone(),
            pool.clone(),
            crypto.clone(),
            "test.plugin",
        ));
        let account = AccountRow {
            id: "acc_plugin_lease_generation".into(),
            provider_id: "provider_plugin_lease_generation".into(),
            label: "plugin-account".into(),
            secret_enc: String::new(),
            key_mask: String::new(),
            status: "healthy".into(),
            status_reason: "healthy".into(),
            status_changed_at: None,
            account_state_version: 0,
            cooldown_until: None,
            quota_reset_at: None,
            quota_type: "none".into(),
            quota_window_s: None,
            soft_quota_usd: None,
            priority: 1,
            weight: 1,
            last_error: None,
            last_probe_at: None,
            circuit_open_until: None,
            consecutive_failures: 0,
            created_at: db::now_iso(),
        };
        let coordinator = crate::credential_refresh::RefreshCoordinator::default();

        let first = coordinator
            .resolve(
                "provider_plugin_lease_generation",
                strategy.clone(),
                &account,
            )
            .await
            .unwrap();
        assert!(!first.rotated);
        assert_eq!(coordinator.claim_due(chrono::Utc::now()).len(), 1);

        assert!(coordinator
            .rotate_scheduled("provider_plugin_lease_generation", strategy, &account,)
            .await
            .unwrap());
        assert_eq!(host.resolutions.load(Ordering::Relaxed), 3);
        assert_eq!(host.rotations.load(Ordering::Relaxed), 1);
        let now = chrono::Utc::now();
        assert!(
            coordinator
                .next_attempt_at("provider_plugin_lease_generation", &account.id)
                .unwrap()
                >= now + chrono::Duration::seconds(59)
        );
        assert!(coordinator.claim_due(now).is_empty());

        let secret_refresh_host = Arc::new(ChangingLeaseHost {
            pool: pool.clone(),
            crypto: crypto.clone(),
            resolutions: AtomicUsize::new(0),
            rotations: AtomicUsize::new(0),
            expires_at: (now + chrono::Duration::hours(1)).to_rfc3339(),
            refresh_after: (now - chrono::Duration::seconds(1)).to_rfc3339(),
            change_secret_on_resolve: true,
            advance_expiry_on_resolve: false,
            omit_timing_metadata: false,
        });
        let secret_refresh_strategy = Arc::new(PluginCredentialStrategy::with_host(
            secret_refresh_host.clone(),
            pool.clone(),
            crypto.clone(),
            "test.plugin",
        ));
        let secret_refresh_account = AccountRow {
            id: "acc_plugin_secret_generation".into(),
            provider_id: "provider_plugin_secret_generation".into(),
            label: "plugin-account-secret-generation".into(),
            secret_enc: String::new(),
            key_mask: String::new(),
            status: "healthy".into(),
            status_reason: "healthy".into(),
            status_changed_at: None,
            account_state_version: 0,
            cooldown_until: None,
            quota_reset_at: None,
            quota_type: "none".into(),
            quota_window_s: None,
            soft_quota_usd: None,
            priority: 1,
            weight: 1,
            last_error: None,
            last_probe_at: None,
            circuit_open_until: None,
            consecutive_failures: 0,
            created_at: db::now_iso(),
        };
        let first_secret_lease = coordinator
            .resolve(
                &secret_refresh_account.provider_id,
                secret_refresh_strategy.clone(),
                &secret_refresh_account,
            )
            .await
            .unwrap();
        assert!(!first_secret_lease.rotated);
        assert!(coordinator
            .claim_due(chrono::Utc::now())
            .iter()
            .any(|key| key.account_id == secret_refresh_account.id));
        assert!(!coordinator
            .rotate_scheduled(
                &secret_refresh_account.provider_id,
                secret_refresh_strategy,
                &secret_refresh_account,
            )
            .await
            .unwrap());
        assert_eq!(secret_refresh_host.resolutions.load(Ordering::Relaxed), 2);
        assert_eq!(secret_refresh_host.rotations.load(Ordering::Relaxed), 0);
        let now = chrono::Utc::now();
        assert!(
            coordinator
                .next_attempt_at(
                    &secret_refresh_account.provider_id,
                    &secret_refresh_account.id
                )
                .unwrap()
                >= now + chrono::Duration::seconds(59)
        );
        assert!(coordinator.claim_due(now).is_empty());

        let timing_refresh_host = Arc::new(ChangingLeaseHost {
            pool: pool.clone(),
            crypto: crypto.clone(),
            resolutions: AtomicUsize::new(0),
            rotations: AtomicUsize::new(0),
            expires_at: (now + chrono::Duration::hours(1)).to_rfc3339(),
            refresh_after: (now - chrono::Duration::seconds(1)).to_rfc3339(),
            change_secret_on_resolve: false,
            advance_expiry_on_resolve: true,
            omit_timing_metadata: false,
        });
        let timing_refresh_strategy = Arc::new(PluginCredentialStrategy::with_host(
            timing_refresh_host,
            pool.clone(),
            crypto.clone(),
            "test.plugin",
        ));
        let timing_refresh_account = AccountRow {
            id: "acc_plugin_timing_generation".into(),
            provider_id: "provider_plugin_timing_generation".into(),
            label: "plugin-account-timing-generation".into(),
            secret_enc: String::new(),
            key_mask: String::new(),
            status: "healthy".into(),
            status_reason: "healthy".into(),
            status_changed_at: None,
            account_state_version: 0,
            cooldown_until: None,
            quota_reset_at: None,
            quota_type: "none".into(),
            quota_window_s: None,
            soft_quota_usd: None,
            priority: 1,
            weight: 1,
            last_error: None,
            last_probe_at: None,
            circuit_open_until: None,
            consecutive_failures: 0,
            created_at: db::now_iso(),
        };
        let first_timing_lease = coordinator
            .resolve(
                &timing_refresh_account.provider_id,
                timing_refresh_strategy.clone(),
                &timing_refresh_account,
            )
            .await
            .unwrap();
        assert!(!first_timing_lease.rotated);
        assert!(coordinator
            .claim_due(chrono::Utc::now())
            .iter()
            .any(|key| key.account_id == timing_refresh_account.id));
        let did_rotate = coordinator
            .rotate_scheduled(
                &timing_refresh_account.provider_id,
                timing_refresh_strategy,
                &timing_refresh_account,
            )
            .await
            .unwrap();
        assert!(!did_rotate);
        let now = chrono::Utc::now();
        assert!(
            coordinator
                .next_attempt_at(
                    &timing_refresh_account.provider_id,
                    &timing_refresh_account.id
                )
                .unwrap()
                >= now + chrono::Duration::seconds(59)
        );
        assert!(coordinator.claim_due(now).is_empty());

        let cached_now = chrono::Utc::now();
        let cached_host = Arc::new(ChangingLeaseHost {
            pool: pool.clone(),
            crypto: crypto.clone(),
            resolutions: AtomicUsize::new(0),
            rotations: AtomicUsize::new(0),
            expires_at: (cached_now + chrono::Duration::hours(1)).to_rfc3339(),
            refresh_after: (cached_now + chrono::Duration::minutes(30)).to_rfc3339(),
            change_secret_on_resolve: true,
            advance_expiry_on_resolve: false,
            omit_timing_metadata: false,
        });
        let cached_strategy = PluginCredentialStrategy::with_host(
            cached_host.clone(),
            pool.clone(),
            crypto.clone(),
            "test.plugin",
        );
        let mut cached_account = timing_refresh_account.clone();
        cached_account.id = "acc_plugin_cached_lease".into();
        cached_account.provider_id = "provider_plugin_cached_lease".into();
        let resolved = cached_strategy.resolve(&cached_account).await.unwrap();
        let cached = cached_strategy
            .resolve_cached(&cached_account)
            .await
            .unwrap()
            .expect("fresh plugin lease should be available without invoking the plugin");
        assert_eq!(cached.secret, resolved.secret);
        assert_eq!(cached_host.resolutions.load(Ordering::Relaxed), 1);

        let unbounded_host = Arc::new(ChangingLeaseHost {
            pool: pool.clone(),
            crypto: crypto.clone(),
            resolutions: AtomicUsize::new(0),
            rotations: AtomicUsize::new(0),
            expires_at: String::new(),
            refresh_after: String::new(),
            change_secret_on_resolve: true,
            advance_expiry_on_resolve: false,
            omit_timing_metadata: true,
        });
        let unbounded_strategy = PluginCredentialStrategy::with_host(
            unbounded_host.clone(),
            pool.clone(),
            crypto.clone(),
            "test.plugin",
        );
        let mut unbounded_account = cached_account.clone();
        unbounded_account.id = "acc_plugin_unbounded_lease".into();
        unbounded_account.provider_id = "provider_plugin_unbounded_lease".into();
        let resolved = unbounded_strategy
            .resolve(&unbounded_account)
            .await
            .unwrap();
        let cached = unbounded_strategy
            .resolve_cached(&unbounded_account)
            .await
            .unwrap()
            .expect("lease without timing metadata should remain available");
        assert_eq!(cached.secret, resolved.secret);
        assert_eq!(unbounded_host.resolutions.load(Ordering::Relaxed), 1);

        pool.close().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
