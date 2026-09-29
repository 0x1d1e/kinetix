# Troubleshooting

## The server doesn't start

- **"Address already in use"** — another instance holds the port. Find it with
  `ss -ltnp | grep 8080` and stop it, or bind elsewhere (`--bind`/`KINETIX_BIND`).
- **"migration 1 was previously applied but has been modified"** — an
  already-applied migration file was edited in place (its checksum changed). In
  production, add a **new** migration. In dev, reset the database (see below).
- **Bare `kinetix` prints help** — that is expected; start the server with
  `kinetix serve`.

## Admin login fails

- The admin password is shown **once** at first run. If lost, reset it:
  `kinetix password set <new>` (works with the server stopped), or delete the DB
  in dev and re-init.
- A **server restart invalidates sessions** by design; log in again.
- If you supply `KINETIX_ADMIN_TOKEN` but login still fails, a stale
  `~/.config/kinetix/admin_password.hash` may exist. The provided credential is
  authoritative on the next start; or remove the stale file.

## Requests return 503 "TLS is mandatory"

Your provider uses a plain-HTTP URL. Use HTTPS, or — for local dev only — set
`KINETIX_ALLOW_INSECURE_TLS=true` (or the provider's `allow_insecure_tls`).

## Requests return 503 "all targets unavailable"

This error means routing had no usable candidate before an upstream attempt could
be made. Likely causes:

- every candidate account is already in cooldown, exhausted, disabled, or blocked
  by an open account circuit;
- predicates, capabilities, context-window limits, provider restrictions, or soft
  quota checks exclude every target;
- a free-tier/account quota is exhausted — wait for the reset window, or reset the
  account (`kinetix account reset <id>`).

A generic upstream 502/503/504, connection failure, or timeout does **not** put an
otherwise healthy account into cooldown. If an upstream was actually attempted,
Kinetix preserves that final upstream failure instead of replacing it with
`all targets unavailable`. Resolve `X-Kinetix-Route-Id` in the admin Route
Trace to distinguish upstream failure from local cooldown, exhaustion,
disablement, circuit-open, predicate/capability, context, or soft-quota skips.

## A disabled model is still served / a config change didn't apply

The registry snapshot reloads every **1 second**. Wait ~1–2s after a
config change. If you edited the DB out-of-band, the serving snapshot is refreshed
by the same loop.

## Reasoning models return empty content

A reasoning model can spend its whole token budget on thinking tokens before
emitting text (finish_reason `length`, empty content). Raise `max_tokens`.

## Plugin-backed provider fails or is skipped

If a provider bound to a plugin (`wire_plugin` or `credential_plugin`) is skipped or fails:

1. **Verify plugin status**: Run `kinetix plugin show <id>`. The plugin must have `status: enabled`. If it is `disabled`, run `kinetix plugin enable <id>`.
2. **Check approved permissions**: Run `kinetix plugin permissions <id>`. Approval is all-or-nothing: if any declared permission is missing or was revoked, run `kinetix plugin approve <id>` followed by `kinetix plugin enable <id>`.
3. **Check circuit breaker**: Check the `runtime` section in `kinetix plugin show <id>` or `GET /admin/api/plugins/{id}/metrics`. If `circuit_state` is `open`, the plugin suffered repeated unhandled traps or timeouts. Run `kinetix plugin validate <id>` to test component linking, and check logs for guest panic messages.
4. **Inspect the Route Trace**: Request diagnostics via `GET /admin/api/requests/{id}/route-trace`. If a plugin was disabled or failed during execution, the trace records the exact explanation.

## Plugin installation or validation fails

- **"archive is invalid or tar traversal detected"**: The `.kxp` package format must be a clean archive containing `plugin.toml` and `plugin.wasm` at the root, with no absolute paths or symlinks. Build packages from the `PrightCord/kinetix-plugins` repository using `scripts/build-plugin.sh`.
- **"untrusted signature"**: If the plugin contains `plugin.sig`, provide the publisher's Ed25519 public key via `--trusted-key <key>` (base64 or hex), or use `--allow-untrusted-signature` in development.
- **"component linking failed"**: The component was compiled against an incompatible WIT version or missing required exports. Recompile the plugin against the SDK/WIT in `PrightCord/kinetix-plugins` (currently `wit-bindgen 0.62`).

## A client stays logged in after a restart

It shouldn't — sessions are in-memory and a restart invalidates them. If you see
this, ensure you're on a build that uses the in-memory session store (older
builds used a deterministic token). Clear cookies and log in again.

## Client disconnect cancellation

Kinetix cancels pre-commit work when it observes the client connection close.
If cancellation happens before dispatch, it drops the RPM/TPM/budget reservation.
After dispatch, Kinetix treats usage as incomplete and keeps the conservative
reservation; it does not infer whether the upstream processed or billed the
request. Concurrency capacity is released either way. After response commit,
dropping the response stream cancels active stream work and releases its
concurrency capacity.

## Test/throwaway instances write to my real config

Launch throwaway instances with `--home` (or `KINETIX_HOME`), which also ignores
any `.env`:

```bash
kinetix --home /tmp/kx1 init
kinetix --home /tmp/kx1 serve
```

## Reset a dev database

```bash
systemctl stop kinetix        # or stop your process
rm -f /var/lib/kinetix/data/kinetix.db /var/lib/kinetix/data/kinetix.db-wal /var/lib/kinetix/data/kinetix.db-shm
systemctl start kinetix       # re-seeds from the bootstrap file, if any
```

> Removing the DB changes the once-shown bootstrap virtual key.

## Where are the logs?

`$KINETIX_STATE_HOME/kinetix.log`, or journald/stdout. Set `KINETIX_LOG_JSON=true`
for structured logs.

## Still stuck?

- `kinetix doctor` checks dirs, the database, and server reachability.
- `GET /admin/api/requests/{id}/diagnostics` shows the flight recorder for a
  failing request.
- `GET /admin/api/overview` and `/admin/api/metrics` summarize health.
