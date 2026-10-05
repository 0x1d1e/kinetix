# 026: Optional --live corpus replay

Status: open
Sequence step: optional ([reboot.md](../reboot.md#sequence))
Blocked by: [002](002-corpus-runner.md)

## Goal

Replay safe corpus cases against real providers.

## Scope

- `--live` mode with operator-supplied credentials; marks cases as safe explicitly.
- Never runs in CI.

## Out of scope

- Release gate.

## Acceptance

- [ ] Documented; CI unaffected.
