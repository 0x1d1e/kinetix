# Protocol v1 field compatibility

<!-- GENERATED: scripts/render-protocol-v1-compat.py; edit tests/fixtures/protocol-v1-compatibility.json instead. -->

This document is generated from the same versioned compatibility contract consumed by the
deterministic protocol matrix. It describes Kinetix v1 behavior at the field level; it is
not a claim that every field of every upstream vendor API is implemented.

## Status semantics

- **passthrough**: same-format requests keep the original client body; provider-specific fields survive Kinetix.
- **translated**: the field has an explicit canonical representation and is rebuilt for the target adapter.
- **rejected (422)**: the field changes semantics but has no faithful cross-format representation.
- **not guaranteed**: cosmetic/unknown data may be ignored on translation; use same-format passthrough if it matters.
- **exact / estimated**: token-count responses expose the selected mode in `X-Kinetix-Token-Count`.

## POST /v1/chat/completions

| Field / behavior | Native / same-format | Translated / other path | Contract | Evidence |
|---|---|---|---|---|
| model | required / route resolved | required / route resolved | Required client model selector. | `chat.native.openai.sync`<br>`chat.translate.gemini.sync`<br>`chat.translate.anthropic.sync` |
| messages: text | passthrough | translated | System/developer text is hoisted; user/assistant text is canonicalized. | `chat.native.openai.sync`<br>`chat.translate.gemini.sync`<br>`chat.translate.anthropic.sync` |
| messages: image_url / data URL | passthrough | translated | URL and data-URL images map to canonical image parts. | `chat.image.variants`<br>`chat.native.openai.stream`<br>`chat.translate.gemini.stream`<br>`chat.translate.anthropic.stream` |
| assistant tool_calls + tool results | passthrough | translated | Tool ids are retained; result names are resolved from prior calls when needed. | `chat.native.openai.tool_continuation`<br>`chat.fallback.tool_continuation`<br>`chat.translate.anthropic.tool_continuation` |
| function tools | passthrough | translated | Function name, description and JSON schema are canonicalized. | `chat.native.openai.sync`<br>`chat.translate.gemini.nested_schema` |
| parallel tool calls | passthrough | translated | Stable call identity is required across translated streams. | `chat.translate.gemini.parallel_tools` |
| tool_choice | passthrough | translated | auto/none/required/specific function map to canonical tool choice. | `chat.tool_choice.variants`<br>`chat.translate.anthropic.sync` |
| temperature / top_p / top_k | passthrough | translated when target model policy supports them | Target parameter policy can reject unsupported controls. | `chat.native.openai.sync`<br>`chat.translate.gemini.sync`<br>`chat.translate.anthropic.sync` |
| max_tokens / max_completion_tokens | passthrough | translated | Both map to canonical max output tokens. | `chat.native.openai.sync`<br>`chat.translate.gemini.sync`<br>`chat.translate.anthropic.sync` |
| stop / seed / presence_penalty / frequency_penalty | passthrough | translated where the target has a mapping; otherwise rejected (400) unless the model's parameter policy is drop | Gemini proves stop/seed/penalties; Anthropic has no seed or penalties and refuses them; same-format OpenAI preserves all four. | `chat.native.openai.sync`<br>`chat.translate.gemini.sync`<br>`translate.target_fields.policy` |
| reasoning_effort | passthrough | translated only with model thinking_map | No reasoning control is invented when no mapping exists. | `chat.native.openai.sync`<br>`chat.translate.gemini.sync`<br>`chat.translate.anthropic.sync` |
| assistant reasoning history/signature | passthrough (forwarded verbatim to same-provider OpenAI Chat target) | portable only on compatible path; otherwise route portability policy applies | Same-format, same-provider replay is forwarded unchanged; the upstream owns replay semantics. Opaque state is stripped-with-warning or rejected at the first cross-format/provider boundary. | `chat.native.openai.reasoning_replay`<br>`chat.fallback.opaque_reasoning` |
| stream | passthrough | translated | Streaming is normalized through canonical events on translation. | `chat.native.openai.sync`<br>`chat.native.openai.stream`<br>`chat.translate.gemini.sync`<br>`chat.translate.gemini.stream`<br>`chat.translate.anthropic.sync`<br>`chat.translate.anthropic.stream` |
| stream_options.include_usage | passthrough | client preference preserved on translation | Internal accounting may request upstream usage even when client did not. | `chat.native.openai.stream`<br>`chat.translate.gemini.stream`<br>`chat.translate.anthropic.stream` |
| n > 1 | passthrough | rejected (400) | Multiple completions cannot be represented faithfully on translated paths. | `chat.native.openai.provider_extensions`<br>`chat.translate.gemini.unsupported_fields.reject` |
| logprobs / top_logprobs | passthrough | rejected (400) | No canonical cross-format representation. | `chat.translate.gemini.unsupported_fields.reject` |
| response_format.json_object / json_schema | passthrough | rejected (400) | Structured output enforcement cannot be guaranteed cross-format. | `chat.translate.gemini.unsupported_fields.reject`<br>`chat.translate.anthropic.unsupported_fields.reject`<br>`field_contract.dispositions` |
| modalities / audio output | passthrough | rejected (400) | Audio output is not in the v1 canonical model. | `chat.translate.gemini.unsupported_fields.reject` |
| prediction | passthrough | rejected (400) | Prediction semantics are not portable. | `chat.translate.gemini.unsupported_fields.reject` |
| unsupported nested content (file/audio/refusal/reasoning_details) | passthrough | rejected (400) | Behaviorally significant nested content is never silently dropped. | `chat.translate.gemini.nested_content.reject` |
| user / parallel_tool_calls / service_tier / verbosity / logit_bias / web_search_options / store / prompt_cache_key | passthrough | translated where the target has an official equivalent; otherwise rejected (400) unless the model's parameter policy is drop | user -> Responses user / Anthropic metadata.user_id; parallel_tool_calls:false -> Anthropic disable_parallel_tool_use; verbosity -> Responses text.verbosity; store:true is refused; prompt_cache_key also drives Route cache affinity. | `translate.target_fields.policy`<br>`field_contract.dispositions` |
| unknown/provider-specific top-level fields | passthrough verbatim | rejected (400) unless the model's parameter policy for that name is drop | Use a same-format provider when vendor extensions are required. | `chat.native.openai.provider_extensions`<br>`translate.target_fields.policy` |

## POST /v1/messages

| Field / behavior | Native / same-format | Translated / other path | Contract | Evidence |
|---|---|---|---|---|
| model / messages | required | required / translated | Anthropic Messages is accepted as a first-class frontend. | `messages.native.anthropic.sync`<br>`messages.translate.gemini.sync`<br>`messages.translate.openai.sync` |
| system text blocks | passthrough | translated | String and text-block system prompts map to canonical system entries. | `messages.system.variants` |
| text content | passthrough | translated | Text blocks are canonicalized. | `messages.native.anthropic.sync`<br>`messages.translate.gemini.sync`<br>`messages.translate.openai.sync` |
| image source (base64/URL) | passthrough | translated | Images map through the canonical image representation. | `messages.image.variants`<br>`messages.native.anthropic.stream` |
| tool_use / tool_result | passthrough | translated | Tool-use ids and result identity are retained. | `messages.native.anthropic.tool_continuation`<br>`messages.translate.gemini.tool_continuation`<br>`messages.translate.openai.tool_continuation` |
| function tools + input_schema | passthrough | translated | Nested schemas are preserved where target schema permits. | `messages.native.anthropic.sync`<br>`messages.translate.gemini.sync`<br>`messages.translate.openai.sync` |
| parallel tool_use blocks | passthrough | translated | Distinct ids/indexes must survive translation. | `messages.translate.gemini.parallel_tools` |
| tool_choice | passthrough | translated | auto/any/none/specific tool are normalized. | `messages.tool_choice.variants`<br>`messages.translate.openai.sync` |
| temperature / top_p / top_k / max_tokens / stop_sequences | passthrough | translated subject to target policy | Target-model policy remains authoritative. | `messages.native.anthropic.sync`<br>`messages.translate.gemini.sync`<br>`messages.translate.openai.sync` |
| thinking control | passthrough | translated only with model thinking_map | Thinking budget maps to configured low/medium/high levels. | `messages.native.anthropic.sync`<br>`messages.translate.gemini.sync`<br>`messages.translate.openai.sync` |
| thinking history/signature | passthrough | portable only on compatible path | Cross-provider/cross-format portability policy applies before dispatch. | `messages.translate.gemini.opaque_reasoning` |
| stream | passthrough | translated | Translated streams emit Anthropic event lifecycle. | `messages.native.anthropic.sync`<br>`messages.native.anthropic.stream`<br>`messages.translate.gemini.sync`<br>`messages.translate.gemini.stream`<br>`messages.translate.openai.sync`<br>`messages.translate.openai.stream` |
| multimodal tool_result / documents / server-tool-only blocks | passthrough when upstream supports them | rejected (400) | Unsupported semantic content fails explicitly. | `messages.translate.multimodal_tool_result.reject` |
| unknown/provider-specific fields (service_tier, mcp_servers, container, context_management, ...) | passthrough verbatim | rejected (400) unless the model's parameter policy for that name is drop; metadata.user_id -> OpenAI user | Same-format passthrough is the extension-preserving path. | `messages.native.anthropic.provider_extensions`<br>`translate.target_fields.policy` |

## POST /v1/messages/count_tokens

| Field / behavior | Native / same-format | Translated / other path | Contract | Evidence |
|---|---|---|---|---|
| model / system / messages / tools | accepted | canonicalized for estimate | Counting does not consume route balancing state. | `messages.count_tokens.native_exact`<br>`messages.count_tokens.translated_estimated`<br>`messages.count_tokens.openai_estimated`<br>`messages.count_tokens.heterogeneous_estimated` |
| healthy unambiguous built-in Anthropic target | exact | n/a | Calls the native Anthropic count_tokens endpoint. | `messages.count_tokens.native_exact` |
| Gemini / OpenAI-compatible / heterogeneous route | n/a | estimated | Non-Anthropic direct targets and heterogeneous routes use the deterministic local estimate. | `messages.count_tokens.translated_estimated`<br>`messages.count_tokens.openai_estimated`<br>`messages.count_tokens.heterogeneous_estimated` |
| response shape | {"input_tokens": N} | same | Header exposes exact vs estimated mode without changing Anthropic JSON. | `messages.count_tokens.native_exact`<br>`messages.count_tokens.translated_estimated`<br>`messages.count_tokens.openai_estimated`<br>`messages.count_tokens.heterogeneous_estimated` |

## POST /v1/responses

| Field / behavior | Native / same-format | Translated / other path | Contract | Evidence |
|---|---|---|---|---|
| model / outbound transport | required | translated | Validated Responses-to-Responses requests use raw passthrough to preserve same-format semantics; cross-format requests are re-encoded from canonical state. | `responses.native.openai.sync`<br>`responses.native.openai.passthrough`<br>`responses.translate.openai.sync`<br>`responses.translate.gemini.sync`<br>`responses.translate.anthropic.sync` |
| input string / message items | Responses-native encoding | translated | Same-format Responses requests preserve validated input items; cross-format requests map through canonical messages and the selected target dialect. | `responses.input.variants`<br>`responses.native.openai.sync`<br>`responses.native.openai.passthrough` |
| input_image URL/data URL | Responses-native encoding | translated | Supported image input maps through canonical vision parts. | `responses.image.variants`<br>`responses.native.openai.sync` |
| instructions | Responses-native instructions | translated | Text instructions map through canonical system prompts. | `responses.native.openai.sync`<br>`responses.translate.openai.sync`<br>`responses.translate.gemini.sync`<br>`responses.translate.anthropic.sync` |
| function tools / function_call / function_call_output | Responses-native function items | translated | Custom function tools and supported tool history/results use Responses function-call items. | `responses.native.openai.sync`<br>`responses.translate.openai.sync`<br>`responses.translate.openai.tool_continuation`<br>`responses.translate.gemini.tool_continuation`<br>`responses.translate.anthropic.tool_continuation` |
| Response object / output_text event required fields | parallel_tool_calls, tool_choice, tools; logprobs on output_text delta/done; usage details always present | synthesized Responses objects use request tool metadata and required fields | Schema regressions deserialize created, in-progress, completed, incomplete, and failed Responses plus text events; absent usage breakdown counts default to zero. | `responses.synthetic.wire_schema`<br>`responses.translate.stream_failure` |
| tool_choice auto/none/required/named function | n/a | translated | Other tool-choice semantics are rejected. | `responses.tool_choice.variants` |
| parallel tool calls from upstream | n/a | translated | Distinct tool ids are retained; request field parallel_tool_calls is not enforceable and is rejected. | `responses.translate.anthropic.parallel_tools` |
| temperature / top_p / max_output_tokens / max_tokens | target defaults/clamps apply; max_tokens and route token aliases normalize to capped max_output_tokens | translated subject to target capability | Same-format passthrough and translated requests both apply resolved model policy, including defaults, bounds and model output-token caps. | `responses.native.openai.sync`<br>`responses.native.openai.passthrough`<br>`responses.translate.openai.sync` |
| top_k / stop / seed / presence_penalty / frequency_penalty | explicit target drop policy or rejection; fields outside the accepted subset are rejected | translated only where supported | Same-format passthrough applies the same resolved target parameter policy instead of forwarding unsupported values literally. | `responses.native.unsupported_parameters`<br>`responses.native.openai.passthrough`<br>`responses.translate.openai.sync` |
| reasoning.effort / reasoning_effort | normalized through executable model thinking_map | translated only with model thinking_map | The legacy reasoning_effort alias is removed and emitted in the configured Responses reasoning shape, and an unmapped level is rejected on passthrough. reasoning.summary is preserved on same-format Responses and rejected on translating paths; reasoning output items are not implemented. | `responses.native.openai.sync`<br>`responses.native.openai.passthrough`<br>`responses.translate.openai.sync`<br>`responses.translate.gemini.sync`<br>`responses.translate.anthropic.sync` |
| prompt_cache_key | preserved | forwarded to OpenAI targets; Route cache-affinity key everywhere | Namespaced per virtual key; used only for Route affinity, never for continuation provenance. | `responses.native.openai.sync`<br>`responses.translate.openai.sync` |
| stream false / true | native Responses SSE + canonical aggregation | translated | Context-window exhaustion maps to max_output_tokens. Unknown terminal reasons use a ResponseError-valid server_error; streamed Anthropic refusals fail rather than completing with text-only content. Chat [DONE] is not emitted. | `responses.native.openai.sync`<br>`responses.native.openai.stream`<br>`responses.native.openai.incomplete`<br>`responses.translate.openai.sync`<br>`responses.translate.openai.stream`<br>`responses.translate.gemini.sync`<br>`responses.translate.gemini.stream`<br>`responses.translate.anthropic.sync`<br>`responses.translate.anthropic.stream`<br>`responses.translate.provider_terminal_semantics` |
| usage input_tokens_details / output_tokens_details | known cached, cache-write and reasoning token breakdowns preserved | canonical usage | Responses aggregation always emits the documented plural detail objects, defaults missing breakdown counts to zero, and preserves cached, cache-write, and reasoning counts when supplied. | `responses.native.openai.sync`<br>`responses.native.openai.passthrough`<br>`responses.synthetic.wire_schema` |
| refusal content / response.refusal.delta | preserved as typed refusal item/event | typed canonical refusal; rendered as refusal for Responses and text for Chat/Anthropic | Non-stream Anthropic stop_reason=refusal becomes content.type=refusal; streaming Anthropic refusals fail rather than completing with text-only content. | `responses.native.openai.refusal`<br>`responses.native.openai.stream`<br>`responses.native.openai.passthrough`<br>`responses.translate.provider_terminal_semantics` |
| store:true / background:true | rejected (422) | rejected (422) | Kinetix has no Responses object store/background execution; native calls explicitly disable storage. | `responses.supported_options`<br>`responses.unsupported_fields.reject`<br>`responses.native.openai.sync` |
| include expansions | n/a | only empty array accepted | Non-empty include is rejected. | `responses.supported_options`<br>`responses.unsupported_fields.reject` |
| text.format | n/a | only {type:"text"} accepted | Structured output formats are rejected. | `responses.supported_options`<br>`responses.unsupported_fields.reject` |
| truncation | n/a | only "disabled" accepted | Automatic truncation is not implemented. | `responses.supported_options`<br>`responses.unsupported_fields.reject` |
| stream_options | n/a | only include_obfuscation:false accepted | Obfuscation and unknown options are rejected. | `responses.supported_options`<br>`responses.unsupported_fields.reject` |
| metadata / parallel_tool_calls | n/a | rejected (422) | Storage/enforcement semantics are not approximated. | `responses.unsupported_fields.reject` |
| unknown top-level fields / unknown nested semantic items | n/a | rejected (422) | Responses subset fails closed. | `responses.unsupported_fields.reject` |

## GET /v1/models

| Field / behavior | Native / same-format | Translated / other path | Contract | Evidence |
|---|---|---|---|---|
| authentication | virtual key required | same | Visibility is filtered by the caller's virtual key. | `models.visible` |
| model ids | provider models + aliases + routes | same | Returns client-visible selectors, not serving account identities. | `models.visible` |
| object shape | OpenAI-compatible list | same | Used by compatible clients for discovery. | `models.visible` |
| capabilities / kinetix metadata | accepted state only; Routes report the target intersection | Anthropic format: not listed | OpenAI-format items add `capabilities` (`tools`, `images`, `streaming`, `structured_output`, `thinking`) and `kinetix` (`state`, `transport`, `provenance`). Unknown values are omitted; raw observations stay admin-only. | `models.visible` |

## Plugin adapter contract

| Field / behavior | Native / same-format | Translated / other path | Contract | Evidence |
|---|---|---|---|---|
| canonical request schema/version | kinetix.plugin.request v1 | same | Host serializes canonical requests before guest adapter translation. | `plugin.request.mixed_contract` |
| text / image / tool call / tool result / reasoning / defer_loading | preserved in canonical request | same | Mixed request state and deferred-tool metadata are contract-tested before a real guest is involved. | `plugin.request.mixed_contract` |
| parallel tool ids / arguments | versioned canonical events | same | Response fixture verifies distinct tool indexes and ids. | `plugin.response.parallel_contract` |
| thinking signature / usage / finish | versioned canonical events | same | Malformed/incompatible plugin responses fail closed. | `plugin.response.parallel_contract` |
| real .kxp guest execution | manual release acceptance | manual release acceptance | KINETIX_PLUGIN_E2E_PACKAGE keeps normal core CI hermetic. | `plugin.release.real_guest` |

## Executable evidence catalog

| Case | Runner | What it proves |
|---|---|---|
| `chat.native.openai.sync` | http | chat same-format OpenAI synchronous path preserves documented request/response semantics |
| `chat.native.openai.stream` | http | chat same-format OpenAI streaming path preserves terminal, usage and tool identity semantics |
| `chat.translate.gemini.sync` | http | chat translated Gemini synchronous path preserves documented request/response semantics |
| `chat.translate.gemini.stream` | http | chat translated Gemini streaming path preserves terminal, usage and tool identity semantics |
| `chat.translate.anthropic.sync` | http | chat translated Anthropic synchronous path preserves documented request/response semantics |
| `chat.translate.anthropic.stream` | http | chat translated Anthropic streaming path preserves terminal, usage and tool identity semantics |
| `messages.native.anthropic.sync` | http | messages same-format Anthropic synchronous path preserves documented request/response semantics |
| `messages.native.anthropic.stream` | http | messages same-format Anthropic streaming path preserves terminal, usage and tool identity semantics |
| `messages.translate.gemini.sync` | http | messages translated Gemini synchronous path preserves documented request/response semantics |
| `messages.translate.gemini.stream` | http | messages translated Gemini streaming path preserves terminal, usage and tool identity semantics |
| `messages.translate.openai.sync` | http | messages translated OpenAI-compatible synchronous path preserves documented request/response semantics |
| `messages.translate.openai.stream` | http | messages translated OpenAI-compatible streaming path preserves terminal, usage and tool identity semantics |
| `responses.translate.openai.sync` | http | responses translated OpenAI-compatible synchronous path preserves documented request/response semantics |
| `responses.translate.openai.stream` | http | responses translated OpenAI-compatible streaming path preserves terminal, usage and tool identity semantics |
| `responses.native.openai.sync` | cargo | Native Responses adapter pairs the Responses body dialect with POST /responses and preserves Responses usage breakdowns |
| `responses.native.openai.passthrough` | cargo | Responses API passthrough normalizes all route token aliases to capped max_output_tokens and applies defaults, clamps, reasoning and parameter policy while rewriting model, store and endpoint |
| `responses.native.openai.stream` | cargo | Native Responses SSE events normalize typed refusals and incomplete terminal status for Responses clients |
| `responses.native.openai.refusal` | cargo | Native Responses refusal objects and refusal.delta events preserve typed refusal content through non-stream aggregation |
| `responses.native.openai.incomplete` | cargo | Native response.incomplete events preserve usage and aggregate status=incomplete with incomplete_details.reason |
| `responses.synthetic.wire_schema` | cargo | Synthesized Responses lifecycle/output-text events and aggregates deserialize against current SDK types, including required request fields, logprobs and usage details |
| `responses.translate.stream_failure` | cargo | Translated Responses streams ending before a terminal event emit schema-valid response.failed with ResponseError.code=server_error |
| `responses.native.unsupported_parameters` | cargo | Unsupported Responses transport parameters fail closed rather than being dropped |
| `responses.translate.gemini.sync` | http | responses translated Gemini synchronous path preserves documented request/response semantics |
| `responses.translate.gemini.stream` | http | responses translated Gemini streaming path preserves terminal, usage and tool identity semantics |
| `responses.translate.anthropic.sync` | http | responses translated Anthropic synchronous path preserves documented request/response semantics |
| `responses.translate.anthropic.stream` | http | responses translated Anthropic streaming path preserves terminal, usage and tool identity semantics |
| `responses.translate.provider_terminal_semantics` | cargo | Anthropic context-window exhaustion maps to max_output_tokens; non-stream refusals become typed content; streamed refusals and unknown reasons fail with a ResponseError-valid server_error |
| `chat.translate.gemini.parallel_tools` | http | Parallel Chat tool identities survive OpenAI-to-Gemini translation |
| `messages.translate.gemini.parallel_tools` | http | Parallel Messages tool identities survive Anthropic-to-Gemini translation |
| `responses.translate.anthropic.parallel_tools` | http | Parallel Responses tool identities survive Responses-to-Anthropic translation |
| `chat.translate.gemini.nested_schema` | http | Nested Chat tool schema survives Gemini translation |
| `chat.fallback.tool_continuation` | http | Chat tool-result identity survives route fallback |
| `chat.native.openai.tool_continuation` | http | Chat tool-result identity survives same-format OpenAI passthrough |
| `chat.translate.anthropic.tool_continuation` | http | Chat tool-result identity survives Anthropic translation |
| `messages.translate.openai.tool_continuation` | http | Messages tool-result identity survives OpenAI translation |
| `messages.native.anthropic.tool_continuation` | http | Messages tool-result identity survives same-format Anthropic passthrough |
| `messages.translate.gemini.tool_continuation` | http | Messages tool-result identity survives Gemini translation |
| `chat.fallback.opaque_reasoning` | http | Opaque reasoning state is stripped with warning at first portability boundary |
| `chat.native.openai.reasoning_replay` | http | Chat reasoning_content/reasoning_signature replay to a same-provider OpenAI Chat passthrough target is forwarded verbatim |
| `messages.translate.multimodal_tool_result.reject` | http | Unsupported multimodal tool results, documents, and server-tool semantics fail explicitly |
| `chat.native.openai.provider_extensions` | http | Chat same-format OpenAI preserves n and provider-specific fields |
| `messages.native.anthropic.provider_extensions` | http | Messages same-format Anthropic preserves provider-specific fields |
| `chat.translate.gemini.unsupported_fields.reject` | http | Chat n/logprobs/json_schema/audio/prediction pass through same-format OpenAI and are rejected on Gemini translation |
| `chat.translate.anthropic.unsupported_fields.reject` | http | Chat n/logprobs/json_schema/audio/prediction pass through same-format OpenAI and are rejected on Anthropic translation |
| `translate.target_fields.policy` | http | Non-canonical fields the translated target cannot honor (seed, penalties, service_tier, identity hints, unknown) are rejected; honored ones are translated |
| `responses.unsupported_fields.reject` | http | Every documented unsupported Responses semantic is explicitly rejected |
| `messages.count_tokens.native_exact` | http | Native Anthropic token count accepts system/messages/tools and is exact |
| `messages.count_tokens.translated_estimated` | http | Translated token count accepts system/messages/tools and is explicitly estimated |
| `models.visible` | http | Models endpoint enforces auth, key-scoped visibility, and provider-model/alias/route selector discovery |
| `plugin.request.mixed_contract` | cargo | Canonical plugin request contract preserves vision, tool identity, reasoning and deferred-tool metadata |
| `plugin.response.parallel_contract` | cargo | Canonical plugin response contract preserves parallel tool ids and reasoning signatures |
| `plugin.release.real_guest` | manual | Release-only external .kxp execution exercises the real plugin host |
| `chat.image.variants` | http | Chat image URL and data-URL variants translate explicitly |
| `chat.tool_choice.variants` | http | Chat auto/none/required/specific tool choice variants translate explicitly |
| `messages.system.variants` | http | Messages string and text-block system prompts translate explicitly |
| `messages.image.variants` | http | Messages base64 and URL image sources translate explicitly |
| `messages.tool_choice.variants` | http | Messages auto/none/any/specific tool choice variants translate explicitly |
| `messages.translate.gemini.opaque_reasoning` | http | Messages opaque thinking signature is stripped with warning at portability boundary |
| `chat.translate.gemini.nested_content.reject` | http | Chat file/audio/function_call/refusal/reasoning_details semantics reject explicitly on translation |
| `responses.input.variants` | http | Responses string and message input variants translate explicitly |
| `responses.image.variants` | http | Responses URL and data-URL image variants translate explicitly |
| `responses.tool_choice.variants` | http | Responses auto/none/required/named function tool choices translate explicitly |
| `responses.translate.openai.tool_continuation` | http | Responses function_call/function_call_output identity survives translation |
| `responses.translate.gemini.tool_continuation` | http | Responses function_call/function_call_output identity survives Gemini translation |
| `responses.translate.anthropic.tool_continuation` | http | Responses function_call/function_call_output identity survives Anthropic translation |
| `responses.supported_options` | http | Responses accepted false/empty/text/disabled option forms remain accepted |
| `messages.count_tokens.openai_estimated` | http | OpenAI-compatible token count uses the documented local estimate |
| `messages.count_tokens.heterogeneous_estimated` | http | Heterogeneous route token count stays local and does not consume route-balancing state |
| `chat.translate.gemini.tool_signature_continuation` | http | Gemini function-call thoughtSignature survives an OpenAI/Pi multi-turn tool continuation without client-visible vendor state |
| `chat.translate.gemini.cross_model_placeholder` | http | A Gemini signature captured on one model is translated with the provider's documented placeholder, not replayed or stripped, when the continuation moves to another model |
| `chat.translate.gemini.cross_model_placeholder_direct` | http | A direct same-family Gemini model switch with no Route policy is translated with the provider's documented placeholder instead of being refused |
| `chat.translate.gemini.legacy_model_strip_without_placeholder` | http | A continuation onto a pre-Gemini-3 model strips the non-portable signature instead of injecting the Gemini 3 validator-bypass placeholder |
| `field_contract.dispositions` | cargo | tests/field_contract.rs: every decoded frontend field has an executed preserved/translated/consumed/rejected disposition per outbound transport |

The `http` cases run through `scripts/protocol-v1-matrix.py` inside the existing synthetic compatibility harness. The `cargo` cases run as normal Rust integration tests. Entries marked `manual` are executable release acceptance and are intentionally excluded from normal PR/local CI. Real Pi, Claude Code, Responses-client, and external `.kxp` sessions remain release-only.

