# 027: Release gate and 0.1.0 publish

Status: open
Sequence step: 8 ([reboot.md](../reboot.md#sequence))
Blocked by: [014](014-pipeline-attempt-loop.md), [018](018-responses-continuation.md), [019](019-reasoning-summary.md), [020](020-store-false-no-content.md), [022](022-wit-plugin-contract.md), [023](023-antigravity-reduce.md), [024](024-admin-db-split.md), [025](025-delete-uncontracted-provider-logic.md)
ADRs: [ADR-0001](../adr/0001-reboot-versioning.md)

## Goal

Ship 0.1.0 once the corpus passes.

## Scope

- Expected-failure manifest empty.
- Release gate table in reboot.md green against OpenAI, Anthropic, and Gemini Targets with native/plugin parity.
- Regenerate `catalog.json` and `src/plugins/catalog.snapshot.json`; update `install.sh` for the 0.1.0 release URLs.
- Publish Kinetix 0.1.0 and kinetix-plugins 0.1.0, then delete old releases and tags so `latest` is never empty.
- Move `docs/reboot.md` and `docs/reboot/` to `docs/archive/`.

## Acceptance

- [ ] 0.1.0 installs fresh via `install.sh`.
- [ ] Plugins install from the new catalog.
