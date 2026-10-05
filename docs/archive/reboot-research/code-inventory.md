# Inventory current code

I checked current code, including **Kinetix** **`ba4ee06`**, **kinetix-plugins** **`edf34c3`**, pi-free, 9router, and OmniRoute.

## Inventory verdict

|Area|Verdict|Why|
| ------------------------------------------| ---------| --------------------------------------------------------------------|
|Field/thinking/wire conformance tests|**KEEP**|Probably Kinetix’s strongest asset|
|WIT/plugin conformance/security|**KEEP**|Good explicit contract boundary|
|Canonical request/event model|**KEEP concept, REWORK model**|Better foundation than 9router/OmniRoute’s OpenAI-hub translation|
|Frontend decoders/encoders|**KEEP + refactor**|Correct separation|
|Same-format passthrough|**KEEP**|Essential compatibility escape hatch|
|`pre_dispatch.rs`|**KEEP**|Pure deterministic policy; good boundary|
|`attempt_budget.rs`|**KEEP**|Small, bounded, testable|
|`stream_outcome.rs`/ commit-point semantics|**KEEP**|Correct resilience primitive|
|pool/circuit/admission/telemetry|**KEEP + simplify**|Useful domain primitives|
|`opaque_state.rs`|**KEEP concept, refactor**|Needed for signatures/continuations|
|`Adapter`trait|**SPLIT**|Auth + codec + discovery + errors + state currently bundled|
|`pipeline.rs`|**REWRITE orchestration**|**11,855 lines / 455 KB**; too many policies reconverged|
|`admin.rs`|**REWRITE/SPLIT**|**1.1 MB single file**|
|`db.rs`|**SPLIT**|207 KB; persistence leaking across domains|
|thinking mapping|**REWRITE**|Ownership split core ↔ plugin|
|failure/retry/fallback model|**REWRITE**|One`FailureKind`insufficient for recovery decisions|
|schema policy|**KEEP engine, rewrite policy/defaults**|Current fail-closed behavior hurts real clients|
|Antigravity adapter|**SALVAGE behavior, rewrite structure**|~99 KB adapter + 124 KB`lib.rs`|
|Provider auth/OAuth implementations|**KEEP heavily**|Expensive proven interoperability work|
|duplicated/provider-specific core policy|**MOVE/DELETE**|Belongs in integration/plugin layer|

### Biggest thing **not** to throw away

Kinetix's contract suite:

`tests/field_contract.rs` explicitly requires every client semantic field to be `preserved | translated | consumed | rejected`, running through the real outbound path.

Keep:

- field-contract fixtures
- thinking-translation fixtures
- wire fixtures
- plugin request/response fixtures
- commit-point tests
- continuation tests
- credential conformance
- discovery conformance
- capability-security conformance

These should become the **v0.1.0 executable specification**.

---

## What the references teach

### pi-free → copy its **ownership discipline**

pi-free makes a useful distinction:

> same-model retry semantics ≠ switching-provider fallback semantics.

It deliberately waits until Pi's own retry lifecycle settles before fallback, avoiding competing recovery mechanisms. [GitHub](https://github.com/apmantza/pi-free/?utm_source=chatgpt.com)

Kinetix should formalize:

```
failure
  ↓
request-local repair
  ↓
same-connection retry
  ↓
credential refresh
  ↓
account rotation
  ↓
target/model fallback
  ↓
route fallback
```

Each stage gets an explicit disposition. No giant pipeline deciding everything.

Also copy pi-free's provider layout:

```
provider/
  auth
  models/discovery
  transport
  transforms
  errors
```

Not one giant adapter.

---

### 9router → use as **compatibility oracle**, not architecture

9router's translator does approximately:

```
source
  → OpenAI intermediate
  → target
```

with direct source→target translations added where the OpenAI pivot becomes lossy.

That explains why it handles lots of real clients, but I **wouldn't copy this architecture**. It inevitably accumulates pair-specific repairs.

9router is still valuable for:

- tool-history normalization
- thinking capture before translation
- thought-signature persistence
- provider/model-specific param filtering
- account fallback/error heuristics
- real-world schema quirks

Its signature store already scopes Gemini/Claude signatures by model family to prevent cross-family replay-the exact class of continuation problem Kinetix has encountered. 9router explicitly targets broad CLI compatibility and automatic fallback. [GitHub](https://github.com/greytuy/decolua-9router?utm_source=chatgpt.com)

**Use its tests/behavior as corpus. Don't copy its translator.**

---

### OmniRoute → best **behavioral corpus**, warning about architecture

OmniRoute has pushed compatibility much further:

- direct translators where hub translation is lossy
- provider/model param rules
- tool-schema sanitation
- reasoning replay/cache
- signature recovery
- structured error classification
- bounded transient retries
- huge amount of provider-specific compatibility

It advertises cross-format OpenAI/Claude/Gemini compatibility and evolved directly from 9router. [GitHub](https://github.com/devolkus/omniroute?utm_source=chatgpt.com)

But its current structure also demonstrates what Kinetix should avoid. `open-sse/handlers/chatCore/` now contains dozens upon dozens of special-purpose compatibility modules.

Use OmniRoute to answer:

>  **“What weird behavior must Kinetix support?”**

Not:

>  **“How should Kinetix be structured?”**

---

# Important architectural changes for v0.1

### 1. Preserve canonical translation

Do **not** switch to 9router's OpenAI pivot.

Instead evolve:

```
Client Protocol
      ↓
Protocol Decoder
      ↓
Canonical Semantic Request
      ↓
Target Capability Negotiation
      ↓
Provider Codec
      ↓
Upstream
```

Kinetix already has the beginnings of the better design.

But `InternalRequest` currently mixes:

```
typed fields
extra
raw_body
```

Make this explicit:

```
CanonicalRequest
├─ semantic fields
├─ reasoning intent
├─ tools
├─ continuation state
└─ protocol extensions

PassthroughRequest
└─ original wire document
```

Two deliberate lanes instead of a dual source of truth.

---

### 2. Replace `ThinkingLevel`

Current:

```
Off
Default
Minimal
Low
Medium
High
XHigh
Max
```

is too lossy.

Use something closer to:

```
ReasoningIntent
├─ enabled
├─ effort?
├─ budget_tokens?
├─ adaptive?
├─ summary?
└─ source_semantics
```

Then **provider codec owns encoding**.

Core must stop knowing that Gemini/Anthropic/OpenAI represent thinking differently.

---

### 3. Split `Adapter`

Instead of one trait:

```
Integration
├─ Codec
├─ Authentication
├─ ErrorClassifier
├─ ModelDiscovery
├─ ContinuationCodec
└─ CapabilityDescriptor
```

Built-ins and plugins implement the **same interfaces**.

That removes the current “built-in behavior vs plugin behavior” divergence.

---

### 4. Replace failure classification with recovery classification

Current `FailureKind` is useful but insufficient.

Make failure describe both **what happened** and **what can recover it**:

```
Failure
├─ kind
├─ scope
│   ├─ request
│   ├─ model
│   ├─ account
│   ├─ provider
│   └─ route
├─ retry
│   ├─ never
│   ├─ same_target
│   └─ after_delay
├─ credential_action
├─ fallback_allowed
└─ retry_after
```

This is the main lesson worth taking from **pi-free + 9router + OmniRoute**.

---

### 5. Change schema philosophy

Keep the new SDK schema engine.

Change policy.

Current idea:

```
unknown / unsupported → reject
```

is too hostile for a compatibility gateway.

v0.1 should use:

```
representable semantic feature → preserve/translate
known upstream incompatibility → normalize
safe compatibility constraint → consume + trace
semantic feature impossible to represent → reject
unknown extension:
    passthrough target → preserve
    restrictive target → target profile decides
```

So `maxLength`, `patternProperties`, tuple shapes, etc. stop randomly making entire coding agents unusable.

OmniRoute's sanitizer deliberately repairs schemas accepted by lenient clients but rejected by stricter upstreams; that practical behavior is worth copying, while retaining Kinetix's traceability.

---

# Final cut

I would reboot from the **existing repository**, not clean-sheet.

```
KEEP
  contracts/tests
  canonical architecture
  frontends
  commit semantics
  routing primitives
  plugin runtime/security
  auth/OAuth
  discovery work
  continuation-state concepts

REWRITE
  pipeline orchestration
  admin/control-plane decomposition
  adapter boundaries
  failure/recovery model
  reasoning model
  schema compatibility policy

MOVE
  provider quirks → integrations/plugins

DELETE
  duplicate policy
  provider-specific core knowledge
  accidental cross-layer compatibility hacks
```

**v0.1.0 should essentially be: Kinetix's contracts + Kinetix's good primitives + pi-free's ownership discipline + 9router/OmniRoute's compatibility corpus.**

That is a reboot worth doing.
