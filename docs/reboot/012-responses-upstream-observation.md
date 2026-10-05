# 012: Observe OpenAI Responses continuation scope

Status: open
Sequence step: before 6 ([reboot.md](../reboot.md#sequence))
Blocked by: [001](001-legacy-branch.md)
ADRs: [ADR-0005](../adr/0005-responses-continuation-retention.md)

## Goal

Back the continuation rules with observed upstream behavior instead of assumptions.

## Scope

- Against real OpenAI Responses, determine whether `previous_response_id` and `encrypted_content` reasoning items replay across: same Account, other Account same org, other org, other model.
- Check what `store` values the upstream accepts with each.
- Record method, dates, and results in an archive research note; turn safe observations into corpus cases.

## Out of scope

- Implementation.

## Acceptance

- [ ] Scope per item recorded with evidence.
- [ ] reboot.md Continuation updated with the confirmed scope.
