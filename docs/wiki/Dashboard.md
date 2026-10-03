# Dashboard

Kinetix embeds a React dashboard into the same binary (via `rust-embed`), served
at `/admin`. It is fully data-backed — there is no mock data in the shipped app.

## Layout

A grouped sidebar (with a mobile drawer) plus a slim top bar:

- **Gateway** — Virtual Keys, Routes & Fallback
- **Configuration** — Upstream Providers, Accounts & Pools, Model Aliases
- **Observability** — Usage & Spend, Request Inspector, Audit Log
- **System** — Settings & Security

The top bar carries the tunnel status, active streams, current spend, a Refresh
button, a three-way theme toggle (Light / Dark / System), and the Live Proxy Test
launcher.

## Pages

| Page | What you can do |
| --- | --- |
| **Virtual Keys** | Create keys (the full key is shown once), edit limits/budgets, edit the IP allowlist inline, revoke (hard-delete with a confirmation). |
| **Routes & Fallback** | Create/edit Routes with strategy, portability policy, cache affinity, and targets; run a Route Dry Run. |
| **Upstream Providers** | Add/edit providers (wire format, auth scheme, custom header/param, timeout, compatibility policy (`capability_mode`), credential-host binding, redirects, plain-HTTP dev toggle, models path, extra headers), attach an API key + account label, Test Ping, Fetch Models (discovery with fuzzy search), import discovered models, edit/delete models. |
| **Accounts & Pools** | Add/rotate credentials, edit priority/weight/soft quota/quota type, clear cooldown, delete. |
| **Model Aliases** | Add/remove aliases pointing at a model or a Route. |
| **Usage & Spend** | Spend vs budgets, a Today / 24h / 7d / 30d window, and per-day JSONL/CSV exports (list, export, delete). |
| **Request Inspector** | Live in-flight view plus finished rows; open a row for its Route Trace and flight-recorder diagnostics. |
| **Audit Log** | The append-only audit trail (`system` actions are highlighted). |
| **Settings & Security** | Change the admin password (forces re-login), session policy notes, CLI equivalents, sign out. |

## Theming

The hand-drawn design system is driven through CSS custom properties, so the
Light / Dark / System toggle switches every surface at once. The choice is stored
in `localStorage` and applied before React mounts to avoid a flash.

## Live Proxy Tester

Opens from the top bar. Two modes:

- **Admin mode** — runs through `POST /admin/api/test-stream`; the secret never
  enters the browser.
- **Raw key mode** — the browser calls `/v1` directly with a pasted key.

It shows the real serving result, TTFT/total latency, the opaque Route ID, and
any fallback/portability warnings. The default prompt is
``REPLY BACK EXACTLY: `Testing` ``.

## Building the dashboard

```bash
cd dashboard
npm ci
npm run build          # emits dashboard/dist
cd .. && touch src/assets.rs && cargo build   # re-embed
```

`scripts/build-dashboard.sh` does all of the above and then a release build.
`scripts/run-ci.sh` mirrors the GitHub Actions pipeline locally.
