# 024: Split admin.rs and db.rs by domain

Status: open
Sequence step: parallel after 2 ([reboot.md](../reboot.md#sequence))
Blocked by: [003](003-migrate-existing-fixtures.md), [004](004-corpus-text-tools.md), [005](005-corpus-streaming.md), [006](006-corpus-schemas.md), [007](007-corpus-thinking.md), [008](008-corpus-continuation.md), [009](009-corpus-failures.md), [010](010-client-captures.md), [011](011-reference-regressions.md)

## Goal

Make one policy change read one module.

## Scope

- Split `src/admin.rs` and `src/db.rs` by domain: Providers, Accounts, Routes, Models, plugins.
- No behavior change; no schema change.

## Acceptance

- [ ] `tests/admin_api_contract.rs` and corpus unchanged and green.
- [ ] `admin.rs` and `db.rs` are module roots that only wire the domain modules together.
