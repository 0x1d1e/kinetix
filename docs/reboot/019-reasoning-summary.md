# 019: ReasoningSummary as a separate canonical field

Status: open
Sequence step: 6 ([reboot.md](../reboot.md#sequence))
Blocked by: [016](016-thinking-intent.md), [017](017-request-lanes.md)

## Goal

Decouple reasoning visibility from reasoning intent.

## Scope

- Add `ReasoningSummary`; map or reject per Target independently of `ThinkingIntent`.
- Cover replayed reasoning items keeping or normalizing their summary (OmniRoute #11108) and stream/non-stream parity (#10166).

## Acceptance

- [ ] Related reference regressions green.
