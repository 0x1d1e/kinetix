//! Provider-level circuit breaker for correlated transient upstream failures.
//!
//! This is deliberately separate from account health and target congestion.
//! Only failures that are plausibly provider-wide participate, and closed-state
//! opening requires evidence from distinct accounts.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::Serialize;

use crate::types::FailureKind;

const CORRELATION_WINDOW_SECS: i64 = 30;
const OPEN_COOLDOWN_SECS: i64 = 30;
const ABANDONED_PROBE_RETRY_SECS: i64 = 1;
const MIN_DISTINCT_ACCOUNTS: usize = 2;
const MIN_DISTINCT_TARGETS: usize = 2;
const MAX_RECENT_FAILURES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCircuitState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderFailureSnapshot {
    pub at: DateTime<Utc>,
    pub account_id: String,
    pub target_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCircuitSnapshot {
    pub provider_id: String,
    pub state: ProviderCircuitState,
    pub recent_qualifying_failures: usize,
    pub distinct_failing_accounts: usize,
    pub distinct_failing_targets: usize,
    pub recent_failures: Vec<ProviderFailureSnapshot>,
    pub opened_at: Option<DateTime<Utc>>,
    pub retry_at: Option<DateTime<Utc>>,
    pub last_successful_probe: Option<DateTime<Utc>>,
    pub opens: u64,
    pub recoveries: u64,
    pub rejects: u64,
    pub half_open_probes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCorrelationPolicy {
    /// Credential-backed providers require failures from multiple accounts.
    DistinctAccounts,
    /// Account-less providers have no credential diversity; failures across
    /// logical targets can provide the correlation signal instead.
    AccountlessTargets,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderCircuitTransition {
    pub opened: bool,
    pub recovered: bool,
    pub half_open_probe: bool,
}

impl ProviderCircuitTransition {
    pub fn merge(self, later: Self) -> Self {
        Self {
            opened: self.opened || later.opened,
            recovered: self.recovered || later.recovered,
            half_open_probe: self.half_open_probe || later.half_open_probe,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderCircuitReject {
    pub retry_after_secs: u64,
    pub snapshot: ProviderCircuitSnapshot,
}

#[derive(Clone, Default)]
pub struct ProviderCircuits {
    inner: Arc<DashMap<String, Arc<Mutex<Circuit>>>>,
}

#[derive(Debug, Clone)]
struct FailureEvidence {
    at: DateTime<Utc>,
    account_id: String,
    target_id: String,
}

#[derive(Debug)]
struct Circuit {
    state: ProviderCircuitState,
    failures: VecDeque<FailureEvidence>,
    opened_at: Option<DateTime<Utc>>,
    retry_at: Option<DateTime<Utc>>,
    last_successful_probe: Option<DateTime<Utc>>,
    half_open_inflight: u32,
    generation: u64,
    opens: u64,
    recoveries: u64,
    rejects: u64,
    half_open_probes: u64,
}

impl Default for Circuit {
    fn default() -> Self {
        Self {
            state: ProviderCircuitState::Closed,
            failures: VecDeque::new(),
            opened_at: None,
            retry_at: None,
            last_successful_probe: None,
            half_open_inflight: 0,
            generation: 0,
            opens: 0,
            recoveries: 0,
            rejects: 0,
            half_open_probes: 0,
        }
    }
}

impl Circuit {
    fn purge_old(&mut self, now: DateTime<Utc>) {
        let cutoff = now - ChronoDuration::seconds(CORRELATION_WINDOW_SECS);
        while self
            .failures
            .front()
            .is_some_and(|failure| failure.at < cutoff)
        {
            self.failures.pop_front();
        }
    }

    fn snapshot(&mut self, provider_id: &str, now: DateTime<Utc>) -> ProviderCircuitSnapshot {
        self.purge_old(now);
        let distinct_failing_accounts = self
            .failures
            .iter()
            .map(|failure| failure.account_id.as_str())
            .collect::<HashSet<_>>()
            .len();
        let distinct_failing_targets = self
            .failures
            .iter()
            .map(|failure| failure.target_id.as_str())
            .collect::<HashSet<_>>()
            .len();
        let recent_failures = self
            .failures
            .iter()
            .map(|failure| ProviderFailureSnapshot {
                at: failure.at,
                account_id: failure.account_id.clone(),
                target_id: failure.target_id.clone(),
            })
            .collect();
        ProviderCircuitSnapshot {
            provider_id: provider_id.to_string(),
            state: self.state,
            recent_qualifying_failures: self.failures.len(),
            distinct_failing_accounts,
            distinct_failing_targets,
            recent_failures,
            opened_at: self.opened_at,
            retry_at: self.retry_at,
            last_successful_probe: self.last_successful_probe,
            opens: self.opens,
            recoveries: self.recoveries,
            rejects: self.rejects,
            half_open_probes: self.half_open_probes,
        }
    }

    fn open(&mut self, now: DateTime<Utc>) -> bool {
        let changed = self.state != ProviderCircuitState::Open;
        self.state = ProviderCircuitState::Open;
        self.opened_at = Some(now);
        self.retry_at = Some(now + ChronoDuration::seconds(OPEN_COOLDOWN_SECS));
        self.half_open_inflight = 0;
        if changed {
            self.opens = self.opens.saturating_add(1);
        }
        changed
    }

    fn release_probe_slot(&mut self, generation: u64) -> bool {
        if generation != self.generation || self.state != ProviderCircuitState::HalfOpen {
            return false;
        }
        self.half_open_inflight = 0;
        true
    }

    fn reopen_abandoned_probe(&mut self, generation: u64, now: DateTime<Utc>) {
        if self.release_probe_slot(generation) {
            self.state = ProviderCircuitState::Open;
            self.retry_at = Some(now + ChronoDuration::seconds(ABANDONED_PROBE_RETRY_SECS));
        }
    }
}

impl ProviderCircuits {
    fn circuit(&self, provider_id: &str) -> Arc<Mutex<Circuit>> {
        self.inner
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Circuit::default())))
            .clone()
    }

    pub fn begin_attempt(
        &self,
        provider_id: &str,
        account_id: &str,
        target_id: &str,
    ) -> Result<ProviderAttempt, ProviderCircuitReject> {
        self.begin_attempt_with_policy(
            provider_id,
            account_id,
            target_id,
            ProviderCorrelationPolicy::DistinctAccounts,
        )
    }

    pub fn begin_attempt_with_policy(
        &self,
        provider_id: &str,
        account_id: &str,
        target_id: &str,
        correlation_policy: ProviderCorrelationPolicy,
    ) -> Result<ProviderAttempt, ProviderCircuitReject> {
        let circuit = self.circuit(provider_id);
        let now = Utc::now();
        let mut state = circuit.lock();
        state.purge_old(now);

        let half_open_probe = match state.state {
            ProviderCircuitState::Closed => false,
            ProviderCircuitState::Open => {
                if state.retry_at.is_some_and(|retry_at| retry_at > now) {
                    state.rejects = state.rejects.saturating_add(1);
                    let retry_after_secs = state
                        .retry_at
                        .map(|retry_at| (retry_at - now).num_seconds().max(1) as u64)
                        .unwrap_or(1);
                    let snapshot = state.snapshot(provider_id, now);
                    return Err(ProviderCircuitReject {
                        retry_after_secs,
                        snapshot,
                    });
                }
                state.state = ProviderCircuitState::HalfOpen;
                state.half_open_inflight = 1;
                state.half_open_probes = state.half_open_probes.saturating_add(1);
                true
            }
            ProviderCircuitState::HalfOpen => {
                if state.half_open_inflight > 0 {
                    state.rejects = state.rejects.saturating_add(1);
                    let snapshot = state.snapshot(provider_id, now);
                    return Err(ProviderCircuitReject {
                        retry_after_secs: 1,
                        snapshot,
                    });
                }
                state.half_open_inflight = 1;
                state.half_open_probes = state.half_open_probes.saturating_add(1);
                true
            }
        };
        let generation = state.generation;
        drop(state);

        Ok(ProviderAttempt {
            account_id: account_id.to_string(),
            target_id: target_id.to_string(),
            circuit,
            generation,
            correlation_policy,
            half_open_probe,
            validated_success: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        })
    }

    pub fn snapshot(&self, provider_id: &str) -> ProviderCircuitSnapshot {
        let circuit = self.circuit(provider_id);
        let mut state = circuit.lock();
        state.snapshot(provider_id, Utc::now())
    }

    pub fn snapshots(&self) -> Vec<ProviderCircuitSnapshot> {
        let now = Utc::now();
        self.inner
            .iter()
            .map(|entry| entry.value().lock().snapshot(entry.key(), now))
            .collect()
    }
}

pub struct ProviderAttempt {
    account_id: String,
    target_id: String,
    circuit: Arc<Mutex<Circuit>>,
    generation: u64,
    correlation_policy: ProviderCorrelationPolicy,
    half_open_probe: bool,
    validated_success: AtomicBool,
    finished: AtomicBool,
}

impl ProviderAttempt {
    pub fn is_half_open_probe(&self) -> bool {
        self.half_open_probe
    }

    /// Record that a half-open probe produced a validated response without
    /// consuming its lease; the stream may still fail before termination.
    pub fn mark_validated_success(&self) -> ProviderCircuitTransition {
        if !self.half_open_probe
            || self.finished.load(Ordering::Acquire)
            || self.validated_success.swap(true, Ordering::AcqRel)
        {
            return ProviderCircuitTransition::default();
        }

        let now = Utc::now();
        let mut state = self.circuit.lock();
        let recovered =
            state.generation == self.generation && state.state == ProviderCircuitState::HalfOpen;
        if recovered {
            state.half_open_inflight = 0;
            state.state = ProviderCircuitState::Closed;
            state.opened_at = None;
            state.retry_at = None;
            state.failures.clear();
            state.last_successful_probe = Some(now);
            state.recoveries = state.recoveries.saturating_add(1);
        }
        ProviderCircuitTransition {
            recovered,
            half_open_probe: true,
            ..Default::default()
        }
    }

    pub fn finish_success(&self) -> ProviderCircuitTransition {
        if self.finished.swap(true, Ordering::AcqRel) {
            return ProviderCircuitTransition::default();
        }
        if self.half_open_probe && self.validated_success.load(Ordering::Acquire) {
            return ProviderCircuitTransition {
                half_open_probe: true,
                ..Default::default()
            };
        }

        let now = Utc::now();
        let mut state = self.circuit.lock();
        let mut recovered = false;
        if state.generation == self.generation {
            if self.half_open_probe && state.state == ProviderCircuitState::HalfOpen {
                state.half_open_inflight = 0;
                state.state = ProviderCircuitState::Closed;
                state.opened_at = None;
                state.retry_at = None;
                state.failures.clear();
                state.last_successful_probe = Some(now);
                state.recoveries = state.recoveries.saturating_add(1);
                recovered = true;
            } else if !self.half_open_probe && state.state == ProviderCircuitState::Closed {
                // Deliberate conservative policy: a successful closed-state
                // attempt resets correlation evidence, requiring failures to
                // accumulate without an intervening success.
                state.failures.clear();
            }
        }
        ProviderCircuitTransition {
            recovered,
            half_open_probe: self.half_open_probe,
            ..Default::default()
        }
    }

    pub fn finish_failure(
        &self,
        kind: FailureKind,
        status: Option<u16>,
    ) -> ProviderCircuitTransition {
        if self.finished.swap(true, Ordering::AcqRel) {
            return ProviderCircuitTransition::default();
        }
        let now = Utc::now();
        let mut state = self.circuit.lock();
        let attempt_owns_state = state.generation == self.generation
            && if self.half_open_probe {
                state.state == ProviderCircuitState::HalfOpen
                    || (self.validated_success.load(Ordering::Acquire)
                        && state.state == ProviderCircuitState::Closed)
            } else {
                state.state == ProviderCircuitState::Closed
            };
        if !attempt_owns_state {
            return ProviderCircuitTransition {
                half_open_probe: self.half_open_probe,
                ..Default::default()
            };
        }

        if !qualifies(kind, status) {
            if self.half_open_probe {
                state.release_probe_slot(self.generation);
            }
            return ProviderCircuitTransition {
                half_open_probe: self.half_open_probe,
                ..Default::default()
            };
        }

        state.purge_old(now);
        state.failures.push_back(FailureEvidence {
            at: now,
            account_id: self.account_id.clone(),
            target_id: self.target_id.clone(),
        });
        while state.failures.len() > MAX_RECENT_FAILURES {
            state.failures.pop_front();
        }

        let distinct_accounts = state
            .failures
            .iter()
            .map(|failure| failure.account_id.as_str())
            .collect::<HashSet<_>>()
            .len();
        let distinct_targets = state
            .failures
            .iter()
            .map(|failure| failure.target_id.as_str())
            .collect::<HashSet<_>>()
            .len();

        let correlated = match self.correlation_policy {
            ProviderCorrelationPolicy::DistinctAccounts => {
                distinct_accounts >= MIN_DISTINCT_ACCOUNTS
            }
            ProviderCorrelationPolicy::AccountlessTargets => {
                distinct_targets >= MIN_DISTINCT_TARGETS
            }
        };
        let validated_success = self.validated_success.load(Ordering::Acquire);
        let unvalidated_probe_failure = self.half_open_probe && !validated_success;
        let opened = if unvalidated_probe_failure || correlated {
            let changed = state.open(now);
            if changed {
                state.generation = state.generation.wrapping_add(1);
            }
            changed
        } else {
            false
        };

        ProviderCircuitTransition {
            opened,
            half_open_probe: self.half_open_probe,
            ..Default::default()
        }
    }

    pub fn finish_neutral(&self) -> ProviderCircuitTransition {
        if self.finished.swap(true, Ordering::AcqRel) {
            return ProviderCircuitTransition::default();
        }
        if self.half_open_probe {
            self.circuit
                .lock()
                .reopen_abandoned_probe(self.generation, Utc::now());
        }
        ProviderCircuitTransition {
            half_open_probe: self.half_open_probe,
            ..Default::default()
        }
    }
}

impl Drop for ProviderAttempt {
    fn drop(&mut self) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.half_open_probe {
            self.circuit
                .lock()
                .reopen_abandoned_probe(self.generation, Utc::now());
        }
    }
}

pub fn qualifies(kind: FailureKind, status: Option<u16>) -> bool {
    match kind {
        FailureKind::ConnectionError | FailureKind::Timeout => true,
        FailureKind::ServerError => matches!(status, Some(502 | 503 | 504)),
        FailureKind::RateLimit
        | FailureKind::QuotaExhausted
        | FailureKind::AuthError
        | FailureKind::TargetError
        | FailureKind::BadRequest => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_account_cannot_open_provider_circuit() {
        let circuits = ProviderCircuits::default();
        for _ in 0..4 {
            let attempt = circuits.begin_attempt("p", "a", "m").unwrap();
            let transition = attempt.finish_failure(FailureKind::ServerError, Some(503));
            assert!(!transition.opened);
        }
        assert_eq!(circuits.snapshot("p").state, ProviderCircuitState::Closed);
    }

    #[test]
    fn correlated_distinct_accounts_open_provider_circuit() {
        let circuits = ProviderCircuits::default();
        circuits
            .begin_attempt("p", "a", "m")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        let transition = circuits
            .begin_attempt("p", "b", "m")
            .unwrap()
            .finish_failure(FailureKind::ConnectionError, None);

        assert!(transition.opened);
        let snapshot = circuits.snapshot("p");
        assert_eq!(snapshot.state, ProviderCircuitState::Open);
        assert_eq!(snapshot.distinct_failing_accounts, 2);
        assert!(circuits.begin_attempt("p", "c", "m").is_err());
    }

    #[test]
    fn same_account_failures_across_targets_do_not_open_credentialed_provider() {
        let circuits = ProviderCircuits::default();
        circuits
            .begin_attempt("p", "a", "route-a")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        let transition = circuits
            .begin_attempt("p", "a", "route-b")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));

        assert!(!transition.opened);
        let snapshot = circuits.snapshot("p");
        assert_eq!(snapshot.state, ProviderCircuitState::Closed);
        assert_eq!(snapshot.distinct_failing_accounts, 1);
        assert_eq!(snapshot.distinct_failing_targets, 2);
    }

    #[test]
    fn accountless_provider_can_correlate_failures_across_targets() {
        let circuits = ProviderCircuits::default();
        circuits
            .begin_attempt_with_policy(
                "p",
                "noauth",
                "route-a",
                ProviderCorrelationPolicy::AccountlessTargets,
            )
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        let transition = circuits
            .begin_attempt_with_policy(
                "p",
                "noauth",
                "route-b",
                ProviderCorrelationPolicy::AccountlessTargets,
            )
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));

        assert!(transition.opened);
        assert_eq!(circuits.snapshot("p").state, ProviderCircuitState::Open);
    }

    #[test]
    fn late_success_cannot_close_a_circuit_opened_after_admission() {
        let circuits = ProviderCircuits::default();
        let late_success = circuits.begin_attempt("p", "c", "route-c").unwrap();
        circuits
            .begin_attempt("p", "a", "route-a")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        circuits
            .begin_attempt("p", "b", "route-b")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));

        assert_eq!(circuits.snapshot("p").state, ProviderCircuitState::Open);
        assert!(!late_success.finish_success().recovered);
        let snapshot = circuits.snapshot("p");
        assert_eq!(snapshot.state, ProviderCircuitState::Open);
        assert_eq!(snapshot.recent_qualifying_failures, 2);
    }

    #[test]
    fn half_open_probe_recovers_at_validated_success_boundary() {
        let circuits = ProviderCircuits::default();
        circuits
            .begin_attempt("p", "a", "route-a")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        circuits
            .begin_attempt("p", "b", "route-b")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));

        {
            let circuit = circuits.circuit("p");
            let mut state = circuit.lock();
            state.retry_at = Some(Utc::now() - ChronoDuration::seconds(1));
        }

        let probe = circuits.begin_attempt("p", "a", "route-a").unwrap();
        assert!(probe.is_half_open_probe());
        assert!(circuits.begin_attempt("p", "b", "route-b").is_err());

        // The validated response recovers before stream completion without
        // consuming the attempt's lease.
        let transition = probe.mark_validated_success();
        assert!(transition.recovered);
        assert_eq!(circuits.snapshot("p").state, ProviderCircuitState::Closed);
        assert!(!probe.finish_success().recovered);
    }

    #[test]
    fn validated_probe_stream_failure_obeys_closed_correlation_threshold() {
        let circuits = ProviderCircuits::default();
        circuits
            .begin_attempt("p", "a", "route-a")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        circuits
            .begin_attempt("p", "b", "route-b")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));

        {
            let circuit = circuits.circuit("p");
            let mut state = circuit.lock();
            state.retry_at = Some(Utc::now() - ChronoDuration::seconds(1));
        }

        let probe = circuits.begin_attempt("p", "a", "route-a").unwrap();
        let validation = probe.mark_validated_success();
        assert!(validation.recovered);
        assert_eq!(circuits.snapshot("p").state, ProviderCircuitState::Closed);

        for account in ["c", "d", "e"] {
            circuits
                .begin_attempt("p", account, "route-normal")
                .unwrap()
                .finish_success();
        }

        let terminal = probe.finish_failure(FailureKind::Timeout, None);
        assert!(!terminal.opened);
        let snapshot = circuits.snapshot("p");
        assert_eq!(snapshot.state, ProviderCircuitState::Closed);
        assert_eq!(snapshot.recent_qualifying_failures, 1);
        assert_eq!(snapshot.recent_failures[0].account_id, "a");

        let second = circuits
            .begin_attempt("p", "b", "route-b")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        assert!(second.opened);
        assert_eq!(circuits.snapshot("p").state, ProviderCircuitState::Open);
        let combined = validation.merge(terminal);
        assert!(combined.recovered);
        assert!(!combined.opened);
    }

    #[test]
    fn closed_success_clears_correlation_evidence_by_policy() {
        let circuits = ProviderCircuits::default();
        circuits
            .begin_attempt("p", "a", "route-a")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        circuits
            .begin_attempt("p", "b", "route-b")
            .unwrap()
            .finish_success();
        circuits
            .begin_attempt("p", "c", "route-c")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));

        let snapshot = circuits.snapshot("p");
        assert_eq!(snapshot.state, ProviderCircuitState::Closed);
        assert_eq!(snapshot.recent_qualifying_failures, 1);
        assert_eq!(snapshot.distinct_failing_accounts, 1);
    }

    #[test]
    fn non_provider_half_open_failures_release_the_probe_slot_for_retry() {
        for (kind, status) in [
            (FailureKind::AuthError, Some(401)),
            (FailureKind::QuotaExhausted, Some(429)),
            (FailureKind::RateLimit, Some(429)),
            (FailureKind::BadRequest, Some(400)),
            (FailureKind::TargetError, None),
        ] {
            let circuits = ProviderCircuits::default();
            circuits
                .begin_attempt("p", "a", "route-a")
                .unwrap()
                .finish_failure(FailureKind::ServerError, Some(503));
            circuits
                .begin_attempt("p", "b", "route-b")
                .unwrap()
                .finish_failure(FailureKind::ServerError, Some(503));
            {
                let circuit = circuits.circuit("p");
                let mut state = circuit.lock();
                state.retry_at = Some(Utc::now() - ChronoDuration::seconds(1));
            }

            let probe = circuits.begin_attempt("p", "a", "route-a").unwrap();
            let transition = probe.finish_failure(kind, status);
            assert!(!transition.opened, "kind={kind:?}");
            assert_eq!(
                circuits.snapshot("p").state,
                ProviderCircuitState::HalfOpen,
                "kind={kind:?}"
            );

            let retry = circuits.begin_attempt("p", "b", "route-b").unwrap();
            assert!(retry.is_half_open_probe(), "kind={kind:?}");
            assert!(retry.finish_success().recovered, "kind={kind:?}");
            assert_eq!(circuits.snapshot("p").state, ProviderCircuitState::Closed);
        }
    }

    #[test]
    fn dropping_an_abandoned_half_open_probe_reopens_briefly() {
        let circuits = ProviderCircuits::default();
        circuits
            .begin_attempt("p", "a", "route-a")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        circuits
            .begin_attempt("p", "b", "route-b")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        {
            let circuit = circuits.circuit("p");
            let mut state = circuit.lock();
            state.retry_at = Some(Utc::now() - ChronoDuration::seconds(1));
        }

        let probe = circuits.begin_attempt("p", "a", "route-a").unwrap();
        let before_drop = Utc::now();
        drop(probe);
        let snapshot = circuits.snapshot("p");
        assert_eq!(snapshot.state, ProviderCircuitState::Open);
        assert!(snapshot
            .retry_at
            .is_some_and(|retry_at| retry_at > before_drop));
    }

    #[test]
    fn qualifying_half_open_failure_reopens_provider() {
        let circuits = ProviderCircuits::default();
        circuits
            .begin_attempt("p", "a", "route-a")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));
        circuits
            .begin_attempt("p", "b", "route-b")
            .unwrap()
            .finish_failure(FailureKind::ServerError, Some(503));

        {
            let circuit = circuits.circuit("p");
            let mut state = circuit.lock();
            state.retry_at = Some(Utc::now() - ChronoDuration::seconds(1));
        }

        let probe = circuits.begin_attempt("p", "a", "route-a").unwrap();
        let transition = probe.finish_failure(FailureKind::Timeout, None);
        assert!(transition.opened);
        assert_eq!(circuits.snapshot("p").state, ProviderCircuitState::Open);
    }

    #[test]
    fn arbitrary_500_and_account_failures_do_not_trip_provider() {
        let circuits = ProviderCircuits::default();
        for (account, kind, status) in [
            ("a", FailureKind::ServerError, Some(500)),
            ("b", FailureKind::AuthError, Some(401)),
            ("c", FailureKind::QuotaExhausted, Some(429)),
            ("d", FailureKind::BadRequest, Some(400)),
        ] {
            circuits
                .begin_attempt("p", account, "m")
                .unwrap()
                .finish_failure(kind, status);
        }
        assert_eq!(circuits.snapshot("p").state, ProviderCircuitState::Closed);
    }

    #[test]
    fn only_gateway_style_server_errors_qualify() {
        assert!(!qualifies(FailureKind::ServerError, Some(500)));
        assert!(qualifies(FailureKind::ServerError, Some(502)));
        assert!(qualifies(FailureKind::ServerError, Some(503)));
        assert!(qualifies(FailureKind::ServerError, Some(504)));
        assert!(qualifies(FailureKind::Timeout, None));
        assert!(qualifies(FailureKind::ConnectionError, None));
    }
}
