# Plugin response contract v1

Provider-adapter plugins return one canonical JSON contract for both streaming parser calls and full non-streaming parser calls. This contract is independent of OpenAI, Anthropic, Gemini, or any other provider wire format.

## Envelope

```json
{
  "schema": "kinetix.plugin.response",
  "schema_version": 1,
  "events": []
}
```

`schema`, `schema_version`, and `events` are required. Kinetix rejects unknown schema names, unsupported versions, non-array `events`, malformed events, and unknown event types.

The host currently accepts a **legacy bare event array** only as a migration shim for already-published plugins. That form is not part of v1 and must not be used by new or updated plugins.

## Events

| `type` | Required fields | Optional fields | Meaning |
| --- | --- | --- | --- |
| `start` | — | `upstream_request_id: string\|null` | Provider request/response identity metadata. |
| `text_delta` | `text: string` | — | Assistant-visible text delta. |
| `thinking_delta` | `text: string` | `signature: string\|null` | Reasoning/thinking delta plus provider signature when present. |
| `tool_call_start` | `index: u32`, `name: string` | `id: string\|null`, `signature: string\|null` | Starts one tool call. |
| `tool_call_args_delta` | `index: u32`, `args: string` | — | Incremental serialized tool arguments. |
| `usage` | — | `input`, `output`, `cached`, `cache_write`, `thinking`: non-negative integer or null | Canonical token accounting. |
| `finish` | `reason: string` | — | Terminal model finish reason. Canonical values are `stop`, `length`, `tool_calls`, and `content_filter`; non-empty future/provider-specific reasons are preserved. |
| `warning` | `code: string`, `message: string` | — | Diagnostic warning. Kinetix validates and redacts it before logging; it is not forwarded as client content. |
| `error` | `kind: string`, `message: string` | `status: u16\|null`, `retry_after_secs: u64\|null`, `quota_reset_at: RFC3339 string\|null` | Terminal normalized failure. It must be the only event in its envelope. |

Valid `error.kind` values are `rate_limit`, `quota_exhausted`, `auth_error`, `target_error`, `server_error`, `connection_error`, `timeout`, `bad_request`, `malformed_upstream`, `plugin_failure`, `policy_rejected`, and `client_cancelled`.

`malformed_upstream` reports invalid upstream JSON, SSE framing, or response structure. `plugin_failure` reports plugin execution or contract failures. `policy_rejected` is a host compatibility/routing rejection. `client_cancelled` records a client disconnect and must not be treated as provider or plugin health failure.

## Validation

Kinetix validates the contract before converting guest output into internal stream events.

- Required fields must exist with the documented JSON type; no defaults are guessed.
- Numeric fields must be non-negative integers and fit the target integer width.
- Optional fields may be absent or `null`; a wrong non-null type is rejected.
- Unknown event types are rejected.
- A terminal `error` event must be the sole event in its envelope so successful deltas cannot be silently discarded with the failure.
- Terminal error messages and warning messages are redacted again by the host.
- Unknown **fields** are ignored so v1 can grow by adding optional metadata without breaking older consumers.

## Evolution rules

Within `schema_version = 1`, changes are additive only:

- optional fields may be added;
- existing optional fields may gain documented semantics without changing their type;
- canonical examples/fixtures may add cases that use already-defined event types.

A new schema version is required for:

- removing or renaming a field;
- making an optional field required;
- changing a field type or meaning incompatibly;
- changing terminal-error semantics;
- adding an event type that old v1 hosts would need to understand rather than safely ignore;
- changing the envelope shape or schema identifier.

Plugins must emit the version they implement. Hosts reject unsupported versions instead of attempting to infer compatibility.

## Golden fixtures

The machine-readable schema is `wit/contracts/kinetix.plugin.response.v1.schema.json`.

Golden fixtures live under `wit/fixtures/plugin-response/v1/`. Host tests consume these files directly. Guest SDK/plugin tests should consume the same files byte-for-byte when the WIT contract is synced, rather than recreating examples in plugin code.
