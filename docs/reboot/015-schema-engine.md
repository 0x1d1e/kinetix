# 015: One core tool-schema engine

Status: open
Sequence step: 4 ([reboot.md](../reboot.md#sequence))
Blocked by: [003](003-migrate-existing-fixtures.md), [004](004-corpus-text-tools.md), [005](005-corpus-streaming.md), [006](006-corpus-schemas.md), [007](007-corpus-thinking.md), [008](008-corpus-continuation.md), [009](009-corpus-failures.md), [010](010-client-captures.md), [011](011-reference-regressions.md)
ADRs: [ADR-0003](../adr/0003-tool-schema-compatibility.md)

## Goal

Single core engine with `strict`/`compatible` modes and classified transforms.

## Scope

- Port the tested SDK `schema` behavior into core; built-in adapters use it. The SDK module stays until 022.
- Schema profiles as versioned data per wire format.
- Target-owned mode: Provider default, Target narrow or override, Route never. New migration, admin API, import/CLI validation, and dashboard field.
- Record each transform classification in the Route Trace.
- Field-contract disposition for any new client-visible field.

## Out of scope

- Plugin migration (022).

## Acceptance

- [ ] Schema corpus green in both modes on native integrations.
- [ ] Dashboard typechecks and builds.
- [ ] Docs updated.
