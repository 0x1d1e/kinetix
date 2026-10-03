//! Key-pool health state and account/soft-quota bookkeeping (FR-4, FR-12).
//!
//! Selection is health-aware: disabled/cooldown/exhausted accounts are skipped,
//! and selection order depends on the route strategy. State changes are
//! persisted so they survive restarts (open issue: rate-limit state location).

use chrono::{DateTime, Duration, Utc};

use crate::db::{self, AccountRow, Pool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    Healthy,
    Cooldown,
    Exhausted,
    Disabled,
    CircuitOpen,
}

impl AccountStatus {
    pub fn parse(s: &str) -> Self {
        match s {
            "cooldown" => AccountStatus::Cooldown,
            "exhausted" => AccountStatus::Exhausted,
            "disabled" => AccountStatus::Disabled,
            "circuit_open" => AccountStatus::CircuitOpen,
            _ => AccountStatus::Healthy,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AccountStatus::Healthy => "healthy",
            AccountStatus::Cooldown => "cooldown",
            AccountStatus::Exhausted => "exhausted",
            AccountStatus::Disabled => "disabled",
            AccountStatus::CircuitOpen => "circuit_open",
        }
    }

    pub fn as_admin_str(&self) -> &'static str {
        if *self == AccountStatus::CircuitOpen {
            "degraded"
        } else {
            self.as_str()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountLifecycle {
    pub status: AccountStatus,
    pub reason_code: String,
    pub retry_at: Option<String>,
    pub status_changed_at: Option<String>,
}

/// Effective status accounting for elapsed cooldown/quota-reset/circuit windows.
pub fn effective_status(account: &AccountRow) -> AccountStatus {
    effective_status_at(account, Utc::now())
}

pub fn effective_status_at(account: &AccountRow, now: DateTime<Utc>) -> AccountStatus {
    let configured = AccountStatus::parse(&account.status);

    // Administrative/quota state always wins over circuit state. Half-open
    // probing must never make disabled, cooling-down, or exhausted credentials eligible.
    match configured {
        AccountStatus::Disabled => return AccountStatus::Disabled,
        AccountStatus::Cooldown => match account.cooldown_until.as_deref().and_then(db::parse_dt) {
            Some(until) if until <= now => {}
            _ => return AccountStatus::Cooldown,
        },
        AccountStatus::Exhausted => {
            match account.quota_reset_at.as_deref().and_then(db::parse_dt) {
                Some(reset) if reset <= now => {}
                _ => return AccountStatus::Exhausted,
            }
        }
        AccountStatus::Healthy | AccountStatus::CircuitOpen => {}
    }

    // An expired circuit remains logically degraded until a half-open probe succeeds.
    if account.circuit_open_until.is_some() || configured == AccountStatus::CircuitOpen {
        AccountStatus::CircuitOpen
    } else {
        AccountStatus::Healthy
    }
}

pub fn lifecycle_at(account: &AccountRow, now: DateTime<Utc>) -> AccountLifecycle {
    let configured = AccountStatus::parse(&account.status);
    let status = effective_status_at(account, now);
    let stored_reason = account.status_reason.as_str();
    let (reason_code, retry_at) = match status {
        AccountStatus::Disabled => (stored_reason.to_string(), None),
        AccountStatus::Cooldown => {
            let until = account.cooldown_until.as_deref().and_then(db::parse_dt);
            match until {
                Some(until) if until > now => (stored_reason.to_string(), Some(until.to_rfc3339())),
                _ => ("cooldown_elapsed".into(), None),
            }
        }
        AccountStatus::Exhausted => {
            let reset = account.quota_reset_at.as_deref().and_then(db::parse_dt);
            match reset {
                Some(reset) if reset > now => (stored_reason.to_string(), Some(reset.to_rfc3339())),
                _ => ("quota_reset".into(), None),
            }
        }
        AccountStatus::CircuitOpen => {
            let until = account.circuit_open_until.as_deref().and_then(db::parse_dt);
            (
                "circuit_open".into(),
                until
                    .filter(|until| *until > now)
                    .map(|until| until.to_rfc3339()),
            )
        }
        AccountStatus::Healthy => match configured {
            AccountStatus::Cooldown => ("cooldown_elapsed".into(), None),
            AccountStatus::Exhausted => ("quota_reset".into(), None),
            _ => (stored_reason.to_string(), None),
        },
    };
    AccountLifecycle {
        status,
        reason_code: if reason_code.is_empty() || reason_code == "unknown" {
            status.as_admin_str().to_string()
        } else {
            reason_code
        },
        retry_at,
        status_changed_at: account.status_changed_at.clone(),
    }
}

/// Bump the failure counter and open the circuit breaker when the threshold is
/// reached (FR-4.7). Returns the new consecutive-failure count.
pub async fn record_failure(
    pool: &Pool,
    account_id: &str,
    threshold: i64,
    open_secs: i64,
) -> anyhow::Result<i64> {
    db::record_account_failure(pool, account_id, threshold, open_secs).await
}

/// Clear the circuit breaker after a successful attempt or manual reset.
pub async fn clear_circuit(pool: &Pool, account_id: &str) -> anyhow::Result<()> {
    db::reset_account_failures(pool, account_id).await
}

/// Default interval between half-open recovery probes (FR-4.7, SHOULD).
pub const HALF_OPEN_PROBE_SECS: i64 = 5;

/// Minimum spacing between successive half-open probes for one account.
pub const HALF_OPEN_PROBE_MIN_GAP_SECS: i64 = 2;

/// Bounded half-open recovery probing (FR-4.7): once an account's circuit-open
/// window has elapsed it may be tried again, but at most one probe per
/// [`HALF_OPEN_PROBE_MIN_GAP_SECS`] so a still-broken upstream is not hammered
/// by every concurrent request. Returns `false` while the circuit is still open.
pub fn should_probe(account: &AccountRow) -> bool {
    should_probe_at(account, Utc::now())
}

pub fn should_probe_at(account: &AccountRow, now: DateTime<Utc>) -> bool {
    // Disabled accounts are never probed. Cooldown and quota states block
    // probes only while their own recovery window is active.
    match AccountStatus::parse(&account.status) {
        AccountStatus::Disabled => return false,
        AccountStatus::Cooldown => {
            if !matches!(
                account.cooldown_until.as_deref().and_then(db::parse_dt),
                Some(until) if until <= now
            ) {
                return false;
            }
        }
        AccountStatus::Exhausted => {
            if !matches!(
                account.quota_reset_at.as_deref().and_then(db::parse_dt),
                Some(reset) if reset <= now
            ) {
                return false;
            }
        }
        AccountStatus::Healthy | AccountStatus::CircuitOpen => {}
    }

    let Some(until) = account.circuit_open_until.as_deref().and_then(db::parse_dt) else {
        return false;
    };
    if now < until {
        return false;
    }
    match account.last_probe_at.as_deref().and_then(db::parse_dt) {
        Some(last) => now >= last + Duration::seconds(HALF_OPEN_PROBE_MIN_GAP_SECS),
        None => true,
    }
}

pub fn is_available(account: &AccountRow) -> bool {
    effective_status(account) == AccountStatus::Healthy
}

/// How sibling accounts are ordered. Live requests draw weighted picks at
/// random; dry runs derive them from a stable seed so a simulation is
/// reproducible and never consumes live randomness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountOrdering {
    Live,
    Simulated(u64),
}

impl AccountOrdering {
    /// A seed for one independent sub-decision identified by `key`.
    pub(crate) fn derive(self, key: &[u8]) -> Self {
        match self {
            Self::Live => Self::Live,
            Self::Simulated(seed) => Self::Simulated(stable_hash(seed, key)),
        }
    }

    pub(crate) fn seed(self) -> Option<u64> {
        match self {
            Self::Live => None,
            Self::Simulated(seed) => Some(seed),
        }
    }
}

/// Order accounts by priority tier. Within a tier, weight biases which account
/// leads while every sibling is retained for fallback.
pub(crate) fn order_accounts(
    mut accounts: Vec<AccountRow>,
    ordering: AccountOrdering,
) -> Vec<AccountRow> {
    use rand::Rng;

    accounts.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut rng = rand::thread_rng();
    let mut start = 0;
    while start < accounts.len() {
        let priority = accounts[start].priority;
        let mut end = start + 1;
        while end < accounts.len() && accounts[end].priority == priority {
            end += 1;
        }
        for index in start..end {
            let total: u64 = accounts[index..end]
                .iter()
                .map(|account| crate::pre_dispatch::normalized_weight(account.weight))
                .sum();
            let pick = match ordering {
                AccountOrdering::Live => rng.gen_range(0..total),
                AccountOrdering::Simulated(seed) => {
                    let identity = accounts[index..end]
                        .iter()
                        .map(|account| account.id.as_str())
                        .collect::<Vec<_>>()
                        .join("|");
                    stable_hash(seed ^ index as u64, identity.as_bytes()) % total
                }
            };
            let mut cumulative = 0;
            let mut selected = index;
            for (offset, account) in accounts[index..end].iter().enumerate() {
                cumulative += crate::pre_dispatch::normalized_weight(account.weight);
                if pick < cumulative {
                    selected = index + offset;
                    break;
                }
            }
            accounts.swap(index, selected);
        }
        start = end;
    }
    accounts
}

/// FNV-1a over `bytes`, seeded. Stable across processes and releases so dry
/// runs reproduce the same ordering.
pub(crate) fn stable_hash(seed: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0xcbf29ce484222325_u64 ^ seed, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

/// Mark an account rate-limited for `cooldown` seconds.
pub async fn mark_rate_limited(
    pool: &Pool,
    account_id: &str,
    cooldown_secs: u64,
    error: &str,
) -> anyhow::Result<DateTime<Utc>> {
    let until = Utc::now() + Duration::seconds(cooldown_secs as i64);
    db::set_account_status(
        pool,
        account_id,
        "cooldown",
        "rate_limited",
        Some(&until.to_rfc3339()),
        None,
        Some(&crate::crypto::redact(error)),
    )
    .await?;
    Ok(until)
}

/// Mark an account quota-exhausted until `reset_at` (or a default window).
pub async fn mark_exhausted(
    pool: &Pool,
    account_id: &str,
    reset_at: Option<DateTime<Utc>>,
    default_window_secs: i64,
    error: &str,
) -> anyhow::Result<DateTime<Utc>> {
    let reset = reset_at.unwrap_or_else(|| Utc::now() + Duration::seconds(default_window_secs));
    db::set_account_status(
        pool,
        account_id,
        "exhausted",
        "account_quota_exhausted",
        None,
        Some(&reset.to_rfc3339()),
        Some(&crate::crypto::redact(error)),
    )
    .await?;
    Ok(reset)
}

pub async fn mark_healthy(pool: &Pool, account_id: &str) -> anyhow::Result<()> {
    db::set_account_status(
        pool,
        account_id,
        "healthy",
        "operator_reset",
        None,
        None,
        None,
    )
    .await
}

pub async fn recover_after_success(
    pool: &Pool,
    account_id: &str,
    observed_state_version: i64,
    is_half_open_probe: bool,
) -> anyhow::Result<bool> {
    db::recover_account_after_success(pool, account_id, observed_state_version, is_half_open_probe)
        .await
}

/// Clear a cooldown and put the account back in service.
pub async fn clear_cooldown(pool: &Pool, account_id: &str) -> anyhow::Result<()> {
    db::set_account_status(
        pool,
        account_id,
        "healthy",
        "cooldown_cleared",
        None,
        None,
        None,
    )
    .await
}

/// The soonest future recovery time across a set of accounts.
pub fn soonest_recovery(accounts: &[AccountRow]) -> Option<DateTime<Utc>> {
    let now = Utc::now();
    accounts
        .iter()
        .filter_map(|account| {
            lifecycle_at(account, now)
                .retry_at
                .as_deref()
                .and_then(db::parse_dt)
        })
        .min()
}

/// Whether a soft quota (FR-12.5) is reached for an account.
pub async fn soft_quota_reached(pool: &Pool, account: &AccountRow) -> anyhow::Result<bool> {
    let Some(limit) = account.soft_quota_usd else {
        return Ok(false);
    };
    if limit <= 0.0 {
        return Ok(false);
    }
    let since = window_start(&account.quota_type, account.quota_window_s);
    let spent = db::account_spend_since(pool, &account.id, &since).await?;
    Ok(spent >= limit)
}

/// Compute the start of a quota window as an ISO timestamp.
pub fn window_start(quota_type: &str, window_secs: Option<i64>) -> String {
    let now = Utc::now();
    let start = match quota_type {
        "daily" => now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|d| d.and_utc())
            .unwrap_or(now),
        "monthly" => {
            let first = now.date_naive().with_day(1).unwrap_or(now.date_naive());
            first
                .and_hms_opt(0, 0, 0)
                .map(|d| d.and_utc())
                .unwrap_or(now)
        }
        "rolling" => now - Duration::seconds(window_secs.unwrap_or(86400)),
        _ => now - Duration::days(1),
    };
    start.to_rfc3339()
}

use chrono::Datelike;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn account(
        status: &str,
        circuit_until: Option<String>,
        last_probe: Option<String>,
    ) -> AccountRow {
        AccountRow {
            id: "acc".into(),
            provider_id: "p".into(),
            label: "l".into(),
            secret_enc: String::new(),
            key_mask: String::new(),
            status: status.into(),
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
            last_probe_at: last_probe,
            circuit_open_until: circuit_until,
            consecutive_failures: 0,
            created_at: Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn circuit_open_takes_precedence_and_expires() {
        let future = (Utc::now() + Duration::seconds(60)).to_rfc3339();
        let a = account("healthy", Some(future), None);
        assert_eq!(effective_status(&a), AccountStatus::CircuitOpen);
        assert!(!should_probe(&a), "open circuit must not be probed");

        // Once the window has elapsed the circuit is half-open and probeable.
        let past = (Utc::now() - Duration::seconds(1)).to_rfc3339();
        let b = account("healthy", Some(past), None);
        assert_eq!(effective_status(&b), AccountStatus::CircuitOpen);
        assert!(should_probe(&b), "half-open circuit should be probed");
    }

    #[test]
    fn probe_is_throttled_by_min_gap() {
        let past = (Utc::now() - Duration::seconds(30)).to_rfc3339();
        let just_probed = Utc::now().to_rfc3339();
        let a = account("healthy", Some(past.clone()), Some(just_probed));
        assert!(
            !should_probe(&a),
            "a recent probe must throttle the next one"
        );

        let old = (Utc::now() - Duration::seconds(HALF_OPEN_PROBE_MIN_GAP_SECS + 1)).to_rfc3339();
        let b = account("healthy", Some(past), Some(old));
        assert!(should_probe(&b));
    }

    #[test]
    fn no_circuit_history_is_healthy_but_not_a_probe() {
        let a = account("healthy", None, None);
        assert_eq!(effective_status(&a), AccountStatus::Healthy);
        assert!(!should_probe(&a));
    }

    #[test]
    fn non_circuit_states_are_not_half_open_probes() {
        for status in ["cooldown", "exhausted", "disabled"] {
            let a = account(status, None, None);
            assert_eq!(effective_status(&a), AccountStatus::parse(status));
            assert!(!should_probe(&a), "{status} must not be probeable");
        }
    }

    #[test]
    fn disabled_state_beats_an_expired_circuit() {
        let past = (Utc::now() - Duration::seconds(30)).to_rfc3339();
        let a = account("disabled", Some(past), None);
        assert_eq!(effective_status(&a), AccountStatus::Disabled);
        assert!(!should_probe(&a));
    }

    #[test]
    fn expired_cooldown_recovers_without_half_open_probe() {
        let mut a = account("cooldown", None, None);
        a.status_reason = "rate_limited".into();
        a.cooldown_until = Some((Utc::now() - Duration::seconds(1)).to_rfc3339());
        assert_eq!(effective_status(&a), AccountStatus::Healthy);
        assert_eq!(lifecycle_at(&a, Utc::now()).reason_code, "cooldown_elapsed");
        assert!(!should_probe(&a));
    }

    #[test]
    fn expired_quota_reports_reset_reason() {
        let mut a = account("exhausted", None, None);
        a.status_reason = "account_quota_exhausted".into();
        a.quota_reset_at = Some((Utc::now() - Duration::seconds(1)).to_rfc3339());
        let lifecycle = lifecycle_at(&a, Utc::now());
        assert_eq!(lifecycle.status, AccountStatus::Healthy);
        assert_eq!(lifecycle.reason_code, "quota_reset");
        assert_eq!(lifecycle.retry_at, None);
    }

    #[test]
    fn account_weight_biases_first_choice_within_priority_tier() {
        let mut light = account("healthy", None, None);
        light.id = "light".into();
        light.weight = 1;

        let mut heavy = account("healthy", None, None);
        heavy.id = "heavy".into();
        heavy.weight = 9;

        let mut heavy_first = 0usize;
        for _ in 0..2000 {
            let ordered = order_accounts(vec![light.clone(), heavy.clone()], AccountOrdering::Live);
            if ordered[0].id == "heavy" {
                heavy_first += 1;
            }
        }

        assert!(
            heavy_first > 1500,
            "weight 9 account should lead most selections, got {heavy_first}/2000"
        );
    }

    #[test]
    fn simulated_ordering_is_reproducible_and_keeps_priority_tiers() {
        let accounts: Vec<_> = [("a", 2), ("b", 1), ("c", 1), ("d", 1)]
            .into_iter()
            .map(|(id, priority)| {
                let mut row = account("healthy", None, None);
                row.id = id.into();
                row.priority = priority;
                row
            })
            .collect();
        let ids = |ordering| -> Vec<String> {
            order_accounts(accounts.clone(), ordering)
                .into_iter()
                .map(|account| account.id)
                .collect()
        };
        let first = ids(AccountOrdering::Simulated(7));
        let mut reversed = accounts.clone();
        reversed.reverse();
        let from_reversed: Vec<_> = order_accounts(reversed, AccountOrdering::Simulated(7))
            .into_iter()
            .map(|account| account.id)
            .collect();
        assert_eq!(first, from_reversed);
        assert_eq!(first.last().map(String::as_str), Some("a"));
        let seeds_differ = (0..32).any(|seed| ids(AccountOrdering::Simulated(seed)) != first);
        assert!(seeds_differ, "the seed must influence the in-tier order");
    }
}
