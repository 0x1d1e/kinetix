# Kinetix

Kinetix is a self-hosted, streaming-first LLM gateway written in Rust with an embedded React admin dashboard. It exposes OpenAI and Anthropic-compatible APIs while routing requests across operator-configured providers, accounts, models, Routes, and plugins.

## Architecture

- `src/frontends/` — decode client wire formats into canonical Kinetix types.
- `src/pipeline.rs` / `src/router.rs` — routing, execution, retries, failover, accounting, and request lifecycle.
- `src/adapters/` — encode canonical requests into upstream provider protocols and normalize responses.
- `src/pool.rs` — account selection, health, cooldown, quota, and load-balancing.
- `src/plugins/` + `wit/` — sandboxed WASM plugin host and public plugin contract.
- `src/db.rs` + `migrations/` — SQLite control-plane persistence.
- `dashboard/` — React/TypeScript admin UI embedded into the Rust binary.
- `PrightCord/kinetix-plugins` — guest SDK, first-party plugins, catalog, and plugin packaging.

Keep protocol frontends, canonical representation, routing, adapters, plugin execution, and persistence as separate boundaries.

## Core invariants

- Streaming is the primary execution path.
- Never retry/fail over after client-visible response commitment.
- Preserve supported protocol semantics: preserve, translate, explicitly reject, or apply a documented compatibility policy. Never silently discard them.
- Wire format is not provider identity.
- Do not invent model capabilities, reasoning mappings, pricing, token limits, credential behavior, or provider defaults.
- Missing metadata remains unknown unless a verified source provides it.
- Backend validation is authoritative. Dashboard, import, CLI, and API paths must enforce the same semantic invariants.
- Never expose upstream credentials, decrypted secrets, internal provider URLs, account identities, or private routing topology downstream.
- Treat plugin input/output as untrusted and fail closed at the host boundary.
- Public WIT/plugin-contract changes require compatibility handling and matching tests. `wit/` is canonical; mirror it into `kinetix-plugins` with its `scripts/sync_host_contract.py`.
- Existing migrations are immutable. Schema changes use a new timestamped migration.
- Keep async data-plane work non-blocking and queues bounded.
- Provider-specific behavior must be backed by official documentation or observed upstream behavior. Similar projects such as 9Router, pi-free, and OmniRoute are useful implementation references, not authority.

## Development

Keep changes scoped to the requested work. Avoid unrelated refactors, renames, formatting churn, or behavior changes.

Bug fixes require a regression test when reasonably testable.

Protocol/translation changes must cover the relevant compatibility fixtures/matrix, including streaming where applicable.

Dashboard changes must typecheck and build.

Public API, configuration, endpoint, or behavior changes must update the relevant `README.md` / `docs/`.

Update this file only when a stable architecture boundary, development workflow, or durable project invariant changes. Do not turn it into a feature history or implementation diary.

## Validation

Normal completion gate:

```bash
scripts/run-ci.sh
```

Useful narrower checks:

```bash
cargo test --quiet
cargo fmt --all -- --check
cargo clippy --all-targets

cd dashboard
npx tsc --noEmit
npm run build
```

Use narrower validation only when the task explicitly justifies it.

Do not rely on remote GitHub Actions as the normal development loop.

Release builds and real-client/provider acceptance are release activities, not required for every change.

## Repository conventions

- Rust: idiomatic `snake_case`, `PascalCase`, and `SCREAMING_SNAKE_CASE`.
- Schema changes: new timestamped migration under `migrations/`.
- SQL: parameterized queries only.
- Client virtual keys use the `sk-kinetix-` prefix.
- Kinetix diagnostic response headers use `X-Kinetix-`.
- Use current domain terms: Provider, Account, Model, Route, Target, credential enrollment.
- Use Conventional Commits.

## Agent skills

### Issue tracker

Issues live in this repo's GitHub Issues; use `gh`. See `docs/agents/issue-tracker.md`.

### Domain docs

Single-context layout: root `CONTEXT.md` and `docs/adr/`. See `docs/agents/domain.md`.
