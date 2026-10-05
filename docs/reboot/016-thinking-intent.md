# 016: ThinkingIntent

Status: open
Sequence step: 5 ([reboot.md](../reboot.md#sequence))
Blocked by: [003](003-migrate-existing-fixtures.md), [004](004-corpus-text-tools.md), [005](005-corpus-streaming.md), [006](006-corpus-schemas.md), [007](007-corpus-thinking.md), [008](008-corpus-continuation.md), [009](009-corpus-failures.md), [010](010-client-captures.md), [011](011-reference-regressions.md)

## Goal

Replace `ThinkingLevel` as client intent and remove the 2048/8192 bucketing.

## Scope

- Add `ThinkingIntent` and decode it in every frontend without loss.
- Core codecs map intent to exact wire JSON per `ThinkingMap`; reject what the map does not declare.
- Update `docs/thinking-contract.md` and the thinking fixtures.

## Out of scope

- Plugin-side thinking translation (022).

## Acceptance

- [ ] Thinking corpus green on native integrations.
