# CLI Reference

`kinetix` is both the proxy **and** its administration tool. Running it with no
subcommand prints help; `kinetix serve` runs the proxy. Every other subcommand
opens the SQLite control plane **directly** (deriving the master key from the
config directory), so it works with the server stopped and needs no admin
password.

## Global options

| Option | Description |
| --- | --- |
| `--home <DIR>` | Root holding `config/`, `data/`, `state/` — overrides XDG and ignores any `.env` (env `KINETIX_HOME`). Use it for isolated/test instances. |
| `--config <FILE>` | Explicit config file path. |
| `--bind <ADDR>` | Override the bind address (e.g. `127.0.0.1:8080`). |
| `--database-url <URL>` | Override the database URL (`sqlite://...`). |
| `-V`, `--version` | Print the version. |

Configuration precedence is **CLI flag > environment variable > config file >
default**.

## Subcommands

### `serve`
Run the proxy. Same startup sequence as any other deployment.

```bash
kinetix serve
kinetix serve --allow-private-upstreams --allow-insecure-tls   # local dev only
```

| Flag | Effect |
| --- | --- |
| `--log-json` | Emit JSON logs. |
| `--allow-private-upstreams` | Permit private/internal upstream endpoints (SSRF guard bypass). Dev only. |
| `--allow-insecure-tls` | Permit plain-HTTP upstreams. Dev only. |

> A boolean flag can only force the value **on**; leaving it off does not shadow
> an enabling environment variable.

### `init`
Create the XDG directories and generate + print the admin password once.

### `status`
Print version, resolved paths, limits, and control-plane counts.

### `doctor`
Check that the config/data/state dirs exist, the database is reachable and
migrated, and whether a server is responding on the public base URL.

### `password`
```bash
kinetix password set <PASSWORD>   # min 8 chars; invalidates all sessions
kinetix password show             # whether a password is configured
```

### `key`
```bash
kinetix key create --name pi --owner me [--tag dev] [--allowed-models '*']
                   [--rpm N] [--tpm N] [--daily-budget N] [--monthly-budget N]
kinetix key list
kinetix key disable <ID> | enable <ID>
kinetix key revoke <ID>           # hard-delete the key AND its usage logs
```

### `provider`
```bash
kinetix provider add --name "My Provider" --base-url https://.../v1 \
  --wire-format openai|anthropic|gemini --auth-scheme none|bearer|custom_header|query_param \
  [--custom-header-name X] [--custom-param-name key] [--models-path /models] \
  [--timeout-ms 120000] [--api-key sk-...] [--account-label primary] \
  [--connection-parameters public-parameters.json]
kinetix provider list
kinetix provider remove <ID>
```
`--api-key` + `--account-label` create the provider's first account in one step.
With `--auth-scheme none`, omit credential/custom-auth flags; Kinetix creates a
secret-free routing account. For bounded public identifier files and templates,
see [Public connection parameters](Plugins#public-connection-parameters).

### `model`
```bash
kinetix model add --provider "My Provider" --upstream-id gpt-4o-mini \
  --display-name "GPT-4o mini"
kinetix model list
kinetix model remove <ID>
```

### `account`
```bash
kinetix account add --provider "My Provider" --label primary --api-key sk-... \
  [--priority 1] [--weight 1] [--soft-quota-usd 5.0] [--quota-type daily|monthly|none]
kinetix account list
kinetix account reset <ID>        # clear cooldown/exhaustion/circuit state
kinetix account remove <ID>
```

### `route`
```bash
kinetix route add --name resilient \
  --target "ProviderA/model-x" --target "ProviderB/model-y" \
  [--strategy priority|round-robin|weighted|least-used|adaptive] \
  [--portability-policy reject|strip_with_warning] [--cache-affinity] \
  [--max-attempts 5]
kinetix route list
kinetix route validate <NAME_OR_ID>
kinetix route dry-run --model <MODEL_OR_ROUTE> [--descriptor <JSON_FILE>]
kinetix route remove <ID>
```
`--target` is repeatable and ordered (`provider/upstream_id`). `route validate`
checks a saved Route against local provider, model, account, plugin, alias, and
execution-profile metadata. `route dry-run` simulates a request without upstream
calls or routing-state mutation; its optional descriptor uses the Admin API
request fields. See [Routing and Fallback](Routing-and-Fallback).

`route validate` prints structured errors and warnings, returning a non-zero exit
status when the Route is invalid. `route dry-run` prints candidate eligibility,
ordering, and selection diagnostics as JSON. These commands open the local
control-plane database directly and do not require a running server.

### `alias`
```bash
kinetix alias add --alias coder --target-type model --target "Provider/model-x"
kinetix alias add --alias fast  --target-type route --target resilient
kinetix alias list
kinetix alias remove <ALIAS>      # remove takes the alias NAME, not an id
```

### `plugin`
Manage WebAssembly plugins (`.kxp` packages). Plugins are installed disabled by default and require explicit permission approval and enablement.

```bash
# Install a .kxp package (installed-disabled by default)
kinetix plugin install <PATH> [--sha256 <HEX>] [--trusted-key <KEY>]... [--allow-untrusted-signature]

# Enumerate installed plugins
kinetix plugin list

# Show detailed manifest, approved permissions, and circuit state
kinetix plugin show <ID>

# Validate component instantiation and capability exports
kinetix plugin validate <ID>

# Approve declared permissions (all-or-nothing)
kinetix plugin approve <ID>

# Enable an installed plugin
kinetix plugin enable <ID>

# Disable a plugin (new requests stop referencing it)
kinetix plugin disable <ID>

# List approved permission grants
kinetix plugin permissions <ID>

# Revoke a single permission grant (disables plugin, retains KV state)
kinetix plugin revoke <ID> <PERMISSION>

# Remove a plugin and cascade-delete its permissions, circuit state, and KV storage
kinetix plugin remove <ID>
```

| Subcommand | Purpose |
| --- | --- |
| `install` | Validates archive integrity, records SHA-256, checks optional Ed25519 publisher signature against trusted keys (base64 or hex), stores component bytes in SQLite, and leaves plugin disabled. |
| `list` | Lists installed plugin id, version, status (`enabled` / `disabled`), and display name. |
| `show <id>` | Prints the full manifest summary, declared capabilities, requested permissions, and runtime circuit-breaker state as JSON. |
| `validate <id>` | Instantiates the WebAssembly component in a test store to verify linking and export conformance. |
| `approve <id>` | Approves all declared permissions for the installed version. |
| `enable <id>` | Validates exports, checks that all requested permissions are approved, and marks the plugin active in memory and SQLite. |
| `disable <id>` | Marks the plugin disabled; any bound provider or Route fails closed immediately. |
| `permissions <id>` | Prints approved permissions and their parameters. |
| `revoke <id> <perm>` | Revokes an approved permission grant. |
| `remove <id>` | Cascades removal of the plugin, its approved permissions, runtime state, and encrypted KV store. |

### `export`
```bash
kinetix export run [--day YYYY-MM-DD]   # default: yesterday (UTC)
kinetix export list
kinetix export prune
```
Writes `usage-<day>.jsonl`, `usage-<day>.csv`, and `summary-<day>.csv` under
`$KINETIX_DATA_DIR/exports`.

### `backup`
```bash
kinetix backup run     # VACUUM INTO a consistent snapshot
kinetix backup list
```

### `uninstall`
```bash
kinetix uninstall [--yes] [--remove-binary] [--keep-data] [--dry-run]
```
Removes the config/state directories (and the data directory unless
`--keep-data`), optionally the binary, with a confirmation prompt unless
`--yes`. See [Configuration](Configuration) and the `uninstall.sh` script.

## Examples

```bash
# A complete first-time setup in one screen
kinetix provider add --name Gemini --base-url https://generativelanguage.googleapis.com/v1beta \
  --wire-format gemini --auth-scheme custom_header --custom-header-name x-goog-api-key \
  --api-key "$GEMINI_KEY" --account-label primary
kinetix model add --provider Gemini --upstream-id gemini-2.5-flash --display-name "Gemini 2.5 Flash"
kinetix key create --name pi --owner me
kinetix serve
```
