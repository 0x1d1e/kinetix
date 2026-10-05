# FAQ

**What is Kinetix, in one sentence?**
A self-hosted, single-binary proxy that speaks OpenAI and Anthropic wire formats
in front of admin-configured upstream LLM APIs, adding virtual keys, account
pools, executable Routes with fallback, cost tracking, and an embedded dashboard.

**Do I need `.env` or a config file?**
No. A normal install is configured with the CLI and stored under XDG directories.
A `.env` and a bootstrap TOML file are optional overrides.

**Where is my data stored?**
`~/.config/kinetix` (config/keys), `~/.local/share/kinetix` (database, exports,
backups), `~/.local/state/kinetix` (logs) — all overridable and easy to back up
or remove.

**Does it bundle provider presets or price lists?**
No. Every provider, model, capability, parameter, and price is admin-defined.

**Which upstream providers work?**
Anything that speaks the OpenAI, Anthropic, or Gemini wire format (the three
outbound adapters), selected by the provider's configured `wire_format`.

**Does it cache responses locally?**
No — local response caching is intentionally excluded from v1. Kinetix *does*
preserve upstream prompt-cache hints and offers cache-aware sticky routing.

**Can it enforce a hard budget?**
It enforces RPM/TPM and daily/monthly budgets, but budget **reservation** is not
implemented, so concurrent requests may briefly overshoot. Documented and accepted.

**Why is `/healthz` still 200 when the database is down?**
Because inference still works from the in-memory snapshot. The body reports
`control_plane: degraded`; the HTTP status stays 200 so the instance isn't dropped
from a load balancer.

**Can clients see which account served them?**
No. Serving topology is admin-only. Clients get an opaque `X-Kinetix-Route-Id`
that an admin can resolve to a Route Trace.

**How do I point Pi (or any OpenAI client) at it?**
Set the base URL to `http://127.0.0.1:8080/v1` and the API key to a
`sk-kinetix-…` virtual key. Anthropic clients use `http://127.0.0.1:8080` with
`/v1/messages`.

**How do I get the dashboard?**
It's embedded — open `http://127.0.0.1:8080/admin` and log in with the admin
password.

**How do I update or uninstall?**
Update: install the new binary and restart (migrations run automatically after a
pre-migration backup). Uninstall: `kinetix uninstall` or `uninstall.sh`.

**Is there a plugin system?**
Yes. Kinetix features an opt-in WebAssembly Component Model plugin host powered
by Wasmtime 48. Plugins run in a strict sandbox (zero ambient authority,
preemptive epoch interruption, encrypted namespaced storage) and can contribute
custom provider wire adapters (`wire_plugin`), dynamic credential strategies
(`credential_plugin`), routing facts (`plugin.<id>.<name>`), health probes, and
model discovery. Native providers remain zero-overhead and completely unaffected
by installed plugins. See [Plugins](Plugins) for operator documentation and
[the current plugin contract](https://github.com/0x1d1e/kinetix/blob/main/wit/kinetix-plugin.wit).

**Is it multi-tenant / highly available?**
No. It's a single-machine, small-team gateway. Multi-tenant SaaS and HA are
explicit non-goals.

**What's the license?**
MIT.
