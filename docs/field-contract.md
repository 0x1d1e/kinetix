# Field-disposition contract

Every semantic field a frontend accepts has an explicit disposition per outbound transport. Nothing is left to serde keeping or dropping it, a JSON merge leaking it, or an adapter ignoring it.

| Disposition | Meaning |
|---|---|
| `preserved` | Sent upstream unchanged (same value, same location). No `upstream`/`expect` allowed; the sample is the expected value. |
| `translated` | Sent upstream in the target protocol's shape. Fixture declares `upstream` (JSON pointer) and `expect`. |
| `consumed` | Client value is consumed internally and not forwarded. An adapter-owned replacement may be emitted when declared via `upstream` + `expect`. |
| `rejected` | Request fails before any upstream call; nothing is sent. No `upstream`/`expect`. |

Source of truth: `tests/fixtures/field-contract/{openai-chat,anthropic-messages,openai-responses}.json`. Harness: `tests/field_contract.rs`. It decodes a sample request, runs the real translation gate and `pipeline::build_upstream_body` per transport (`openai`, `openai-responses`, `anthropic`, `gemini`), and compares the result to the fixture. Thinking levels are pinned end to end in [thinking-contract.md](thinking-contract.md).

CI fails when:

- a frontend decodes a field not listed in its `DECODED_FIELDS` and the fixture (or vice versa);
- an adapter emits a top-level key not covered by a field or `adapter_owned`;
- a translated field vanishes or lands elsewhere;
- a rejected field is not rejected, or leaks upstream;
- a consumed field is serialized upstream;
- an unknown field (`x_vendor_marker`) is not handled as declared.

## Fixture features

- `path`: JSON pointer inside `field` (e.g. `/stream_options/include_usage`); `sample` is the value at that path. Use it so subfields get their own disposition.
- `consumed` + `upstream` + `expect`: the adapter overrides that location with its own value (e.g. `store` forced `false`, `include_usage` forced `true`). This is enforced: the sample must differ from `expect`, otherwise use `preserved` (or `rejected` if the other value is refused). Declare one case per client value where normalization exists.
- Plain `consumed` fails if the sample is found at its wire path upstream (works for booleans) or any unique scalar leaf appears anywhere upstream.
- `companions`: extra top-level request fields the sample needs (e.g. `thinking` for `output_config.effort`).

`DECODED_FIELDS` is hand-maintained, but `decoders_only_read_registered_fields` scans decoder sources for `obj.get("x")`/`contains_key`/`remove` and fails if `x` is neither registered nor in the fixture. Deriving both from one declaration remains a follow-up.

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
