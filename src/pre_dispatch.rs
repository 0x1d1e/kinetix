//! Canonical ordered gates for deciding whether a routing candidate may be
//! dispatched. Runtime supplies facts as it reaches each gate; simulation can
//! supply a complete read-only snapshot to the same planner.

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

/// Advance the candidate through gates in runtime order. In particular, quota
/// is resolved before provider-circuit availability, so a quota policy that
/// stops fallback cannot be bypassed by an open provider circuit.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> PreDispatchFacts {
        PreDispatchFacts {
            request_eligible: true,
            account_eligible: true,
            account_quota_reached: None,
            quota_fallback_allowed: false,
            provider_circuit_available: None,
            route_capacity_available: true,
            quota_override_available: true,
            adaptive_capacity_available: true,
        }
    }

    #[test]
    fn quota_stop_precedes_open_provider_circuit() {
        let facts = PreDispatchFacts {
            account_quota_reached: Some(true),
            provider_circuit_available: Some(false),
            ..facts()
        };
        assert_eq!(plan_candidate(facts), PreDispatchDecision::RateLimited);
    }

    #[test]
    fn quota_fallback_skips_candidate_before_provider_circuit() {
        let facts = PreDispatchFacts {
            account_quota_reached: Some(true),
            quota_fallback_allowed: true,
            provider_circuit_available: Some(false),
            ..facts()
        };
        assert_eq!(plan_candidate(facts), PreDispatchDecision::SkipAfterQuota);
    }

    #[test]
    fn planner_requests_quota_before_provider_circuit() {
        assert_eq!(
            plan_candidate(facts()),
            PreDispatchDecision::CheckAccountQuota
        );
    }
}
