# Client compatibility notes

Checked-in notes for the clients Kinetix is actually exercised against by
maintainers. This is documentation, **not** a public certification claim.
Protocol behavior is evidenced by fixtures, adversarial tests, and real
acceptance sessions.

## Pi (coding agent) — OpenAI Chat Completions

Pi is the primary client and is configured as an ordinary OpenAI-compatible
provider pointed at Kinetix. Nothing Pi-specific is required on the Kinetix side.

```jsonc
// Pi provider config (illustrative; exact fields follow Pi docs)
{
  "provider": "kinetix",
  "baseUrl": "http://127.0.0.1:8080/v1", // or https://api.example.com/v1
  "apiKey": "sk-kinetix-…",              // a Kinetix virtual key
  "apiMode": "openai-chat-completions"
}
```

Verified acceptance behaviour:

- A multi-turn streaming conversation, including a tool call, completes through
  OpenAI format. The tool call arrives as an assistant `tool_calls` delta with a
  stable `id` (`call_…`), the function name, and arguments streamed as JSON text
  fragments that reassemble correctly.
- `GET /v1/models` lists the aliases and Routes the key may use, plus bare
  upstream model IDs.
- Request headers Kinetix adds: `X-Request-Id`, `X-Kinetix-Route-Id` (opaque),
  `X-Kinetix-Fallback: 1` when a fallback occurred, and `X-Kinetix-Warning` when
  a `strip_with_warning` portability action affected the request. The fallback
  header is a presence flag (the hop count is internal routing detail and is not
  exposed); resolve the opaque route id for the full trace. Serving
  account/provider names are **not** exposed to clients.

### Session identity for cache affinity

Kinetix does not guess a conversation identity. To use cache-aware sticky
routing, the client must send an explicit session header:
`X-Kinetix-Session`, `X-Session-Id`, `Session-Id` (underscore spelling),
`X-Conversation-Id`, or `X-Session-Affinity`. Without one, each request is
routed independently. Pi sends `X-Session-Id` when configured with
`sendSessionAffinityHeaders` and `sessionAffinityFormat: "openrouter"`, or
`Session-Id` with the `"openai"` format (see `docs/pi-compatibility.md`).

Cache affinity is routing locality only; it is not prompt caching. On the
Anthropic path, same-format requests preserve client cache controls. Translated
requests automatically receive generated cache controls only for confirmed
Claude Code OAuth targets, where Kinetix marks a deterministic stable system/tool
prefix with `ephemeral` / `1h`. Kinetix does not inject those controls for
generic Anthropic API-key or Anthropic-compatible providers. Cache reads/writes
are measured from upstream `cache_read_input_tokens` and
`cache_creation_input_tokens`. Prompt caches are model-specific, so switching
Claude models is expected to miss.

## OpenAI Responses API Clients (Next-Gen Coding Agents)

Kinetix serves an **explicit subset** of `POST /v1/responses`. Supported
requests to an `openai-responses` Target are forwarded by same-format
passthrough with upstream `store` forced to `false`. For other Targets,
Kinetix normalizes the request into its canonical model and translates it
through the Gemini, OpenAI-compatible Chat Completions, Anthropic, or plugin
adapters. Kinetix has **no Responses object store**.

Supported request semantics:

- text input, `instructions`, message input items, and URL/data-URL image input;
- custom function tools, function-call history, and function-call outputs;
- `tool_choice`: `auto`, `none`, `required`, or one named function;
- `temperature`, `top_p`, `max_output_tokens`, and Kinetix compatibility
  aliases/controls already represented by the canonical model;
- `reasoning.effort` as an input control when the selected model has an
  explicit thinking mapping (an unmapped level is rejected, also on
  same-format passthrough);
- `reasoning.summary` as a same-format Responses control, forwarded unchanged
  alongside the mapped `reasoning.effort`; translating paths reject it;
- `prompt_cache_key` as an advisory OpenAI prompt-cache hint: forwarded to
  OpenAI targets, used as the Route cache-affinity key (scoped per virtual key)
  when the Route enables affinity or sticky routing, and otherwise accepted
  and recorded in the Route trace as `prompt_cache_key ignored: affinity
  disabled` (never a 400);
- streaming and non-streaming output for text and custom function calls.

Streaming emits the supported semantic lifecycle events:
`response.created`, `response.in_progress`, output/content item events,
`response.output_text.*`, function-call argument events, and
`response.completed`. `response.completed` is terminal; Kinetix does not add
the Chat Completions `[DONE]` sentinel.

Unsupported semantics fail explicitly instead of being approximated. This
includes `previous_response_id`/conversation state, response storage,
background responses, hosted/MCP/computer/code-interpreter tools,
`include` expansions, structured `text.format`, reasoning output items,
`reasoning.generate_summary`, automatic truncation, metadata storage, and
unknown Responses fields.

## Hermetic coding-agent compatibility matrix

Wire compatibility is exercised by `scripts/compat-matrix.sh` against deterministic
OpenAI, Gemini, and Anthropic upstreams. The original #75/#83 coding-agent profiles
remain in `scripts/compat-matrix.py`; `scripts/protocol-v1-matrix.py` adds the
versioned v1 contract from `tests/fixtures/protocol-v1-compatibility.json`.

The path matrix explicitly runs **sync and streaming** for every built-in frontend /
adapter combination Kinetix supports:

| Inbound API | Same-format / native | Translated paths |
|---|---|---|
| `/v1/chat/completions` | OpenAI-compatible | Gemini, Anthropic |
| `/v1/messages` | Anthropic | Gemini, OpenAI-compatible |
| `/v1/responses` | none | OpenAI-compatible, Gemini, Anthropic |

That is 18 path/mode cells before specialized cases. Sync cases assert aggregated
usage and tool identity. Streaming cases assert terminal events, usage, and stable
tool-call identity. Specialized cases cover parallel tools, tool-result continuation,
Gemini tool-call signature replay, vision variants, tool-choice variants, nested
schemas/content rejection, opaque reasoning portability, token-count modes, model
discovery/auth, fallback, and same-format provider extensions.

The `chat.translate.gemini.tool_signature_continuation` case drives a full two-turn
tool conversation through the OpenAI frontend. The synthetic Gemini upstream fails
closed with the provider's real "Function call is missing a thought_signature" 400
unless the historical function-call part carries the exact signature Kinetix stored
on the previous turn, so a 200 is evidence that Kinetix captured, persisted, and
replayed the signature without the client ever seeing it. The same case includes a
negative control: a never-seen tool-call id must *not* be given an invented
signature, and the strict upstream rejects it.

The `chat.translate.gemini.cross_model_placeholder` case continues a trace that
started on one Gemini model onto a second model. The strict upstream accepts only
the provider's documented `skip_thought_signature_validator` placeholder on the
second model and rejects both an unsigned call and the first model's real
signature, so a 200 is evidence that Kinetix substituted the documented
placeholder rather than replaying a foreign signature or stripping the call. Its
negative control proves an uncaptured id is never given an invented placeholder.
The placeholder is gated to models whose `generateContent` validates replayed
function-call signatures (the Gemini 3 family): the
`chat.translate.gemini.legacy_model_strip_without_placeholder` case continues the
same trace onto a pre-Gemini-3 model, and the strict upstream rejects *any*
`thoughtSignature` there, so a 200 with a warning proves Kinetix stripped the
incompatible state instead of injecting the Gemini 3 sentinel.

Positive mixed fixtures send the documented sampling, tool-choice, vision, and
reasoning fields. `scripts/synthetic_upstream.py` rejects the request if required
translated wire fields are missing, so a 200 response is evidence that those fields
actually reached the selected adapter wire format rather than merely surviving
frontend decoding.

The matrix is hermetic, has no paid/public provider dependency, and remains in the
normal `scripts/run-ci.sh` gate. Real installed clients are deliberately separate.

## Model listing capabilities

OpenAI-format `GET /v1/models` items keep the standard `id`, `object`,
`created`, and `owned_by` fields and add accepted model metadata:

```json
{
  "id": "claude-sonnet-5",
  "object": "model",
  "owned_by": "kinetix",
  "context_window": 200000,
  "capabilities": {
    "tools": true,
    "images": true,
    "streaming": true,
    "structured_output": false,
    "thinking": {"modes": ["off", "adaptive"], "levels": ["low", "medium", "high"]}
  },
  "kinetix": {
    "state": "accepted",
    "transport": "anthropic",
    "provenance": {"context_window": "models.dev", "thinking": "operator", "transport": "provider"}
  }
}
```

- Values come from accepted, effective model state only. Raw discovery
  observations stay on the admin API ([model-state.md](model-state.md)).
- Unknown values are omitted, never guessed. `streaming` is always `true`:
  Kinetix serves streaming requests for every listed model.
- `thinking.modes` uses `off`, `on`, `level`, `budget`, and `adaptive`.
- A Route reports the intersection of the targets the key can reach: a
  capability is `true` only when every target supports it and `false` when any
  target lacks it. Context and output limits are the smallest target limit.
  `transport` and provenance entries appear only when all targets agree.
- `transport` is `openai`, `openai-responses`, `anthropic`, `gemini`, or
  `plugin`. Provenance is `operator`, `models.dev`, `provider`, `plugin`, or
  `probe`.
- Anthropic-format listings keep the native shape without these fields.

## Field-level v1 contract

The generated field matrix lives at
[`docs/protocol-v1-compatibility.md`](protocol-v1-compatibility.md). The
path-by-feature summary (tools, images, structured output, thinking, streaming,
continuation, reasoning replay) lives at
[`docs/generated/compatibility-matrix.md`](generated/compatibility-matrix.md); each
cell is `native`, `translated`, `supported`, `rejected`, or `conditional` and cites
evidence case IDs. Both are generated from
`tests/fixtures/protocol-v1-compatibility.json` (the `matrix` section feeds the
summary).

Each semantic row cites one or more concrete evidence cases. Rows that describe both
same-format and translated behavior cite the relevant paths independently instead of
using one broad case as a proxy for both. Rejection rows have explicit probes for the
documented fields, including Chat `n`, logprobs, structured response format,
modalities/audio and prediction, plus Responses storage/background/include/text
format/truncation/stream options/metadata/parallel-tool/unknown semantics.

Regenerate and verify it with:

```bash
python3 scripts/render-protocol-v1-compat.py
python3 scripts/render-protocol-v1-compat.py --check
```

The renderer rejects missing/unknown evidence references and the compatibility runner
executes every declared HTTP case. Cargo-backed plugin request/response contracts run
as normal Rust integration tests; the real external `.kxp` case is explicitly marked
as manual release acceptance.

## Real-client release acceptance

Real Pi, Claude Code, Codex/Responses, and optional external `.kxp` sessions are a
release gate, not normal CI. They may consume provider quota and depend on installed
client versions.

Configure explicit models/routes for each behavior instead of pointing every client at
one generic model:

```bash
export KINETIX_BASE=https://kinetix.example.com
export KINETIX_KEY=sk-kinetix-...
export KINETIX_ADMIN_TOKEN='admin credential'

export KINETIX_ACCEPT_PI_SAME_MODEL=direct-openai
export KINETIX_ACCEPT_PI_TRANSLATED_MODEL=translated-non-openai
export KINETIX_ACCEPT_PI_FALLBACK_MODEL=forced-fallback-route
export KINETIX_ACCEPT_PI_AFFINITY_MODEL=sticky-route

export KINETIX_ACCEPT_CLAUDE_SAME_MODEL=direct-anthropic
export KINETIX_ACCEPT_CLAUDE_TRANSLATED_MODEL=translated-non-anthropic
export KINETIX_ACCEPT_CLAUDE_FALLBACK_MODEL=forced-fallback-route
export KINETIX_ACCEPT_CLAUDE_AFFINITY_MODEL=sticky-route

export KINETIX_ACCEPT_RESPONSES_OPENAI_MODEL=responses-via-openai
export KINETIX_ACCEPT_RESPONSES_GEMINI_MODEL=responses-via-gemini
export KINETIX_ACCEPT_RESPONSES_ANTHROPIC_MODEL=responses-via-anthropic
export KINETIX_ACCEPT_RESPONSES_FALLBACK_MODEL=forced-fallback-route
export KINETIX_ACCEPT_RESPONSES_AFFINITY_MODEL=sticky-route

bash scripts/release-client-acceptance.sh all
```

The Pi, Claude Code, and Codex runners get their configuration from Kinetix's
client-profile endpoint, so the active key supplied as `KINETIX_KEY` must grant each
selected model. This keeps real-client acceptance on the same renderer used by the
dashboard.

For the fallback selectors, configure a Route whose first eligible target fails and a
later target succeeds. For affinity selectors, use a sticky Route with multiple
eligible targets. The Responses real-client matrix covers each built-in translation
adapter; native `openai-responses` passthrough is not yet in the matrix.

The Pi section also writes and gates on `pi-acceptance.json`; release candidates
attach it to the release. See [Pi compatibility](pi-compatibility.md#tier-2-real-pi-release-gate).

The runner starts `scripts/release-client-proxy.py` locally for each case. It forwards
the real client's bytes unchanged while recording client-visible evidence: request
`stream`, response `Content-Type`, session headers, Kinetix request/opaque route IDs,
tool-call IDs, returned tool-result references, fallback/warning headers, statuses, and
error excerpts. It does not record credentials. Every inference turn must prove
`stream: true` and `text/event-stream`. A client case passes only when it completes a multi-turn streaming
session, returns both grounded sentinels, produces at least two distinct tool calls,
and returns at least two tool results.

Fallback cases additionally require `X-Kinetix-Fallback: 1`. Affinity cases require
a stable session header **and** use the admin-only opaque Route Trace endpoint to prove
that every turn resolved to the same `final_target`. Claude acceptance also performs
an explicit `/v1/messages/count_tokens` probe against the same-format Anthropic
selector and requires `X-Kinetix-Token-Count: exact`.

Artifacts include client versions, raw client logs, evidence-proxy JSONL, verification
logs, and a TSV summary. Failure categories distinguish `auth`, `model`,
`transport`, `frontend`, `translation`, `routing`, `upstream`, and `client`.
Set `KINETIX_PLUGIN_E2E_PACKAGE=/path/to/plugin.kxp` to include real plugin-host
execution. Keep this runner out of normal PR CI.

## Anthropic-format clients

`POST /v1/messages` accepts the Anthropic Messages shape with `x-api-key` auth
and `anthropic-version`. Streaming emits `message_start` → content blocks →
`message_delta` → `message_stop`. Known fidelity note: because Kinetix's Gemini
upstream reports usage only at stream end, `message_start` reports
`input_tokens: 0` and `message_delta` carries the output count; the authoritative
input/output/thinking counts are recorded in the usage log and the Route Trace.

`POST /v1/messages/count_tokens` returns the Anthropic-compatible
`{"input_tokens": N}` shape. Kinetix uses the upstream Anthropic token-count API
when routing resolves unambiguously to a healthy built-in Anthropic target.
Plugin adapters, heterogeneous Routes, non-Anthropic targets, or temporarily
unavailable exact targets use a deterministic local estimate instead. The
estimate is based on canonical system/messages plus tool names, descriptions,
and schemas; images use Kinetix's coarse canonical image estimate. The response
header `X-Kinetix-Token-Count` is `exact` or `estimated` so callers can tell
which path was used. Token counting never advances round-robin/weighted route
state.

## Reasoning / thinking models

Kinetix passes reasoning through the portable-extension layer and never invents
reasoning fields for models without configuration. Consequence: with a
small `max_tokens`, a reasoning model may spend the entire budget on thinking and
return empty content with `finish_reason: "length"` — this is upstream behaviour,
not a Kinetix bug. Raise `max_tokens` or lower the thinking level.

## Provider wire-format notes

- **Gemini:** `streamGenerateContent?alt=sse`; SSE frames are CRLF-separated and
  are normalized to LF by the byte-robust framer. `thoughtSignature`
  values are round-tripped through the internal model's signature slots. Because a
  translated client (OpenAI Chat Completions, Responses, Anthropic Messages) cannot
  represent a `thoughtSignature`, Kinetix also persists each function-call signature
  server-side keyed by the client-visible tool-call id and replays it on the next
  turn; see [Opaque provider state](#opaque-provider-state) below.
- **OpenAI-compatible and Anthropic:** same-format SSE passthrough forwards
  event fields and payloads without JSON re-encoding, preserving unknown/vendor
  fields such as `cost`, `reasoning_details`, `thinking_delta`, and
  `signature_delta`. It is not byte-transparent: Kinetix normalizes line
  endings to LF and reconstructs frame delimiters.
- **Anthropic:** inbound `anthropic-version` and `anthropic-beta` are forwarded
  to Anthropic upstreams; Kinetix does not invent hidden version/beta defaults.

## Opaque provider state

Some providers attach state to a tool call or reasoning block that clients
must replay unchanged. Anthropic `thinking` signatures and `redacted_thinking`
blocks are represented canonically. They can be replayed to the same provider
and upstream model, or across other targets whose
`capabilities.continuation_families` explicitly share an operator-verified
family. Kinetix does not infer continuation compatibility from provider, wire
format, or upstream model name. For other targets without a matching family,
the Route's `reject` or `strip_with_warning` policy applies.

For example, configure `anthropic_thinking_signature:v1` on both models only
when both upstream targets accept the same historical thinking and redacted
thinking blocks unchanged. Family names are operator assertions, not automatic
provider capability detection.

Gemini's `thoughtSignature` is the canonical hidden-state example: the model
returns it beside a `functionCall`, and requires it back on the *same*
historical function-call part when the conversation is continued. An OpenAI
Chat Completions or Anthropic Messages tool-use block has no corresponding
per-function-call signature field, which is why multi-turn Gemini tool calling
through those frontends previously failed with `Function call is missing a thought_signature in
functionCall parts.`

Kinetix keeps this state host-side instead of pushing it through the client:

- **Capture.** As a translated response streams, the Gemini adapter surfaces the
  signature on the normalized tool-call event. The pipeline captures it keyed by
  the *post-normalization* client-visible tool-call id (so generated ids work
  too), the provider, the exact originating model, the protocol family/producer,
  and the client scope (the virtual key id, or `internal` for keyless requests).
- **Storage.** Values are encrypted at rest with a cipher derived specifically
  for this subsystem (distinct from the credential and plugin-KV ciphers), and
  stored in the `opaque_provider_state` table. Only SHA-256 hashes of the scope,
  tool-call id, session id, and tool name are persisted; raw identifiers and raw
  signatures never are. A bounded RAM cache is written synchronously on the
  request path, while encryption, the SQLite UPSERT, and periodic pruning run on
  a bounded background worker: a slow or locked database can never stall the
  streaming tool-call event, the immediately following request never races
  persistence (the RAM entry is already present), and a saturated durability
  queue drops the write with a counter rather than blocking the response. A
  graceful shutdown flushes the queue after request draining, so a signature the
  client was already told was accepted survives a restart. The RAM cache's TTL
  (1h) is deliberately shorter than SQLite's (24h); when a RAM entry has expired
  it is evicted and the lookup falls through to SQLite instead of reporting
  `Missing`, so a continuation on a long-running process keeps working for the
  full 24h SQLite retention window rather than only the 1h RAM window.
- **Replay.** On the next request, tool-call parts whose signature slot is empty
  are looked up. A compatible value is restored onto the exact historical part
  before dispatch. An explicit client/canonical signature is never overwritten,
  and a missing/unknown id is never given an invented signature.
  For targets that explicitly declare Gemini-style opaque state, including
  model-scoped plugin producers, translated thinking blocks may accompany
  restored tool calls. Each signed block must match a compatible stored
  signature; unsigned summaries are allowed alongside compatible restored
  calls. Foreign or unverified signatures and unsupported redacted thinking
  still enter portability handling. A transport name alone never grants replay.
- **Scope and compatibility.** Replay is scoped to the originating virtual key.
  A stored value is only reused when the target's provider id, protocol family,
  producer, and **exact originating model** all match; the account may change
  (same-provider account failover stays compatible). The originating model is
  deliberately part of the identity: Google's `generateContent` contract only
  guarantees a signature is accepted by the model that produced it. A cross-model
  continuation is reported non-portable and translated with the documented
  placeholder (see below) rather than reusing the original signature. Reusing a
  tool-call id with a *different* tool name is rejected with HTTP 400 before any
  upstream request is sent, and a conflicting explicit session is refused. When
  a row exists for the exact target model, identity is validated against *that
  row* first, so another model's row that merely shares the tool-call id can
  never shadow it into a false non-portable classification; only when no
  exact-model row exists is the identity check widened to the remaining
  (cross-model) rows to decide portability. Those identity checks always run
  before a row is classified as non-portable, so a cross-model switch can never
  launder a reused tool-call id or a stranger's session into a placeholder.
- **Portability.** Stored state that the selected target cannot carry feeds the
  Route's existing `reject` / `strip_with_warning` portability policy exactly
  like inline client state — including when neither the provider nor the wire
  format crossed over (for example an OpenAI client whose earlier Gemini turn
  stored a signature is later routed to an OpenAI target). Compatible stored
  state is restored only after the portability decision, so a
  `strip_with_warning` boundary never deletes state that the chosen target can
  use, and a direct target with no Route refuses known non-portable state
  instead of silently dropping it.
- **OpenAI Chat reasoning replay.** An OpenAI Chat assistant message carrying
  `reasoning_content` / `reasoning_signature` sent to a same-provider OpenAI
  Chat target on the same-format passthrough path is forwarded verbatim. The
  upstream that produced the reasoning owns its replay semantics (some
  OpenAI-compatible providers require it during tool loops, others reject it),
  so Kinetix neither strips nor refuses it. Cross-format or cross-provider
  replay still follows the Route portability policy above. This is the
  reasoning replay column of the
  [compatibility matrix](generated/compatibility-matrix.md).
- **Cross-model continuation.** Exact-model scoping stops a real signature from
  being replayed onto a model that did not produce it, but leaving the
  historical `functionCall` unsigned would still fail the next `generateContent`
  call. When the target adapter declares a documented placeholder for
  non-portable state (the Gemini adapter returns the provider's
  `skip_thought_signature_validator` sentinel), a `strip_with_warning` Route
  substitutes that placeholder onto the specific incompatible historical call
  instead of stripping it, and reports the substitution in the
  `X-Kinetix-Warning` header. The same documented translation applies to a
  direct same-family switch with no Route policy (for example a deliberate
  Flash→Pro change): the adapter's placeholder is a protocol-valid
  substitute, so the request proceeds with a warning rather than being refused.
  The placeholder is **gated to the family that documents it**: the Gemini
  adapter returns the sentinel only for Gemini 3 model ids, because only that
  family is documented to validate the signature of a replayed function call.
  Gemini 2.5 and older treat the signature as optional and never documented the
  sentinel, so a continuation onto such a model is stripped and continues
  unsigned rather than receiving the Gemini 3 validator-bypass token. A later
  major family (for example `gemini-4-*`) is not assumed to inherit the Gemini 3
  contract either, and falls back to the same strip/reject portability path
  until provider documentation or capability metadata says otherwise.
  The placeholder
  is painted only onto calls the store knew about but the target cannot carry —
  an id that was never captured is still left untouched, so missing state is
  never invented. A `reject` Route still refuses before dispatch, and a direct
  target whose adapter declares no placeholder still refuses known non-portable
  state instead of dropping it.
- **Observability.** `GET /admin/metrics` exports
  `kinetix_opaque_state_entries`, `kinetix_opaque_state_captured_total`,
  `kinetix_opaque_state_replaced_total`,
  `kinetix_opaque_state_capture_dropped_total`,
  `kinetix_opaque_state_capture_storage_errors_total`, and
  `kinetix_opaque_state_lookups_total{outcome=...}`. These are counts and bucket
  sizes only; no signature, tool-call id, or session identifier is exported.
  Signatures never appear in logs, traces, the dashboard, or client responses.

Only adapters that explicitly opt in participate (native Gemini today). A plugin
adapter that happens to populate a signature is *not* assumed compatible, because
it could multiplex unrelated opaque-state protocols; blind replay across
adapters would be a correctness and security bug.
