# Authentication

Kinetix has two independent auth surfaces: **virtual keys** for inference
clients, and **admin auth** for the dashboard/API.

## Virtual keys (clients)

- A virtual key has the form `sk-kinetix-…`, is shown **once** at creation, and
  is stored only as a SHA-256 hash.
- Clients present it as `Authorization: Bearer <key>` (OpenAI clients) or
  `x-api-key: <key>` (Anthropic clients). Both are accepted on `/v1/*`.
- Lookup is by hash with a constant-time comparison.

A key can carry optional limits (see [Usage, Cost and Accounting](Usage-Cost-and-Accounting)):

- allowed models/aliases/Routes (`*` wildcard and prefix matching, e.g. `gemini-*`)
- allowed providers (a Route never serves a key a provider it is not permitted)
- requests/minute (RPM) and tokens/minute (TPM)
- daily and monthly USD budgets
- expiry
- an IP allowlist (exact addresses and CIDR)
- a per-key body-logging flag

Enforcement order on `/v1/*`:

```
per-IP abuse limit  →  virtual-key auth  →  IP allowlist  →  limits (status/expiry/models/RPM/TPM/budget)
```

Client IP is taken from the trusted ingress headers `CF-Connecting-IP`, then the
first `X-Forwarded-For` hop, then `X-Real-IP` (sound because cloudflared is the
sole ingress).

Revocation and limit changes take effect within a few seconds without a restart.

## Client connection profiles

In **Virtual Keys**, select a key, a client, and one of that key's currently
usable models or Routes to generate configuration for Pi, Claude Code, Codex, or
OpenCode. The generator uses the effective public base URL and preserves each
client's endpoint format: Chat Completions for Pi and OpenCode, Responses for
Codex, and Anthropic Messages for Claude Code. OpenCode profiles target the
stable v1 custom-provider schema, not the separate v2 configuration format.

Profiles are previews and downloads only; Kinetix does not edit local files.
When the full virtual key is available, it is included in the generated
credential file, not saved by Kinetix. Otherwise, the output contains a
placeholder. Kinetix cannot recover an existing key from its stored hash.
Profiles include no upstream credentials or guessed model capabilities.

## Admin auth

The dashboard/API uses a **password + in-memory session** model:

- The admin password is generated on first run and printed once; only its hash is
  stored (DB setting `admin_password_hash`, with a `<config_dir>/admin_password.hash`
  file fallback).
- Logging in mints a random session token stored **in memory** and sets an
  `httpOnly`, `SameSite=Lax` cookie. Sessions have a TTL
  (`KINETIX_SESSION_TTL_MINUTES`, default 12h).
- A **server restart invalidates every session**, and **changing the password
  invalidates all sessions** — so the dashboard requires re-login after a restart.
- For CLI/curl, the header `x-kinetix-admin-token` may carry either a live session
  token or the raw admin password. A raw password is **never** accepted from the
  cookie.

```bash
# Log in and reuse the cookie
curl -c cookie.txt -X POST http://127.0.0.1:8080/admin/api/login \
  -H 'content-type: application/json' -d '{"password":"..."}'
curl -b cookie.txt http://127.0.0.1:8080/admin/api/overview
```

## Cloudflare Access (production)

Set `KINETIX_CF_ACCESS_AUD` and `KINETIX_CF_ACCESS_TEAM_DOMAIN` to require a valid
Cloudflare Access JWT (`cf-access-jwt-assertion`) in addition to the password/
session check. Kinetix validates the JWT (RS256) against the team's JWKS with the
configured audience. Leave it unset to rely on the password/session alone
(convenient in dev).

## Design notes

- Upstream credentials are encrypted at rest with the master key and never
  exposed by the API (only a mask).
- Admin **mutations fail closed** when the store is degraded; admin reads and
  inference keep working.
- Secrets, auth headers, and prompt/completion bodies are never logged by default
  (see [Observability](Observability) and [Security](Security)).
