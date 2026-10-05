# Contributing to Kinetix

Thank you for your interest in contributing to Kinetix! Kinetix is a streaming-first LLM reverse proxy and routing engine tailored for AI coding agents.

## Development Principles

1. **Protocol Fidelity**: Kinetix prioritizes exact protocol semantics. Do not guess provider features or mutate request parameters unless explicitly governed by admin policies.
2. **Streaming-First Architecture**: Responses must stream incrementally with low Time-To-First-Token (TTFT). Non-streaming is supported via stream aggregation.
3. **Topology Privacy**: Client-facing endpoints must never leak upstream account details, provider names, or routing topologies.
4. **Supply Chain Hygiene**: Dependencies must pass strict `cargo-deny` checks for licenses (OSI permissive only), vulnerability advisories, and banned crates.
5. **Living Agent Instructions**: Guidelines and constraints for AI coding agents are maintained in [`AGENTS.md`](AGENTS.md). Whenever architecture, tools, conventions, or commands change, update `AGENTS.md` accordingly.

## Prerequisites

- **Rust toolchain**: Stable 1.80+ (`rustup toolchain install stable`) with `rustfmt` and `clippy`.
- **Node.js**: 20 or 22 LTS with `npm` (for the embedded admin dashboard).
- **Python 3**: For running synthetic upstream servers and compatibility matrices.
- **cargo-deny**: Recommended locally (`cargo install --locked cargo-deny`).

## Local Setup

1. **Clone the repository**:
   ```bash
   git clone https://github.com/0x1d1e/kinetix.git
   cd kinetix
   ```

2. **Build the dashboard bundle**:
   The admin dashboard is embedded directly into the Rust binary via `rust-embed`. Build it before compiling Rust:
   ```bash
   cd dashboard
   npm ci
   npm run build
   cd ..
   ```

3. **Build the Kinetix binary**:
   ```bash
   cargo build
   ```

## Verification & CI Suite

Before submitting changes, ensure all tests and lint checks pass. We provide a local CI script that mirrors the GitHub Actions workflow:

```bash
# Run the full gate: format, dashboard build, clippy + tests + compat matrix, cargo-deny
scripts/run-ci.sh

# Skip the dependency-policy (cargo-deny) job, which in ci.yml only runs on push
scripts/run-ci.sh --skip-deps
```

### Individual Test Suites

- **Unit and Wire Fixture Tests**:
  ```bash
  cargo test
  cargo test --test decode_fixtures
  cargo test --test wire_fixtures
  ```

- **End-to-End Smoke Tests**:
  Tests proxy routing, translation, auth, model discovery, and admin APIs against a deterministic synthetic upstream:
  ```bash
  scripts/smoke.sh 127.0.0.1:8180
  ```

- **Coding-Agent Compatibility Matrix**:
  Tests the #75/#83 client profiles plus the v1 native/translated/fallback field contract:
  ```bash
  scripts/compat-matrix.sh 127.0.0.1:8186
  python3 scripts/render-protocol-v1-compat.py --check
  ```

- **Real-client release acceptance**:
  Required before release candidates, but intentionally excluded from normal CI because it uses installed Pi/Claude Code/Codex clients and can consume real provider quota. Configure explicit same-format/translated/fallback/affinity selectors as documented in `docs/compatibility.md`, then run:
  ```bash
  KINETIX_BASE=https://kinetix.example.com \
  KINETIX_KEY=sk-kinetix-... \
  KINETIX_ADMIN_TOKEN='admin credential' \
  KINETIX_ACCEPT_PI_SAME_MODEL=direct-openai \
  KINETIX_ACCEPT_PI_TRANSLATED_MODEL=translated-non-openai \
  KINETIX_ACCEPT_PI_FALLBACK_MODEL=forced-fallback-route \
  KINETIX_ACCEPT_PI_AFFINITY_MODEL=sticky-route \
  KINETIX_ACCEPT_CLAUDE_SAME_MODEL=direct-anthropic \
  KINETIX_ACCEPT_CLAUDE_TRANSLATED_MODEL=translated-non-anthropic \
  KINETIX_ACCEPT_CLAUDE_FALLBACK_MODEL=forced-fallback-route \
  KINETIX_ACCEPT_CLAUDE_AFFINITY_MODEL=sticky-route \
  KINETIX_ACCEPT_RESPONSES_OPENAI_MODEL=responses-via-openai \
  KINETIX_ACCEPT_RESPONSES_GEMINI_MODEL=responses-via-gemini \
  KINETIX_ACCEPT_RESPONSES_ANTHROPIC_MODEL=responses-via-anthropic \
  KINETIX_ACCEPT_RESPONSES_FALLBACK_MODEL=forced-fallback-route \
  KINETIX_ACCEPT_RESPONSES_AFFINITY_MODEL=sticky-route \
    bash scripts/release-client-acceptance.sh all
  ```
  The runner requires multi-turn/two-tool continuation, verifies fallback headers, resolves admin-only Route Traces to prove sticky `final_target`, and probes exact Claude `count_tokens`. Set `KINETIX_PLUGIN_E2E_PACKAGE=/path/to/plugin.kxp` to include external plugin host/guest acceptance.

## Submitting Changes & Push Policy

1. Create a feature branch off `main` or commit locally.
2. Keep commits atomic and write descriptive commit messages (e.g. `feat(frontends): add foo`, `fix(adapters): handle bar`).
3. If introducing changes to wire formats, add corresponding test cases in `tests/decode_fixtures.rs`, `tests/wire_fixtures.rs`, or the compatibility matrix.
4. Run `scripts/run-ci.sh` locally to ensure all checks pass before pushing.
5. Keep [`AGENTS.md`](AGENTS.md) synchronized if your changes affect architecture, commands, dependencies, schemas, or conventions.
6. **Conserve GitHub Actions resources**: Do not push every intermediate commit. Batch commits locally and push when a cohesive milestone is ready.

## Local Release & Publishing

Before cutting a release candidate, run the real-client acceptance command above and keep its `acceptance-artifacts/` output with the release verification record.

Releases are built locally on the maintainer system to conserve GitHub Actions runner minutes:

```bash
# Build multi-arch Linux packages (x86_64 and aarch64) locally
scripts/release-local.sh v0.1.0

# Build and publish directly to GitHub Releases via gh CLI
scripts/release-local.sh v0.1.0 --publish
```

The release tag must exactly match the package version in `Cargo.toml`. Existing tags are rebuilt from the tagged commit in a detached worktree. For a new tag, the script requires a clean working tree, builds the current commit in a detached worktree, and creates the tag only after all artifacts and checksums have been produced successfully.

The script installs dashboard dependencies with `npm ci`, builds Rust with the committed lockfile, builds both `x86_64-unknown-linux-gnu` (native) and `aarch64-unknown-linux-gnu` (via `cross`), packages `.tar.gz` archives and individual `.sha256` files, computes the canonical `SHA256SUMS`, and pushes the release assets to GitHub.
