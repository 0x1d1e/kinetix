# 008: Corpus: continuation cases

Status: open
Sequence step: 2 ([reboot.md](../reboot.md#sequence))
Blocked by: [002](002-corpus-runner.md)
ADRs: [ADR-0005](../adr/0005-responses-continuation-retention.md)

## Goal

Pin continuation and opaque-state portability.

## Scope

- Gemini thought signatures, Anthropic thinking blocks, tool-call continuation.
- Responses `previous_response_id`: native same Account passes through; every other Target rejects.
- Responses encrypted reasoning items with `include: ["reasoning.encrypted_content"]`: native passthrough; rejection elsewhere.
- Rejection of non-portable state across Account, Provider, model, and format, per the opaque-state rules.

## Acceptance

- [ ] Cases exist for every listed item; failing ones are in the manifest.
