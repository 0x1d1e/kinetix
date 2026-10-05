# ADR-0004: Separate failure classification from recovery decisions

## Status

Accepted

## Context

`FailureKind` (`src/types.rs:651`, 13 variants) says what happened. What to
do about it is derived ad hoc: `src/pipeline.rs` matches on `FailureKind` 175
times to compute fallback eligibility, cooldowns, account health, telemetry,
traffic, and stream outcome. One HTTP status can need different recovery: a
429 can be a short rate limit or exhausted quota; a 401 can be refreshable or
a revoked credential.

## Decision

A single core function maps failure evidence to a `Failure`:

```text
Failure {
  class           // what happened
  retry_scope     // none | same_account | another_account | another_target | route_fallback
  retry_after     // optional
  credential_action  // none | refresh | disable
  account_effect  // health/cooldown effect on the Account
  target_effect   // health effect on the Target / provider circuit
}
```

Recovery proceeds in a fixed order, and each stage consumes `retry_scope`:
request-local repair, same-account retry, credential refresh, account
rotation, Target fallback, Route fallback. Nothing retries after the commit
point.

Integrations supply evidence only (status, provider error code, quota reset).
Route configuration can narrow `retry_scope` (for example, disable fallback
on 429) but cannot widen it.

## Alternatives considered

- **Keep `FailureKind` and derive actions at each use site.** Current state;
  175 match sites drift independently.
- **`retryable: bool` taxonomy.** Cannot express "rotate account but do not
  change target".

## Consequences

- Recovery behavior becomes testable as a pure mapping from evidence to
  `Failure`, pinned by the failure corpus.
- Telemetry, health, and fallback sites consume `Failure` fields instead of
  re-matching on kind.
