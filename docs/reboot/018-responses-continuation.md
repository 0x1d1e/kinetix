# 018: Upstream-owned Responses continuation

Status: open
Sequence step: 6 ([reboot.md](../reboot.md#sequence))
Blocked by: [012](012-responses-upstream-observation.md), [017](017-request-lanes.md)
ADRs: [ADR-0005](../adr/0005-responses-continuation-retention.md)

## Goal

Codex CLI works on native Responses Targets, statelessly and with `previous_response_id`.

## Scope

- Native passthrough forwards the client `store` value instead of forcing `false`.
- Accept `previous_response_id` on native Responses Targets; pin the issuing Account; no fallback for that request.
- Accept `include: ["reasoning.encrypted_content"]` and encrypted reasoning items on native Targets, scoped per 012.
- Explicit rejection on every other Target.
- Update `docs/compatibility.md` Responses section and the real-client matrix.

## Acceptance

- [ ] Continuation corpus and Codex captures green.
- [ ] Codex CLI release-gate row passes.
