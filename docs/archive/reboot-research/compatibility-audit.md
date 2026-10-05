# Compatibility audit

**Verdict:**  Kinetix should reboot, but **do not throw away its compatibility architecture**. Current Kinetix is stricter and more principled than the references in several areas. The main problem is **missing compatibility coverage + wrong policy boundaries**, not that everything is fundamentally bad.

### Comparison

|Area|Kinetix current|pi-free|9router|OmniRoute|0.1.0 decision|
| ---------------------------| ----------------------------------------| ------------------------------| --------------------------------| -------------------------| ----------------------------|
|Same-format passthrough|**Good**|delegates to pi-ai|Good|Good|**Keep**|
|Cross-format translation|Explicit canonical contract|Mostly avoids it|Huge translator layer|Huge translator layer|**Keep canonical, shrink scope**|
|Per-model wire protocol|Good concept|**Excellent** `model.api`/`apiForModel`|Mixed capability/profile logic|Provider/model profiles|**Copy pi-free principle**|
|Tool-schema compatibility|Improving; Antigravity compatible mode|Mostly delegates|**Very tolerant**|**Very tolerant/coercive**|**Needs shared schema policy**|
|Gemini signatures|**Strong**|underlying provider handling|Historically fragile|**Strong cache/replay**|**Keep Kinetix design**|
|Thinking translation|Explicit mappings, fail closed|delegates to native API|Broad but heuristic bugs|Broad/profile-driven|**Keep explicit mappings**|
|OpenAI Responses|**Major gap**|native pi-ai transport|Supported but bugs exist|**Strongest**|**P0 rewrite/finish**|
|`previous_response_id`|**Rejected**|native transport dependent|partial/provider-dependent|**Virtualized server-side**|**P0**|
|Streaming|Good contract/matrix|native pi-ai|broad|very mature|Keep, simplify readers|
|Error classification|Improving|simple/retry-oriented|large provider rules|**Very mature**|**P0 typed taxonomy**|
|Fallback safety|Explicit|conservative|broad|sophisticated|Preserve Kinetix semantics|
|Model discovery|**Strong accepted/runtime separation**|native Provider lifecycle|heuristic-heavy|broad registry|**Keep Kinetix**|
|Compatibility tests|**Strong deterministic matrix**|provider tests|huge regression corpus|huge regression corpus|Expand, don't replace|

---

## What each reference should teach Kinetix

### pi-free - copy its **boundary design**

This is probably the most important reference for the reboot.

Current pi-free increasingly avoids owning protocol semantics itself. Providers register using Pi's native `Provider` abstraction, models carry their actual runtime `api`, and `apiForModel` allows different models behind one provider to use different transports. Standard streaming goes through pi-ai; provider code adds only narrow request/response hooks.

Example from current `apmantza/pi-free/lib/native-provider.ts`:

```
apiForModel(modelId)
       ↓
model.api
       ↓
pi-ai's native transport implementation
```

This is the right principle for **kinetix-plugins**:

> plugin describes provider differences; core owns generic execution.

Do **not** make every plugin implement another complete OpenAI↔Anthropic↔Gemini translator.

---

### 9router - copy its **compatibility corpus**, not its architecture

9router handles an enormous number of weird real-world payloads. Its Gemini sanitizer explicitly handles/removes unsupported JSON-Schema constructs, and recent releases added things such as `prefixItems` conversion and array-schema normalization. [GitHub](https://github.com/decolua/9router/blob/master/CHANGELOG.md?utm_source=chatgpt.com)

This explains why tools frequently "just work" through 9router even when their schema isn't valid Gemini schema.

But its architecture is also evidence of what **not** to reproduce. Current/recent bugs include:

- wrong Responses reasoning shape: `reasoning_effort` instead of `reasoning.effort`; [GitHub](https://github.com/decolua/9router/issues/3154?utm_source=chatgpt.com)
- model/profile matching silently dropping thinking controls; [GitHub](https://github.com/decolua/9router/issues/2690?utm_source=chatgpt.com)
- provider-specific thinking mappings producing unsupported levels. [GitHub](https://github.com/decolua/9router/issues/3939?utm_source=chatgpt.com)

So:

> **Steal 9router's fixtures and edge cases. Don't steal its heuristic translation model.**

---

### OmniRoute - copy its **state/resilience semantics**

OmniRoute has the best idea I found for Responses compatibility: it virtualizes `previous_response_id`.

Its current implementation stores the completed response/transcript and, when a later request references that response ID, reconstructs the conversation **before provider translation**. Therefore a Responses client can retain Responses semantics even if the selected upstream doesn't natively support OpenAI Responses.

That directly addresses Kinetix's largest current hole.

OmniRoute also distinguishes:

- credential invalidity;
- refreshable auth failures;
- exhausted credits;
- rate limiting;
- model lockout;
- provider failure;
- connection cooldown.

That is substantially safer than treating every `401/403/429/5xx` as essentially the same routing failure.

Its scale is not desirable for Kinetix, though. Copy the **state machine**, not the product breadth.

---

# Critical Kinetix findings

Current `PrightCord/kinetix` HEAD I inspected is `de1143f`.

### P0 - Responses is not actually compatible enough

Kinetix explicitly documents:

> no native Responses upstream passthrough or Responses object store

and rejects:

`previous_response_id`, storage, background mode, hosted tools, `include`, structured `text.format`, reasoning output items, etc.

That is fine for a documented subset, but **not fine if 0.1.0's goal is transparent compatibility with modern coding agents**.

OmniRoute's continuation virtualization is the model to follow.

**0.1.0 minimum:**

```
/v1/responses
  ├─ native Responses target → passthrough
  └─ translated target
       ├─ synthesize response ID
       ├─ persist portable continuation
       └─ previous_response_id → rehydrate before translation
```

No native Responses passthrough today is especially important. 9router itself has run into endpoint-specific Responses-vs-Chat reasoning differences, demonstrating why collapsing the two protocols is unsafe. [GitHub](https://github.com/decolua/9router/issues/3154?utm_source=chatgpt.com)

---

### P0 - Formalize schema compatibility as a core/plugin contract

This is already **partly fixed** since your earlier Antigravity failures.

Current `kinetix-plugins` Antigravity code now has:

```
strict      → reject unsupported semantics
compatible  → normalize safely
```

The compatible path already:

- recursively drops `maxLength`;
- widens tuple `items`;
- translates `prefixItems`;
- removes incompatible tuple constraints;
- retains supported constraints such as `pattern`.

That's the right direction.

But it needs to become a **shared SDK contract**, rather than Antigravity-specific behavior.

The key distinction should be:

|Field class|Action|
| ------------------------------------| --------------------------------------------|
|Semantic field|preserve / translate / reject|
|Validation-only constraint|safely weaken/drop if target can't express|
|Structurally equivalent schema|rewrite|
|Opaque provider state|preserve/replay or reject|
|Unknown potentially-semantic field|reject|

This resolves the old false dichotomy of:

> "Never silently drop anything" vs "make Chrome DevTools work."

Dropping `maxLength: 2000` from a function declaration does **not** change what the LLM is being asked to do in the same way dropping `tool_choice` or a thinking level does.

---

### P0 - Typed failure taxonomy before fallback

Make adapters/plugins return something roughly equivalent to:

```
bad_request
unsupported_capability
auth_invalid
auth_refreshable
quota_exhausted
rate_limited
capacity_exhausted
model_unavailable
provider_unavailable
timeout
network
protocol_error
```

Then routing policy decides:

```
retry same account?
rotate account?
fallback target?
cooldown?
disable credential?
return immediately?
```

OmniRoute explicitly differentiates exhausted credits from ordinary 429 rate limiting and permanent account failure from refreshable authentication failure. That's exactly the distinction Kinetix previously struggled with.

---

### P0 - Exact wire format must be model state, never inferred

Kinetix's new **observed → accepted → runtime** state is actually better than 9router's heuristic-heavy approach.

Keep it.

Extend the accepted compatibility envelope to make these authoritative:

```
wire_format
schema_profile
thinking_profile
tool_profile
continuation_family
stream_profile
error_profile
```

A model called `deepseek-v4-flash` must not cause Kinetix to infer DeepSeek semantics from its name. 9router's current issues show why that approach eventually breaks. [GitHub](https://github.com/decolua/9router/issues/3939?utm_source=chatgpt.com)

---

### P1 - Preserve Kinetix's opaque-state system

**Do not rewrite this away.**

Kinetix's current approach is one of its strongest pieces:

```
Gemini thoughtSignature
        ↓
captured against tool call
        ↓
same compatible continuation → replay
cross-model compatible Gemini → documented placeholder
foreign/incompatible target → portability policy
```

The broader ecosystem repeatedly breaks Gemini multi-turn tools because `thought_signature` disappears. [GitHub](https://github.com/JetBrains/koog/issues/2113?utm_source=chatgpt.com)

9router historically hit exactly this problem too. [GitHub](https://github.com/decolua/9router/issues/93?utm_source=chatgpt.com)

Keep Kinetix's explicit `continuation_families` rather than simply saying "both are Gemini, probably okay."

---

## What I'd require before `0.1.0`

1. **Protocol contract**

   - Chat Completions
   - Responses
   - Anthropic Messages
   - Gemini/provider native
   - exact same-format vs translated behavior
2. **Shared compatibility profiles**

   - schema
   - thinking
   - tools
   - continuation
   - streaming
   - errors
3. **Responses continuation store**

   - native passthrough
   - `previous_response_id` virtualization
4. **Typed error/fallback state machine**
5. **Cross-project fixture corpus**

   - import/reimplement every useful 9router/OmniRoute/pi-free regression:
   - weird JSON Schema
   - tool round trips
   - Gemini signatures
   - reasoning levels
   - Responses state
   - stream terminal events
   - malformed SSE
   - quota/auth/fallback
6. **Golden rule**

   ```
   Same format → preserve.
   Equivalent representation → translate.
   Safe validation weakening → normalize.
   Semantic loss → reject.
   Opaque state → replay only when compatibility proven.
   ```

### Bottom line

**Don't reboot because Kinetix's architecture is all wrong.**

Reboot because the contract should become:

> **pi-free's clean provider boundaries + Kinetix's explicit semantics/state + 9router's compatibility corpus + OmniRoute's continuation/resilience model.**

That is the architecture I would freeze **before writing Kinetix 0.1.0**.


