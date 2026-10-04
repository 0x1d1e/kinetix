# Thinking translation contract

Pins both sides of the thinking translation:

```text
client intent -> canonical intent -> exact provider body
```

A fixture change is a deliberate compatibility change. Asserting only the canonical level is not enough: every case also pins the complete provider-facing JSON, or the fail-closed rejection.

Harness: `tests/thinking_translation_contract.rs`. Fixtures: `tests/fixtures/thinking-translation/`. It drives the real path: frontend decode, `resolve_execution_profile_for_target`, `pipeline::check_resolved_thinking_translation`, then `pipeline::build_upstream_body` (same-format passthrough skips the gate, as in the dispatcher).

## Fixtures

- `client-intents.json`: client wire value to canonical `ThinkingLevel` per frontend (OpenAI Chat `reasoning_effort`, Responses `reasoning.effort`, Anthropic `thinking` / `output_config.effort` / `budget_tokens`). Pins Anthropic budget bucketing: `0` is off, `<=2048` low, `<=8192` medium, otherwise high.
- `{openai-chat,openai-responses,anthropic,gemini}.json`: per transport, named model capability profiles (`thinking_map` or `discovery`) plus cases `{name, model, frontend, intent, canonical, expect}`. `expect` is `{"body": <full JSON>}` or `{"rejected": "<message substring>"}`.

## Matrix

For every transport and model capability:

| Case | Asserted |
|---|---|
| Off | exact disable shape (`reasoning_effort: "none"`, `thinking: {"type":"disabled"}`, `thinkingBudget: 0`), or rejection when the map has no `off` entry |
| Adaptive | `thinking: {"type":"adaptive"}` plus `output_config.effort` for the level; rejection when the map lacks the level |
| Explicit budget | client `budget_tokens` 8192 buckets to medium, then the map value lands at `thinking.budget_tokens` / `generationConfig.thinkingConfig.thinkingBudget` |
| Categorical | each of minimal, low, medium, high, xhigh, max, mapped or rejected |
| Absent | no thinking field emitted |
| Passthrough | same-format client bytes preserved, except Anthropic adaptive normalization and Responses, which rebuilds reasoning from the model map (legacy flat `reasoning_effort` becomes `reasoning.effort`, or is rejected when the level has no executable mapping) |

Only a Level-mode map is executable. Discovered `adaptive` / `manual_budget` metadata (Anthropic, Gemini, OpenAI Chat fixtures) is descriptive: any requested level, including `off` and `default`, is rejected instead of being guessed into a provider body. Operator-configured maps are executable in every mode.

The matrix pins support or explicit rejection per transport. It does not force every provider into every class (Gemini has no adaptive envelope, so Gemini adaptive is unsupported).

## Changing a golden

1. Change the behavior and run `cargo test --test thinking_translation_contract`.
2. Review each reported `expected` / `actual` diff against the provider's documentation.
3. Edit the fixture `expect` by hand and explain the compatibility change in the PR.
