//! Request-scoped pre-commit attempt budget.
//!
//! One budget covers every pre-commit attempt of a request: the attempt count,
//! the wall-clock deadline, the inter-attempt backoff, and the per-attempt
//! provider phase timeout. All methods take the time elapsed since request
//! start instead of reading a clock, so the runtime attempt loop and
//! [`simulate`] apply identical policy and the simulation is deterministic.

use std::time::Duration;

/// Wall-clock allowance for all pre-commit attempts of one request.
pub(crate) const PRE_COMMIT_DEADLINE: Duration = Duration::from_secs(30);
/// Bounded exponential backoff between pre-commit retry attempts (FR-4.4):
/// 100ms, 200ms, 400ms, capped at 1s, so a failing pool cannot be hot-looped.
const BACKOFF_BASE: Duration = Duration::from_millis(100);
const BACKOFF_CAP: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exhausted {
    Attempts,
    Deadline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttemptBudget {
    deadline: Duration,
    max_attempts: usize,
}

impl AttemptBudget {
    pub(crate) fn new(deadline: Duration, max_attempts: usize) -> Self {
        Self {
            deadline,
            max_attempts,
        }
    }

    pub(crate) fn pre_commit(max_attempts: usize) -> Self {
        Self::new(PRE_COMMIT_DEADLINE, max_attempts)
    }

    fn remaining(&self, elapsed: Duration) -> Duration {
        self.deadline.saturating_sub(elapsed)
    }

    /// Whether another attempt may start. The deadline is reported in
    /// preference to the attempt cap when both are spent.
    pub(crate) fn admit(&self, elapsed: Duration, attempts_done: usize) -> Result<(), Exhausted> {
        if self.remaining(elapsed).is_zero() {
            Err(Exhausted::Deadline)
        } else if attempts_done >= self.max_attempts {
            Err(Exhausted::Attempts)
        } else {
            Ok(())
        }
    }

    /// Backoff to wait before the next attempt. Zero before the first attempt.
    /// Fails when the wait would consume the rest of the deadline, since no
    /// attempt could run afterwards.
    pub(crate) fn backoff(
        &self,
        elapsed: Duration,
        attempts_done: usize,
    ) -> Result<Duration, Exhausted> {
        if attempts_done == 0 {
            return Ok(Duration::ZERO);
        }
        let delay = BACKOFF_BASE
            .saturating_mul(1u32 << (attempts_done - 1).min(4))
            .min(BACKOFF_CAP);
        if delay >= self.remaining(elapsed) {
            return Err(Exhausted::Deadline);
        }
        Ok(delay)
    }

    /// Time allowed for one attempt's connect/first-event phase: the provider
    /// phase timeout, clipped to what is left of the request deadline.
    pub(crate) fn phase(&self, elapsed: Duration, provider_timeout: Duration) -> Option<Duration> {
        let remaining = self.remaining(elapsed);
        (!remaining.is_zero()).then(|| remaining.min(provider_timeout))
    }
}

/// Scripted behavior of one candidate attempt.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScriptedAttempt {
    pub provider_timeout: Duration,
    /// Time the upstream needs to produce its verdict.
    pub latency: Duration,
    pub succeeds: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptResult {
    Succeeded,
    Failed,
    /// The phase budget elapsed before the upstream answered.
    TimedOut,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SimulatedAttempt {
    pub candidate: usize,
    pub started: Duration,
    pub finished: Duration,
    pub result: AttemptResult,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SimulationEnd {
    Succeeded,
    Exhausted(Exhausted),
    /// Every candidate was tried and failed within budget.
    CandidatesExhausted,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Simulation {
    pub attempts: Vec<SimulatedAttempt>,
    pub end: SimulationEnd,
    pub elapsed: Duration,
}

/// Replay the attempt loop over scripted candidates on a virtual clock that
/// starts `initial_elapsed` after request start. Fully deterministic.
#[cfg(test)]
pub(crate) fn simulate(
    budget: AttemptBudget,
    initial_elapsed: Duration,
    candidates: &[ScriptedAttempt],
) -> Simulation {
    let mut elapsed = initial_elapsed;
    let mut attempts = Vec::new();
    let mut end = SimulationEnd::CandidatesExhausted;
    for (candidate, script) in candidates.iter().enumerate() {
        let done = attempts.len();
        let admitted = budget
            .admit(elapsed, done)
            .and_then(|()| budget.backoff(elapsed, done));
        match admitted {
            Ok(delay) => elapsed += delay,
            Err(reason) => {
                end = SimulationEnd::Exhausted(reason);
                break;
            }
        }
        let Some(phase) = budget.phase(elapsed, script.provider_timeout) else {
            end = SimulationEnd::Exhausted(Exhausted::Deadline);
            break;
        };
        let started = elapsed;
        let (spent, result) = if script.latency >= phase {
            (phase, AttemptResult::TimedOut)
        } else if script.succeeds {
            (script.latency, AttemptResult::Succeeded)
        } else {
            (script.latency, AttemptResult::Failed)
        };
        elapsed += spent;
        attempts.push(SimulatedAttempt {
            candidate,
            started,
            finished: elapsed,
            result,
        });
        if result == AttemptResult::Succeeded {
            end = SimulationEnd::Succeeded;
            break;
        }
    }
    Simulation {
        attempts,
        end,
        elapsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn failing(timeout_ms: u64, latency_ms: u64) -> ScriptedAttempt {
        ScriptedAttempt {
            provider_timeout: ms(timeout_ms),
            latency: ms(latency_ms),
            succeeds: false,
        }
    }

    #[test]
    fn backoff_doubles_and_caps_and_skips_first_attempt() {
        let budget = AttemptBudget::pre_commit(10);
        let delays: Vec<_> = (0..8)
            .map(|done| budget.backoff(Duration::ZERO, done).unwrap())
            .collect();
        assert_eq!(
            delays,
            [0, 100, 200, 400, 800, 1000, 1000, 1000].map(ms).to_vec()
        );
    }

    #[test]
    fn backoff_that_would_consume_the_deadline_is_refused() {
        let budget = AttemptBudget::new(ms(1000), 5);
        assert_eq!(budget.backoff(ms(850), 1), Ok(ms(100)));
        assert_eq!(budget.backoff(ms(900), 1), Err(Exhausted::Deadline));
        assert_eq!(budget.backoff(ms(2000), 1), Err(Exhausted::Deadline));
    }

    #[test]
    fn phase_is_clipped_to_the_remaining_deadline() {
        let budget = AttemptBudget::new(ms(1000), 5);
        assert_eq!(budget.phase(ms(0), ms(300)), Some(ms(300)));
        assert_eq!(budget.phase(ms(800), ms(300)), Some(ms(200)));
        assert_eq!(budget.phase(ms(1000), ms(300)), None);
    }

    #[test]
    fn admit_prefers_deadline_over_attempt_cap() {
        let budget = AttemptBudget::new(ms(1000), 2);
        assert_eq!(budget.admit(ms(0), 1), Ok(()));
        assert_eq!(budget.admit(ms(0), 2), Err(Exhausted::Attempts));
        assert_eq!(budget.admit(ms(1000), 2), Err(Exhausted::Deadline));
    }

    #[test]
    fn attempt_cap_stops_a_fast_failing_pool() {
        let script = [failing(5000, 10); 4];
        let sim = simulate(AttemptBudget::new(ms(30_000), 3), Duration::ZERO, &script);
        assert_eq!(sim.attempts.len(), 3);
        assert_eq!(sim.end, SimulationEnd::Exhausted(Exhausted::Attempts));
        // 10 + (100 + 10) + (200 + 10)
        assert_eq!(sim.elapsed, ms(330));
    }

    #[test]
    fn slow_timeouts_cannot_exceed_the_request_deadline() {
        // Each target would wait its full 20s provider timeout.
        let script = [failing(20_000, 60_000); 5];
        let sim = simulate(AttemptBudget::pre_commit(5), Duration::ZERO, &script);
        assert_eq!(sim.attempts.len(), 2);
        assert_eq!(sim.attempts[0].result, AttemptResult::TimedOut);
        assert_eq!(sim.attempts[0].finished, ms(20_000));
        // The second attempt starts after backoff and gets only what is left.
        assert_eq!(sim.attempts[1].started, ms(20_100));
        assert_eq!(sim.attempts[1].finished, PRE_COMMIT_DEADLINE);
        assert_eq!(sim.end, SimulationEnd::Exhausted(Exhausted::Deadline));
        assert_eq!(sim.elapsed, PRE_COMMIT_DEADLINE);
    }

    #[test]
    fn routing_overhead_counts_against_the_deadline() {
        let script = [failing(20_000, 60_000)];
        let sim = simulate(AttemptBudget::pre_commit(5), ms(29_000), &script);
        assert_eq!(sim.attempts[0].started, ms(29_000));
        assert_eq!(sim.attempts[0].finished, PRE_COMMIT_DEADLINE);
        let spent = simulate(AttemptBudget::pre_commit(5), PRE_COMMIT_DEADLINE, &script);
        assert!(spent.attempts.is_empty());
        assert_eq!(spent.end, SimulationEnd::Exhausted(Exhausted::Deadline));
    }

    #[test]
    fn failover_stops_at_the_first_success() {
        let script = [
            failing(5000, 50),
            ScriptedAttempt {
                provider_timeout: ms(5000),
                latency: ms(70),
                succeeds: true,
            },
            failing(5000, 50),
        ];
        let sim = simulate(AttemptBudget::pre_commit(5), Duration::ZERO, &script);
        assert_eq!(sim.end, SimulationEnd::Succeeded);
        assert_eq!(
            sim.attempts,
            vec![
                SimulatedAttempt {
                    candidate: 0,
                    started: ms(0),
                    finished: ms(50),
                    result: AttemptResult::Failed,
                },
                SimulatedAttempt {
                    candidate: 1,
                    started: ms(150),
                    finished: ms(220),
                    result: AttemptResult::Succeeded,
                },
            ]
        );
    }

    #[test]
    fn exhausting_candidates_within_budget_is_distinct() {
        let sim = simulate(
            AttemptBudget::pre_commit(5),
            Duration::ZERO,
            &[failing(1000, 5)],
        );
        assert_eq!(sim.end, SimulationEnd::CandidatesExhausted);
        assert_eq!(
            simulate(AttemptBudget::pre_commit(5), Duration::ZERO, &[]),
            Simulation {
                attempts: vec![],
                end: SimulationEnd::CandidatesExhausted,
                elapsed: Duration::ZERO,
            }
        );
    }

    #[test]
    fn simulation_is_deterministic() {
        let script = [failing(3000, 4000), failing(3000, 100), failing(3000, 3000)];
        let run = || simulate(AttemptBudget::pre_commit(5), ms(7), &script);
        assert_eq!(run(), run());
    }
}
