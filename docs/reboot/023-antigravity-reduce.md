# 023: Reduce Antigravity to real protocol differences

Status: open
Sequence step: 7 ([reboot.md](../reboot.md#sequence))
Blocked by: [022](022-wit-plugin-contract.md)

## Goal

The Antigravity plugin keeps only what its protocol actually differs in.

## Scope

- Remove logic now owned by core (schema, thinking, recovery).
- Schema mode moves from its Provider-only setting to the Target-owned setting.

## Acceptance

- [ ] Antigravity corpus cases green.
- [ ] No duplicated core policy in the plugin.
