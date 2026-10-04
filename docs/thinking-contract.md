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
| Passthrough | same-format client bytes preserved, except Anthropic adaptive normalization and Responses legacy flat `reasoning_effort` removal |

Only a Level-mode map is executable. Discovered `adaptive` / `manual_budget` metadata is descriptive, so requests with a level are rejected rather than guessed.

## Changing a golden

1. Change the behavior and run `cargo test --test thinking_translation_contract`.
2. Review each reported `expected` / `actual` diff against the provider's documentation.
3. Edit the fixture `expect` by hand and explain the compatibility change in the PR.
