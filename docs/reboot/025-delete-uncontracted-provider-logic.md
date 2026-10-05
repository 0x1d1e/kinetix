# 025: Delete provider-specific core logic without a contract

Status: open
Sequence step: after 7 ([reboot.md](../reboot.md#sequence))
Blocked by: [021](021-integration-seams.md)

## Goal

Remove provider quirks in core that no descriptor, ADR, or corpus case backs.

## Scope

- Inventory provider-name checks and special cases in core.
- For each: move into a descriptor or integration with a corpus case, or delete.

## Acceptance

- [ ] Inventory recorded in this ticket.
- [ ] Corpus green.
