# Kinetix architecture

## Overview

Kinetix is a single Rust service and streaming-first LLM gateway. OpenAI and
Anthropic client frontends translate requests into a canonical internal model;
executable Routes plan target attempts over account pools. Built-in and
plugin-provided outbound adapters translate to upstream protocols; sandboxed
WASM plugins also add integration capabilities. SQLite backs the control plane,
and the React dashboard is embedded in the service.

## Request flow

```text
Client
  ↓
Frontend
  ↓
Canonical request
  ↓
Route / target planning
  ↓
Account selection
  ↓
Adapter
  ↓
Upstream

Upstream stream
  ↓
Adapter
  ↓
Canonical events
  ↓
Frontend encoder
  ↓
Client
```

The commit point is when a response becomes client-visible. After commitment,
Kinetix does not retry or fail over to another target.

## Module map

| Area | Primary location |
| --- | --- |
| HTTP routing | `src/router.rs` |
| Public API and frontends | `src/api.rs`, `src/frontends/` |
| Request execution | `src/pipeline.rs` |
| Route planning | `src/router.rs` and routing-related modules |
| Account health and selection | `src/pool.rs` |
| Canonical protocol model | `src/types.rs` and related canonical types |
| Outbound adapters | `src/adapters/` |
| Plugins | `src/plugins/` |
| Plugin contract | `wit/` |
| Model metadata and catalog | `src/model_catalog.rs` |
| Persistence | `src/db.rs`, `migrations/` |
| Dashboard | `dashboard/` |
| Compatibility evidence | `tests/`, `tests/fixtures/`, compatibility scripts |

## Durable boundaries

- A frontend is an inbound protocol boundary; it is not a provider.
- Wire format is not provider identity.
- Routing policy belongs to Kinetix core. Plugins extend integrations, not
  routing policy.
- Backend validation is authoritative across configuration entry points.
- Unknown metadata remains unknown; it is not treated as false or unsupported.
- Fallback is permitted only before the response commit point.

## Sources of truth

- **Protocol behavior:** `tests/fixtures/`, protocol matrices, and implementation.
- **Plugin ABI:** `wit/`.
- **Database history:** `migrations/`.
- **Current executable behavior:** implementation and tests.
- **Domain terminology:** [Glossary](../CONTEXT.md).
- **Contributor invariants:** [`AGENTS.md`](../AGENTS.md).
- **Agent enforcement:** [`pi-warden.md`](../pi-warden.md).

## Further reading

- [Protocol compatibility](protocol-v1-compatibility.md)
- [Pi compatibility](pi-compatibility.md)
- [Plugins](wiki/Plugins.md)
- [Routing and Fallback](wiki/Routing-and-Fallback.md)
- [Security](../SECURITY.md)
- [Deployment](../deploy/README.md)

Historical designs are preserved in [the archive](archive/README.md); they do
not define current behavior.
