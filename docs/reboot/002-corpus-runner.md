# 002: Transaction corpus format, runner, and expected-failure manifest

Status: open
Sequence step: 2 ([reboot.md](../reboot.md#sequence))
Blocked by: [001](001-legacy-branch.md)

## Goal

One data-driven runner that executes a whole observable transaction per case, against native and plugin integrations.

## Scope

- Define the case format under `tests/fixtures/` (JSON, like the existing fixtures): client, frontend, request, Target (transport, model capabilities, schema mode), expected canonical request (absent on the passthrough lane), exact upstream request, upstream response or SSE with chunking spec, exact client response or SSE, expected usage and recovery outcome.
- Runner as a Rust integration test that drives the real pipeline against an in-process synthetic upstream. No network.
- Plugin execution: run each case also through the plugin integration for that wire format (`b-ai`, `claude-code-oauth`, `ai-studio`). Decide and document how test components are built or vendored from kinetix-plugins.
- Expected-failure manifest: listed cases may fail; a listed case that passes fails CI. Wire into `scripts/run-ci.sh`.
- Seed with a handful of cases to prove the format end to end, including one streaming and one failure case.

## Out of scope

- Migrating existing fixtures (003) and filling case groups (004-011).

## Acceptance

- [ ] `scripts/run-ci.sh` runs the corpus and stays green.
- [ ] A case removed from the manifest that fails, and a listed case that passes, both fail CI.
- [ ] Format documented in `docs/` next to `field-contract.md`.
