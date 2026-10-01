//! Credential strategies (FR-11.2). A strategy supplies and refreshes the
//! credential for an account and reports health to the pool exactly like a
//! static key does. Built-ins only do static API keys today; the seam is
//! capable of representing refresh/expiry/rotation and health reporting so
//! future login-session strategies can be added without touching pool logic.

use async_trait::async_trait;

use crate::crypto::Crypto;
use crate::db::AccountRow;

/// Health of a resolved credential, as reported back to the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialHealth {
    /// Usable now.
    Healthy,
    /// Usable now but expiring at the given RFC3339 instant; the pool may
    /// prefer to refresh before that.
    ExpiringSoon(String),
    /// Not usable; the account should be treated as unhealthy until refreshed.
    Unusable(String),
}

/// Non-secret context associated with a credential lease. Values are supplied
/// separately from the leased secret and may be exposed to the same plugin's
/// adapter for request translation.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialMetadata {
    #[serde(default)]
    pub project_id: Option<String>,
}

impl CredentialMetadata {
    pub fn normalize(&mut self) {
        self.project_id = self
            .project_id
            .take()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
    }
}

/// A credential resolved from a strategy, with optional lease scheduling,
/// non-secret metadata, and rotation metadata. Static keys return no
/// expiry/refresh deadline or adapter metadata.
#[derive(Debug, Clone)]
pub struct ResolvedCredential {
    pub secret: String,
    pub metadata: CredentialMetadata,
    /// RFC3339 instant after which the credential is no longer valid, if known.
    pub expires_at: Option<String>,
    /// RFC3339 instant at which core should proactively renew the credential.
    /// Plugin-provided scheduling wins over core's derived expiry lead.
    pub refresh_after: Option<String>,
    /// Whether this resolution performed a rotation (informational).
    pub rotated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRotationError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub retry_after_secs: Option<u64>,
}

impl CredentialRotationError {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        retry_after_secs: Option<u64>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
            retry_after_secs,
        }
    }

    pub fn invalid_credential(&self) -> bool {
        !self.retryable
            && matches!(
                self.code.as_str(),
                "credential_expired" | "unauthorized" | "auth_error"
            )
    }
}

impl std::fmt::Display for CredentialRotationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CredentialRotationError {}

impl ResolvedCredential {
    fn static_key(secret: String) -> Self {
        ResolvedCredential {
            secret,
            metadata: CredentialMetadata::default(),
            expires_at: None,
            refresh_after: None,
            rotated: false,
        }
    }
}

#[async_trait]
pub trait CredentialStrategy: Send + Sync {
    fn name(&self) -> &'static str;

    /// Return a credential from a local cache without invoking refresh-capable
    /// strategy work. `None` means a full resolution is required.
    async fn resolve_cached(
        &self,
        _account: &AccountRow,
    ) -> std::result::Result<Option<ResolvedCredential>, CredentialRotationError> {
        Ok(None)
    }

    /// Evict local credential state after this account is deleted or replaced.
    /// Strategies without account-scoped caches may keep the default no-op.
    fn forget_account(&self, _provider_id: &str, _account_id: &str) {}

    /// Evict all local credential state for a deleted provider.
    /// Strategies without provider-scoped caches may keep the default no-op.
    fn forget_provider(&self, _provider_id: &str) {}

    /// Resolve the plaintext credential to send upstream for this account.
    /// Refresh-capable strategies may perform network work here.
    async fn resolve(
        &self,
        account: &AccountRow,
    ) -> std::result::Result<ResolvedCredential, CredentialRotationError>;

    /// Report whether the current credential is healthy/expiring/unusable
    /// (FR-11.2 health reporting). Static keys are always healthy.
    async fn health(&self, account: &AccountRow) -> CredentialHealth {
        match self.resolve(account).await {
            Ok(_) => CredentialHealth::Healthy,
            Err(e) => CredentialHealth::Unusable(e.to_string()),
        }
    }

    /// Force a rotation of the credential (FR-11.2 rotation). Static strategies
    /// cannot rotate and report a non-retryable configuration error.
    async fn rotate(
        &self,
        _account: &AccountRow,
    ) -> std::result::Result<(), CredentialRotationError> {
        Err(CredentialRotationError::new(
            "invalid_configuration",
            format!(
                "credential strategy '{}' does not support rotation",
                self.name()
            ),
            false,
            None,
        ))
    }
}

/// Static API key stored encrypted at rest (NFR-3.4).
pub struct StaticKeyStrategy {
    crypto: std::sync::Arc<Crypto>,
}

impl StaticKeyStrategy {
    pub fn new(crypto: std::sync::Arc<Crypto>) -> Self {
        StaticKeyStrategy { crypto }
    }
}

#[async_trait]
impl CredentialStrategy for StaticKeyStrategy {
    fn name(&self) -> &'static str {
        "static_api_key"
    }

    async fn resolve_cached(
        &self,
        account: &AccountRow,
    ) -> std::result::Result<Option<ResolvedCredential>, CredentialRotationError> {
        self.resolve(account).await.map(Some)
    }

    async fn resolve(
        &self,
        account: &AccountRow,
    ) -> std::result::Result<ResolvedCredential, CredentialRotationError> {
        let secret = self.crypto.decrypt(&account.secret_enc).map_err(|error| {
            CredentialRotationError::new(
                "credential_internal",
                format!("decrypting static credential: {error}"),
                false,
                None,
            )
        })?;
        Ok(ResolvedCredential::static_key(secret))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Crypto;

    fn account(secret: &str, crypto: &Crypto) -> AccountRow {
        AccountRow {
            id: "a1".into(),
            provider_id: "p1".into(),
            label: "acct".into(),
            secret_enc: crypto.encrypt(secret).unwrap(),
            key_mask: "sk-…".into(),
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
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn static_strategy_round_trips_and_reports_health() {
        let crypto = Crypto::new(&[7u8; 32]);
        let acct = account("sk-secret", &crypto);
        let strategy = StaticKeyStrategy::new(std::sync::Arc::new(crypto));
        let resolved = strategy.resolve(&acct).await.unwrap();
        assert_eq!(resolved.secret, "sk-secret");
        assert!(resolved.expires_at.is_none());
        assert!(resolved.refresh_after.is_none());
        assert_eq!(strategy.health(&acct).await, CredentialHealth::Healthy);
        assert!(strategy.rotate(&acct).await.is_err());
    }
}
