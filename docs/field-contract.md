# Field-disposition contract

Every semantic field a frontend accepts has an explicit disposition per outbound transport. Nothing is left to serde keeping or dropping it, a JSON merge leaking it, or an adapter ignoring it.

| Disposition | Meaning |
|---|---|
| `preserved` | Sent upstream unchanged (same value, same location). |
| `translated` | Sent upstream in the target protocol's shape. Fixture declares `upstream` (JSON pointer) and `expect`. |
| `consumed` | Used by Kinetix or has no upstream equivalent; must not appear upstream. |
| `rejected` | Request fails before any upstream call; nothing is sent. |

Source of truth: `tests/fixtures/field-contract/{openai-chat,anthropic-messages,openai-responses}.json`. Harness: `tests/field_contract.rs`. It decodes a sample request, runs the real translation gate and `pipeline::build_upstream_body` per transport (`openai`, `openai-responses`, `anthropic`, `gemini`), and compares the result to the fixture.

CI fails when:

- a frontend decodes a field not listed in its `DECODED_FIELDS` and the fixture (or vice versa);
- an adapter emits a top-level key not covered by a field or `adapter_owned`;
- a translated field vanishes or lands elsewhere;
- a rejected field is not rejected, or leaks upstream;
- a consumed field is serialized upstream;
- an unknown field (`x_vendor_marker`) is not handled as declared.

## Adding a field

1. Add it to the frontend's `DECODED_FIELDS` (or leave it in `extra`, it still needs an entry).
2. Add a `fields[]` entry with a `sample` and an `outbound` disposition for every transport.
3. Run `cargo test --test field_contract`.

## Known gaps

Entries whose note starts with `gap:` are `consumed` only because the default compatibility policy silently drops them today. They record current behavior, not endorsement. Each should become `translated` or `rejected` (or get a documented policy) in follow-ups, notably:

- `seed`, `presence_penalty`, `frequency_penalty`, `logit_bias`, `parallel_tool_calls`, `verbosity`, `service_tier`, `prompt_cache_key`, `web_search_options` when going to transports without an equivalent;
- `response_format: json_object` to non-OpenAI-chat transports;
- Anthropic `mcp_servers`, `container`, `context_management`, `service_tier` to other transports.

Changing a gap's runtime behavior means flipping its fixture entry in the same PR.
