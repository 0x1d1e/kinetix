# 009: Corpus: failure and recovery cases

Status: open
Sequence step: 2 ([reboot.md](../reboot.md#sequence))
Blocked by: [002](002-corpus-runner.md)
ADRs: [ADR-0004](../adr/0004-failure-recovery-model.md)

## Goal

Pin recovery outcomes for each failure class.

## Scope

- 400, 401 refreshable vs revoked, 403, 429 rate limit vs quota, 402, 5xx, timeout before commit, failure after commit.
- Each case asserts the recovery outcome: retry scope taken, credential action, Account and Target health effects, client-visible result.

## Acceptance

- [ ] Cases exist for every listed item; failing ones are in the manifest.
