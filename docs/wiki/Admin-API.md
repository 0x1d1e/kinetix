# Admin API

The admin API lives under `/admin/api/*` on the same host as the proxy. It is
guarded by admin auth (see [Authentication](Authentication)). In production,
expose it behind Cloudflare Access plus the in-Kinetix password/session check.

## Auth & session

| Method & path | Purpose |
| --- | --- |
| `POST /admin/api/login` | `{"password": "..."}` → sets the session cookie, `{ok, user}`. |
| `POST /admin/api/logout` | Clears the session. |
| `GET /admin/api/me` | `{authenticated, user}`. |
| `POST /admin/api/password` | `{current_password, new_password}` → change password (invalidates all sessions). |

## Overview & metrics

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/overview` | Active streams, totals, spend, fallback rate, latency, key/account counts, queue depth, uptime, usage-confidence counts. |
| `GET /admin/api/health/runtime` | Persisted telemetry for `window` (`5m`, `1h`, or `24h`; defaults to `1h`), live provider circuit states, and quota evidence. See [Observability](Observability). |
| `GET /admin/api/metrics` | Prometheus text (`kinetix_*`). See [Observability](Observability). |

## Virtual keys

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/keys` | List keys (masked; never the secret) with lifetime totals and current spend. |
| `POST /admin/api/keys` | Create a key; returns `{key, full_key}` (the secret is shown **once**). |
| `PUT /admin/api/keys/{id}` | Update limits/budgets/expiry/status/`allowed_ips`/`body_logging`. |
| `DELETE /admin/api/keys/{id}` | Delete the key (and its usage). |

## Providers

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/providers` | List with `accounts_count` / `models_count` / `healthy_accounts`. |
| `POST /admin/api/providers` | Create; optional `api_key` + `account_label` create the first account. Supports plugin capability bindings (`wire_plugin`, `credential_plugin`, `model_source_plugin`). |
| `GET /admin/api/providers/{id}` | Full config, plugin provenance, and normalized `credential_enrollment` state. |
| `PUT /admin/api/providers/{id}` | Update; a non-empty `api_key` rotates the first account's credential. Supports updating plugin bindings. |
| `DELETE /admin/api/providers/{id}` | Delete. |
| `POST /admin/api/providers/{id}/discover` | Fetch the upstream model list (via HTTP or bound `model_source_plugin`); flags already-imported and disappeared models. |
| `POST /admin/api/providers/{id}/test` | Minimal connectivity probe; returns status + latency + a bounded preview. |
| `POST /admin/api/providers/{id}/credential-enrollment/start` | Start the provider's declared auth flow. Rejects manual and credential-free providers and never falls back to API-key entry. |

## Models

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/models` | List (all providers), including effective values and the latest discovery/probe state. |
| `GET /admin/api/models/{id}/observations` | Append-only metadata-discovery and capability-probe history (`limit` defaults to 100, maximum 500; pass `next_cursor` as `cursor` to continue). |
| `POST /admin/api/providers/{id}/models` | Create a model for a provider. Optional `transport_override` selects `openai`, `openai-responses`, `anthropic`, `gemini`, or a `plugin:<id>/<adapter>` reference; omit/null to use discovered transport, then provider default. |
| `PUT /admin/api/models/{id}` | Update, including optional `transport_override`. |
| `DELETE /admin/api/models/{id}` | Delete the model; its observation history remains available by model id. |

Model observations retain normalized non-pricing evidence, source provenance, scope, and observation time. Effective model configuration remains in the model record; historical and effective prices continue to use price versions.

## Accounts

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/accounts` | List (label, `key_mask`, effective status, lifecycle reason/timestamp, retry time, quotas, totals); accepts optional `provider_id` and includes disabled accounts. |
| `POST /admin/api/accounts` | Create (requires `api_key`). |
| `PUT /admin/api/accounts/{id}` | Update fields; a non-empty `api_key` rotates the credential. Omitted `status` preserves lifecycle state; explicit status changes accept only `healthy` or `disabled`. Cooldown, quota, and circuit states are runtime-managed. |
| `POST /admin/api/accounts/{id}/reset` | Clear cooldown/exhaustion/circuit state; does not re-enable a manually disabled account. |
| `DELETE /admin/api/accounts/{id}` | Delete. |

Account responses include `status_reason`, `status_changed_at`, and `retry_at`. Effective `status` is one of `healthy`, `cooldown`, `exhausted`, `disabled`, or `degraded` (`degraded` is a circuit-open account). Expired cooldown and quota windows are reported as healthy with reason `cooldown_elapsed` or `quota_reset`; `retry_at` is `null` when no future recovery time is known. The reason code describes the latest lifecycle transition. `status_changed_at` records when persisted status/reason last changed; cooldown or quota expiry can alter the effective status without writing to the account row.

## Routes

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/routes` | List with resolved targets and policy fields. |
| `POST /admin/api/routes` | Create (name, strategy, fallback triggers, `portability_policy`, `cache_affinity`, `max_attempts`, targets). |
| `PUT /admin/api/routes/{id}` | Update (replaces targets). |
| `DELETE /admin/api/routes/{id}` | Delete. |
| `POST /admin/api/routes/validate` | Validate a proposed Route against persisted provider, model, account, plugin, alias, and execution-profile metadata. Returns `valid` plus structured errors and warnings; does not save the Route. |
| `POST /admin/api/routes/dry-run` | Simulate a representative request; returns candidate ordering, predicate outcomes, capability states, eligibility reasons, and the would-be selection without contacting an upstream. |

Route creation, updates, and config imports apply the same semantic validation before saving. The dry-run descriptor accepts request capability flags, input-token count, provider allowlist, an optional quota override, request-level `allow_fallback` (omitted defaults to enabled), and an optional session key. For Routes with sticky or cache affinity enabled, a known session mapping promotes its target; unknown sessions do not. Candidate output distinguishes supported, unsupported, and unknown capabilities and includes account quota and current circuit/concurrency availability. When account quota is exhausted, the simulation follows both `allow_fallback` and the Route's `onQuota` trigger; with fallback disabled it reports HTTP 429 and marks later candidates unreachable. `selection_mode` is stochastic when runtime weighted selection or equal-priority account weighting can change the chosen target; in that case `would_select` is null and no exact strategy rank is claimed. Simulation reads routing snapshots but does not reserve capacity, advance round-robin state, or update affinity.

## Aliases

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/aliases` | List. |
| `POST /admin/api/aliases` | Create (`alias`, `target_type`, `target_id`/`target`). |
| `DELETE /admin/api/aliases/{id}` | Delete. |

## Validate / Dry Run

| Method & path | Purpose |
| --- | --- |
| `POST /admin/api/validate` | Generic endpoint/connectivity validation (resolved IPs; ASN shown `unknown` when it cannot be resolved). |
| `POST /admin/api/validate/provider` | Provider schema + outbound security + credential-host binding. |
| `POST /admin/api/validate/model` | Model metadata; unknown prices/capabilities reported as `unknown` (never assumed). |
| `POST /admin/api/validate/account` | Account label/credential/quota validation. |

## Config export / import

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/config/export` | Export version 2 config (secret-free; `?include_secrets=true` adds encrypted blobs and opaque account references). |
| `POST /admin/api/config/import` | Accepts version 1 and 2; unversioned configs are treated as version 1. `apply:false` validates without writes and reports problems, conflicts, warnings, and missing resources. `apply:true` applies all upserts in one transaction and rolls back on failure. Imported secrets do not replace existing credentials; changing a provider to `credential_mode: none` may remove its credential accounts. |

## Usage, requests, traces

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/usage` (alias `GET /admin/api/requests`) | Usage rows + summary (`limit` capped). |
| `GET /admin/api/requests/live` | Live in-flight view (metadata only). |
| `GET /admin/api/requests/{id}/route-trace` | Route Trace for a request. |
| `GET /admin/api/requests/{id}/diagnostics` | Flight-recorder diagnostics + trace + usage. |
| `GET /admin/api/route-traces/{opaque_id}` | Resolve an opaque `krt_…` id to its Route Trace. |

## Audit, exports, testing

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/audit` | Append-only audit rows. |
| `GET /admin/api/exports` | List per-day export files + days. |
| `POST /admin/api/exports` | Export a day (default yesterday UTC). |
| `DELETE /admin/api/exports/{name}` | Delete an export file. |
| `POST /admin/api/test-stream` | Run a real request through the pipeline for a key id + model (used by the Live Tester; the raw key never enters the browser). |

## Plugins

Manage WebAssembly Component plugins (`.kxp` packages).

| Method & path | Purpose |
| --- | --- |
| `GET /admin/api/plugins` | List all installed plugins with manifest summaries, status, and provided capabilities. |
| `GET /admin/api/plugins/catalog` | Return embedded official discovery metadata. Catalog metadata does not bypass package signature/hash/permission review. |
| `GET /admin/api/plugins/catalog/{id}/preview` | Download and fully verify the catalog artifact without mutation, then return target permissions and the semantic authority delta relative to the installed version. |
| `POST /admin/api/plugins/catalog/{id}/install` | Download and install an install-ready catalog package. Kinetix constrains HTTPS redirect hosts, package size, SHA-256, catalog id/version, and requires a signature from the separately trusted publisher key. Installs disabled. |
| `POST /admin/api/plugins/install` | Install or upgrade a `.kxp` package from `package_base64` or a server-local `path`. Accepts `sha256`, `trusted_keys` array, and `allow_untrusted_signature`. Plugins are installed disabled; the response includes the computed SHA-256 and the exact package is retained in the content-addressed package store. |
| `POST /admin/api/plugins/auth/start` | Start a named plugin browser-account flow for an existing provider binding. Requires admin auth and returns the provider authorization URL. |
| `POST /admin/api/plugins/{id}/integrations/{integration}/provider` | Create or reuse the provider declared by an Integration template. Re-validates outbound URL security and requires every derived plugin capability binding to be enabled and approved. |
| `GET /admin/api/plugins/auth/callback` | One-time provider callback authenticated by expiring random state. Exchanges the code inside WASM, validates/encrypts returned credential JSON, creates the account, and redirects to Plugins. |
| `GET /admin/api/plugins/{id}` | Plugin detail: manifest metadata, requested/approved permissions, runtime circuit state, and retained `.kxp` package provenance/history. |
| `DELETE /admin/api/plugins/{id}` | Remove a plugin and cascade-delete its permissions, circuit state, and encrypted KV storage. |
| `POST /admin/api/plugins/{id}/enable` | Enable an installed plugin. Verifies component linking and registers capabilities. |
| `POST /admin/api/plugins/{id}/disable` | Disable a plugin. Bound providers/routes fail closed immediately. |
| `POST /admin/api/plugins/{id}/validate` | Re-instantiate the component in a test store to verify exports and linking. |
| `GET /admin/api/plugins/{id}/packages/{sha256}/preview` | Re-hash/revalidate a retained package and return its manifest plus semantic permission delta relative to the current active manifest. |
| `POST /admin/api/plugins/{id}/rollback` | Reactivate a retained package by SHA-256 after path/hash/manifest/compile checks. The plugin is left disabled and all permission grants are cleared. |
| `GET /admin/api/plugins/{id}/settings` | Read declarative host-owned plugin settings. Secret values are never returned; only `configured` is exposed. |
| `PUT /admin/api/plugins/{id}/settings` | Partially update manifest-declared plugin settings. Values are type-checked and encrypted; audit logs record keys/counts, never values. |
| `GET /admin/api/plugins/{id}/permissions` | View requested permissions from manifest vs currently approved grants. |
| `POST /admin/api/plugins/{id}/permissions/approve` | Approve all permissions declared by the plugin manifest (all-or-nothing). |
| `POST /admin/api/plugins/{id}/permissions/revoke` | Revoke a single permission grant (`{"permission": "..."}`). Disables the plugin while retaining its KV state. |
| `GET /admin/api/plugins/{id}/audit` | Filtered audit log entries where target is this plugin. |
| `GET /admin/api/plugins/{id}/metrics` | Per-plugin runtime state, storage bytes, host invocation totals, and `by_capability` counters for successes/faults/timeouts/cancellations/HTTP attempts/cumulative duration. |

## Error shape

Errors are JSON `{ "error": "..." }` with the HTTP status from the error kind
(401 unauthorized, 403 forbidden, 404 not found, 429 rate-limited, 503
all-targets-unavailable / service-unavailable, 502 upstream/internal). Non-GET
admin requests return 503 when the control-plane store is degraded (fail closed).
