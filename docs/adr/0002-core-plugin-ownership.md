# ADR-0002: Core owns semantics and recovery policy; integrations supply provider facts

## Status

Accepted

## Context

The same compatibility rule is implemented in more than one place:

- Gemini tool schemas pass through core `sanitize_schema`
  (`src/adapters/gemini.rs:446`) for the built-in Gemini adapter, and through
  the SDK `schema` module for `antigravity-oauth` and `opencode-free`. The two
  apply different policies (core keeps `maxLength`; the SDK compatible mode
  drops it).
- Thinking translation is core-owned for built-in adapters but can be
  plugin-owned via `handles_thinking_translation`.
- The `Adapter` trait (`src/adapters/mod.rs:593`) bundles wire format, URL,
  auth, body encoding, thinking ownership, token counting, and response
  parsing. AI Studio reuses the Gemini wire format with its own auth, but the
  trait cannot express "same codec, different auth".

## Decision

Kinetix core owns, and no integration may override:

- canonical request/event model, frontends, and same-format passthrough;
- Route, Target, and Account selection; retry, fallback, cooldown, commit
  point;
- failure classification into recovery decisions (ADR-0004);
- tool-schema compatibility (ADR-0003), thinking intent mapping, and
  continuation portability;
- codecs for standard wire formats: OpenAI Chat Completions, OpenAI
  Responses, Anthropic Messages, Gemini;
- HTTP execution, streaming, cancellation, credential storage and refresh
  scheduling.

Integrations (built-in or plugin) supply provider facts through three seams
plus a declarative descriptor:

| Seam | Owns | Varies independently because |
| --- | --- | --- |
| Codec | request encoding, response/stream decoding, failure evidence extraction, opaque-state capture | one per wire format; proprietary formats only in plugins |
| Auth | credential enrollment, request signing, refresh request/response | AI Studio and Claude Code OAuth reuse standard codecs with custom auth |
| Discovery | model listing and capability observations | independent of codec and auth |

The descriptor is data: wire format, endpoints, schema profile, thinking map,
continuation family, error profile. An OpenAI-compatible API-key provider
needs a descriptor and no code.

Built-in integrations and plugins implement the same seams. Plugins return
evidence (status, provider error code, reset time), never decisions.

## Alternatives considered

- **Keep one `Adapter` trait.** Keeps auth coupled to codec, so a standard
  wire format with custom auth duplicates the codec in a plugin.
- **Separate traits for continuation, error classification, capabilities.**
  Continuation capture is tied to the wire format and belongs in the codec.
  Error *classification* is core policy; the codec only extracts evidence.
  Capabilities are data. Separate traits would be pass-throughs.
- **Generic compatibility in the plugin SDK.** Every plugin links its own copy,
  versions drift, and core's built-in adapters still need the same logic.

## Consequences

### Positive

- Each compatibility rule has one implementation, tested once by the
  conformance corpus against native and plugin paths.
- Antigravity's plugin shrinks to its real protocol differences.

### Negative / trade-offs

- WIT contract change: plugins can no longer own thinking translation or
  schema sanitization for standard wire formats.
- The SDK `schema` module is removed. Its tested behavior moves into core.
