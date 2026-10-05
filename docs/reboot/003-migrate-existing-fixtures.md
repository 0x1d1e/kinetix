# 003: Fold existing fixtures into the corpus

Status: open
Sequence step: 2 ([reboot.md](../reboot.md#sequence))
Blocked by: [002](002-corpus-runner.md)

## Goal

Keep the existing contract assets as the executable spec, in one place.

## Scope

- Migrate or link field-contract, thinking-translation, wire, decode, plugin request/response, commit-point, and continuation fixtures into corpus cases where they describe a transaction.
- Fixtures that check one piece (for example field dispositions) can stay as they are if the corpus references them. No duplicated sources of truth.
- Keep the field-contract disposition requirement from `AGENTS.md` intact.

## Out of scope

- New behavior.

## Acceptance

- [ ] No existing assertion is lost: every removed test maps to a corpus case.
- [ ] `cargo test` and the corpus pass, with failures only in the manifest.
