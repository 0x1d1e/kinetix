//! Proactive credential lease scheduling and refresh singleflight.
//!
//! Core owns *when* a credential should be renewed; credential strategies own
//! *how* renewal happens. The coordinator never stores plaintext credentials.
//! It keeps only account identity, lease deadlines, retry state, and the
//! account-scoped rotation gate shared by proactive and reactive refreshes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dashmap::DashMap;
use sha2::{Digest, Sha256};

use crate::credentials::{CredentialRotationError, CredentialStrategy, ResolvedCredential};
use crate::db::AccountRow;

const DEFAULT_REFRESH_LEAD_SECS: i64 = 5 * 60;
const MIN_RETRY_SECS: u64 = 5;
const MAX_RETRY_SECS: u64 = 5 * 60;
const CLAIM_GRACE_SECS: i64 = 60;
const MIN_SUCCESSFUL_REFRESH_INTERVAL_SECS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialKey {
    pub provider_id: String,
    pub account_id: String,
}

impl CredentialKey {
    pub fn new(provider_id: impl Into<String>, account_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            account_id: account_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaseIdentity {
    expires_at: Option<DateTime<Utc>>,
    refresh_after: Option<DateTime<Utc>>,
    secret_fingerprint: [u8; 32],
}

#[derive(Debug, Clone)]
struct LeaseSchedule {
    next_attempt_at: DateTime<Utc>,
    failures: u32,
    lease_identity: LeaseIdentity,
    claim_until: Option<DateTime<Utc>>,
}

struct RotationGate {
    lock: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    last_auth_result:
        parking_lot::Mutex<Option<std::result::Result<bool, CredentialRotationError>>>,
}

impl RotationGate {
    fn new() -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            last_auth_result: parking_lot::Mutex::new(None),
        }
    }
}

/// Host-owned credential refresh scheduler.
///
/// Cloning this type is cheap. All state is shared and bounded by the number of
/// plugin-backed accounts Kinetix has observed.
#[derive(Clone, Default)]
pub struct RefreshCoordinator {
    schedules: Arc<DashMap<CredentialKey, LeaseSchedule>>,
    gates: Arc<DashMap<CredentialKey, Arc<RotationGate>>>,
}

impl RefreshCoordinator {
    /// Observe a freshly resolved credential lease and schedule its next
    /// proactive renewal. Explicit plugin-provided `refresh_after` wins;
    /// otherwise core derives a provider-neutral five-minute lead from expiry.
    pub fn observe(&self, provider_id: &str, account_id: &str, credential: &ResolvedCredential) {
        self.store_schedule(provider_id, account_id, credential, false, Utc::now(), None);
    }

    /// Read a cached credential under the account-scoped gate so it cannot
    /// race with a scheduled or reactive rotation.
    pub async fn resolve_cached(
        &self,
        provider_id: &str,
        strategy: Arc<dyn CredentialStrategy>,
        account: &AccountRow,
    ) -> std::result::Result<Option<ResolvedCredential>, CredentialRotationError> {
        let key = CredentialKey::new(provider_id, &account.id);
        let gate = self.gate(&key);
        let _guard = gate.lock.lock().await;
        let current = strategy.resolve_cached(account).await?;
        if let Some(current) = &current {
            self.observe(provider_id, &account.id, current);
        }
        Ok(current)
    }

    /// Resolve an account credential under the same account-scoped gate used
    /// by cached reads and scheduled/reactive refresh. Some strategies refresh
    /// inside resolve(), so the cache check and full resolution share the gate.
    pub async fn resolve(
        &self,
        provider_id: &str,
        strategy: Arc<dyn CredentialStrategy>,
        account: &AccountRow,
    ) -> std::result::Result<ResolvedCredential, CredentialRotationError> {
        let key = CredentialKey::new(provider_id, &account.id);
        let gate = self.gate(&key);
        let _guard = gate.lock.lock().await;

        if let Some(current) = strategy.resolve_cached(account).await? {
            self.observe(provider_id, &account.id, &current);
            return Ok(current);
        }

        match strategy.resolve(account).await {
            Ok(current) => {
                if current.rotated {
                    self.observe_successful_rotation(
                        provider_id,
                        &account.id,
                        &current,
                        Utc::now(),
                    );
                } else {
                    self.observe(provider_id, &account.id, &current);
                }
                Ok(current)
            }
            Err(error) => Err(error),
        }
    }

    fn observe_refreshed(
        &self,
        provider_id: &str,
        account_id: &str,
        credential: &ResolvedCredential,
    ) {
        self.store_schedule(provider_id, account_id, credential, true, Utc::now(), None);
    }

    fn observe_successful_rotation(
        &self,
        provider_id: &str,
        account_id: &str,
        credential: &ResolvedCredential,
        now: DateTime<Utc>,
    ) {
        let not_before =
            now.to_owned() + ChronoDuration::seconds(MIN_SUCCESSFUL_REFRESH_INTERVAL_SECS);
        self.store_schedule(
            provider_id,
            account_id,
            credential,
            true,
            now,
            Some(not_before),
        );
    }

    fn store_schedule(
        &self,
        provider_id: &str,
        account_id: &str,
        credential: &ResolvedCredential,
        reset_failures: bool,
        now: DateTime<Utc>,
        not_before: Option<DateTime<Utc>>,
    ) {
        let key = CredentialKey::new(provider_id, account_id);
        let Some(mut schedule) = schedule_from_credential(credential, now) else {
            self.schedules.remove(&key);
            return;
        };
        let mut identity_changed = false;
        if let Some(existing) = self.schedules.get(&key) {
            let same_timing = existing.lease_identity.expires_at
                == schedule.lease_identity.expires_at
                && existing.lease_identity.refresh_after == schedule.lease_identity.refresh_after;
            let same_secret = existing.lease_identity.secret_fingerprint
                == schedule.lease_identity.secret_fingerprint;
            if same_timing {
                // Keep the deadline anchored to the first observation of these
                // timing hints, even when successful resolution changes only
                // the secret. Reset failures after success, but clear any
                // outstanding claim when the secret generation changed.
                schedule.next_attempt_at = existing.next_attempt_at;
                if !reset_failures {
                    schedule.failures = existing.failures;
                }
                schedule.claim_until = if !reset_failures && same_secret {
                    existing.claim_until
                } else {
                    None
                };
                identity_changed = !same_secret;
            } else {
                // A changed timing identity makes the prior claim stale.
                schedule.claim_until = None;
                identity_changed = true;
                if !reset_failures && existing.failures > 0 {
                    schedule.failures = existing.failures;
                    schedule.next_attempt_at =
                        schedule.next_attempt_at.max(existing.next_attempt_at);
                }
            }
        }

        if identity_changed || not_before.is_some() {
            let retry_floor = not_before.unwrap_or_else(|| {
                now + ChronoDuration::seconds(MIN_SUCCESSFUL_REFRESH_INTERVAL_SECS)
            });
            Self::apply_minimum_retry_floor(&mut schedule, now, retry_floor);
        }

        self.schedules.insert(key, schedule);
    }

    fn apply_minimum_retry_floor(
        schedule: &mut LeaseSchedule,
        now: DateTime<Utc>,
        floor: DateTime<Utc>,
    ) {
        if schedule.next_attempt_at > now {
            return;
        }

        let floor = schedule
            .lease_identity
            .expires_at
            .as_ref()
            .filter(|expires_at| **expires_at > now)
            .map_or(floor, |expires_at| floor.min(*expires_at));
        schedule.next_attempt_at = schedule.next_attempt_at.max(floor);
    }

    /// Forget all proactive state for an account. Used when an account is
    /// removed, disabled, or a refresh token is confirmed invalid.
    pub fn forget(&self, provider_id: &str, account_id: &str) {
        let key = CredentialKey::new(provider_id, account_id);
        self.schedules.remove(&key);
        self.gates.remove(&key);
    }

    /// Forget all proactive refresh state for a deleted provider.
    pub fn forget_provider(&self, provider_id: &str) {
        self.schedules
            .retain(|key, _| key.provider_id != provider_id);
        self.gates.retain(|key, _| key.provider_id != provider_id);
    }

    /// Atomically claim every lease whose refresh deadline has arrived.
    ///
    /// Claiming records a separate grace marker so the scheduler cannot
    /// enqueue duplicate work while a slow plugin refresh is running; it does
    /// not move the lease's stable refresh deadline.
    pub fn claim_due(&self, now: DateTime<Utc>) -> Vec<CredentialKey> {
        let mut due = Vec::new();
        for mut entry in self.schedules.iter_mut() {
            let claim_active = entry
                .claim_until
                .as_ref()
                .is_some_and(|claim_until| claim_until > &now);
            if entry.next_attempt_at <= now && !claim_active {
                entry.claim_until =
                    Some(now.to_owned() + ChronoDuration::seconds(CLAIM_GRACE_SECS));
                due.push(entry.key().clone());
            }
        }
        due
    }

    /// Reactive 401/auth-error renewal. This preserves the existing semantic:
    /// concurrent failures for one account perform one rotation and all waiters
    /// reuse its result. It shares the exact same gate as proactive refresh.
    pub async fn rotate_after_auth_error(
        &self,
        provider_id: &str,
        strategy: Arc<dyn CredentialStrategy>,
        account: &AccountRow,
        failed_secret: &str,
    ) -> std::result::Result<bool, CredentialRotationError> {
        let key = CredentialKey::new(provider_id, &account.id);
        let gate = self.gate(&key);
        let observed_generation = gate.generation.load(Ordering::Acquire);
        let _guard = gate.lock.lock().await;

        if gate.generation.load(Ordering::Acquire) != observed_generation {
            if let Some(result) = gate.last_auth_result.lock().clone() {
                return result;
            }
        }

        let result = match strategy.resolve(account).await {
            Ok(current) if current.secret != failed_secret => {
                self.observe_successful_rotation(provider_id, &account.id, &current, Utc::now());
                Ok(true)
            }
            Err(error) if error.invalid_credential() => Err(error),
            Ok(_) | Err(_) => match strategy.rotate(account).await {
                Ok(()) => match strategy.resolve(account).await {
                    Ok(current) => {
                        self.observe_successful_rotation(
                            provider_id,
                            &account.id,
                            &current,
                            Utc::now(),
                        );
                        Ok(true)
                    }
                    Err(error) => {
                        if !error.invalid_credential() {
                            self.record_failure(&key, &error);
                        }
                        Err(error)
                    }
                },
                Err(error) => {
                    if !error.invalid_credential() {
                        self.record_failure(&key, &error);
                    }
                    Err(error)
                }
            },
        };

        *gate.last_auth_result.lock() = Some(result.clone());
        gate.generation.fetch_add(1, Ordering::Release);
        result
    }

    /// Refresh one scheduler-claimed account. A re-resolve under the shared
    /// account lock prevents a proactive tick from rotating a credential that
    /// a concurrent reactive request already renewed.
    pub async fn rotate_scheduled(
        &self,
        provider_id: &str,
        strategy: Arc<dyn CredentialStrategy>,
        account: &AccountRow,
    ) -> std::result::Result<bool, CredentialRotationError> {
        self.rotate_scheduled_at(provider_id, strategy, account, Utc::now())
            .await
    }

    async fn rotate_scheduled_at(
        &self,
        provider_id: &str,
        strategy: Arc<dyn CredentialStrategy>,
        account: &AccountRow,
        now: DateTime<Utc>,
    ) -> std::result::Result<bool, CredentialRotationError> {
        let key = CredentialKey::new(provider_id, &account.id);
        let gate = self.gate(&key);
        let _guard = gate.lock.lock().await;

        let current = match strategy.resolve(account).await {
            Ok(current) => current,
            Err(error) => {
                if !error.invalid_credential() {
                    self.record_failure(&key, &error);
                }
                return Err(error);
            }
        };

        let Some(current_schedule) = schedule_from_credential(&current, now.to_owned()) else {
            self.schedules.remove(&key);
            return Ok(false);
        };
        let Some(stored_schedule) = self.schedules.get(&key).map(|entry| entry.clone()) else {
            self.observe_refreshed(provider_id, &account.id, &current);
            return Ok(false);
        };
        if current.rotated || stored_schedule.lease_identity != current_schedule.lease_identity {
            // A resolve-side rotation or changed lease means another path has
            // already advanced the credential. Reschedule it safely instead of
            // immediately calling rotate() a second time.
            self.observe_successful_rotation(
                provider_id,
                &account.id,
                &current,
                Utc::now().max(now),
            );
            return Ok(false);
        }
        if stored_schedule.next_attempt_at > now {
            return Ok(false);
        }

        match strategy.rotate(account).await {
            Ok(()) => {
                let current = match strategy.resolve(account).await {
                    Ok(current) => current,
                    Err(error) => {
                        if !error.invalid_credential() {
                            self.record_failure(&key, &error);
                        }
                        return Err(error);
                    }
                };
                self.observe_successful_rotation(
                    provider_id,
                    &account.id,
                    &current,
                    Utc::now().max(now),
                );
                // A scheduled rotation changed the generation, but it did not
                // produce a reactive auth result. Clear any older auth result
                // so a queued 401 waiter re-resolves instead of reusing stale
                // outcome state from an earlier auth failure.
                *gate.last_auth_result.lock() = None;
                gate.generation.fetch_add(1, Ordering::Release);
                Ok(true)
            }
            Err(error) => {
                if !error.invalid_credential() {
                    self.record_failure(&key, &error);
                }
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub fn next_attempt_at(&self, provider_id: &str, account_id: &str) -> Option<DateTime<Utc>> {
        self.schedules
            .get(&CredentialKey::new(provider_id, account_id))
            .map(|entry| entry.next_attempt_at.to_owned())
    }

    fn gate(&self, key: &CredentialKey) -> Arc<RotationGate> {
        self.gates
            .entry(key.clone())
            .or_insert_with(|| Arc::new(RotationGate::new()))
            .clone()
    }

    fn record_failure(&self, key: &CredentialKey, error: &CredentialRotationError) {
        let now = Utc::now();
        let mut schedule = self.schedules.entry(key.clone()).or_insert(LeaseSchedule {
            next_attempt_at: now.to_owned(),
            failures: 0,
            lease_identity: LeaseIdentity {
                expires_at: None,
                refresh_after: None,
                secret_fingerprint: [0; 32],
            },
            claim_until: None,
        });
        schedule.failures = schedule.failures.saturating_add(1);
        let retry = error
            .retry_after_secs
            .unwrap_or_else(|| exponential_backoff_secs(schedule.failures));
        schedule.next_attempt_at = now + ChronoDuration::seconds(retry.min(MAX_RETRY_SECS) as i64);
        schedule.claim_until = None;
    }
}

fn parse_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    value.and_then(crate::db::parse_dt)
}

pub(crate) fn lease_refresh_deadline(
    expires_at: Option<&str>,
    refresh_after: Option<&str>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let expires_at = parse_time(expires_at);
    let requested = parse_time(refresh_after).or_else(|| {
        expires_at.as_ref().map(|expires| {
            // Keep half of a short lease available before refreshing. Applying
            // the fixed lead to a lease shorter than five minutes would make
            // it due immediately after a successful refresh.
            let remaining_secs = (expires.to_owned() - now.to_owned()).num_seconds().max(0);
            let lead_secs = (remaining_secs / 2).min(DEFAULT_REFRESH_LEAD_SECS);
            expires.to_owned() - ChronoDuration::seconds(lead_secs)
        })
    });
    requested.map(|deadline| deadline.max(now))
}

fn schedule_from_credential(
    credential: &ResolvedCredential,
    now: DateTime<Utc>,
) -> Option<LeaseSchedule> {
    // Retain only a one-way in-memory fingerprint as a lease-generation
    // signal. It is neither logged nor persisted.
    let lease_identity = LeaseIdentity {
        expires_at: parse_time(credential.expires_at.as_deref()),
        refresh_after: parse_time(credential.refresh_after.as_deref()),
        secret_fingerprint: Sha256::digest(credential.secret.as_bytes()).into(),
    };

    let next_attempt_at = lease_refresh_deadline(
        credential.expires_at.as_deref(),
        credential.refresh_after.as_deref(),
        now,
    )?;

    Some(LeaseSchedule {
        next_attempt_at,
        failures: 0,
        lease_identity,
        claim_until: None,
    })
}

fn exponential_backoff_secs(failures: u32) -> u64 {
    let shift = failures.saturating_sub(1).min(6);
    MIN_RETRY_SECS
        .saturating_mul(1_u64 << shift)
        .min(MAX_RETRY_SECS)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;

    fn account() -> AccountRow {
        AccountRow {
            id: "a1".into(),
            provider_id: "p1".into(),
            label: "oauth".into(),
            secret_enc: String::new(),
            key_mask: "oauth:****".into(),
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
            created_at: Utc::now().to_rfc3339(),
        }
    }

    struct RotatingStrategy {
        rotations: AtomicUsize,
        secret: tokio::sync::Mutex<String>,
        short_lease_after_rotation: bool,
        initial_expires_at: String,
        rotated_expires_at: String,
        short_expires_at: String,
        initial_refresh_after: String,
        rotated_refresh_after: String,
    }

    impl RotatingStrategy {
        fn new() -> Self {
            Self::with_short_lease(false)
        }

        fn with_short_lease_after_rotation() -> Self {
            Self::with_short_lease(true)
        }

        fn with_short_lease(short_lease_after_rotation: bool) -> Self {
            let now = Utc::now();
            Self {
                rotations: AtomicUsize::new(0),
                secret: tokio::sync::Mutex::new("stale".into()),
                short_lease_after_rotation,
                initial_expires_at: (now.to_owned() + ChronoDuration::hours(2)).to_rfc3339(),
                rotated_expires_at: (now.to_owned() + ChronoDuration::hours(3)).to_rfc3339(),
                short_expires_at: (now.to_owned() + ChronoDuration::minutes(3)).to_rfc3339(),
                initial_refresh_after: (now.to_owned() - ChronoDuration::seconds(1)).to_rfc3339(),
                rotated_refresh_after: (now + ChronoDuration::hours(1)).to_rfc3339(),
            }
        }
    }

    #[async_trait]
    impl CredentialStrategy for RotatingStrategy {
        fn name(&self) -> &'static str {
            "test_rotating"
        }

        async fn resolve_cached(
            &self,
            _account: &AccountRow,
        ) -> std::result::Result<Option<ResolvedCredential>, CredentialRotationError> {
            let now = Utc::now();
            let rotations = self.rotations.load(Ordering::Relaxed);
            let has_short_lease = self.short_lease_after_rotation && rotations > 0;
            let expires_at = if has_short_lease {
                self.short_expires_at.clone()
            } else if rotations == 0 {
                self.initial_expires_at.clone()
            } else {
                self.rotated_expires_at.clone()
            };
            let refresh_after = if has_short_lease {
                None
            } else if rotations == 0 {
                Some(self.initial_refresh_after.clone())
            } else {
                Some(self.rotated_refresh_after.clone())
            };
            if lease_refresh_deadline(Some(&expires_at), refresh_after.as_deref(), now)
                .is_some_and(|deadline| deadline > now)
            {
                Ok(Some(ResolvedCredential {
                    secret: self.secret.lock().await.clone(),
                    expires_at: Some(expires_at),
                    refresh_after,
                    rotated: false,
                }))
            } else {
                Ok(None)
            }
        }

        async fn resolve(
            &self,
            _account: &AccountRow,
        ) -> std::result::Result<ResolvedCredential, CredentialRotationError> {
            let rotations = self.rotations.load(Ordering::Relaxed);
            let has_short_lease = self.short_lease_after_rotation && rotations > 0;
            let expires_at = if has_short_lease {
                self.short_expires_at.clone()
            } else if rotations == 0 {
                self.initial_expires_at.clone()
            } else {
                self.rotated_expires_at.clone()
            };
            Ok(ResolvedCredential {
                secret: self.secret.lock().await.clone(),
                expires_at: Some(expires_at),
                refresh_after: if has_short_lease {
                    None
                } else if rotations == 0 {
                    Some(self.initial_refresh_after.clone())
                } else {
                    Some(self.rotated_refresh_after.clone())
                },
                rotated: rotations > 0,
            })
        }

        async fn rotate(
            &self,
            _account: &AccountRow,
        ) -> std::result::Result<(), CredentialRotationError> {
            self.rotations.fetch_add(1, Ordering::Relaxed);
            *self.secret.lock().await = "fresh".into();
            Ok(())
        }
    }

    struct FixedShortLeaseStrategy {
        expires_at: String,
        refresh_after: Option<String>,
        rotated_on_resolve: bool,
        rotations: AtomicUsize,
    }

    #[async_trait]
    impl CredentialStrategy for FixedShortLeaseStrategy {
        fn name(&self) -> &'static str {
            "test_fixed_short_lease"
        }

        async fn resolve_cached(
            &self,
            _account: &AccountRow,
        ) -> std::result::Result<Option<ResolvedCredential>, CredentialRotationError> {
            let now = Utc::now();
            let is_fresh =
                lease_refresh_deadline(Some(&self.expires_at), self.refresh_after.as_deref(), now)
                    .is_some_and(|deadline| deadline > now);
            Ok(is_fresh.then(|| ResolvedCredential {
                secret: "fixed-lease-token".into(),
                expires_at: Some(self.expires_at.clone()),
                refresh_after: self.refresh_after.clone(),
                rotated: false,
            }))
        }

        async fn resolve(
            &self,
            _account: &AccountRow,
        ) -> std::result::Result<ResolvedCredential, CredentialRotationError> {
            Ok(ResolvedCredential {
                secret: "fixed-lease-token".into(),
                expires_at: Some(self.expires_at.clone()),
                refresh_after: self.refresh_after.clone(),
                rotated: self.rotated_on_resolve,
            })
        }

        async fn rotate(
            &self,
            _account: &AccountRow,
        ) -> std::result::Result<(), CredentialRotationError> {
            self.rotations.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn explicit_refresh_after_wins_over_derived_expiry_lead() {
        let now = Utc::now();
        let explicit = now.to_owned() + ChronoDuration::minutes(30);
        let credential = ResolvedCredential {
            secret: "secret".into(),
            expires_at: Some((now + ChronoDuration::hours(2)).to_rfc3339()),
            refresh_after: Some(explicit.to_rfc3339()),
            rotated: false,
        };
        let schedule = schedule_from_credential(&credential, now).unwrap();
        assert_eq!(schedule.next_attempt_at, explicit);
    }

    #[test]
    fn expiry_without_refresh_hint_uses_core_default_lead() {
        let now = Utc::now();
        let expiry = now.to_owned() + ChronoDuration::minutes(20);
        let credential = ResolvedCredential {
            secret: "secret".into(),
            expires_at: Some(expiry.to_rfc3339()),
            refresh_after: None,
            rotated: false,
        };
        let schedule = schedule_from_credential(&credential, now).unwrap();
        assert_eq!(
            schedule.next_attempt_at,
            expiry - ChronoDuration::seconds(DEFAULT_REFRESH_LEAD_SECS)
        );
    }

    #[test]
    fn short_expiry_uses_proportional_refresh_lead() {
        let now = Utc::now();
        let expiry = now.to_owned() + ChronoDuration::minutes(4);
        let credential = ResolvedCredential {
            secret: "secret".into(),
            expires_at: Some(expiry.to_rfc3339()),
            refresh_after: None,
            rotated: false,
        };

        let schedule = schedule_from_credential(&credential, now).unwrap();

        assert!(schedule.next_attempt_at > now);
        assert_eq!(
            schedule.next_attempt_at,
            expiry - ChronoDuration::minutes(2)
        );
    }

    #[test]
    fn static_or_unscheduled_credentials_are_not_tracked() {
        let coordinator = RefreshCoordinator::default();
        coordinator.observe(
            "p",
            "a",
            &ResolvedCredential {
                secret: "secret".into(),
                expires_at: None,
                refresh_after: None,
                rotated: false,
            },
        );
        assert!(coordinator.next_attempt_at("p", "a").is_none());
    }

    #[test]
    fn forgetting_provider_releases_all_refresh_maps() {
        let coordinator = RefreshCoordinator::default();
        let credential = ResolvedCredential {
            secret: "secret".into(),
            expires_at: Some((Utc::now() + ChronoDuration::hours(1)).to_rfc3339()),
            refresh_after: Some((Utc::now() + ChronoDuration::minutes(10)).to_rfc3339()),
            rotated: false,
        };
        coordinator.observe("p1", "a1", &credential);
        coordinator.observe("p2", "a2", &credential);
        coordinator.gate(&CredentialKey::new("p1", "a1"));
        coordinator.gate(&CredentialKey::new("p2", "a2"));

        coordinator.forget_provider("p1");

        assert!(coordinator.next_attempt_at("p1", "a1").is_none());
        assert!(coordinator.next_attempt_at("p2", "a2").is_some());
        assert_eq!(coordinator.schedules.len(), 1);
        assert_eq!(coordinator.gates.len(), 1);
        assert!(coordinator
            .gates
            .contains_key(&CredentialKey::new("p2", "a2")));
    }

    #[test]
    fn transient_failure_backoff_survives_normal_resolve_observation() {
        let coordinator = RefreshCoordinator::default();
        let credential = ResolvedCredential {
            secret: "secret".into(),
            expires_at: Some((Utc::now() + ChronoDuration::hours(1)).to_rfc3339()),
            refresh_after: Some((Utc::now() - ChronoDuration::seconds(1)).to_rfc3339()),
            rotated: false,
        };
        coordinator.observe("p", "a", &credential);
        let key = CredentialKey::new("p", "a");
        coordinator.record_failure(
            &key,
            &CredentialRotationError::new("upstream_unavailable", "temporary", true, None),
        );
        let retry_at = coordinator.next_attempt_at("p", "a").unwrap();

        coordinator.observe("p", "a", &credential);

        assert_eq!(
            coordinator.next_attempt_at("p", "a").unwrap(),
            retry_at,
            "ordinary credential resolution must not erase refresh backoff"
        );
    }

    #[tokio::test]
    async fn expired_backoff_claim_still_runs_the_due_refresh() {
        let coordinator = RefreshCoordinator::default();
        let strategy = Arc::new(RotatingStrategy::new());
        let account = account();
        let initial = strategy.resolve(&account).await.unwrap();
        coordinator.observe("p1", &account.id, &initial);

        let key = CredentialKey::new("p1", &account.id);
        coordinator.record_failure(
            &key,
            &CredentialRotationError::new("upstream_unavailable", "temporary", true, Some(5)),
        );
        coordinator.schedules.get_mut(&key).unwrap().next_attempt_at =
            Utc::now() - ChronoDuration::seconds(1);

        assert_eq!(coordinator.claim_due(Utc::now()).len(), 1);
        assert!(coordinator
            .rotate_scheduled("p1", strategy.clone(), &account)
            .await
            .unwrap());
        assert_eq!(strategy.rotations.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn scheduled_refresh_rotates_due_lease_and_reschedules() {
        let coordinator = RefreshCoordinator::default();
        let strategy = Arc::new(RotatingStrategy::new());
        let account = account();
        let initial = strategy.resolve(&account).await.unwrap();
        coordinator.observe("p1", &account.id, &initial);

        assert_eq!(coordinator.claim_due(Utc::now()).len(), 1);
        assert!(coordinator
            .rotate_scheduled("p1", strategy.clone(), &account)
            .await
            .unwrap());
        assert_eq!(strategy.rotations.load(Ordering::Relaxed), 1);
        assert!(
            coordinator.next_attempt_at("p1", &account.id).unwrap()
                > Utc::now() + ChronoDuration::minutes(30)
        );
    }

    #[tokio::test]
    async fn unchanged_short_lease_rotates_at_its_original_deadline() {
        let coordinator = RefreshCoordinator::default();
        let account = account();
        let observed_at = Utc::now();
        let strategy = Arc::new(FixedShortLeaseStrategy {
            expires_at: (observed_at.to_owned() + ChronoDuration::minutes(4)).to_rfc3339(),
            refresh_after: None,
            rotated_on_resolve: false,
            rotations: AtomicUsize::new(0),
        });
        let initial = strategy.resolve(&account).await.unwrap();
        coordinator.observe("p1", &account.id, &initial);
        let original_deadline = coordinator.next_attempt_at("p1", &account.id).unwrap();

        assert_eq!(
            coordinator.claim_due(original_deadline).len(),
            1,
            "the unchanged four-minute lease should become due at its first scheduled deadline"
        );
        assert!(coordinator
            .rotate_scheduled_at("p1", strategy.clone(), &account, original_deadline)
            .await
            .unwrap());
        assert_eq!(strategy.rotations.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn rotated_lease_returned_by_resolve_gets_a_safe_retry_delay() {
        let coordinator = RefreshCoordinator::default();
        let account = account();
        let now = Utc::now();
        let strategy = Arc::new(FixedShortLeaseStrategy {
            expires_at: (now.to_owned() + ChronoDuration::minutes(5)).to_rfc3339(),
            refresh_after: Some((now - ChronoDuration::seconds(1)).to_rfc3339()),
            rotated_on_resolve: true,
            rotations: AtomicUsize::new(0),
        });

        let resolved = coordinator.resolve("p1", strategy, &account).await.unwrap();

        assert!(resolved.rotated);
        let now = Utc::now();
        let next_attempt = coordinator.next_attempt_at("p1", &account.id).unwrap();
        assert!(next_attempt >= now + ChronoDuration::seconds(59));
        assert!(coordinator.claim_due(now).is_empty());
    }

    #[tokio::test]
    async fn successful_rotation_preserves_future_explicit_refresh_after() {
        let coordinator = RefreshCoordinator::default();
        let account = account();
        let now = Utc::now();
        let refresh_after = now + ChronoDuration::seconds(10);
        let strategy = Arc::new(FixedShortLeaseStrategy {
            expires_at: (now + ChronoDuration::hours(1)).to_rfc3339(),
            refresh_after: Some(refresh_after.to_rfc3339()),
            rotated_on_resolve: true,
            rotations: AtomicUsize::new(0),
        });

        coordinator.resolve("p1", strategy, &account).await.unwrap();

        assert_eq!(
            coordinator.next_attempt_at("p1", &account.id).unwrap(),
            refresh_after
        );
    }

    #[tokio::test]
    async fn successful_rotation_preserves_future_expiry_derived_deadline() {
        let coordinator = RefreshCoordinator::default();
        let account = account();
        let now = Utc::now();
        let expires_at = now + ChronoDuration::seconds(30);
        let strategy = Arc::new(FixedShortLeaseStrategy {
            expires_at: expires_at.to_rfc3339(),
            refresh_after: None,
            rotated_on_resolve: true,
            rotations: AtomicUsize::new(0),
        });

        coordinator.resolve("p1", strategy, &account).await.unwrap();

        let next_attempt = coordinator.next_attempt_at("p1", &account.id).unwrap();
        assert!(next_attempt > now);
        assert!(next_attempt <= now + ChronoDuration::seconds(20));
        assert!(next_attempt < expires_at);
    }

    #[tokio::test]
    async fn due_retry_floor_is_capped_at_known_expiry() {
        let coordinator = RefreshCoordinator::default();
        let account = account();
        let now = Utc::now();
        let expires_at = now + ChronoDuration::seconds(30);
        let strategy = Arc::new(FixedShortLeaseStrategy {
            expires_at: expires_at.to_rfc3339(),
            refresh_after: Some((now - ChronoDuration::seconds(1)).to_rfc3339()),
            rotated_on_resolve: true,
            rotations: AtomicUsize::new(0),
        });

        coordinator.resolve("p1", strategy, &account).await.unwrap();

        assert_eq!(
            coordinator.next_attempt_at("p1", &account.id).unwrap(),
            expires_at
        );
    }

    #[tokio::test]
    async fn unchanged_lease_retries_at_bounded_intervals_after_expiry() {
        let coordinator = RefreshCoordinator::default();
        let account = account();
        let observed_at = Utc::now();
        let expires_at = observed_at.to_owned() + ChronoDuration::minutes(4);
        let strategy = Arc::new(FixedShortLeaseStrategy {
            expires_at: expires_at.to_rfc3339(),
            refresh_after: None,
            rotated_on_resolve: false,
            rotations: AtomicUsize::new(0),
        });
        let initial = strategy.resolve(&account).await.unwrap();
        coordinator.observe("p1", &account.id, &initial);
        let mut due_at = coordinator.next_attempt_at("p1", &account.id).unwrap();

        for _ in 0..6 {
            assert_eq!(coordinator.claim_due(due_at).len(), 1);
            assert!(
                coordinator
                    .claim_due(due_at + ChronoDuration::seconds(1))
                    .is_empty(),
                "the active claim must prevent another scheduler tick from duplicating work"
            );
            assert!(coordinator
                .rotate_scheduled_at("p1", strategy.clone(), &account, due_at)
                .await
                .unwrap());

            let next_attempt = coordinator.next_attempt_at("p1", &account.id).unwrap();
            if due_at < expires_at {
                assert!(next_attempt >= (due_at + ChronoDuration::seconds(60)).min(expires_at));
                assert!(next_attempt <= expires_at);
            } else {
                assert!(next_attempt >= due_at + ChronoDuration::seconds(60));
                assert!(next_attempt > expires_at);
            }
            assert!(
                coordinator
                    .claim_due(next_attempt - ChronoDuration::seconds(1))
                    .is_empty(),
                "an unchanged expired lease must not be reclaimed on the next scheduler tick"
            );
            due_at = next_attempt;
        }

        assert_eq!(strategy.rotations.load(Ordering::Relaxed), 6);
        assert!(due_at > expires_at + ChronoDuration::minutes(3));
    }

    #[tokio::test]
    async fn successful_short_lease_rotation_is_not_immediately_due_again() {
        let coordinator = RefreshCoordinator::default();
        let strategy = Arc::new(RotatingStrategy::with_short_lease_after_rotation());
        let account = account();
        let initial = strategy.resolve(&account).await.unwrap();
        coordinator.observe("p1", &account.id, &initial);

        assert_eq!(coordinator.claim_due(Utc::now()).len(), 1);
        assert!(coordinator
            .rotate_scheduled("p1", strategy.clone(), &account)
            .await
            .unwrap());

        let now = Utc::now();
        let next_attempt = coordinator.next_attempt_at("p1", &account.id).unwrap();
        assert!(
            next_attempt > now + ChronoDuration::minutes(1),
            "three-minute refreshed lease should retain a proportional lead, not be due now"
        );
        assert!(coordinator.claim_due(now).is_empty());
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(exponential_backoff_secs(1), 5);
        assert_eq!(exponential_backoff_secs(2), 10);
        assert_eq!(exponential_backoff_secs(3), 20);
        assert_eq!(exponential_backoff_secs(50), MAX_RETRY_SECS);
    }
}
