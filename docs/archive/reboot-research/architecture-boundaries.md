# Decide architecture boundaries

## Decision

Kinetix should use a **hybrid of all three**, but structurally closest to **OmniRoute**:

> **Core owns semantics + execution policy. Plugins own provider knowledge. Standard protocols stay in core. Proprietary protocols use plugin adapters.**

### What the references show

|Project|Good boundary|Copy?|
| ---------| --------------------------------------------------------------------------------------------------------------------------| -------|
|**pi-free**|Provider-specific auth/catalog/translation kept near provider; Pi retains runtime lifecycle|**Yes**|
|**9router**|Central translation layer + provider registry; default executor for standard providers, custom executor only when needed|**Yes**|
|**OmniRoute**|Provider registry/manifest is declarative; routing/policy/fallback stay core|**Best model**|
|**Kinetix today**|Canonical IR + core-owned HTTP/stream/fallback already correct|**Keep**|

pi-free notably lets Pi own its normal retry/run lifecycle; its fallback waits until `agent_settled` rather than competing with Pi's retry logic. Provider-specific transforms such as Qoder stay inside that provider implementation. [GitHub](https://github.com/apmantza/pi-free?utm_source=chatgpt.com)

9router uses a provider-agnostic engine with centralized translation and a default executor for ordinary providers; only non-standard providers receive custom executors. Its own docs also admit the `source → OpenAI → target` hub can be lossy for thinking, images, tool IDs, etc. [GitHub](https://github.com/decolua/9router/blob/master/tests/translator/AGENTS.md?utm_source=chatgpt.com)

OmniRoute goes further: its provider manifest exposes declarative provider metadata while intentionally keeping routing decisions, policy, secrets, dynamic execution and fallback in the core runtime. [GitHub](https://github.com/diegosouzapw/OmniRoute/wiki/Provider-Plugin-Manifest?utm_source=chatgpt.com)

---

# Kinetix v0.1 boundary

```
CLIENT
  ↓
Frontend Decoder                    CORE
  ↓
Canonical Request                   CORE
  ↓
Semantic/Compatibility Policy       CORE
  ↓
Route → Target → Account            CORE
  ↓
Execution Profile                   CORE
  ↓
┌─────────────────────────────────────────────┐
│ Standard protocol?                         │
│   OpenAI / Responses / Anthropic / Gemini  │
│        ↓                                    │
│   Core protocol adapter                    │
│                                             │
│ Proprietary protocol?                      │
│        ↓                                    │
│   Plugin adapter                           │
└─────────────────────────────────────────────┘
  ↓
HTTP / streaming / cancellation             CORE
  ↓
canonical events                            CORE
  ↓
Frontend Encoder                            CORE
```

## Core owns

**No plugin allowed to override these:**

- canonical request/event model
- frontend decoding/encoding
- route selection
- account selection
- retry/fallback
- circuit breaker
- commit point
- cancellation/backpressure
- HTTP execution
- SSE lifecycle
- quota interpretation
- health state
- credential storage
- credential refresh scheduling
- observed → accepted → runtime model state
- compatibility policy
- tool-schema normalization
- thinking intent
- standard protocol serialization
- error/failure taxonomy

This largely matches Kinetix's existing declared architecture. [Kinetix architecture](https://github.com/PrightCord/kinetix/blob/main/docs/ARCHITECTURE.md?utm_source=chatgpt.com)

---

## Plugins own

Provider-specific **facts/mechanics**, not gateway policy:

```
provider identity
endpoint discovery
model discovery
OAuth flow details
credential refresh request/response format
provider-specific headers
provider-specific model aliases
provider-specific capability observations
quota extraction
health observations
error classification evidence
proprietary request/response translation
```

They return **evidence**, never decisions:

```
Plugin:
429 + reset=12:00 + quota exhausted

Core:
→ mark account unavailable
→ retry?
→ fallback?
→ cooldown duration?
```

Your WIT already moves in this direction: plugin errors provide evidence while core chooses retry/fallback policy.

[Kinetix plugin contract](https://github.com/PrightCord/kinetix/blob/main/wit/kinetix-plugin.wit?utm_source=chatgpt.com)

---

# Biggest change I would make

## Move generic compatibility logic **out of** **`kinetix-plugins`**

Today you have things like:

```
kinetix-plugins/sdk/src/schema/
  policy.rs
  profiles/
  normalize.rs
  repair.rs
  transform.rs
```

This is the wrong ownership for v0.1.

It creates exactly the class of bugs you've been seeing:

```
Antigravity handles patternProperties one way
AI Studio another
future plugin another
core native Gemini another
```

Instead:

```
kinetix core
  compatibility/
    schema/
    tools/
    thinking/
    continuation/
```

Plugin declares:

```
schema_profile = "gemini"
schema_mode = "compatible"
thinking_profile = ...
continuation_profile = ...
```

Core performs the transformation.

### Exception

A truly proprietary protocol can implement custom transformation in the plugin.

Example:

```
ordinary Gemini API       → core Gemini adapter
AI Studio Gemini          → core Gemini adapter + plugin auth/discovery
Antigravity Gemini-ish    → plugin adapter only for actual Antigravity differences
Qoder proprietary API     → plugin adapter
```

This follows the strongest part of pi-free: Qoder-specific transformation lives with Qoder, rather than making every provider its own protocol implementation. [GitHub](https://github.com/apmantza/pi-free/blob/master/providers/qoder/transform.ts)

---

# Thinking

Same rule.

### Core

```
Client intent
    ↓
ThinkingIntent
    ↓
accepted model capability
    ↓
execution mapping
    ↓
provider wire shape
```

### Plugin

Reports:

```
supports reasoning
supported levels
adaptive/manual/budget capability
provider constraints
```

Plugin should **not** normally implement:

```
low → provider JSON
medium → provider JSON
high → provider JSON
```

for OpenAI/Anthropic/Gemini transports.

That belongs to their core protocol adapters.

Only proprietary thinking formats need plugin code.

---

# Tool schemas

Same:

```
Canonical JSON Schema
        ↓
Core compatibility profile
        ↓
transport-compatible schema
        ↓
plugin/provider
```

Not:

```
Canonical schema
 ↓
every plugin invents sanitizer
```

This is probably the single most important boundary to settle before v0.1.

---

# Provider definition should become declarative

Borrow OmniRoute's provider-manifest idea. [GitHub](https://github.com/diegosouzapw/OmniRoute/wiki/Provider-Plugin-Manifest?utm_source=chatgpt.com)

Something approximately:

```
ProviderDescriptor
├─ id
├─ transport
├─ endpoints
├─ auth
├─ discovery
├─ schema_profile
├─ thinking_profile
├─ continuation_profile
├─ capability observations
├─ error_profile
└─ proprietary_adapter? 
```

Then:

```
OpenAI-compatible BYOK provider
```

may require **zero adapter code**.

Just descriptor + auth + discovery.

This mirrors 9router's useful rule:

> standard provider → default executor  
> weird provider → specialized executor [GitHub](https://github.com/decolua/9router/blob/master/open-sse/AGENTS.md?utm_source=chatgpt.com)

---

# One thing NOT to copy from 9router

Do **not** adopt:

```
source → OpenAI → target
```

as the universal translation architecture.

9router itself documents this as lossy for thinking, image representations and tool information. [GitHub](https://github.com/decolua/9router/blob/master/tests/translator/AGENTS.md?utm_source=chatgpt.com)

Kinetix's:

```
frontend
   ↓
canonical semantic IR
   ↓
target protocol
```

is architecturally better.

Keep it.

---

# Final boundary

I would lock this ADR before reboot:

> **Kinetix core owns meaning, policy and execution.**
>
> **Plugins describe providers and implement only behavior that is genuinely provider-specific.**
>
> **Wire format ≠ provider.**
>
> **OpenAI/Responses/Anthropic/Gemini are core protocols, not plugin implementations.**
>
> **Plugins cannot route, retry, fallback, interpret health, or redefine canonical semantics.**
>
> **Generic schema/thinking/tool/continuation compatibility exists exactly once in core.**
>
> **Provider-specific exceptions are declarative where possible; executable plugin code only where necessary.**

That should be the **first architecture decision frozen before Kinetix/kinetix-plugins v0.1.0**.
