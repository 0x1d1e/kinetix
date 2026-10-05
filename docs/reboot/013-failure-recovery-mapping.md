# 013: Failure recovery mapping

Status: open
Sequence step: 3 ([reboot.md](../reboot.md#sequence))
Blocked by: [003](003-migrate-existing-fixtures.md), [004](004-corpus-text-tools.md), [005](005-corpus-streaming.md), [006](006-corpus-schemas.md), [007](007-corpus-thinking.md), [008](008-corpus-continuation.md), [009](009-corpus-failures.md), [010](010-client-captures.md), [011](011-reference-regressions.md)
ADRs: [ADR-0004](../adr/0004-failure-recovery-model.md)

## Goal

Replace per-site `FailureKind` matching with one pure mapping from evidence to `Failure`.

## Scope

- Add `Failure { class, retry_scope, retry_after, credential_action, account_effect, target_effect }` and the mapping function.
- Move pool, circuit, admission, telemetry, and stream outcome to consume `Failure` fields.
- Route configuration can narrow `retry_scope`, never widen it.
- Recovery order: request-local repair, same-Account retry, credential refresh, Account rotation, Target fallback, Route fallback.

## Out of scope

- Attempt loop extraction (014).

## Acceptance

- [ ] No `match` on `FailureKind` outside the mapping.
- [ ] Failure corpus green, removed from the manifest.
- [ ] Docs updated where recovery behavior is described.
