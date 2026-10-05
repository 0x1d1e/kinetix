# Kinetix Wiki

Kinetix is a single self-hosted Rust service that speaks the **OpenAI Chat
Completions** and **Anthropic Messages** wire formats (streaming first) in front
of admin-configured upstream LLM APIs, and layers on virtual keys, account pools
with executable **Routes** and automatic fallback, cost tracking, and an embedded
admin dashboard. It is built for developers and small technical teams running AI
coding agents such as [Pi](https://pi.dev).

This wiki is the detailed, task-oriented companion to the repo
[`README.md`](https://github.com/0x1d1e/kinetix/blob/main/README.md).

## Start here

| Page | What it covers |
| --- | --- |
| [Getting Started](Getting-Started) | Install, first run, point a client at it |
| [CLI Reference](CLI-Reference) | Every subcommand and flag |
| [Configuration](Configuration) | XDG layout, env vars, precedence, bootstrap TOML |
| [Architecture](Architecture) | Major boundaries and repository navigation |
| [Glossary](Glossary) | Canonical Kinetix domain terminology |
| [Routing and Fallback](Routing-and-Fallback) | Routes, predicates, strategies, continuity |
| [Admin API](Admin-API) | Every `/admin/api/*` endpoint |
| [Dashboard](Dashboard) | The embedded web UI |
| [Authentication](Authentication) | Virtual keys, admin sessions, Cloudflare Access |
| [Providers](Providers) | Wire formats, auth schemes, discovery, security |
| [Plugins](Plugins) | WebAssembly plugins, capability seams, packaging, and SDK |
| [Usage, Cost and Accounting](Usage-Cost-and-Accounting) | Tokens, prices, budgets, truthfulness |
| [Observability](Observability) | Metrics, Route Trace, flight recorder, live view, alerts |
| [Deployment](Deployment) | systemd, Cloudflare Tunnel, Docker, backups |
| [Docker](Docker) | Running Kinetix in a container |
| [Security](Security) | Threat model and hardening |
| [Testing and Benchmarks](Testing-and-Benchmarks) | Fixtures, torture tests, benchmark rigs |
| [Troubleshooting](Troubleshooting) | Common problems and fixes |
| [FAQ](FAQ) | Short answers |

## What it is (and isn't)

- **Is**: a private, single-binary gateway for coding agents; correctness of the
  wire protocol and routing decisions matters more than breadth.
- **Isn't**: a general-purpose AI platform, a multi-tenant SaaS, or a bundled
  provider-preset library.

## Editing this wiki

The wiki pages are generated from the markdown files in
[`docs/wiki/`](https://github.com/0x1d1e/kinetix/tree/main/docs/wiki). Edit
them there (so changes are reviewed and versioned with the code) and mirror them
to the GitHub wiki with:

```bash
scripts/publish-wiki.sh "docs(wiki): describe your change"
```

The script clones `<repo>.wiki.git`, copies `docs/wiki/*.md` over it, and pushes.
If the wiki git repository does not exist yet, enable the wiki in the repo
settings and create **one page** in the web UI first (GitHub only provisions
`<repo>.wiki.git` after the first page exists), then re-run the script.
`Home.md` is the wiki landing page.
