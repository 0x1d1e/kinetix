# ADR-0005: Responses continuation is upstream-owned; Kinetix retains no transcripts by default

## Status

Accepted

## Context

OpenAI Responses clients can continue a conversation with
`previous_response_id` instead of resending history. For a native Responses
Target the upstream owns that state. For a translated Target, Kinetix would
have to store the transcript itself and rehydrate it (OmniRoute's approach).

Today Kinetix rejects `previous_response_id` and `store: true`, and forces
upstream `store: false` on Responses passthrough (`src/passthrough.rs:92`)
and the Responses adapter (`src/adapters/openai_responses.rs:600`). Separately,
per-virtual-key body logging (opt-in, default off) stores redacted plaintext
request bodies for 7 days (`src/pipeline.rs:7500`).

Stored transcripts are prompts and model output at rest in the gateway.
9router states that it does not store prompts or responses. OmniRoute's
persisted transcript surfaces have needed ongoing privacy hardening.

## Decision

- Kinetix retains no conversation content durably by default.
- `store: false` means Kinetix never persists the request or response,
  regardless of any other setting, including body logging.
- `previous_response_id` is upstream-owned: forwarded to a native Responses
  Target that issued it. Native passthrough forwards the client's `store`
  value instead of forcing `false`, so the upstream can retain what the
  client asked it to. The id is opaque state scoped to the issuing Account
  (see Continuation in [docs/reboot.md](../reboot.md)), so it pins the
  Account and does not survive fallback.
- On a Target that cannot honor the id, the request is rejected explicitly.
  It is never dropped silently.

If Kinetix-managed continuation is added later, it requires explicit opt-in,
a bounded TTL, and AES-256-GCM encryption at rest. It is a separate decision
that supersedes this ADR.

## Alternatives considered

- **Virtualize `previous_response_id` for every Target (OmniRoute).** Lets
  Responses clients use any Target, at the cost of a transcript store that
  becomes a privacy and retention liability.
- **Opt-out store.** Retains content for operators who never chose it.

## Consequences

### Positive

- No conversation content at rest by default. `store: false` is a hard
  guarantee.
- No transcript store to secure, expire, or migrate in 0.1.0.

### Negative / trade-offs

- Responses clients that rely on `previous_response_id` work only against
  native Responses Targets, and lose cross-Account fallback for that request.
- Clients that resend full history (`store: false` style) are unaffected.
- Body logging must check `store` before persisting. Body logs remain
  plaintext at rest; encrypting them is out of scope here.
