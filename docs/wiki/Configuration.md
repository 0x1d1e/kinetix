# Configuration

## Where Kinetix stores things

Kinetix follows the XDG base-directory spec. All state is easy to back up or
remove:

| Purpose | Default | Env override |
| --- | --- | --- |
| Config (`config.toml`, `master.key`, `admin_password.hash`) | `~/.config/kinetix` | `KINETIX_CONFIG_DIR` |
| Data (`kinetix.db`, `exports/`, `backups/`, `plugins/packages/`) | `~/.local/share/kinetix` | `KINETIX_DATA_DIR` |
| State (logs) | `~/.local/state/kinetix` | `KINETIX_STATE_DIR` |

`--home <DIR>` (or `KINETIX_HOME`) roots all three under `<DIR>/config`,
`<DIR>/data`, `<DIR>/state` and **ignores any `.env`** — ideal for isolated or
test instances.

## Configuration precedence

```
CLI flag  >  environment variable  >  config file  >  default
```

A normal install needs no `.env` and no config file; everything is set with the
[CLI](CLI-Reference) and persisted in SQLite. The database is **authoritative**
after the first run.

## Environment variables

| Variable | Default | Purpose |
| --- | --- | --- |
| `KINETIX_BIND` | `127.0.0.1:8080` | Listen address. Keep it on localhost behind cloudflared. |
| `KINETIX_PUBLIC_BASE_URL` | bind value | Public URL used in diagnostics/links. |
| `KINETIX_DATABASE_URL` | `sqlite://<data>/kinetix.db?mode=rwc` | SQLite URL. |
| `KINETIX_MASTER_KEY` / `KINETIX_MASTER_KEY_FILE` | generated | 64-hex, base64 (32 bytes), or a ≥16-char passphrase used to encrypt upstream credentials at rest. |
| `KINETIX_ADMIN_TOKEN` | generated | Pre-set the admin password (≥8 chars). If unset, one is generated and printed once. |
| `KINETIX_BOOTSTRAP_FILE` | unset | TOML seeded into an empty database (see below). |
| `KINETIX_DATA_DIR` | XDG | Data directory (also where backups/exports live). |
| `KINETIX_MAX_INFLIGHT_INFERENCES` | `200` | Instance-wide active inference cap; excess requests fail fast with 429. Must be positive. Also configurable as `max_inflight_inferences` in `config.toml`. |
| `KINETIX_SHUTDOWN_GRACE_SECS` | `30` | Graceful-shutdown drain window. |
| `KINETIX_IP_RATE_LIMIT_PER_MIN` | `600` | Per-IP abuse limit applied before virtual-key auth (`0` disables). |
| `KINETIX_SESSION_TTL_MINUTES` | `720` | Admin session lifetime; sessions are in-memory, so a restart forces re-login. |
| `KINETIX_EXPORT_RETENTION_DAYS` | `30` | How long per-day usage exports are kept. |
| `KINETIX_LOG_JSON` | `false` | Emit JSON logs. |
| `KINETIX_ALLOW_PRIVATE_UPSTREAMS` | `false` | Allow private/internal upstream endpoints (SSRF bypass). **Dev only.** |
| `KINETIX_ALLOW_INSECURE_TLS` | `false` | Allow plain-HTTP upstreams. **Dev only.** |
| `KINETIX_CF_ACCESS_AUD` / `KINETIX_CF_ACCESS_TEAM_DOMAIN` | unset | Cloudflare Access JWT validation for the admin surface. |
| `KINETIX_ALERT_WEBHOOK_URL` | unset | Webhook for alerts. Unset disables alerting. |
| `KINETIX_ALERT_FALLBACK_RATE` | `0.25` | Fallback-rate alert threshold. |
| `KINETIX_ALERT_ERROR_RATE` | `0.10` | Error-rate alert threshold. |
| `KINETIX_ALERT_MIN_REQUESTS` | `20` | Minimum requests before rate alerts fire. |
| `KINETIX_ALERT_INTERVAL_SECS` | `60` | Alert evaluation interval. |
| `KINETIX_ALERT_P95_LATENCY_MS` | `100` | p95 added-proxy-latency alert threshold (sustained 10 min). |

A documented template is in
[`.env.example`](https://github.com/PrightCord/kinetix/blob/main/.env.example).

## Bootstrap file (optional)

For a reproducible, file-driven first start you can seed an empty database from a
TOML file (`KINETIX_BOOTSTRAP_FILE=config.toml`). It is used **only when the
database has no providers**; afterwards the database is authoritative. A
documented example is
[`config.toml.example`](https://github.com/PrightCord/kinetix/blob/main/config.toml.example).

Bootstrap accepts the same executable controls as the database: provider failure/security settings, account priority/weight/quota window, model parameter/thinking/extra-request policy, route fallback triggers and optional `max_concurrent_requests`, unpinned provider-pool targets, target predicates/overrides, and virtual-key provider/IP/logging restrictions including optional per-key `max_concurrent_requests`. Zero or omission leaves a key or Route uncapped. Legacy `continuity_policy = "error"` is accepted only for compatibility and maps to `portability_policy = "reject"`; new configuration should use `portability_policy` only.

```toml
[[virtual_keys]]
name = "Local Dev Key"
owner = "admin"
allowed_models = ["*"]

[[providers]]
name = "Google Gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta"
wire_format = "gemini"
auth_scheme = "custom_header"
custom_header_name = "x-goog-api-key"

  [[providers.accounts]]
  label = "primary"
  api_key = "..."

  [[providers.models]]
  upstream_id = "gemini-2.5-flash"
  display_name = "Gemini 2.5 Flash"

# Example provider backed by a WebAssembly plugin
[[providers]]
name = "Google Antigravity"
base_url = "https://autopush-alkalimakersuite-pa.sandbox.googleapis.com"
wire_format = "plugin"
auth_scheme = "bearer"
wire_plugin = "plugin:dev.kinetix.antigravity-oauth/antigravity"
credential_plugin = "plugin:dev.kinetix.antigravity-oauth/antigravity-oauth"

  [[providers.accounts]]
  label = "antigravity-dev"
  api_key = "refresh_token_or_client_payload"

[[aliases]]
alias = "coder"
target_type = "model"
target = "Google Gemini/gemini-2.5-flash"

[[routes]]
name = "resilient"
strategy = "priority"
portability_policy = "strip_with_warning"
targets = [
  { account = "primary", model = "Google Gemini/gemini-2.5-flash" },
]
```

## Moving configuration between installs

Use the admin API's user-authored export/import:

```bash
curl -b cookie.txt http://127.0.0.1:8080/admin/api/config/export            # secret-free
curl -b cookie.txt 'http://127.0.0.1:8080/admin/api/config/export?include_secrets=true'
curl -b cookie.txt -X POST http://127.0.0.1:8080/admin/api/config/import \
  -H 'content-type: application/json' -d '{"config": {...}, "apply": false}'   # dry run
```

Exports use config version 2 and contain opaque account references so Route
targets do not depend on source database IDs. Refs distinguish accounts even
when their labels match. Account policy includes `enabled`, quota settings
(including `quota_window_s`), priority, and weight. Exported `status` is
informational: restore preserves manual enablement but does not restore transient
cooldown or circuit state. Version 1 and unversioned imports remain supported.
Run with `apply:false` first to review validation problems, conflicts, warnings,
and missing resources. Ambiguous existing account labels are conflicts. Applying
uses one transaction; a failed import leaves the database unchanged. Imports
upsert providers, models, Routes, and aliases without deleting them or replacing
existing credentials with imported secrets. Changing a provider to
`credential_mode: none` may remove its credential accounts to enforce that mode.

## Plugin storage and host configuration

WebAssembly plugins require no external files on disk once installed:

* **Single-store persistence**: The `.wasm` component bytes, manifest JSON, version metadata, approved permissions, circuit-breaker runtime state, and encrypted key-value pairs are all stored in `kinetix.db` (`plugins`, `plugin_permissions`, `plugin_kv`, `plugin_runtime_state` tables).
* **Zero-config backup and replication**: Because plugin components and data reside inside SQLite, normal database backups (`kinetix backup run`) and SQLite replication fully encompass all installed plugins and their states.
* **Encrypted storage isolation**: Plugin KV entries are encrypted at rest with AES-256-GCM using a key derived from `KINETIX_MASTER_KEY` under the context label `kinetix-plugin-kv`. Plugins have private logical namespaces and cannot access host master keys or each other's KV pairs.
* **Default Host Policy (`HostPolicy`)**:
  * Max memory per store: 64 MiB (`min(manifest requested, 64 MiB)`)
  * Epoch interruption interval: 10 ms
  * Execution timeout: 10 s for pure/cached evaluations; 30 s for adapter stream setup
  * Max KV storage per plugin: 10 MiB
  * Max HTTP response body size: 10 MiB
  * Concurrency cap: 32 concurrent instances per plugin

## Backups and restore

- A pre-migration backup is written before any schema migration.
- `VACUUM INTO` snapshots run every 6 hours, keeping the newest 14, in
  `$KINETIX_DATA_DIR/backups`. A `RESTORE.txt` is written alongside them.
- Restore: stop the server, copy the snapshot over `kinetix.db`, delete the
  `-wal`/`-shm` files, fix ownership, and start.

See [Deployment](Deployment) for the full runbook.
