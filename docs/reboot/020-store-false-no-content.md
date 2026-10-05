# 020: store: false persists no content

Status: open
Sequence step: 6 ([reboot.md](../reboot.md#sequence))
Blocked by: [003](003-migrate-existing-fixtures.md), [004](004-corpus-text-tools.md), [005](005-corpus-streaming.md), [006](006-corpus-schemas.md), [007](007-corpus-thinking.md), [008](008-corpus-continuation.md), [009](009-corpus-failures.md), [010](010-client-captures.md), [011](011-reference-regressions.md)
ADRs: [ADR-0005](../adr/0005-responses-continuation-retention.md)

## Goal

Make `store: false` a hard no-content guarantee.

## Scope

- Body logging checks `store` before persisting request or response bodies.
- Audit other content persistence, including opaque state values, and record which ones carry prompt or output content.
- Usage and accounting records still written.

## Acceptance

- [ ] Regression test: `store: false` with body logging on writes no body log.
- [ ] Docs updated.
