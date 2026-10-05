# Kinetix 0.1.0 reboot

Status: draft plan; ADRs 0001-0005 are accepted. Once 0.1.0 ships, move this
and [docs/reboot/](reboot/) to `docs/archive/`. Durable decisions live in [docs/adr/](adr/). Earlier
research notes are in [archive/reboot-research](archive/reboot-research/index.md).
Domain terms are in [CONTEXT.md](../CONTEXT.md).

The reboot replaces public contracts and keeps proven code. Breaking changes
are allowed. Kinetix and kinetix-plugins restart together at 0.1.0
([ADR-0001](adr/0001-reboot-versioning.md)).

## Problem

Coding agents (Pi, Claude Code, Codex CLI, OpenCode) hit gaps that the
existing fixture suite does not catch, because the fixtures prove individual
pieces rather than whole transactions. Evidence at `ba4ee06`:

- **Responses.** `previous_response_id`, `store`, `include`, structured
  `text.format`, and reasoning output items are rejected
  ([compatibility.md](compatibility.md)). The rejection also applies on
  same-format passthrough to `openai-responses` Targets
  (`src/passthrough.rs:33`), which forces upstream `store: false`. Codex CLI
  needs `include: ["reasoning.encrypted_content"]` and reasoning-item replay.
- **Tool schemas.** Gemini schemas go through two engines with different
  policies: core `sanitize_schema` (`src/adapters/gemini.rs:446`) and the SDK
  `schema` module used by `antigravity-oauth` and `opencode-free`. The same
  schema succeeds or fails depending on the integration.
- **Thinking.** The Anthropic frontend buckets `budget_tokens` into
  Low/Medium/High at 2048/8192 (`src/frontends/anthropic.rs:122`). No
  provider documents those thresholds, and the exact budget is lost.
- **Failure recovery.** `FailureKind` records what happened. Recovery is
  derived at 175 match sites in `src/pipeline.rs`.
- **Size.** `pipeline.rs` is 11,854 lines, `admin.rs` 31,123, and `db.rs`
  6,057. A change to one policy means reading code for many others.

## Goals

- Pi, Claude Code, Codex CLI, and OpenCode work against OpenAI, Anthropic,
  and Gemini Targets, proven by the conformance corpus (release gate below).
- Each compatibility policy (schema, thinking, continuation, failure
  recovery) has exactly one owner in core ([ADR-0002](adr/0002-core-plugin-ownership.md)).
- Native and plugin integrations pass the same corpus.

## Non-goals

- A clean-sheet rewrite.
- Feature work on `legacy/v0.6`.
- Translating through an OpenAI hub format. The canonical IR stays.
- Differential replay against 9router/OmniRoute as a release gate.
- Provider breadth beyond what the gate requires.

## Design

### Request lanes

`InternalRequest` (`src/types.rs:467`) mixes typed fields, an `extra` map,
and `raw_body`, so translated and passthrough requests share two sources of
truth. Split these into two explicit lanes:

- **Passthrough:** the original wire document for same-format Targets.
  Unknown fields survive.
- **Canonical:** typed semantics for translated Targets. Every client field
  still needs a field-contract disposition (preserved, translated, consumed,
  or rejected).

### Streaming

Streaming is the primary path. Ownership:

| Stage | Owner |
| --- | --- |
| upstream bytes -> SSE/JSON frames, UTF-8 reassembly | core |
| frames -> canonical events, plus failure evidence and opaque-state capture | Codec |
| canonical event ordering (thinking, text, tool calls), usage, terminal event | core |
| canonical events -> client wire framing | frontend encoder |
| commit point, cancellation, backpressure, bounded buffers | core |

On the passthrough lane, native frames go to the client unchanged instead of
through canonical events and the frontend encoder. Core still owns framing,
the commit point, and the terminal error event; the Codec still extracts
usage and failure evidence from the native frames.

The commit point is unchanged: the moment the response becomes
client-visible. Before it, a failure goes through recovery (ADR-0004). After it, the stream ends with a client-visible
error event in the client's protocol and is never retried. The canonical
request and event types are versioned together with the WIT contract, since
plugin Codecs emit canonical events.

### Thinking

Replace `ThinkingLevel` as the client intent with:

```text
ThinkingIntent = Absent | Off | Level(minimal|low|medium|high|xhigh|max)
               | Budget(tokens) | Adaptive(level?)
```

`ThinkingMap` stays the per-model capability. Core codecs map intent to exact
wire JSON for the standard wire formats. Plugins map intent only for
proprietary formats. A downgrade or budget-to-level conversion happens only
when the model's map declares it. Anything else is rejected. This removes
the 2048/8192 bucketing.

`ThinkingIntent` says whether and how much the model reasons. Whether and how
that reasoning is shown to the client (Responses `reasoning.summary`) is a
separate canonical field, `ReasoningSummary`, mapped or rejected on its own.
Coupling the two loses visible reasoning or breaks replay of reasoning items
(OmniRoute #11108, #10166).

### Tool schemas

One core engine with `strict`/`compatible` modes and classified transforms.
The mode is Target-owned: the Provider supplies the default, the Target may
narrow or override it, and a Route never sets it. See [ADR-0003](adr/0003-tool-schema-compatibility.md).

### Continuation

```text
ContinuationState = Portable(data)
                  | Opaque { provider_family, model_family?, account_scope?, value }
```

Portable state may survive fallback. Opaque state replays only where its
provenance permits, and every opaque type has explicit rules for same
Account, same Provider/other Account, same wire format/other Provider,
cross-model, and cross-format. Portability is never inferred from wire
format. This builds on the existing `opaque_state.rs` and continuation
families.

Responses `previous_response_id` is upstream-owned and Kinetix retains no
transcripts by default ([ADR-0005](adr/0005-responses-continuation-retention.md)):

```text
/v1/responses + previous_response_id
  native Responses Target, same Account -> passthrough
  any other Target                      -> explicit rejection
```

Responses reasoning items carrying `encrypted_content` (requested with
`include: ["reasoning.encrypted_content"]`, the stateless `store: false` mode
Codex CLI uses) are opaque state. They are forwarded on native Responses
Targets and rejected elsewhere. Their account and model scope must be
confirmed from observed upstream behavior before step 6.

`store: false` means Kinetix persists no request or response content,
including body logs. Usage and accounting records are still written.

### Failure recovery

Core maps evidence to `Failure { class, retry_scope, ... }`. See
[ADR-0004](adr/0004-failure-recovery-model.md).

### Integration seams

The `Adapter` trait splits into Codec, Auth, and Discovery plus a
declarative descriptor. Built-ins and plugins implement the same seams. See
[ADR-0002](adr/0002-core-plugin-ownership.md).

### Code disposition

| Area | Disposition |
| --- | --- |
| field/thinking/wire fixtures, WIT conformance, credential/discovery/capability-security conformance | keep, become the executable spec |
| frontends, same-format passthrough, `pre_dispatch.rs`, `attempt_budget.rs`, `stream_outcome.rs` | keep |
| pool, circuit, admission, telemetry | keep, consume `Failure` instead of `FailureKind` |
| `opaque_state.rs`, model state (observed/accepted/runtime) | keep, extend accepted state with schema/thinking/continuation/error profiles |
| provider auth and OAuth implementations | keep |
| `Adapter` trait | split (ADR-0002) |
| `pipeline.rs` | extract the recovery mapping first, then the attempt loop |
| `admin.rs`, `db.rs` | split by domain (Providers, Accounts, Routes, Models, plugins) with no behavior change |
| core `sanitize_schema` + SDK `schema` module | merge into one core engine (ADR-0003) |
| Antigravity plugin | keep only real protocol differences |
| provider-specific logic in core without a contract | delete |

## Conformance corpus

Extend `tests/fixtures/` rather than adding parallel `contracts/` or
`conformance/` trees. Each case pins the whole observable transaction:

```text
client request
-> expected canonical request
-> exact upstream request
-> fixture upstream response / SSE
-> exact client response / SSE
-> expected usage and recovery outcome
```

One data-driven runner executes every case against the native and plugin
integrations for the Target. Plugin parity uses `b-ai` (OpenAI Chat),
`claude-code-oauth` (Anthropic), and `ai-studio` (Gemini).

Cases not yet passing are listed in an expected-failure manifest so
`scripts/run-ci.sh` stays green. A listed case that passes also fails CI, so
the manifest only shrinks. Each Sequence step removes the entries it fixes.

Required case groups:

- **Text:** roles, mixed and empty content, sampling params, metadata.
- **Tools:** single, parallel, results, errored results, malformed args or
  history, `tool_choice` variants.
- **Schemas:** the existing real-world corpus (k-jev, Chrome DevTools,
  Claude Code, Codex shell, Zod/TypeBox, `$ref`, tuples, unions) under both
  schema modes.
- **Thinking:** every `ThinkingIntent` variant, pinned to exact upstream
  JSON or a rejection.
- **Continuation:** Gemini thought signatures, Anthropic thinking blocks,
  `previous_response_id`, Responses encrypted reasoning-item replay,
  tool-call continuation, and rejection of non-portable state.
- **Streaming:** arbitrary chunk boundaries, split UTF-8, multiple events per
  chunk, usage events, thinking/text/tool ordering, `[DONE]`, early close,
  error before and after commit.
- **Failures:** 400, 401 refreshable vs revoked, 403, 429 rate vs quota, 402,
  5xx, timeout before commit, failure after commit.
- **Client captures:** real request payloads from Pi, Claude Code, Codex
  CLI, and OpenCode.
- **Reference regressions:** every case in the table below.

### Reference regressions

Each bug shipped in 9router or OmniRoute becomes a Kinetix case. All must
exist before any reboot implementation starts (Sequence step 2).

| Source | Required behavior |
| --- | --- |
| 9router #2896 | Chat `json_schema` -> Responses preserves `strict`, `schema`, and `name`; non-stream response is reconstructed correctly |
| 9router #2311, #2610 | unsupported client fields such as `client_metadata` are translated, dropped by explicit policy, or rejected; never blindly forwarded |
| 9router #2611 | reasoning and tool-call output items get unique `output_index` |
| 9router #3234 | `output_text.done` is emitted only after all deltas |
| 9router #2778 | changing a Target's wire API changes runtime routing; no stale Chat/Responses binding |
| OmniRoute #11108 | a replayed reasoning item retains or normalizes its required summary |
| OmniRoute #10166 | streaming and non-streaming reasoning summaries survive translation |
| OmniRoute #13122 | Responses custom tools, raw input, `custom_tool_call`/`custom_tool_call_output`, and stable `call_id` round-trip losslessly |
| OmniRoute #8145 | namespaced tool identity survives request -> provider -> response |
| OmniRoute #11856 | malformed or unsupported JSON Schema against strict providers |
| OmniRoute #9356, #7215 | reasoning and `tool_choice: required` are translated or explicitly rejected, never silently ignored or stripped |
| OmniRoute #9545, #14651 | Target protocol selection respects model capabilities and aliases; Responses requests never down-convert to an incompatible Chat transport |

The existing contract docs ([field-contract.md](field-contract.md),
[thinking-contract.md](thinking-contract.md),
[PLUGIN-RESPONSE-CONTRACT.md](PLUGIN-RESPONSE-CONTRACT.md),
[protocol-v1-compatibility.md](protocol-v1-compatibility.md)) get updated to
the new contracts. No parallel `*-v1.md` set.

An optional `--live` mode replays safe cases against real providers. CI does
not depend on it.

## Release gate

0.1.0 ships only when the corpus passes:

| Client | Required |
| --- | --- |
| Pi | text, tools, thinking, streaming |
| Claude Code | text, tools, thinking, streaming |
| Codex CLI | Responses, tools, reasoning, streaming; encrypted reasoning-item replay and `previous_response_id` on native Responses Targets |
| OpenCode | text, tools, reasoning, streaming |

The corpus must pass against OpenAI, Anthropic, and Gemini Targets, with
native/plugin parity and the full schema, continuation, and failure groups.

## Sequence

Tickets live in [docs/reboot/](reboot/); each lists its blockers.

1. Cut `legacy/v0.6` in both repos ([001](reboot/001-legacy-branch.md)).
2. Restructure fixtures into the transaction corpus, add client captures and
   every reference regression. Failing cases go into the expected-failure
   manifest. No implementation step starts before this is done
   ([002](reboot/002-corpus-runner.md)-[011](reboot/011-reference-regressions.md)).
3. Failure recovery mapping (ADR-0004). This is the largest single
   simplification of `pipeline.rs`
   ([013](reboot/013-failure-recovery-mapping.md),
   [014](reboot/014-pipeline-attempt-loop.md)).
4. Schema engine consolidation (ADR-0003). Built-in adapters move to the
   core engine. The SDK `schema` module stays until step 7, because plugins
   cannot hand schema handling to core before the WIT change
   ([015](reboot/015-schema-engine.md)).
5. `ThinkingIntent` ([016](reboot/016-thinking-intent.md)).
6. Request lanes, `ReasoningSummary`, upstream-owned
   `previous_response_id`, encrypted reasoning-item replay, and the body-log
   `store` check (ADR-0005)
   ([012](reboot/012-responses-upstream-observation.md),
   [017](reboot/017-request-lanes.md)-[020](reboot/020-store-false-no-content.md)).
7. Integration seam split (ADR-0002) with matching WIT changes, mirrored to
   kinetix-plugins. Remove the SDK `schema` module
   ([021](reboot/021-integration-seams.md)-[023](reboot/023-antigravity-reduce.md),
   then [025](reboot/025-delete-uncontracted-provider-logic.md)).
8. Gate green with an empty expected-failure manifest, then delete old
   releases and publish 0.1.0 (ADR-0001)
   ([027](reboot/027-release-0-1-0.md)).

The `admin.rs`/`db.rs` split ([024](reboot/024-admin-db-split.md)) runs in
parallel after step 2. `--live` replay ([026](reboot/026-live-replay.md)) is
optional and outside the gate.

## Risks

- **Existing installs strand on 0.6.x.** Self-update, `install.sh`, and
  plugin catalog URLs break when releases are deleted
  ([ADR-0001](adr/0001-reboot-versioning.md)).
- **Silent schema weakening.** `compatible` mode weakens validation. Each
  weakening is recorded in the Route Trace, and `strict` remains available.
- **Responses clients on translated Targets.** `previous_response_id` and
  encrypted reasoning items are rejected there (ADR-0005). Clients that
  resend plain history are unaffected, but a Codex session cannot fall back
  from a native Responses Target to a translated one mid-conversation.
- **Scope creep.** Every feature outside the gate waits for 0.1.0.

## Reference projects

Implementation references only, not authority (see `AGENTS.md`):

- **pi-free:** the provider boundary. Providers describe their differences
  and the host owns execution.
- **9router:** a source of real-world corpus cases. Its OpenAI-hub
  translation is not a model to copy. It does not store prompts or
  responses.
- **OmniRoute:** a failure taxonomy that separates credits, rate limits, and
  auth. Its `previous_response_id` virtualization is not adopted (ADR-0005).
