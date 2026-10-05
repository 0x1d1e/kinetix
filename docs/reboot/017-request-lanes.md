# 017: Split request into passthrough and canonical lanes

Status: open
Sequence step: 6 ([reboot.md](../reboot.md#sequence))
Blocked by: [003](003-migrate-existing-fixtures.md), [004](004-corpus-text-tools.md), [005](005-corpus-streaming.md), [006](006-corpus-schemas.md), [007](007-corpus-thinking.md), [008](008-corpus-continuation.md), [009](009-corpus-failures.md), [010](010-client-captures.md), [011](011-reference-regressions.md)

## Goal

Remove the dual source of truth in `InternalRequest`.

## Scope

- Passthrough lane carries the original wire document; canonical lane carries typed semantics.
- Passthrough streaming: native frames to the client; core keeps framing, commit point, terminal error; Codec extracts usage and failure evidence.
- Every field keeps a field-contract disposition. Update `docs/field-contract.md` and `docs/protocol-v1-compatibility.md`.

## Acceptance

- [ ] No code reads both `raw_body` and typed fields for the same request.
- [ ] Text, tools, and streaming corpus green.
