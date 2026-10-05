# 004: Corpus: text and tools cases

Status: open
Sequence step: 2 ([reboot.md](../reboot.md#sequence))
Blocked by: [002](002-corpus-runner.md)

## Goal

Cover the text and tools case groups from reboot.md.

## Scope

- Text: roles (system/developer/user/assistant), mixed and empty content, sampling params, metadata.
- Tools: single, parallel, results, errored results, malformed args, malformed history, `tool_choice` auto/none/required/named.
- Each case for Chat, Responses, and Anthropic frontends against OpenAI, Anthropic, and Gemini Targets where meaningful.

## Acceptance

- [ ] Cases exist for every listed item; failing ones are in the manifest.
