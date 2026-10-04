# Testing and Benchmarks

Kinetix is verified by unit tests, golden wire fixtures, protocol torture/fuzz
tests, an end-to-end smoke test, real client acceptance (Pi), and reproducible
benchmark rigs.

## Test suites

```bash
cargo test                 # unit + integration
```

- **Unit tests** live in `#[cfg(test)]` modules across `src/` (cost, crypto,
  passthrough, predicates, trace, sse, adapters, types, limits, ratelimit, …).
- **Golden wire fixtures** (`tests/wire_fixtures.rs`) lock the exact encoded bytes
  of streaming/non-streaming responses and error frames for both formats, so any
  supported wire-output change fails CI.
- **Inbound decode fixtures** (`tests/decode_fixtures.rs`) lock request decoding
  for both formats (system hoisting, parallel tool calls, images, reasoning
  effort, unknown-extra capture, thinking signatures).
- **Protocol torture + fuzz** (`src/torture.rs`): 1-byte chunks,
  UTF-8/JSON splits, char-by-char tool arguments, interleaved parallel tools,
  reasoning/text interleaving, usage-only-in-final-event, zero-token responses,
  unknown fields, malformed SSE, large tool calls, error classification
  (429/5xx/timeout vs 400, rate-limit vs quota), keepalive cadence, and bounded
  fuzzing.

## End-to-end smoke

```bash
scripts/smoke.sh                # starts a synthetic upstream + a fresh instance
```

Asserts health, auth failures, model listing, passthrough/translation/tool-call
streams, Anthropic inbound, admin login/overview/metrics/usage, Route Trace and
diagnostics, Validate/Dry Run, opaque route-id resolution, and a malformed-URL
rejection. Wired into CI after the release build.

## Local CI mirror

```bash
scripts/run-ci.sh              # format, dashboard, rust (clippy+test+compat-matrix), cargo-deny
scripts/run-ci.sh --skip-deps  # skips the dependency-policy (cargo-deny) job
```

Run this before pushing — it mirrors `.github/workflows/ci.yml`.

## Coding-agent compatibility matrix

`scripts/compat-matrix.sh` starts deterministic synthetic OpenAI, Gemini, and
Anthropic upstreams plus a fresh Kinetix instance. The original #75/#83 profiles
remain in `scripts/compat-matrix.py`; `scripts/protocol-v1-matrix.py` adds the
v1 field/path contract.

The v1 path matrix covers sync + streaming for all 18 built-in frontend/adapter
cells: Chat via OpenAI/Gemini/Anthropic, Messages via Anthropic/Gemini/OpenAI, and
Responses via OpenAI/Gemini/Anthropic. Additional cases exercise explicit
field-level positive/rejection semantics, parallel tools, vision variants, nested
schemas, fallback portability, token counting, and model discovery.

The source of truth is `tests/fixtures/protocol-v1-compatibility.json`. Generate
`docs/protocol-v1-compatibility.md` and `docs/generated/compatibility-matrix.md` with
`python3 scripts/render-protocol-v1-compat.py`; `--check` fails if generated
documentation or evidence references drift. Positive field fixtures are validated
at the synthetic upstream boundary so dropped translated fields fail the matrix.

This hermetic matrix remains part of `scripts/run-ci.sh` and uses no paid/public
model APIs.

## Real-client release acceptance

Real Pi, Claude Code, Codex/Responses, and optional external `.kxp` sessions are a
release gate, never normal CI. Configure separate selectors for the actual path being
proved:

```bash
export KINETIX_BASE=https://kinetix.example.com
export KINETIX_KEY=sk-kinetix-...
export KINETIX_ADMIN_TOKEN='admin credential'

export KINETIX_ACCEPT_PI_SAME_MODEL=direct-openai
export KINETIX_ACCEPT_PI_TRANSLATED_MODEL=translated-non-openai
export KINETIX_ACCEPT_PI_FALLBACK_MODEL=forced-fallback-route
export KINETIX_ACCEPT_PI_AFFINITY_MODEL=sticky-route

export KINETIX_ACCEPT_CLAUDE_SAME_MODEL=direct-anthropic
export KINETIX_ACCEPT_CLAUDE_TRANSLATED_MODEL=translated-non-anthropic
export KINETIX_ACCEPT_CLAUDE_FALLBACK_MODEL=forced-fallback-route
export KINETIX_ACCEPT_CLAUDE_AFFINITY_MODEL=sticky-route

export KINETIX_ACCEPT_RESPONSES_OPENAI_MODEL=responses-via-openai
export KINETIX_ACCEPT_RESPONSES_GEMINI_MODEL=responses-via-gemini
export KINETIX_ACCEPT_RESPONSES_ANTHROPIC_MODEL=responses-via-anthropic
export KINETIX_ACCEPT_RESPONSES_FALLBACK_MODEL=forced-fallback-route
export KINETIX_ACCEPT_RESPONSES_AFFINITY_MODEL=sticky-route

bash scripts/release-client-acceptance.sh all
```

Each client case must complete a multi-turn streaming tool loop, emit two distinct
tool calls, return two tool results, and ground both sentinel files. Fallback cases
must expose `X-Kinetix-Fallback: 1`. Affinity cases require a stable client session
header and matching admin Route Trace `final_target` values across turns. Claude
also gets an explicit exact `count_tokens` probe.

The local evidence proxy records request `stream`, response `Content-Type`, request/route
IDs, session headers, tool identities, tool-result counts, fallback/warning headers, and
errors without recording credentials. Every inference turn must prove `stream: true` and
`text/event-stream`.
Failures are classified as auth/model/transport/frontend/translation/routing/upstream/
client. Set `KINETIX_PLUGIN_E2E_PACKAGE=/path/to/plugin.kxp` to include a real guest.

## Benchmarks

Two rigs, both in `scripts/`:

- **Correctness/path coverage** — `scripts/bench.sh` + `scripts/synthetic_upstream.py`
  (a deterministic Python upstream) across passthrough, translation, tools, large,
  and tools-large-fragments paths.
- **Capacity/overhead** — `scripts/bench-rust.sh` + `scripts/bench_rust.rs` (a
  std-only Rust upstream + load generator) across a concurrency matrix, with
  optional `CPUSET` (1 vCPU) and `ALLOC_STATS=1` (allocations/request).

Cancellation latency: `scripts/cancel_bench.py` measures how long the upstream
keeps being written after a client disconnect.

Measured results, methodology, and caveats are documented in
[`docs/benchmarks.md`](https://github.com/PrightCord/kinetix/blob/main/docs/benchmarks.md).
Headline numbers: added TTFT/total overhead of **~0.1–2 ms**, throughput far above
the 50 req/s target with zero errors, idle RSS ~13 MB, cold start ~75 ms.

> The synthetic upstream is the bottleneck at very high concurrency on one
> machine, so those rows characterize the rig, not Kinetix's ceiling.

## CI

`scripts/run-ci.sh` is the normal verification gate. The same jobs are
available in `.github/workflows/ci.yml`, but automatic push/pull-request
triggers are disabled; GitHub CI runs only via `workflow_dispatch`. The Rust
job runs Clippy, Rust tests, builds the debug `kinetix` binary, and executes
the hermetic compatibility matrix. Real-client release acceptance is never
invoked from this workflow.
