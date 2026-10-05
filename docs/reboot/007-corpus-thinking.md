# 007: Corpus: ThinkingIntent cases

Status: open
Sequence step: 2 ([reboot.md](../reboot.md#sequence))
Blocked by: [002](002-corpus-runner.md)

## Goal

Pin every thinking intent to exact upstream JSON or a rejection.

## Scope

- One case per `ThinkingIntent` variant (Absent, Off, each Level, Budget, Adaptive with and without level) per frontend and standard wire format.
- Include Anthropic `budget_tokens` values around 2048/8192 to prove the bucketing is gone.
- Downgrade and budget-to-level only where the model's `ThinkingMap` declares it.

## Acceptance

- [ ] Matrix complete; failing ones are in the manifest.
