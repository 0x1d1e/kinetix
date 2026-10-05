# 014: Extract the attempt loop from pipeline.rs

Status: open
Sequence step: 3 ([reboot.md](../reboot.md#sequence))
Blocked by: [013](013-failure-recovery-mapping.md)

## Goal

Shrink `pipeline.rs` once recovery is a pure mapping.

## Scope

- Move the attempt loop into its own module that consumes `Failure`.
- No behavior change.

## Out of scope

- Other `pipeline.rs` cleanups.

## Acceptance

- [ ] Corpus unchanged and green for already-passing cases.
