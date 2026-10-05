# 006: Corpus: tool schema cases in both modes

Status: open
Sequence step: 2 ([reboot.md](../reboot.md#sequence))
Blocked by: [002](002-corpus-runner.md)
ADRs: [ADR-0003](../adr/0003-tool-schema-compatibility.md)

## Goal

Make the real-world schema corpus a transaction-level spec for `strict` and `compatible`.

## Scope

- Import `kinetix-plugins/sdk/tests/fixtures/schema-compat/corpus.json` and `provider-compatible.json`.
- Cases: k-jev `patternProperties`, Chrome DevTools `maxLength`, Claude Code tools, Codex shell, Zod/TypeBox output, `$ref`, tuples, unions, records, a property literally named `properties`, nullable schemas.
- Pin exact upstream JSON or rejection per mode, plus the expected Route Trace classification.

## Acceptance

- [ ] Every case has a strict and a compatible expectation; failing ones are in the manifest.
