# 021: Split Adapter into Codec, Auth, Discovery, and descriptor

Status: open
Sequence step: 7 ([reboot.md](../reboot.md#sequence))
Blocked by: [013](013-failure-recovery-mapping.md), [015](015-schema-engine.md), [016](016-thinking-intent.md), [017](017-request-lanes.md)
ADRs: [ADR-0002](../adr/0002-core-plugin-ownership.md)

## Goal

Built-ins and plugins implement the same seams; core owns policy.

## Scope

- Split the `Adapter` trait (`src/adapters/mod.rs`) into Codec, Auth, Discovery plus a declarative descriptor.
- Migrate built-in adapters. AI Studio-style reuse (standard codec, custom auth) needs no codec copy.
- OpenAI-compatible API-key providers need only a descriptor.
- Descriptor carries schema profile, thinking map, continuation family, and error profile; extend accepted model state with the same profiles.

## Out of scope

- WIT changes (022).

## Acceptance

- [ ] No built-in adapter implements policy owned by core per ADR-0002.
- [ ] Corpus green on native integrations.
