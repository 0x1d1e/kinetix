# Write the contracts

## What “write the contracts” should mean for Kinetix 0.1.0

After comparing **pi-free, 9router, OmniRoute, and current Kinetix**, I’d freeze **6 contracts** before reboot.

|Contract|Best reference|Kinetix 0.1.0 direction|
| ----------------------------| ----------------| ----------------------------------|
|Canonical request/response|**Kinetix + pi-free**|versioned semantic IR|
|Thinking|**current Kinetix + OmniRoute**|explicit intent + capability map|
|Tool schema|**9router/OmniRoute lessons**|strict + compat policy|
|Continuation state|**current Kinetix + OmniRoute**|portable vs opaque|
|Error/fallback|**OmniRoute**|failure class + retry scope|
|Plugin ABI/capabilities|**pi-free + current Kinetix**|thin provider boundary|

### 1. Canonical request/response contract - **KEEP Kinetix architecture**

pi-free does something useful: providers implement Pi's native `Provider` contract while Pi owns model storage, credentials, refresh lifecycle, etc. [GitHub](https://github.com/apmantza/pi-free/?utm_source=chatgpt.com)

9router/OmniRoute instead often translate through an OpenAI-ish pivot format. It works, but creates format leakage and many special cases.

Kinetix's existing:

```
Frontend
  ↓
Canonical Request
  ↓
Adapter

Adapter
  ↓
Canonical Events
  ↓
Frontend
```

is the better base.

For 0.1.0, make these **actual versioned contracts**:

```
CanonicalRequestV1
CanonicalEventV1
CanonicalFailureV1
CanonicalCapabilitiesV1
```

Not merely Rust structs.

Every semantic value must carry enough information to distinguish:

```
portable
provider-bound
opaque
unsupported
```

Your current field-disposition contract is already strong and should survive the reboot essentially intact. [Kinetix field contract](https://github.com/PrightCord/kinetix/blob/main/docs/field-contract.md?utm_source=chatgpt.com)

---

### 2. Thinking contract - **mostly already solved**

pi-free uses Pi's model-level:

```
reasoning
thinkingLevelMap
```

where unsupported levels can explicitly be `null`. [GitHub](https://github.com/geekjapan/pi/blob/main/packages/coding-agent/docs/custom-provider.md?utm_source=chatgpt.com)

9router uses provider/model-specific supported-level matrices and normalization. Its recent GPT-5.6 work explicitly scopes Max/Ultra by **provider + model**, rather than assuming the same model name means the same capability everywhere. [GitHub](https://github.com/decolua/9router/blob/master/docs/superpowers/plans/2026-08-02-gpt-5-6-codex-reasoning-overrides.md?utm_source=chatgpt.com)

OmniRoute goes further with a canonical provider-neutral effort vocabulary and maps that to Anthropic/Gemini/OpenAI forms. [GitHub](https://github.com/diegosouzapw/OmniRoute/discussions/7343?utm_source=chatgpt.com)

### Kinetix contract

Keep:

```
client syntax
    ↓
ThinkingIntent
    ↓
model capability
    ↓
exact provider wire value
```

I would define:

```
ThinkingIntent =
  Absent
  Off
  Level(minimal|low|medium|high|xhigh|max|ultra)
  Budget(tokens)
  Adaptive(level?)
```

Then require an **executable per-model map**.

Important:

> No implicit downgrade unless explicitly declared by that model's policy.

9router/OmniRoute frequently clamp unsupported values; that increases compatibility but can silently change intent. An actual gateway contract should make this explicit. 9router has already had bugs where its fallback produced a reasoning tier the target did not support. [GitHub](https://github.com/decolua/9router/issues/4149?utm_source=chatgpt.com)

Current Kinetix's full-wire golden approach is better:

```
client intent → canonical → exact provider JSON OR rejection
```

Keep it. [Current Kinetix thinking contract](https://github.com/PrightCord/kinetix/blob/main/docs/thinking-contract.md?utm_source=chatgpt.com)

---

### 3. Tool-schema contract - **this needs the biggest change**

9router/OmniRoute prioritize interoperability:

```
client JSON Schema
→ provider-specific sanitizer
→ Gemini-compatible schema
```

They remove unsupported keywords, normalize names, ensure object roots, convert tuple/schema structures, etc.

That explains why tools often work there when Kinetix rejects them.

But it has a cost.

Recent OmniRoute bugs show exactly why blindly sanitizing is dangerous:

- `prefixItems` reached Gemini → hard 400; fix proposed stripping/degrading tuple validation. [GitHub](https://github.com/diegosouzapw/OmniRoute/issues/12871?utm_source=chatgpt.com)
- nullable schemas were flattened and changed meaning, causing `"null"` strings or fabricated values. [GitHub](https://github.com/diegosouzapw/OmniRoute/issues/12308?utm_source=chatgpt.com)
- schema traversal itself corrupted a valid property literally named `properties`. [GitHub](https://github.com/diegosouzapw/OmniRoute/issues/13057?utm_source=chatgpt.com)

So don't copy their sanitizer wholesale.

### Kinetix should define

```
schema_policy:
  strict
  compat
```

And every transform must be classified:

```
preserved
equivalent_translation
annotation_removed
validation_weakened
rejected
```

Example:

```
maxLength
strict → reject if unsupported
compat → drop + Route Trace warning

prefixItems
strict → reject
compat → translate if equivalent possible,
         otherwise weaken to generic items + warning

description/$schema
→ safe removal where semantically irrelevant
```

This gives Kinetix **9router-like usability without invisible semantic corruption**.

I'd make **compat the normal coding-agent profile**, strict available for API workloads.

---

### 4. Continuation-state contract - **make portability first-class**

9router stores Gemini `thoughtSignature` separately and reattaches it to subsequent tool calls.

OmniRoute additionally namespaces signatures by connection and has substantial reasoning/replay machinery. Its issue history shows why this matters: plaintext reasoning and opaque continuation state can coexist, and treating them incorrectly breaks later turns. [GitHub](https://github.com/diegosouzapw/OmniRoute/issues/10949?utm_source=chatgpt.com)

Cross-provider opaque reasoning also fundamentally isn't portable. This is now showing up across other clients as well. [GitHub](https://github.com/farion1231/cc-switch/issues/7333?utm_source=chatgpt.com)

Define:

```
ContinuationState =
  Portable(data)
  Opaque {
    provider_family,
    model_family?,
    account_scope?,
    value
  }
```

Rules:

```
Portable → may survive fallback
Opaque   → only replay where provenance permits
```

And explicitly define:

```
same account
same provider / different account
same protocol / different provider
cross-format
cross-provider
cross-model
```

for every opaque state type.

**Never infer portability from wire format.**

Your current Kinetix idea of rejecting non-portable provider state on incompatible fallback is correct. Formalize it instead of removing it.

---

### 5. Error/fallback contract - copy OmniRoute's taxonomy, improve the action model

OmniRoute centrally distinguishes things such as:

```
authentication_error
permission_error
rate_limit
quota_exhausted
timeout
network_error
provider_5xx
invalid_request
model_unavailable
```

and attaches retryability. This is a good baseline.

But `retryable: bool` isn't enough. Different 429s already require different behavior; even OmniRoute has different retry policies depending on subsystem.

Kinetix should define:

```
Failure {
  class
  retry_scope
  retry_after
  target_health_effect
  account_health_effect
}
```

with:

```
retry_scope =
  none
  same_account
  another_account
  another_target
  route_fallback
```

Example:

```
401 invalid token
→ another_account / credential refresh

402 insufficient credits
→ another_account or target

429 rate limit
→ another_account

429 account quota exhausted
→ another_account / cooldown

400 bad schema
→ none

503 transient
→ another_account or target
```

This prevents the historical Kinetix problem of treating every failure as merely “retryable/not retryable.”

---

### 6. Plugin ABI/capability contract - **follow pi-free philosophy**

The best lesson from pi-free:

> provider extension owns provider-specific behavior; host owns lifecycle.

pi-free's native providers let Pi control model persistence, credential lifecycle, refresh scheduling and cancellation rather than each extension inventing its own lifecycle. [GitHub](https://github.com/apmantza/pi-free/?utm_source=chatgpt.com)

For Kinetix:

**Core owns**

```
routing
fallback
accounts
health
catalog accepted/runtime state
capability semantics
field policy
continuation policy
accounting
canonical event machine
```

**Plugin owns**

```
authentication mechanics
catalog fetch
provider-specific request encoding
provider response parsing
provider error extraction
provider quirks
```

Plugin declares capabilities; it does **not** reinterpret canonical semantics.

Your current versioned plugin response envelope is worth keeping. [Current plugin response contract](https://github.com/PrightCord/kinetix/blob/main/docs/PLUGIN-RESPONSE-CONTRACT.md?utm_source=chatgpt.com)

---

## Therefore, before 0.1.0

Don't write six giant prose specs.

Create:

```
contracts/
├── canonical-v1.md
├── thinking-v1.md
├── tool-schema-v1.md
├── continuation-v1.md
├── failure-v1.md
└── plugin-v1.md

fixtures/
├── canonical/
├── thinking/
├── tool-schema/
├── continuation/
├── failure/
└── plugins/
```

Every contract gets **machine-testable fixtures**.

### Most important design choice

**Take:**

- Kinetix's strict field/golden contracts
- pi-free's thin native-provider boundary
- 9router/OmniRoute's real-world compatibility knowledge

**Don't take:**

- OpenAI-as-canonical internal architecture
- scattered provider quirks
- implicit reasoning downgrades
- silent schema destruction
- boolean-only retry semantics

So this is **not a ground-up contract rewrite**. Roughly **40% already exists in current Kinetix**. The critical pre-reboot work is consolidating it into these six explicit v1 contracts, especially **tool-schema + continuation + failure semantics**.
