# 010: Capture real client payloads

Status: open
Sequence step: 2 ([reboot.md](../reboot.md#sequence))
Blocked by: [002](002-corpus-runner.md)

## Goal

Use real coding-agent traffic as corpus input.

## Scope

- Capture requests and expected responses from Pi, Claude Code, Codex CLI, and OpenCode, using `scripts/release-client-proxy.py` or equivalent.
- Cover each release-gate row: text, tools, thinking/reasoning, streaming; Codex stateless `store: false` with encrypted reasoning.
- Redact credentials, account identities, and private content before committing. Record client versions.

## Acceptance

- [ ] Captures committed as corpus cases for all four clients.
- [ ] No secrets or account identifiers in the fixtures.
