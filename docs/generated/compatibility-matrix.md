# Compatibility matrix

<!-- GENERATED: scripts/render-protocol-v1-compat.py; edit the `matrix` section of tests/fixtures/protocol-v1-compatibility.json instead. -->

Compatibility by request path and feature. Every cell is backed by executable evidence
from the deterministic protocol matrix or Rust tests; field-level detail lives in
[protocol-v1-compatibility.md](../protocol-v1-compatibility.md).

## Status semantics

- **native**: Same-format passthrough; the client body reaches the upstream unchanged except for documented model policy.
- **translated**: Rebuilt from the canonical representation in the target dialect.
- **supported**: Multi-turn state survives on this path.
- **rejected**: Refused before any upstream call; never silently dropped.
- **conditional**: Preserved, replaced, or stripped depending on the producing model and the Route portability policy (see note).
- **-**: not claimed; no v1 contract or no executable evidence for this path yet.

## Features

- **tools**: Function tools, tool_choice, and tool-call identity in responses.
- **images**: Image input parts.
- **structured**: Structured output (response_format json_object/json_schema, text.format).
- **thinking**: Reasoning / thinking controls.
- **streaming**: SSE lifecycle, terminal events, usage, and tool identity while streaming.
- **continuation**: Tool-call / tool-result identity across turns.
- **reasoning replay**: Prior-turn reasoning content and signatures replayed by the client.

## Matrix

| Path | tools | images | structured | thinking | streaming | continuation | reasoning replay |
|---|---|---|---|---|---|---|---|
| Chat -> OpenAI Chat | native | native | native* | native | native | supported | supported* |
| Chat -> Gemini | translated | translated | rejected* | translated* | translated | supported | conditional* |
| Chat -> Anthropic | translated | translated | rejected* | translated* | translated | supported | - |
| Messages -> Anthropic | native | native | - | native | native | supported | - |
| Messages -> Gemini | translated | translated | - | translated* | translated | supported | conditional* |
| Messages -> OpenAI Chat | translated | translated | - | translated* | translated | supported | - |
| Responses -> OpenAI Responses | native | native | rejected* | native* | native | - | - |
| Responses -> OpenAI Chat | translated | translated | rejected* | translated* | translated | supported | - |
| Responses -> Gemini | translated | translated | rejected* | translated* | translated | supported | - |
| Responses -> Anthropic | translated | translated | rejected* | translated* | translated | supported | - |

\* See the cell note below.

## Evidence

| Path | Feature | Status | Evidence | Note |
|---|---|---|---|---|
| Chat -> OpenAI Chat | tools | native | `chat.native.openai.sync`<br>`chat.native.openai.stream` |  |
| Chat -> OpenAI Chat | images | native | `chat.native.openai.sync` |  |
| Chat -> OpenAI Chat | structured | native | `chat.translate.gemini.unsupported_fields.reject`<br>`field_contract.dispositions` | response_format is forwarded verbatim. |
| Chat -> OpenAI Chat | thinking | native | `chat.native.openai.sync` |  |
| Chat -> OpenAI Chat | streaming | native | `chat.native.openai.stream` |  |
| Chat -> OpenAI Chat | continuation | supported | `chat.native.openai.tool_continuation` |  |
| Chat -> OpenAI Chat | reasoning replay | supported | `chat.native.openai.reasoning_replay` | Policy (#207): reasoning_content/reasoning_signature are forwarded verbatim to a same-provider target; the upstream owns replay semantics. Cross-provider replay follows the Route portability policy; a direct target with no Route refuses it (400) rather than dropping it. |
| Chat -> Gemini | tools | translated | `chat.translate.gemini.sync`<br>`chat.translate.gemini.stream`<br>`chat.translate.gemini.parallel_tools`<br>`chat.translate.gemini.nested_schema`<br>`chat.tool_choice.variants` |  |
| Chat -> Gemini | images | translated | `chat.translate.gemini.sync`<br>`chat.image.variants` |  |
| Chat -> Gemini | structured | rejected | `chat.translate.gemini.unsupported_fields.reject`<br>`field_contract.dispositions` | Structured response_format (json_object, json_schema) returns 400; plain text is consumed. |
| Chat -> Gemini | thinking | translated | `chat.translate.gemini.sync` | Requires a model thinking_map; no reasoning control is invented without one. |
| Chat -> Gemini | streaming | translated | `chat.translate.gemini.stream` |  |
| Chat -> Gemini | continuation | supported | `chat.fallback.tool_continuation` |  |
| Chat -> Gemini | reasoning replay | conditional | `chat.translate.gemini.tool_signature_continuation`<br>`chat.translate.gemini.cross_model_placeholder`<br>`chat.translate.gemini.legacy_model_strip_without_placeholder`<br>`chat.fallback.opaque_reasoning` | Gemini thought signatures are restored host-side for the producing model, replaced by the documented placeholder on another Gemini 3 model, and stripped before Gemini 3. Foreign reasoning state follows the Route portability policy. |
| Chat -> Anthropic | tools | translated | `chat.translate.anthropic.sync`<br>`chat.translate.anthropic.stream` |  |
| Chat -> Anthropic | images | translated | `chat.translate.anthropic.sync` |  |
| Chat -> Anthropic | structured | rejected | `chat.translate.anthropic.unsupported_fields.reject`<br>`field_contract.dispositions` | Structured response_format (json_object, json_schema) returns 400; plain text is consumed. |
| Chat -> Anthropic | thinking | translated | `chat.translate.anthropic.sync` | Requires a model thinking_map; no reasoning control is invented without one. |
| Chat -> Anthropic | streaming | translated | `chat.translate.anthropic.stream` |  |
| Chat -> Anthropic | continuation | supported | `chat.translate.anthropic.tool_continuation` |  |
| Messages -> Anthropic | tools | native | `messages.native.anthropic.sync`<br>`messages.native.anthropic.stream` |  |
| Messages -> Anthropic | images | native | `messages.native.anthropic.sync` |  |
| Messages -> Anthropic | thinking | native | `messages.native.anthropic.sync` |  |
| Messages -> Anthropic | streaming | native | `messages.native.anthropic.stream` |  |
| Messages -> Anthropic | continuation | supported | `messages.native.anthropic.tool_continuation` |  |
| Messages -> Gemini | tools | translated | `messages.translate.gemini.sync`<br>`messages.translate.gemini.stream`<br>`messages.translate.gemini.parallel_tools`<br>`messages.tool_choice.variants` |  |
| Messages -> Gemini | images | translated | `messages.translate.gemini.sync`<br>`messages.image.variants` |  |
| Messages -> Gemini | thinking | translated | `messages.translate.gemini.sync` | Requires a model thinking_map; no reasoning control is invented without one. |
| Messages -> Gemini | streaming | translated | `messages.translate.gemini.stream` |  |
| Messages -> Gemini | continuation | supported | `messages.translate.gemini.tool_continuation` |  |
| Messages -> Gemini | reasoning replay | conditional | `messages.translate.gemini.opaque_reasoning` | A thinking signature from another provider is stripped with a warning or rejected per the Route portability policy. |
| Messages -> OpenAI Chat | tools | translated | `messages.translate.openai.sync`<br>`messages.translate.openai.stream` |  |
| Messages -> OpenAI Chat | images | translated | `messages.translate.openai.sync` |  |
| Messages -> OpenAI Chat | thinking | translated | `messages.translate.openai.sync` | Requires a model thinking_map; no reasoning control is invented without one. |
| Messages -> OpenAI Chat | streaming | translated | `messages.translate.openai.stream` |  |
| Messages -> OpenAI Chat | continuation | supported | `messages.translate.openai.tool_continuation` |  |
| Responses -> OpenAI Responses | tools | native | `responses.native.openai.sync` |  |
| Responses -> OpenAI Responses | images | native | `responses.native.openai.sync` |  |
| Responses -> OpenAI Responses | structured | rejected | `responses.unsupported_fields.reject`<br>`field_contract.dispositions` | Structured text.format is rejected at decode for every target; only {type:"text"} is accepted. |
| Responses -> OpenAI Responses | thinking | native | `responses.native.openai.passthrough` | reasoning.effort is normalized through the model thinking_map; an unmapped level is rejected. |
| Responses -> OpenAI Responses | streaming | native | `responses.native.openai.stream`<br>`responses.native.openai.incomplete` |  |
| Responses -> OpenAI Chat | tools | translated | `responses.translate.openai.sync`<br>`responses.translate.openai.stream` |  |
| Responses -> OpenAI Chat | images | translated | `responses.translate.openai.sync` |  |
| Responses -> OpenAI Chat | structured | rejected | `responses.unsupported_fields.reject`<br>`field_contract.dispositions` | Structured text.format is rejected at decode for every target; only {type:"text"} is accepted. |
| Responses -> OpenAI Chat | thinking | translated | `responses.translate.openai.sync` | Requires a model thinking_map; no reasoning control is invented without one. |
| Responses -> OpenAI Chat | streaming | translated | `responses.translate.openai.stream`<br>`responses.translate.stream_failure` |  |
| Responses -> OpenAI Chat | continuation | supported | `responses.translate.openai.tool_continuation` |  |
| Responses -> Gemini | tools | translated | `responses.translate.gemini.sync`<br>`responses.translate.gemini.stream`<br>`responses.tool_choice.variants` |  |
| Responses -> Gemini | images | translated | `responses.translate.gemini.sync`<br>`responses.image.variants` |  |
| Responses -> Gemini | structured | rejected | `responses.unsupported_fields.reject`<br>`field_contract.dispositions` | Structured text.format is rejected at decode for every target; only {type:"text"} is accepted. |
| Responses -> Gemini | thinking | translated | `responses.translate.gemini.sync` | Requires a model thinking_map; no reasoning control is invented without one. |
| Responses -> Gemini | streaming | translated | `responses.translate.gemini.stream` |  |
| Responses -> Gemini | continuation | supported | `responses.translate.gemini.tool_continuation` |  |
| Responses -> Anthropic | tools | translated | `responses.translate.anthropic.sync`<br>`responses.translate.anthropic.stream`<br>`responses.translate.anthropic.parallel_tools` |  |
| Responses -> Anthropic | images | translated | `responses.translate.anthropic.sync` |  |
| Responses -> Anthropic | structured | rejected | `responses.unsupported_fields.reject`<br>`field_contract.dispositions` | Structured text.format is rejected at decode for every target; only {type:"text"} is accepted. |
| Responses -> Anthropic | thinking | translated | `responses.translate.anthropic.sync` | Requires a model thinking_map; no reasoning control is invented without one. |
| Responses -> Anthropic | streaming | translated | `responses.translate.anthropic.stream`<br>`responses.translate.provider_terminal_semantics` |  |
| Responses -> Anthropic | continuation | supported | `responses.translate.anthropic.tool_continuation` |  |
