# Plugin response contract

Provider-adapter plugins return one canonical JSON contract for streaming parser calls and full non-streaming parser calls. The contract is independent of provider wire formats.

## Envelope and versions

The current envelope is:

```json
{
  "schema": "kinetix.plugin.response",
  "schema_version": 2,
  "events": []
}
```

Kinetix accepts schema versions 1 and 2. Version 1 remains supported for existing plugins. Use version 2 when emitting block-aware thinking events or redacted-thinking data. The host's serializer emits version 2.

`schema`, `schema_version`, and `events` are required. Kinetix rejects unknown schema names or versions, non-array `events`, malformed events, and unknown event types.

The host also accepts a **legacy bare event array** as a migration shim for already-published plugins. That form is not part of either version and must not be used by new or updated plugins.

## Events

Both versions support these events:

| `type` | Required fields | Optional fields | Meaning |
| --- | --- | --- | --- |
| `start` | - | `upstream_request_id: string\|null` | Provider request/response identity metadata. |
| `text_delta` | `text: string` | - | Assistant-visible text delta. |
| `thinking_delta` | `text: string` | `signature: string\|null` | Reasoning/thinking delta plus provider signature when present. |
| `tool_call_start` | `index: u32`, `name: string` | `id: string\|null`, `signature: string\|null` | Starts one tool call. |
| `tool_call_args_delta` | `index: u32`, `args: string` | - | Incremental serialized tool arguments. |
| `usage` | - | `input`, `output`, `cached`, `cache_write`, `thinking`: non-negative integer or null | Canonical token accounting. |
| `finish` | `reason: string` | - | Terminal model finish reason. Canonical values are `stop`, `length`, `tool_calls`, and `content_filter`; non-empty future/provider-specific reasons are preserved. |
| `warning` | `code: string`, `message: string` | - | Diagnostic warning. Kinetix validates and redacts it before logging; it is not forwarded as client content. |
| `error` | `kind: string`, `message: string` | `status: u16\|null`, `retry_after_secs: u64\|null`, `quota_reset_at: RFC3339 string\|null` | Terminal normalized failure. It must be the only event in its envelope. |

Version 2 adds block-aware thinking events:

| `type` | Required fields | Optional fields | Meaning |
| --- | --- | --- | --- |
| `thinking_block_start` | `index: u32`, `thinking: string` | `signature: string\|null` | Starts a thinking block, retaining its initial content and signature. |
| `thinking_delta` | `text: string` | `block_index: u32\|null`, `signature: string\|null` | Version 2 may associate a delta and signature with a source thinking block. |
| `thinking_block_stop` | `index: u32` | - | Ends a thinking block. |
| `redacted_thinking` | `index: u32`, `data: string` | - | Opaque redacted-thinking payload. Preserve the data unchanged. |

Valid `error.kind` values are `rate_limit`, `quota_exhausted`, `auth_error`, `target_error`, `server_error`, `connection_error`, `timeout`, `bad_request`, `malformed_upstream`, `plugin_failure`, `policy_rejected`, and `client_cancelled`.

`malformed_upstream` reports invalid upstream JSON, SSE framing, or response structure. `plugin_failure` reports plugin execution or contract failures. `policy_rejected` reports a host compatibility/routing rejection; core portability, parameter, and translation policies use this same kind, with HTTP 400 and structured Route Trace fields. `client_cancelled` records a client disconnect and must not be treated as provider or plugin health failure.

## Validation

Kinetix validates the contract before converting guest output into internal stream events.

- Required fields must exist with the documented JSON type; no defaults are guessed.
- Numeric fields must be non-negative integers and fit the target integer width.
- Optional fields may be absent or `null`; a wrong non-null type is rejected.
- Version 1 ignores `thinking_delta.block_index` as an unknown additive field; block-aware thinking events are version 2-only.
- Unknown event types are rejected.
- A terminal `error` event must be the sole event in its envelope so successful deltas cannot be silently discarded with the failure.
- Terminal error messages and warning messages are redacted again by the host.
- Unknown fields are ignored for forward-compatible additions.

## Evolution rules

Within a schema version, changes are additive only. A new schema version is required for an event type that older hosts must understand, or for any incompatible field or envelope change. Plugins must emit the version matching the events they use. Hosts reject unsupported versions instead of inferring compatibility.

## Schemas and fixtures

The machine-readable schemas are `wit/contracts/kinetix.plugin.response.v1.schema.json` and `wit/contracts/kinetix.plugin.response.v2.schema.json`. Golden fixtures live under `wit/fixtures/plugin-response/v1/` and `wit/fixtures/plugin-response/v2/`. Host tests consume these files directly; guest SDK/plugin tests should use the same fixtures rather than recreate examples.
