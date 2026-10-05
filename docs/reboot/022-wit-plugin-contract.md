# 022: WIT contract for the new seams and plugin migration

Status: open
Sequence step: 7 ([reboot.md](../reboot.md#sequence))
Blocked by: [021](021-integration-seams.md)
ADRs: [ADR-0002](../adr/0002-core-plugin-ownership.md), [ADR-0003](../adr/0003-tool-schema-compatibility.md)

## Goal

Plugins implement the same seams and stop owning schema and thinking policy for standard formats.

## Scope

- Change `wit/` with matching tests; mirror with `kinetix-plugins/scripts/sync_host_contract.py`.
- Migrate `b-ai`, `claude-code-oauth`, `ai-studio`, `antigravity-oauth`, `opencode-free`.
- Remove the SDK `schema` module and `handles_thinking_translation`.
- Plugins return evidence, never recovery decisions.
- Version the canonical request and event types together with the WIT contract, since plugin Codecs emit canonical events.

## Acceptance

- [ ] Native/plugin parity green for every corpus case.
- [ ] `docs/PLUGIN-RESPONSE-CONTRACT.md` and plugin contract docs updated.
