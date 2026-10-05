# Build conformance fixtures

## Finding

**Do this before reboot.**  But don’t build another isolated fixture set. Kinetix already has substantial fixture machinery; consolidate it into a **single compatibility corpus**.

### What the references do well

|Project|Useful pattern|Copy?|
| ---------| ---------------------------------------------------------------------------------------------------------------| -------|
|**pi-free**|Regression tests use**real Pi-shaped normalized context**, tools, streams, auth modes, reasoning quirks|**Yes**|
|**9router**|Translator implementations + ability to**replay converted requests directly against providers**|**Yes**|
|**OmniRoute**|Dedicated translation fixtures, including OpenAI↔Claude↔Gemini and**SSE chunk sequences**; broad unit/integration/protocol E2E|**Yes**|
|**Kinetix today**|Strong exact field disposition + thinking body fixtures + real-world schema corpus|**Keep, consolidate**|

pi-free is particularly valuable because it tests failures from the **actual coding-agent contract**, e.g. normalized Pi transcript tools and anonymous-vs-keyed OpenCode streaming-not merely abstract API shapes. [GitHub](https://github.com/apmantza/pi-free?utm_source=chatgpt.com)

9router is much less formal about conformance, but its architecture actively translates among OpenAI/Claude/Gemini/Responses shapes, and it includes a replay workflow for sending translated requests directly to an upstream. [GitHub](https://github.com/lim12137/9router?utm_source=chatgpt.com)

OmniRoute has the most useful structural precedent: translator fixtures, streaming fixtures, unit/integration/E2E layers, and protocol-level E2E. [GitHub](https://github.com/diegosouzapw/OmniRoute/blob/release/v3.8.51/docs/architecture/CODEBASE_DOCUMENTATION.md?utm_source=chatgpt.com) Its recent strict-schema failures also show why **large test count ≠ compatibility proof**: provider-dialect schema cases must themselves be explicit fixtures. [GitHub](https://github.com/diegosouzapw/OmniRoute/issues/13583?utm_source=chatgpt.com)

## Kinetix already has good pieces

Current tree already contains:

```
tests/fixtures/field-contract/
  openai-chat.json
  openai-responses.json
  anthropic-messages.json

tests/fixtures/thinking-translation/
  client-intents.json
  openai-chat.json
  openai-responses.json
  anthropic.json
  gemini.json
  plugin-*.json

kinetix-plugins/.../schema-compat/
  corpus.json

wit/fixtures/plugin-adapter/v1/
  requests/
  responses.json
  schema-keywords.json
```

Your schema corpus is actually ahead of the others in some respects: real shapes for **k-jev** **`patternProperties`** **, Chrome DevTools** **`maxLength`** **, tuples, $ref, TypeBox, Zod, Codex shell**.

The problem: these prove **pieces** of compatibility.

---

# What v0.1.0 needs

Create one authoritative:

```
conformance/
  cases/
    text/
    tools/
    schemas/
    thinking/
    multimodal/
    continuation/
    streaming/
    errors/
    caching/
  clients/
    pi/
    claude-code/
    codex/
    opencode/
  providers/
    openai/
    anthropic/
    gemini/
    antigravity/
    ...
```

Every case should describe the **entire observable transaction**:

```
client request
      ↓
expected canonical representation
      ↓
expected exact upstream request
      ↓
fixture upstream response/SSE
      ↓
expected canonical events
      ↓
expected exact client response/SSE
      ↓
expected accounting / retry / fallback outcome
```

### Mandatory corpus

1. **Text/message semantics**

   - system/developer/user/assistant
   - mixed content
   - empty/null content
   - max tokens, temperature, stop, metadata
2. **Tools**

   - one tool call
   - parallel calls
   - tool results
   - errored result
   - malformed args
   - malformed history
   - forced/required/none tool choice
3. **Real tool schemas**

   - k-jev
   - Chrome DevTools
   - Claude Code tools
   - Codex shell
   - MCP/Zod/TypeBox
   - $ref, tuples, unions, records
   - complete keyword matrix
4. **Thinking**

   ```
   off
   default
   minimal
   low
   medium
   high
   max
   adaptive
   explicit budget
   ```

   Pin **exact upstream JSON**, not “contains reasoning”.
5. **Continuation/state**

   - Gemini thought signatures
   - Anthropic thinking continuation
   - Responses `previous_response_id`
   - tool-call continuation
   - cross-format portable state
   - cross-format **non-portable state must reject**
6. **Streaming**  
   Borrow heavily from OmniRoute:

   - arbitrary chunk boundaries
   - partial UTF-8
   - multiple SSE events/chunk
   - usage events
   - thinking → text → tool ordering
   - parallel tools
   - `[DONE]`
   - upstream closes early
   - error before first token
   - error after commit
7. **Failures/routing**

   ```
   400 → terminal
   401 → credential rotation where supported
   403 → classification
   429 → retry/fallback
   5xx → retry/fallback
   credit exhausted → correct category
   timeout before commit → retry
   failure after commit → no unsafe retry
   ```
8. **Client-native fixtures**  
   This is the key lesson from pi-free.  
   Capture actual requests from:

   ```
   Pi
   Claude Code
   Codex CLI
   OpenCode
   ```

   Then ensure **those exact payloads** survive every supported target.

---

## Important change

Don't make:

```
test_openai_to_gemini
test_claude_to_gemini
test_pi_to_gemini
...
```

Make the fixture **data-driven**:

```
client: pi
frontend: openai-chat

request: ...

target:
  transport: gemini
  capabilities: ...

upstream:
  request: ...
  response: ...

expect:
  client_response: ...
  usage: ...
  retry: false
```

Then the same runner executes it against:

```
native adapter
plugin adapter
future adapters
```

**Native and plugin must produce equivalent observable behavior.**

---

## Add one thing neither 9router nor Kinetix does strongly enough

### Differential replay

For selected corpus cases:

```
fixture
 ├─→ Kinetix
 ├─→ 9router
 ├─→ OmniRoute
 └─→ direct provider/reference implementation
```

Compare semantics, **not arbitrary implementation details**.

Then optionally:

```
conformance --live
```

replays safe cases against real provider credentials.

This gives you the 9router-style real-upstream validation without making CI depend on live APIs.

---

## Reboot release gate

I’d make **v0.1.0 impossible to release unless**:

```
Pi          ✓ text tools thinking streaming
Claude Code ✓ text tools thinking streaming
Codex       ✓ Responses tools reasoning streaming
OpenCode    ✓ text tools reasoning streaming

OpenAI      ✓
Anthropic   ✓
Gemini      ✓

native/plugin behavioral parity ✓
schema corpus ✓
continuation corpus ✓
failure/retry corpus ✓
```

### Bottom line

**Don’t discard current Kinetix fixtures.**

Refactor them into:

> **client payload → canonical → exact upstream wire → upstream events → exact client wire**

That combines:

- **pi-free:**  real-client regressions
- **9router:**  practical translation + upstream replay
- **OmniRoute:**  translation/SSE corpus + E2E
- **Kinetix:**  strict field/thinking/schema contracts

That should be one of the **hard prerequisites for calling the rebooted codebase 0.1.0**.
