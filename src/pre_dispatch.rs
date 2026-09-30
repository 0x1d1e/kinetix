//! Canonical Route pre-dispatch planning shared by runtime and simulation.
//!
//! Callers resolve request facts and route ordering first. This module turns
//! that ordered candidate frontier plus availability facts into gate decisions,
//! a first-dispatch outcome, and candidate-level explanations.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreDispatchFacts {
    pub request_eligible: bool,
    pub account_eligible: bool,
    /// `None` means the planner has not reached the account-quota check yet.
    pub account_quota_reached: Option<bool>,
    pub quota_fallback_allowed: bool,
    /// `None` means the planner has not reached the provider-circuit check yet.
    pub provider_circuit_available: Option<bool>,
    pub route_capacity_available: bool,
    pub quota_override_available: bool,
    pub adaptive_capacity_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreDispatchDecision {
    RequestIneligible,
    AccountUnavailable,
    QuotaOverrideUnavailable,
    CheckAccountQuota,
    SkipAfterQuota,
    RateLimited,
    CheckProviderCircuit,
    ProviderCircuitUnavailable,
    RouteCapacityUnavailable,
    AdaptiveCapacityUnavailable,
    Dispatchable,
}

/// A candidate after request eligibility and route-strategy ordering.
#[derive(Debug, Clone)]
pub(crate) struct DispatchCandidate {
    pub id: String,
    /// `None` candidates remain in diagnostics but are outside the dispatch
    /// frontier, e.g. request-ineligible Route targets.
    pub rank: Option<usize>,
    /// Candidates sharing a logical Route target use one group. Direct model
    /// account pools use a single implicit group.
    pub group_id: Option<String>,
    pub account_priority: i64,
    pub weight: i64,
    /// Circuit-open, non-probeable accounts are deferred behind normal candidates.
    pub defer_for_circuit: bool,
    pub facts: PreDispatchFacts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchStrategy {
    Ordered,
    Weighted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchOutcome {
    Selected(String),
    RateLimited(String),
    Stochastic,
    NoEligibleTarget,
}

#[derive(Debug, Clone)]
pub(crate) struct DispatchCandidateRecord {
    pub candidate_id: String,
    pub strategy_rank: Option<usize>,
    pub decision: PreDispatchDecision,
    pub selected: bool,
    pub decision_reason: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct DispatchPlan {
    /// Candidate IDs in the exact first-dispatch order, after affinity promotion.
    pub ordered_candidate_ids: Vec<String>,
    /// Decision records remain in input order for stable serialization.
    pub candidates: Vec<DispatchCandidateRecord>,
    pub outcome: DispatchOutcome,
    pub stochastic_selection: bool,
}

/// Normalize configured weights with the same semantics used by runtime
/// selection. Non-positive weights still receive a non-zero share.
pub(crate) fn normalized_weight(weight: i64) -> u64 {
    weight.max(1) as u64
}

/// Apply the same ordered gates for an individual candidate in runtime and
/// dry-run. Quota is intentionally checked before provider-circuit availability.
pub(crate) fn plan_candidate(facts: PreDispatchFacts) -> PreDispatchDecision {
    if !facts.request_eligible {
        return PreDispatchDecision::RequestIneligible;
    }
    if !facts.account_eligible {
        return PreDispatchDecision::AccountUnavailable;
    }
    if !facts.quota_override_available {
        return PreDispatchDecision::QuotaOverrideUnavailable;
    }
    let Some(quota_reached) = facts.account_quota_reached else {
        return PreDispatchDecision::CheckAccountQuota;
    };
    if quota_reached {
        return if facts.quota_fallback_allowed {
            PreDispatchDecision::SkipAfterQuota
        } else {
            PreDispatchDecision::RateLimited
        };
    }
    let Some(provider_circuit_available) = facts.provider_circuit_available else {
        return PreDispatchDecision::CheckProviderCircuit;
    };
    if !provider_circuit_available {
        return PreDispatchDecision::ProviderCircuitUnavailable;
    }
    if !facts.route_capacity_available {
        return PreDispatchDecision::RouteCapacityUnavailable;
    }
    if !facts.adaptive_capacity_available {
        return PreDispatchDecision::AdaptiveCapacityUnavailable;
    }
    PreDispatchDecision::Dispatchable
}

/// Build the canonical first-dispatch plan from strategy-ranked candidates.
/// Both runtime and simulation use this for affinity promotion, ordered gates,
/// the reachable first-dispatch frontier, and candidate decision records. Runtime
/// supplies its realized strategy order; simulation supplies a deterministic
/// representative order and asks the planner to classify all reachable outcomes.
pub(crate) fn plan_dispatch(
    candidates: &[DispatchCandidate],
    route_present: bool,
    strategy: DispatchStrategy,
    affinity_candidate_id: Option<&str>,
    classify_stochasticity: bool,
) -> DispatchPlan {
    let decisions: Vec<_> = candidates
        .iter()
        .map(|candidate| plan_candidate(candidate.facts))
        .collect();
    let mut ordered_indices = ordered_indices(candidates);
    if let Some(affinity_id) = affinity_candidate_id {
        if let Some(index) = ordered_indices
            .iter()
            .position(|index| candidates[*index].id == affinity_id)
        {
            ordered_indices.rotate_left(index);
        }
    }
    let effective_ranks: HashMap<_, _> = ordered_indices
        .iter()
        .enumerate()
        .map(|(rank, index)| (*index, rank))
        .collect();
    let (stochastic_selection, stochastic_frontier) = if classify_stochasticity {
        stochastic_frontier(
            candidates,
            &decisions,
            &ordered_indices,
            route_present,
            strategy,
            affinity_candidate_id,
        )
    } else {
        (false, HashSet::new())
    };

    let mut selected_index = None;
    let mut quota_blocker_index = None;
    let outcome = if stochastic_selection {
        DispatchOutcome::Stochastic
    } else {
        let mut outcome = DispatchOutcome::NoEligibleTarget;
        for index in &ordered_indices {
            match decisions[*index] {
                PreDispatchDecision::Dispatchable => {
                    selected_index = Some(*index);
                    outcome = DispatchOutcome::Selected(candidates[*index].id.clone());
                    break;
                }
                PreDispatchDecision::RateLimited => {
                    quota_blocker_index = Some(*index);
                    outcome = DispatchOutcome::RateLimited(candidates[*index].id.clone());
                    break;
                }
                _ => {}
            }
        }
        outcome
    };

    let blocker_rank = quota_blocker_index.and_then(|index| effective_ranks.get(&index).copied());
    let records = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let selected = selected_index == Some(index);
            let blocks_on_quota = quota_blocker_index == Some(index);
            let effective_rank = effective_ranks.get(&index).copied();
            let unreachable_after_quota = blocker_rank
                .is_some_and(|blocked_rank| effective_rank.is_some_and(|rank| rank > blocked_rank));
            let in_stochastic_frontier = stochastic_frontier.contains(&index);
            let decision_reason = if selected {
                if route_present {
                    "selected_by_route_strategy"
                } else {
                    "selected_by_account_pool"
                }
            } else if blocks_on_quota {
                "quota_fallback_disabled"
            } else if unreachable_after_quota {
                "unreachable_after_quota_failure"
            } else if in_stochastic_frontier {
                "stochastic_selection"
            } else if stochastic_selection && decisions[index] == PreDispatchDecision::Dispatchable
            {
                "outside_stochastic_frontier"
            } else if decisions[index] == PreDispatchDecision::SkipAfterQuota {
                "account_soft_quota_fallback"
            } else if decisions[index] == PreDispatchDecision::Dispatchable {
                if route_present {
                    "higher_ranked_candidate_selected"
                } else {
                    "another_account_ordered_first"
                }
            } else {
                "ineligible"
            };
            DispatchCandidateRecord {
                candidate_id: candidate.id.clone(),
                strategy_rank: effective_rank,
                decision: decisions[index],
                selected,
                decision_reason,
            }
        })
        .collect();

    DispatchPlan {
        ordered_candidate_ids: ordered_indices
            .iter()
            .map(|index| candidates[*index].id.clone())
            .collect(),
        candidates: records,
        outcome,
        stochastic_selection,
    }
}

fn ordered_indices(candidates: &[DispatchCandidate]) -> Vec<usize> {
    let mut indices: Vec<_> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| candidate.rank.map(|rank| (rank, index)))
        .collect();
    indices.sort_by_key(|(rank, index)| (candidates[*index].defer_for_circuit, *rank));
    indices.into_iter().map(|(_, index)| index).collect()
}

fn is_dispatch_or_quota_stop(decision: PreDispatchDecision) -> bool {
    matches!(
        decision,
        PreDispatchDecision::Dispatchable | PreDispatchDecision::RateLimited
    )
}

fn first_account_tier(
    group: &[usize],
    candidates: &[DispatchCandidate],
    decisions: &[PreDispatchDecision],
) -> Vec<usize> {
    let first_priority = group
        .iter()
        .filter(|index| is_dispatch_or_quota_stop(decisions[**index]))
        .map(|index| candidates[*index].account_priority)
        .min();
    let Some(first_priority) = first_priority else {
        return Vec::new();
    };
    group
        .iter()
        .copied()
        .filter(|index| {
            candidates[*index].account_priority == first_priority
                && is_dispatch_or_quota_stop(decisions[*index])
        })
        .collect()
}

fn account_tier_is_stochastic(tier: &[usize], decisions: &[PreDispatchDecision]) -> bool {
    let dispatch_count = tier
        .iter()
        .filter(|index| decisions[**index] == PreDispatchDecision::Dispatchable)
        .count();
    let quota_stop_count = tier
        .iter()
        .filter(|index| decisions[**index] == PreDispatchDecision::RateLimited)
        .count();
    dispatch_count > 1 || (dispatch_count > 0 && quota_stop_count > 0)
}

fn stochastic_frontier(
    candidates: &[DispatchCandidate],
    decisions: &[PreDispatchDecision],
    ordered_indices: &[usize],
    route_present: bool,
    strategy: DispatchStrategy,
    affinity_candidate_id: Option<&str>,
) -> (bool, HashSet<usize>) {
    if let Some(affinity_id) = affinity_candidate_id {
        if candidates.iter().enumerate().any(|(index, candidate)| {
            candidate.id == affinity_id && is_dispatch_or_quota_stop(decisions[index])
        }) {
            return (false, HashSet::new());
        }
    }

    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    let mut group_order = Vec::new();
    for index in ordered_indices {
        let candidate = &candidates[*index];
        let group_id = if route_present {
            candidate
                .group_id
                .clone()
                .unwrap_or_else(|| format!("candidate:{}", candidate.id))
        } else {
            "direct-account-pool".to_string()
        };
        if !groups.contains_key(&group_id) {
            group_order.push(group_id.clone());
        }
        groups.entry(group_id).or_default().push(*index);
    }

    if strategy == DispatchStrategy::Weighted && route_present {
        let mut frontier = HashSet::new();
        let mut possible_dispatches = HashSet::new();
        let mut possible_quota_stop = false;
        let mut stochastic = false;
        for group_id in &group_order {
            let Some(group) = groups.get(group_id) else {
                continue;
            };
            let group_weight = group
                .first()
                .map(|index| normalized_weight(candidates[*index].weight))
                .unwrap_or(0);
            if group_weight == 0 {
                continue;
            }
            let tier = first_account_tier(group, candidates, decisions);
            stochastic |= account_tier_is_stochastic(&tier, decisions);
            for index in tier {
                if decisions[index] == PreDispatchDecision::Dispatchable {
                    possible_dispatches.insert(candidates[index].id.as_str());
                } else {
                    possible_quota_stop = true;
                }
                frontier.insert(index);
            }
        }
        stochastic |= possible_dispatches.len() > 1
            || (possible_quota_stop && !possible_dispatches.is_empty());
        return (
            stochastic,
            if stochastic { frontier } else { HashSet::new() },
        );
    }

    if !route_present {
        let group: Vec<_> = ordered_indices.to_vec();
        let tier = first_account_tier(&group, candidates, decisions);
        let stochastic = account_tier_is_stochastic(&tier, decisions);
        return (
            stochastic,
            if stochastic {
                tier.into_iter().collect()
            } else {
                HashSet::new()
            },
        );
    }

    for group_id in group_order {
        let Some(group) = groups.get(&group_id) else {
            continue;
        };
        let tier = first_account_tier(group, candidates, decisions);
        if !tier.is_empty() {
            let stochastic = account_tier_is_stochastic(&tier, decisions);
            return (
                stochastic,
                if stochastic {
                    tier.into_iter().collect()
                } else {
                    HashSet::new()
                },
            );
        }
    }
    (false, HashSet::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> PreDispatchFacts {
        PreDispatchFacts {
            request_eligible: true,
            account_eligible: true,
            account_quota_reached: Some(false),
            quota_fallback_allowed: false,
            provider_circuit_available: Some(true),
            route_capacity_available: true,
            quota_override_available: true,
            adaptive_capacity_available: true,
        }
    }

    fn candidate(id: &str, rank: usize, facts: PreDispatchFacts) -> DispatchCandidate {
        DispatchCandidate {
            id: id.to_owned(),
            rank: Some(rank),
            group_id: None,
            account_priority: 1,
            weight: 1,
            defer_for_circuit: false,
            facts,
        }
    }

    #[test]
    fn quota_stop_precedes_open_provider_circuit_and_stops_the_frontier() {
        let mut quota = facts();
        quota.account_quota_reached = Some(true);
        quota.provider_circuit_available = Some(false);
        let candidates = [
            candidate("quota", 0, quota),
            candidate("fallback", 1, facts()),
        ];
        let plan = plan_dispatch(&candidates, true, DispatchStrategy::Ordered, None, false);

        assert_eq!(plan.outcome, DispatchOutcome::RateLimited("quota".into()));
        assert_eq!(
            plan.candidates[0].decision,
            PreDispatchDecision::RateLimited
        );
        assert_eq!(
            plan.candidates[1].decision_reason,
            "unreachable_after_quota_failure"
        );
    }

    #[test]
    fn weighted_route_and_account_priority_share_the_same_stochastic_frontier() {
        let first = candidate("first", 0, facts());
        let mut second = candidate("second", 1, facts());
        second.group_id = Some("other-route-target".into());
        let plan = plan_dispatch(
            &[first, second],
            true,
            DispatchStrategy::Weighted,
            None,
            true,
        );
        assert_eq!(plan.outcome, DispatchOutcome::Stochastic);
        assert!(plan
            .candidates
            .iter()
            .all(|candidate| candidate.decision_reason == "stochastic_selection"));
        assert!(plan.candidates.iter().all(|candidate| !candidate.selected));
    }

    #[test]
    fn affinity_candidate_makes_a_weighted_frontier_deterministic() {
        let first = candidate("sticky", 0, facts());
        let mut second = candidate("other", 1, facts());
        second.group_id = Some("other-route-target".into());
        let plan = plan_dispatch(
            &[first, second],
            true,
            DispatchStrategy::Weighted,
            Some("sticky"),
            true,
        );
        assert_eq!(plan.outcome, DispatchOutcome::Selected("sticky".into()));
        assert!(!plan.stochastic_selection);
        assert!(plan.candidates[0].selected);
    }

    #[test]
    fn later_account_priority_tier_is_not_the_stochastic_first_frontier() {
        let mut first = candidate("first", 0, facts());
        first.account_priority = 1;
        let mut lower = candidate("lower", 1, facts());
        lower.account_priority = 2;
        let plan = plan_dispatch(
            &[first, lower],
            false,
            DispatchStrategy::Ordered,
            None,
            true,
        );
        assert_eq!(plan.outcome, DispatchOutcome::Selected("first".into()));
        assert!(!plan.stochastic_selection);
        assert_eq!(
            plan.candidates[1].decision_reason,
            "another_account_ordered_first"
        );
    }

    #[test]
    fn quota_fallback_skips_candidate_before_provider_circuit() {
        let mut exhausted = facts();
        exhausted.account_quota_reached = Some(true);
        exhausted.quota_fallback_allowed = true;
        exhausted.provider_circuit_available = Some(false);
        assert_eq!(
            plan_candidate(exhausted),
            PreDispatchDecision::SkipAfterQuota
        );
    }

    #[test]
    fn planner_requests_quota_before_provider_circuit() {
        let mut unresolved = facts();
        unresolved.account_quota_reached = None;
        unresolved.provider_circuit_available = None;
        assert_eq!(
            plan_candidate(unresolved),
            PreDispatchDecision::CheckAccountQuota
        );
    }
}
