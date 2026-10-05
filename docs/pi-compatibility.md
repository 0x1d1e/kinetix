# Pi compatibility notes

This document records Kinetix's compatibility with **Pi** (the coding agent this
proxy targets), based on direct observation of Pi's real wire behavior rather
than specification reading. It is internal documentation, not a public
certification claim.

## Configuring Pi against Kinetix

Pi is configured as an OpenAI-compatible provider. Add this to Pi's
`models.json` (see Pi's docs for the exact location):

```json
{
  "providers": {
    "kinetix": {
      "baseUrl": "http://127.0.0.1:8080/v1",
      "api": "openai-completions",
      "apiKey": "sk-kinetix-…",
      "models": [
        { "id": "coder", "name": "Kinetix coder", "reasoning": true,
          "input": ["text", "image"], "contextWindow": 1000000, "maxTokens": 65536 },
        { "id": "free", "name": "Kinetix free", "reasoning": true,
          "input": ["text"], "contextWindow": 200000, "maxTokens": 64000,
          "compat": { "sendSessionAffinityHeaders": true, "sessionAffinityFormat": "openrouter" } }
      ]
    }
  }
}
```

## Verified acceptance

All of the following were exercised end to end with the real `pi` binary
(`pi -p`, non-interactive) pointed at a running Kinetix:

- **Plain streaming chat.** `pi -p --provider kinetix --model free "…"`
  returns the model's answer. Streaming OpenAI Chat Completions is the primary
  path.
- **Tool use.** `pi -p "Read the file /tmp/pitest.txt and tell me the secret
  word."` caused Pi to emit a tool call (`read`), Kinetix streamed the tool
  call back in OpenAI format, Pi executed the tool, sent the tool result in a
  follow-up turn, and the model produced a grounded answer. This is a
  multi-turn streaming session with tool use.
- **Multi-turn conversation.** Two turns against the same Pi session id
  (`--session-id`) preserved conversation state (turn 1 stated a fact; turn 2
  recalled it), confirming multi-turn streaming history round-trips.
- **GET /v1/models.** Pi's model picker sees the models Kinetix exposes to the
  key.

## Acceptance tiers

### Tier 1: hermetic CI

`scripts/compat-matrix.sh` (part of `scripts/run-ci.sh`) sends Pi-shaped
requests through Kinetix to the strict synthetic upstream, with no credentials.
Pi requests always stream and carry `user-agent: pi (<os>; <arch>)`, plus
`x-session-id` when session affinity is enabled. The strict upstream rejects a
request whose semantics did not arrive natively.

| Capability | Cases (`scripts/compat-matrix.py`) |
| --- | --- |
| Streaming | `plain_stream`, `tool_use_stream` |
| Multi-turn tools | `tool_loop` (OpenAI and Gemini targets), `multi_turn`, `parallel_tools` |
| Images | `vision` (OpenAI and Gemini targets) |
| Reasoning | `reasoning_control` (OpenAI and Gemini targets) |
| Signatures / continuation | `signature_contin`: Kinetix restores the Gemini thought signature Pi never sends back |
| Fallback | `fallback_stream` |
| Sticky affinity | `session_affinity`, `affinity_openrouter` |

### Tier 2: real Pi release gate

`scripts/release-client-acceptance.sh pi` drives the real `pi` binary against a
running Kinetix through a recording proxy (same-format, translated, fallback, and
affinity Routes). It then writes `pi-acceptance.json` to the artifact directory
with `scripts/pi-acceptance-report.py`:

- `pi_version`, and `kinetix_version` of the deployment under test (from
  `GET /admin/api/reference`; `unknown` if unreachable, which fails the release
  step)
- per case: `scenario`, `model`, `transport` (Pi `api`), feature cases, `result`

Feature cases: `streaming`, `multi_turn_tools`, `tool_continuation` (every tool
result answers a call id Kinetix returned earlier), `grounded_tool_results`,
`images` and `reasoning` (required when the generated Pi model entry declares
them), `reasoning_replay` (recorded, not required), `fallback`, and
`sticky_affinity`. The gate fails when a required feature fails. Set
`KINETIX_ACCEPT_PI_BASELINE=<previous pi-acceptance.json>` to also fail on any
feature that passed in the baseline and no longer does. The artifact holds no
API key, base URL, or account identity.

### Release candidates

Run tier 2 against the release-candidate build, then pass the artifact to the
release script. It checks that the artifact passed and matches the tag version
before building, and attaches it to the GitHub Release:

```bash
bash scripts/release-client-acceptance.sh pi   # KINETIX_BASE points at the RC deployment
scripts/release-local.sh vX.Y.Z --publish --pi-acceptance acceptance-artifacts/<stamp>/pi-acceptance.json
```

Publishing without it requires an explicit `--skip-pi-acceptance`.

## Headers Pi sends (observed)

A capture against a local HTTP listener showed Pi's OpenAI-compatible client
sends:

- `authorization: Bearer sk-kinetix-…` — the virtual key in the OpenAI-native
  style.
- `user-agent: pi (<os>; <arch>)`.
- `x-session-id: <pi session id>` — **only** when the model entry sets
  `compat.sendSessionAffinityHeaders: true`. By default Pi sends no session
  header.

## Cache-aware sticky routing with Pi

Kinetix requires an **explicit** session header (it never guesses a
conversation identity) and accepts `x-kinetix-session`,
`x-session-id`, or `x-conversation-id`. Pi's `x-session-id` is therefore a
direct, documented match. To keep a Pi session pinned to the same Route target
(so a provider's prompt cache keeps paying off), set
`compat.sendSessionAffinityHeaders: true` and `sessionAffinityFormat:
"openrouter"` on the Pi model entry and enable cache affinity on the Route.

## Known fidelity note

The Anthropic inbound encoder reports `message_start.usage.input_tokens: 0`
because Gemini reports usage only at the end of the stream; the final
`message_delta` carries the output tokens and the full counts are in the
usage log. This is documented in `docs/compatibility.md`.
