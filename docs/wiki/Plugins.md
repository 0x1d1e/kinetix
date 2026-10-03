# Plugins

Kinetix features an opt-in, sandboxed WebAssembly plugin system based on the **WebAssembly Component Model** (powered by Wasmtime 48).

Plugins allow operators to extend Kinetix without modifying the core gateway or sacrificing security, memory safety, or routing predictability:

* **Custom Wire Protocols (`wire_plugin`)**: Implement proprietary or non-standard outbound wire protocols (such as Google's internal `v1internal` API) as pure translation libraries, while Kinetix manages HTTP transport and SSE streaming.
* **Dynamic Credential Strategies (`credential_plugin`)**: Handle automated token acquisition and refresh (e.g., OAuth 2.0 refresh-token exchanges, cloud IAM credentials) with host-managed encrypted leases.
* **Routing Facts (`plugin.<id>.<name>`)**: Supply custom typed facts for target predicate evaluation in executable Routes.
* **Account Health Probes**: Perform scheduled background quota and availability health checks.
* **Model Discovery (`model_source_plugin`)**: Interrogate upstream APIs to discover available models.
* **Read-Only Lifecycle Hooks**: Observe request, candidate, and usage events asynchronously on a non-blocking queue.

Native adapters and standard configuration-driven providers remain zero-overhead and completely isolated from installed plugins.

---

## Architecture and Sandbox Model

Plugins execute inside a strictly isolated WebAssembly sandbox with **zero ambient authority**:

```
+-------------------------------------------------------------+
|                        Kinetix Core                         |
|  [Pipeline]   [Router / Predicates]   [Account Pools / DB]   |
+------------------------------+------------------------------+
                               | Typed WIT Seams
                               v
+-------------------------------------------------------------+
|                 Wasmtime Component Sandbox                  |
|  +-----------------------+     +-------------------------+  |
|  |     Plugin World      |     |  Plugin-Adapter World   |  |
|  | - CredentialStrategy  |     | - Pure wire translation |  |
|  | - RoutingFactProvider |     | - No network imports    |  |
|  | - ModelSource         |     | - SSE framing owned by  |  |
|  | - HealthProbe         |     |   Kinetix core host     |  |
|  | - Read-only Hooks     |     +-------------------------+  |
|  +-----------------------+                                  |
|          | (Host-mediated capabilities)                     |
|          v                                                  |
|  [Encrypted Namespaced KV]    [Host HTTP (Approved Hosts)]  |
+-------------------------------------------------------------+
```

### Safety Guarantees

1. **Hardware-Enforced Memory Isolation**: Plugins run in WebAssembly linear memory. They cannot inspect host process memory, execute arbitrary system calls, or access the local filesystem or environment.
2. **Mediated Network Access**: Plugins have no direct socket access. Outbound HTTP requests must pass through the host HTTP capability and the plugin's approved `network_hosts`. Immediately before each request, Kinetix resolves the destination, rejects the entire answer set if any address is private/reserved (unless the explicit development override is enabled), and pins reqwest to those validated addresses. Plugin HTTP disables system proxies and redirects, requires HTTPS, rejects guest `Host` overrides, and applies a request timeout bounded by the effective plugin wall-time. API v1/v2 retain their legacy adapter imports; API v3 adapters have no host imports.
3. **Encrypted Storage Isolation**: Each plugin receives an isolated logical namespace in SQLite (`plugin_kv`). Values are encrypted with AES-256-GCM using a key derived from `KINETIX_MASTER_KEY` under the context label `kinetix-plugin-kv`.
4. **All-or-Nothing Permissions**: Operators approve the entire declared permission set before a plugin can be enabled. Revoking any grant disables the plugin immediately.
5. **Preemptive Execution Limits**: Execution is preempted by Wasmtime epoch interruption (10 ms ticks, default 10 s deadline for evaluations; 30 s for adapter stream setup). Memory is capped at 64 MiB per store.
6. **Per-Plugin Circuit Breaker**: Persisted in SQLite (`plugin_runtime_state`). Consecutive unhandled traps or errors trip the circuit to `open`, causing bound providers to fail closed cleanly without risking proxy stability.
7. **Client Cancellation Neutrality**: Client disconnects trigger epoch interruption and are recorded as cancellations, never penalizing the plugin's circuit-breaker state.

---

## ABI Compatibility

The `plugin_api` major selects the provider-adapter ABI. API v1 uses the unchanged `plugin-adapter` world from `kinetix:plugin@1.0.0`. API v2 uses `plugin-adapter-v2` from `kinetix:plugin@2.0.0`, adding optional session context while retaining the host imports and runtime semantics already available to v2 packages. API v3 uses `plugin-adapter-v3` from `kinetix:plugin@3.0.0`, retaining the session-aware signatures in an import-free adapter world; core provides reserved `_kinetix` context in `provider-json`.

Kinetix supports API v1, v2, and v3 concurrently and selects the adapter world from the manifest. API v1 and v2 packages keep their existing WIT and host-capability behavior. API v1-only hosts reject newer API majors. Do not ship incompatible exports under an existing `plugin_api` major or use a minor WIT package version for a breaking ABI change.

## Capability Seams

Plugins interact with Kinetix through versioned typed interfaces: API v1 is defined in `wit/kinetix-plugin.wit`; the API v2 adapter world is in `wit/v2/kinetix-plugin.wit`; and the import-free API v3 adapter world is in `wit/v3/kinetix-plugin.wit`.

### 1. Provider Adapter (`wire_plugin`)
* **WIT World**: `plugin-adapter` for API v1; `plugin-adapter-v2` for API v2; `plugin-adapter-v3` for API v3
* **Exported Functions**: `wire-format`, `build-url`, `apply-auth`, `build-body`, `classify-error`, `parse-stream-chunk`, `parse-full-response`
* **Contract**: API v3 adapters are pure translation libraries: they convert canonical Kinetix requests (`src/types.rs`) into upstream request bodies and response chunks into canonical SSE events. Kinetix core owns HTTP transport, connection pooling, keepalives, and SSE framing. API v1/v2 retain their existing adapter imports and runtime semantics for compatibility. API v3 calls cannot use host storage, logging, clock, credentials, or buffered HTTP; multi-capability components retain imports for other worlds, but calls are denied in the v3 adapter context.

### 2. Credential Strategy (`credential_plugin`)
* **WIT World**: `plugin` (`interface credential-strategy`)
* **Exported Functions**: `resolve`, `refresh`, `revoke`
* **Contract**: Takes a configured credential handle and returns an authorization token. The secret token is stored as an encrypted lease (`lease:<handle>`) in the host-managed KV store and refreshed automatically before expiration.

### 3. Routing Fact Provider
* **WIT World**: `plugin` (`interface routing-fact-provider`)
* **Exported Functions**: `evaluate-facts`
* **Contract**: Computes typed facts exposed to executable Route predicates under the namespace `plugin.<plugin-id>.<fact-name>` (e.g. `plugin.dev.example.geo.region`).
  * `pure` providers: Evaluated on the request path with buffered HTTP disabled.
  * `cached` providers: Refreshed by Kinetix off the request path at `routing_facts_refresh_ms` (default 30s, allowed 5s–1h). Approved buffered HTTP is available only during that refresh. The returned facts and any `cache-set` publications are validated, host-stamped, and committed as one atomic snapshot. Values older than `max_age_ms` expire and evaluate as `unknown`; a failed refresh leaves the previous snapshot intact.

### 4. Health Probe
* **WIT World**: `plugin` (`interface health-probe`)
* **Exported Functions**: `check-health`
* **Contract**: Executes on a core-owned background schedule (never on the client request path) to verify provider accounts and report availability or quota evidence. Kinetix core retains ownership of cooldown and circuit policies.

### 5. Model Source (`model_source_plugin`)
* **WIT World**: `plugin` (`interface model-source`)
* **Exported Functions**: `discover-models`
* **Contract**: Invoked by `POST /admin/api/providers/:id/discover` to discover available upstream models and their capabilities.

### 6. Read-Only Hooks
* **WIT World**: `plugin` (`interface hooks`)
* **Exported Functions**: `on-request-normalized`, `on-target-candidate`, `on-usage-finalized`
* **Contract**: Dispatched asynchronously on a bounded fire-and-forget channel. Hooks can never modify payloads, delay responses, or crash in-flight requests.

---

## Package Format (`.kxp`)

A Kinetix Plugin package is an uncompressed tar archive containing:

```text
foo.kxp
├── plugin.toml          # Plugin manifest
├── plugin.wasm          # Compiled WebAssembly component
├── signature.ed25519    # Optional Ed25519 signature
├── README.md            # Optional documentation
└── LICENSE              # Optional license text
```

### Plugin API and host compatibility

`plugin_api = "1"` selects the Plugin API 1 ABI. Kinetix 1.x preserves that ABI: existing API-1 plugins remain loadable on later 1.x hosts unless the plugin declares a host-version bound that excludes the host. A manifest without bounds is not limited to the Kinetix version on which it was built.

API-1 revisions may add capabilities through separate optional worlds or interfaces. They must not remove or rename existing interfaces, change existing function signatures or meanings, or make a new guest export mandatory in an existing world. A breaking ABI change requires a new `plugin_api` and WIT package major; hosts may support multiple majors concurrently.

Kinetix pins its Wasmtime/component-model runtime. Runtime upgrades must pass conformance checks with existing API-1 components before release; a runtime upgrade is not a way to bypass the API compatibility promise.

Plugins can opt into an inclusive Kinetix host-version range using full semantic versions:

```toml
[compatibility]
min_host_version = "1.0.0"
max_host_version = "1.9.99"
```

Either bound may be omitted. An absent `[compatibility]` table means no host-version bound. A malformed range or a host outside the range is rejected during installation and validation.

### Manifest Example (`plugin.toml`)

```toml
manifest_version = 1
plugin_api = "1"
id = "dev.kinetix.antigravity-oauth"
name = "Antigravity OAuth"
version = "0.1.0"

[provides]
credential_strategies = ["antigravity-oauth"]
auth_flows = ["antigravity"]
account_model_sources = ["antigravity-models"]
provider_adapters = ["antigravity"]

[[integrations]]
id = "antigravity"
name = "Google Antigravity"
description = "Connect a Google Antigravity account and use the v1internal model API."
provider_adapter = "antigravity"
credential_strategy = "antigravity-oauth"
auth_flow = "antigravity"
model_source = "antigravity-models"

[integrations.provider]
base_url = "https://autopush-alkalimakersuite-pa.sandbox.googleapis.com"
wire_format = "plugin"
auth_scheme = "bearer"
timeout_ms = 120000
capability_mode = "permissive"
follow_redirects = false

[permissions]
network_hosts = ["accounts.google.com", "oauth2.googleapis.com", "www.googleapis.com", "daily-cloudcode-pa.sandbox.googleapis.com"]
credential_scopes = ["credential_strategy:antigravity-oauth"]
credential_read = true

[limits]
memory = "64MiB"
wall_time_ms = 10000
max_outbound_requests = 2
max_http_body = "1MiB"
storage = "1MiB"
```

### Installation validation

CLI installs, dashboard marketplace installs, and local package installs use the same backend validation before package bytes or active plugin metadata are published. Kinetix rejects malformed or unknown manifest fields, unsupported API/host ranges, invalid or duplicate capabilities, undeclared integration references, invalid permission and limit requests, malformed packages, and hash mismatches when an expected SHA-256 is supplied. Local unsigned packages are allowed; a present signature must be well-formed, and an untrusted signature requires an explicit override. Marketplace artifacts require the catalog's exact SHA-256 and a trusted publisher signature.

Kinetix compiles the component and instantiates every world required by its declared capabilities under validation limits with no network or credential authority and isolated temporary storage. Missing exports or failed initialization abort installation. Accepted plugins remain disabled and receive no permission grants until an operator approves them.

### Integration descriptors

An optional `[[integrations]]` entry groups low-level capabilities into a
user-facing integration. It is declarative metadata only: it executes no
dashboard code and grants no additional authority.

Every referenced `provider_adapter`, `credential_strategy`, `auth_flow`, or
`model_source` must be declared by the same plugin in `[provides]`. Kinetix
rejects duplicate integration IDs, empty integrations, and references to
undeclared capabilities during installation.

Credential enrollment is explicit and separate from request authentication:

```toml
[[integrations]]
id = "example"
credential_mode = "manual" # manual | auth_flow | none
```

`manual` means the user supplies a credential, `auth_flow` means Kinetix
starts the integration's host-managed auth flow, and `none` means no user
credential is required. Older packages without the field remain compatible:
an integration with `auth_flow` resolves to `auth_flow`; otherwise plugins
requesting credential scopes/read resolve to `manual`; all others resolve to
`none`. Provider rows persist the resolved mode and source plugin/integration
so the dashboard does not infer enrollment behavior from `credential_plugin`.

### Credential scopes for generated providers

Credential access may be scoped either to a concrete provider id or to the
provider's plugin binding:

```toml
[permissions]
credential_scopes = ["credential_strategy:antigravity-oauth"]
credential_read = true
```

`credential_strategy:<name>` is resolved by the host at credential-use time.
It authorizes this plugin only when the target provider's
`credential_plugin` is exactly `plugin:<this-plugin-id>/<name>`. This is the
preferred scope for Integration-created providers because their database ids
are generated at runtime. Literal `provider:<id>` scopes and `*` remain
supported; `*` should be reserved for plugins that genuinely need access
across provider bindings.

### Integration provider templates

An Integration may declare host-owned provider defaults:

```toml
[integrations.provider]
base_url = "https://api.example.com"
wire_format = "plugin"
auth_scheme = "bearer"
timeout_ms = 120000
capability_mode = "permissive"
follow_redirects = false
```

Kinetix validates the template when the package is installed. Creating the
provider is a separate admin operation and re-runs outbound URL checks plus
capability-binding checks. The host derives `wire_plugin`,
`credential_plugin`, and `model_source_plugin` from the parent Integration;
the package cannot inject bindings to another plugin. Repeating setup with the
same connection values returns the existing Provider; different values create
another Provider without retargeting existing instances. When multiple instances
exist, setup requires explicit `connection_values`.

### Anonymous authentication

`credential_mode = "none"` only describes enrollment. For an API that genuinely
accepts anonymous requests, also declare `auth_scheme = "none"` in
`[integrations.provider]`. Do not bind a credential strategy or auth flow, set
custom auth fields, or supply auth headers. Native inference and discovery then
send no authentication material. Kinetix retains a secret-free routing account
for health, throttling, and Route selection; existing authenticated schemes
still require their usual credentials.

### Public connection parameters

Use bounded identifiers in complete URL path segments when an API needs a
public account identifier alongside a separate secret token:

```toml
[permissions]
network_hosts = ["api.example.com"]
credential_read = false

[[integrations]]
id = "identifier-api"
name = "Identifier API"
credential_mode = "manual"

[integrations.provider]
base_url = "https://api.example.com/accounts/{account_id}/v1"
wire_format = "openai"
auth_scheme = "bearer"
models_path = "/models"

[integrations.provider.parameters.account_id]
type = "identifier"
min_length = 1
max_length = 64
```

Enter values in Plugins during provider setup, or POST
`{"connection_values":{"account_id":"tenant-123"}}` to
`/admin/api/plugins/:id/integrations/:integration_id/provider`. Edit them on
the Provider page or with the same `connection_values` field in a provider
update. Validate an edit through `/admin/api/validate/provider` with
`provider_id` so validation uses the saved declarations.

Values are provider-scoped: all its credential accounts share the identifier.
Use **Add another Provider** in Plugins for each distinct identifier, or repeat
the setup API call with different values. Native inference,
native discovery, and plugin discovery receive the same resolved base URL;
`models_path` is appended to that base, not resolved from the host root.
Account-aware discovery receives its account reference as before, without
needing `credential_read` to obtain the public identifier.

Identifiers allow only ASCII letters, digits, underscores and hyphens, with
explicit bounds of 1-256 bytes and at most 16 declarations. Missing, extra,
invalid or oversized values are rejected before outbound work. Variables
cannot occupy the scheme, host, query, fragment or partial path segments.
Encoding, delimiters and traversal cannot enter through a value; expansion
preserves the origin. Declared `network_hosts` also constrain redirects;
DNS/SSRF checks and credential-host binding remain in force.

Public declarations and values round-trip in `connection_parameters` in
configuration exports, separately from encrypted credentials. Bootstrap and
CLI-created providers accept that object with `declarations`, `values`, and
`network_hosts`; the CLI reads it from `--connection-parameters <JSON-file>`.
Do not put secrets in these fields: the dashboard and configuration exports
expose them. Existing manifests without parameters retain their behavior;
this adds no WIT requirement to API-v1 or API-v2 guests.

### Account-aware model discovery

Legacy `model_sources` keep the original API-v1 discovery contract and receive
provider/base/path metadata only. They remain fully compatible.

Plugins that need an authenticated provider account declare
`account_model_sources` instead:

```toml
[provides]
account_model_sources = ["antigravity-models"]
```

Kinetix invokes these through the separate optional `plugin-model-source`
world and supplies an explicit `account-ref { provider-id, account-id }`.
Credential access still passes through the plugin's approved
`credential_scopes` and `credential_read` policy. A provider binding keeps
the same `plugin:<id>/<name>` syntax; the host resolves account-aware discovery
first and falls back to legacy `model_sources`.

After a browser AuthFlow succeeds, Kinetix may return the non-secret provider
id to the dashboard so it can immediately run discovery. Authorization codes,
access tokens, refresh tokens, and account secrets never appear in that URL.

### Draft install proposal metadata

Integrations may declare `manual_credential` (a kind and named requirements) and `install` (an optional account label and route IDs/models). These fields contain no credential values, approval tokens, activation flags, or account pins. Legacy manual declarations without this metadata remain valid.

Kinetix parses, validates, and retains these declarations; it does not apply their proposals. Enrollment, object persistence, permission approval, and traffic activation remain host-owned. Proposed routes target the provider account pool, not the initially proposed account. Runtime account selection remains unchanged.

The proposed contract and planner live in [kinetix-plugins PR #78](https://github.com/PrightCord/kinetix-plugins/pull/78). The mirrored corpus at `wit/fixtures/plugin-manifest/v1/cases.json` is checked by both repositories; it covers native/anonymous providers, public parameters, legacy manifests, installation metadata, and invalid declarations. Keep the contract draft until both changes land.

### Native dashboard actions

Plugins may optionally declare `[[ui.actions]]` records. These are
host-rendered controls, not plugin JavaScript. Kinetix validates each action at
install time and the dashboard maps it to an operation already implemented and
authorized by Kinetix core.

The first supported kind is `auth`:

```toml
[[ui.actions]]
id = "connect-account"
label = "Connect account"
kind = "auth"
integration = "antigravity"
description = "Sign in and add an account to a compatible provider."
```

An `auth` action must reference an integration that declares both
`auth_flow` and `credential_strategy`. The browser never executes guest code
and never receives the credential returned by the authorization exchange.

### Storage quota

`limits.storage` applies to all encrypted plugin KV values, including normal
guest storage, cached routing facts, and host-owned `_config:` settings.
Kinetix measures decrypted value bytes, accounts correctly for key replacement,
and serializes competing writes so concurrent calls cannot overcommit the
configured budget.

### Host-owned settings

Plugins may also declare `[[ui.settings]]` fields of kind `text`, `secret`,
`boolean`, or `select`. The dashboard renders these with Kinetix-owned form
controls. Values are validated against the manifest and encrypted in the
plugin KV store under the reserved `_config:` namespace.

Guests may read `_config:<key>` through `host-storage`, but guest writes and
deletes to that namespace are rejected. Secret values are write-only from the
dashboard's perspective: the API reports only whether they are configured.

Example:

```toml
[[ui.settings]]
key = "login_hint"
label = "Google account hint"
kind = "text"
description = "Optional email address used as an OAuth login hint."
```

---

## Developing Plugins

The official Rust SDK, first-party plugins, catalog source, and packaging tooling live in the separate [`PrightCord/kinetix-plugins`](https://github.com/PrightCord/kinetix-plugins) repository.

### 1. Project Setup

Add `kinetix-plugin-sdk` to your `Cargo.toml`:

```toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
kinetix-plugin-sdk = { path = "../../sdk" }
wit-bindgen = "0.62"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
```

### 2. Implement Capabilities

Export the WIT world using `kinetix_plugin_sdk`:

```rust
use kinetix_plugin_sdk::guest::*;

struct MyPlugin;

impl Guest for MyPlugin {
    // Implement required world interfaces...
}

export!(MyPlugin);
```

### 3. Build from `kinetix-plugins`

From a checkout of `PrightCord/kinetix-plugins`, the build script compiles the crate for `wasm32-unknown-unknown`, converts it to a component using `wasm-tools`, validates WIT compliance, and packages the deterministic `.kxp` archive:

```bash
./scripts/build-plugin.sh plugins/antigravity-oauth
```

The output package is written beside the plugin source, for example `plugins/antigravity-oauth/dev.kinetix.antigravity-oauth-0.1.0.kxp`.

### Installed package retention

When Kinetix accepts a package, it preserves the exact `.kxp` bytes in a
content-addressed cache under:

```text
$KINETIX_DATA_DIR/plugins/packages/<plugin-id>/<sha256>.kxp
```

SQLite records the plugin id, declared version, SHA-256, signature status,
source, and relative package path. Previous package versions are retained
across upgrades, and package provenance survives plugin removal. This provides
immutable audit history and the artifact inputs needed for a future rollback
operation. Plugins never receive filesystem access to this cache.

---

## Plugin Catalog

The authoritative official catalog metadata lives in `PrightCord/kinetix-plugins/catalog.json`.
Kinetix vendors a default offline snapshot at `src/plugins/catalog.snapshot.json`, exposed through
`GET /admin/api/plugins/catalog`. The catalog powers dashboard discovery, but it is deliberately
**not** a trust root for package installation.

Catalog metadata may describe publisher, capabilities, version, and expected
artifact naming. Package installation still requires the normal Kinetix package
pipeline: package bytes are hashed, signatures are evaluated, permissions are
reviewed, and the plugin installs disabled.

Remote signed release-asset installation is enabled only when all of the
following are present:

- the catalog entry is marked `installable = true`;
- it contains an HTTPS distribution URL, exact SHA-256, publisher key id, and
  explicit redirect-host allow-list;
- the publisher key id resolves in the separately compiled
  `src/plugins/trusted-publishers.snapshot.json` trust-store snapshot;
- the downloaded package's SHA, manifest id/version, and Ed25519 signature all
  verify before installation.

The dashboard sends only the catalog plugin id. It cannot supply an arbitrary
download URL or signing key.

### Publisher bootstrap

Generate the signing key offline and keep the private key out of the repository:

Run publisher-key and package tooling from the `PrightCord/kinetix-plugins` checkout:

```sh
openssl genpkey -algorithm ED25519 -out kinetix-plugin-signing.pem
bash scripts/plugin-publisher-key.sh kinetix-plugin-signing.pem
KINETIX_PLUGIN_SIGNING_KEY_FILE=kinetix-plugin-signing.pem \
  scripts/build-plugin.sh plugins/antigravity-oauth
```

Commit only the printed raw public key (base64) to `trusted-publishers.json`, with a stable key id
such as `kinetix-official-v1`. The plugin build embeds `signature.ed25519` in the deterministic
`.kxp`; plugin release assets and checksums are owned by the plugin repository, not Kinetix core.

Do not set an entry `installable = true` until the signed release asset exists
and its exact SHA-256 and redirect hosts have been committed to the catalog.

### Install and update preview

For an install-ready catalog entry, the dashboard performs a verified preview
before allowing installation. The preview downloads the exact release artifact
and applies the same distribution trust checks as installation:

- HTTPS and per-hop redirect-host allow-list;
- package size bound;
- exact catalog SHA-256;
- manifest id and version match;
- Ed25519 signature from the separately trusted publisher key.

Kinetix compares the target manifest with the currently active manifest and
shows added/removed `network_hosts` and `credential_scopes`, changes to
`credential_read`, and increases/decreases for memory, wall time, outbound
requests, HTTP body size, and storage limits. The same diff is available for
local package previews and in the CLI.

Confirmation does not reuse the preview as an authorization token. Kinetix
downloads and verifies the artifact again before installation, and the
catalog install request includes the previewed package SHA so changed releases
are rejected. New installs and updates that expand authority remain disabled
with no grants until the operator reviews and approves permissions. A
non-expanding update preserves prior grants and enabled state only when the
stored grants, including declared limits, exactly match the old manifest. A
plugin that was already disabled stays disabled.

## Version history and rollback

A version pin records the currently installed version and blocks updates to a
different version until unpinned. Catalog entries may include explicit release
or changelog URLs; Kinetix exposes links only when supplied by the catalog.

Kinetix retains accepted package bytes independently from the active plugin row.
The dashboard shows every retained version, its SHA-256, provenance source, and
which package is currently active.

Rolling back is deliberately a reactivation, not a pointer swap:

1. resolve the retained package by plugin id + SHA-256;
2. reject invalid/escaping package paths;
3. read the exact retained `.kxp`;
4. recompute and verify its SHA-256;
5. re-parse and validate the manifest id/version;
6. recompile the WebAssembly component;
7. activate it through the normal plugin upsert path.

Before rollback, the dashboard requests a retained-package preview. Kinetix
re-hashes and revalidates the target package, then computes the same semantic
diff across network hosts, credential scopes, credential-read access, and all
runtime/request limits.

The reactivated package is always **disabled** and all permission grants are
cleared even when the diff is empty. An operator must review and approve the
rolled-back manifest before it can be enabled again.

## Operating Plugins

Before disabling or removing a plugin, Kinetix previews Providers bound through
plugin capabilities and Routes reaching Models from those Providers. Both
operations refuse to proceed while referenced unless the caller acknowledges
the current impact fingerprint. Provider bindings and Route targets are
retained and fail closed after disable or removal; Kinetix never substitutes a
native capability.

### CLI Workflow

```bash
# Install packages; review the exact authority delta printed for updates.
kinetix plugin install /path/to/dev.kinetix.antigravity-oauth-0.1.0.kxp \
  --allow-untrusted-signature

# New installs and expanding updates require approval before enablement.
kinetix plugin show dev.kinetix.antigravity-oauth
kinetix plugin approve dev.kinetix.antigravity-oauth
kinetix plugin enable dev.kinetix.antigravity-oauth
kinetix plugin validate dev.kinetix.antigravity-oauth

# Pin the installed version to prevent updates; unpin before changing version.
kinetix plugin pin dev.kinetix.antigravity-oauth
kinetix plugin unpin dev.kinetix.antigravity-oauth

# Review bindings before disabling/removing. --force acknowledges that preview.
kinetix plugin impact dev.kinetix.antigravity-oauth
kinetix plugin disable dev.kinetix.antigravity-oauth --force
kinetix plugin remove dev.kinetix.antigravity-oauth --force
```

### Binding to Providers

Once enabled, bind the plugin's capabilities to providers in your configuration or bootstrap file:

```toml
[[providers]]
name = "Google Antigravity"
base_url = "https://autopush-alkalimakersuite-pa.sandbox.googleapis.com"
wire_format = "plugin"
auth_scheme = "bearer"
wire_plugin = "plugin:dev.kinetix.antigravity-oauth/antigravity"
credential_plugin = "plugin:dev.kinetix.antigravity-oauth/antigravity-oauth"

  [[providers.accounts]]
  label = "primary"
  api_key = "refresh_token_here"
```

Or configure via the Admin API:

```bash
curl -X POST http://127.0.0.1:8080/admin/api/providers \
  -H "Content-Type: application/json" \
  -b cookie.txt \
  -d '{
    "name": "Google Antigravity",
    "base_url": "https://autopush-alkalimakersuite-pa.sandbox.googleapis.com",
    "wire_format": "antigravity",
    "auth_scheme": "bearer",
    "wire_plugin": "plugin:dev.kinetix.antigravity-oauth/antigravity",
    "credential_plugin": "plugin:dev.kinetix.antigravity-oauth/antigravity-oauth",
    "api_key": "refresh_token_here",
    "account_label": "primary"
  }'
```

### Using Plugin Routing Facts in Routes

```json
{
  "name": "region-aware-route",
  "strategy": "priority",
  "targets": [
    {
      "model": "ProviderEU/model-1",
      "predicate": {
        "expr": {
          "fact": "plugin.dev.example.geo.region",
          "op": "eq",
          "value": "eu"
        }
      }
    },
    {
      "model": "ProviderUS/model-1",
      "predicate": {
        "expr": {
          "fact": "plugin.dev.example.geo.region",
          "op": "eq",
          "value": "us"
        }
      }
    }
  ]
}
```

---

## Bundled Plugin: Antigravity OAuth

The repository includes a production-grade component at `plugins/antigravity-oauth`:

* **Capabilities**:
  * Credential strategy `antigravity-oauth`: Exchanges Google OAuth 2.0 refresh tokens for temporary access tokens via `oauth2.googleapis.com`.
  * Provider adapter `antigravity`: Translates between standard OpenAI/Anthropic messages and Google's internal `v1internal` protocol, parsing stream chunks and mapping 429 quota-reset headers.
* **Tests**: `tests/plugin_e2e.rs` verifies real component installation, permission approval, capability resolution, token refresh, and stream chunk parsing against a live SQLite database.
