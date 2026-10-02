# Getting Started

## Requirements

For the normal release installation:

* Linux `x86_64` or `aarch64`
* a shell
* `curl`

The installer downloads a prebuilt binary for supported Linux architectures, so Rust and Node.js are **not** required for a normal release installation.

Building from source additionally requires:

* Rust / Cargo
* Git
* Node.js
* npm

Other platforms may work from source but are not currently targeted by the release installer.

## Install

The recommended installation needs no `.env` file and no hand-written configuration file.

```bash
curl -fsSL https://raw.githubusercontent.com/PrightCord/kinetix/main/install.sh | bash
```

By default, the installer:

1. Resolves the latest GitHub release.
2. Downloads the matching prebuilt binary for `x86_64` or `aarch64`.
3. Verifies the release SHA-256 checksum when the checksum manifest is available.
4. Falls back to a source build if a usable prebuilt release cannot be obtained.
5. Installs `kinetix` under `~/.local/bin`.
6. Adds the binary directory to your shell `PATH` when necessary.

The installer does not initialize local state. When ready, run:

```bash
kinetix init
```

This creates the configuration, data, and state directories and prints the dashboard admin password **once**.

Kinetix stores its default state under:

```text
~/.config/kinetix
~/.local/share/kinetix
~/.local/state/kinetix
```

### Pin a release

Set `KINETIX_VERSION` to a release tag:

```bash
curl -fsSL https://raw.githubusercontent.com/PrightCord/kinetix/main/install.sh \
  | KINETIX_VERSION=v0.1.0 bash
```

### Install from `main`

To explicitly build the current development branch:

```bash
curl -fsSL https://raw.githubusercontent.com/PrightCord/kinetix/main/install.sh \
  | KINETIX_VERSION=main bash
```

A source build requires Rust/Cargo, Git, Node.js, and npm.

### Custom install prefix

The default install prefix is:

```text
~/.local
```

Override it with:

```bash
curl -fsSL https://raw.githubusercontent.com/PrightCord/kinetix/main/install.sh \
  | KINETIX_PREFIX=/custom/prefix bash
```

## First configuration

Kinetix intentionally ships without provider presets. Add the upstreams, models, credentials, and prices appropriate for your deployment.

The following example configures Gemini.

### 1. Add a provider and account

```bash
kinetix provider add \
  --name Gemini \
  --base-url https://generativelanguage.googleapis.com/v1beta \
  --wire-format gemini \
  --auth-scheme custom_header \
  --custom-header-name x-goog-api-key \
  --api-key "$GEMINI_API_KEY" \
  --account-label primary
```

Passing both `--api-key` and `--account-label` creates the provider's first credential account at the same time.

### 2. Register a model

```bash
kinetix model add \
  --provider Gemini \
  --upstream-id gemini-2.5-flash \
  --display-name "Gemini 2.5 Flash"
```

Optional model metadata includes context limits, capabilities, and pricing.

See [Providers](Providers) for the full provider and model configuration model.

### 3. Create a virtual key

```bash
kinetix key create --name local-client --owner me
```

The resulting secret begins with:

```text
sk-kinetix-...
```

The plaintext secret is shown once. Kinetix stores only its hash.

### 4. Start the proxy

```bash
kinetix serve
```

The default local address is:

```text
http://127.0.0.1:8080
```

The embedded dashboard is available at:

```text
http://127.0.0.1:8080/admin
```

Log in with the admin password printed by `kinetix init`.

## Verify the installation

### Health check

```bash
curl -s http://127.0.0.1:8080/healthz
```

A healthy instance returns a response similar to:

```json
{
  "status": "ok",
  "uptime_secs": 3,
  "database": "ok",
  "data_plane": "serving",
  "control_plane": "ok"
}
```

`status: ok` indicates that the data plane can serve requests.

A degraded control plane does not necessarily make the inference data plane unavailable: Kinetix can continue serving from its in-memory registry snapshot in supported failure scenarios. See [Observability](Observability) for health semantics.

### Send an inference request

Using the virtual key created earlier:

```bash
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-kinetix-..." \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gemini-2.5-flash",
    "messages": [
      {
        "role": "user",
        "content": "Say hello from Kinetix."
      }
    ]
  }'
```

A successful response confirms the complete path:

```text
client
  → virtual-key authentication
  → model resolution
  → provider account
  → upstream model
  → translated response
```

## Point a client at Kinetix

Kinetix supports OpenAI Chat Completions, a documented translated subset of OpenAI Responses, and Anthropic Messages clients.

### OpenAI-compatible clients

Use:

```text
http://127.0.0.1:8080/v1
```

with a Kinetix virtual key.

Example Pi configuration:

```json
{
  "providers": {
    "kinetix": {
      "baseUrl": "http://127.0.0.1:8080/v1",
      "apiKey": "sk-kinetix-...",
      "api": "openai-completions"
    }
  }
}
```

### Anthropic-format clients

Point Anthropic-format clients at:

```text
http://127.0.0.1:8080
```

Kinetix serves:

```text
POST /v1/messages
```

### OpenAI Responses clients

Modern coding agents can use:

```text
POST /v1/responses
```

This endpoint implements Kinetix's translated Responses subset, not native
Responses storage/chaining or hosted tools. Unsupported semantics are rejected
explicitly. See [Compatibility](https://github.com/PrightCord/kinetix/blob/main/docs/compatibility.md) for the supported request and streaming contract.

See [Pi Compatibility](https://github.com/PrightCord/kinetix/blob/main/docs/pi-compatibility.md) for Pi-specific setup and acceptance notes.

## Useful commands

Inspect the installation:

```bash
kinetix status
```

Run diagnostics:

```bash
kinetix doctor
```

Show the command surface:

```bash
kinetix --help
```

Examples:

```bash
kinetix provider --help
kinetix model --help
kinetix account --help
kinetix route --help
kinetix alias --help
kinetix key --help
```

The Kinetix binary is both the inference server and the administrative CLI. Administrative commands operate directly on the SQLite control plane and generally work while the server is stopped.

See [CLI Reference](CLI-Reference) for the complete command surface.

## Docker

For Docker deployment:

```bash
cp .env.docker.example .env
docker compose up -d --build
docker compose logs kinetix | grep -i password
```

All persistent state is stored under the container's `/data` volume.

See [Docker](Docker) for the full container workflow.

## Production deployment

The default service binds locally. For production, keep Kinetix behind an appropriate authenticated reverse proxy or tunnel rather than exposing the administrative surface directly.

The repository includes:

* a hardened systemd unit
* graceful shutdown/drain behavior
* automatic restart
* Cloudflare Tunnel guidance
* Cloudflare Access guidance
* backup/restore procedures
* upgrade procedures

See [Deployment](Deployment) and [`deploy/README.md`](https://github.com/PrightCord/kinetix/blob/main/deploy/README.md).

## Uninstall

Using the installed binary:

```bash
kinetix uninstall
```

Additional options:

```bash
kinetix uninstall [--yes] [--remove-binary] [--keep-data] [--dry-run]
```

Or use the standalone uninstall script:

```bash
curl -fsSL https://raw.githubusercontent.com/PrightCord/kinetix/main/uninstall.sh | bash
```

## Next steps

* [CLI Reference](CLI-Reference) — complete command surface
* [Providers](Providers) — providers, models, capabilities, parameters, and pricing
* [Routing and Fallback](Routing-and-Fallback) — account pools, Routes, selection, and fallback
* [Authentication](Authentication) — admin and client authentication
* [Dashboard](Dashboard) — embedded control plane
* [Observability](Observability) — health, metrics, request tracing, and diagnostics
* [Deployment](Deployment) — production operation
* [Troubleshooting](Troubleshooting) — common operational problems
