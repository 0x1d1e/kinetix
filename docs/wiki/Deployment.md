# Deployment

Kinetix is a single binary. The recommended production shape is: a dedicated
machine (or container) running `kinetix serve` bound to localhost, exposed by a
**Cloudflare Tunnel**, with the admin surface behind **Cloudflare Access**.

## systemd

`deploy/kinetix.service` runs Kinetix under systemd:

- `Type=simple`, `Restart=always`, `RestartSec=2` (recovery ≤10s).
- `TimeoutStopSec=45` so the graceful drain (default 30s) can finish.
- Secrets come from `EnvironmentFile=/etc/kinetix/kinetix.env` (never in the unit).
- Hardening: `NoNewPrivileges`, `PrivateTmp`, `ProtectSystem=strict`,
  `ProtectHome=true`, `ReadWritePaths=/var/lib/kinetix`.

```bash
sudo useradd --system --home /var/lib/kinetix kinetix
sudo install -m0755 target/release/kinetix /usr/local/bin/kinetix
sudo install -d -o kinetix -g kinetix /var/lib/kinetix
sudo install -m0644 deploy/kinetix.service /etc/systemd/system/kinetix.service
sudo install -d /etc/kinetix && sudo install -m0600 .env.example /etc/kinetix/kinetix.env
sudo systemctl daemon-reload && sudo systemctl enable --now kinetix
```

> The unit must start the server explicitly with the `serve` subcommand.

## Cloudflare Tunnel + Access

- Point two hostnames (e.g. `api.example.com` and `admin.example.com`) at
  `http://127.0.0.1:8080`.
- Protect the admin hostname with Cloudflare Access, and set
  `KINETIX_CF_ACCESS_AUD` / `KINETIX_CF_ACCESS_TEAM_DOMAIN` so Kinetix also
  validates the Access JWT.

Example `cloudflared` config:

```yaml
tunnel: <id>
credentials-file: /etc/cloudflared/<id>.json
ingress:
  - hostname: api.example.com
    service: http://127.0.0.1:8080
  - hostname: admin.example.com
    service: http://127.0.0.1:8080
  - service: http_status:404
```

> Cloudflare drops proxied connections idle for ~100s (HTTP 524). Kinetix sends
> SSE keepalives well under that bound, so long silent thinking phases stay alive.

## Backups and restore

- Pre-migration snapshots named `kinetix-pre-migration-<ts>-<uuid>.db` are
  written before pending migrations or pricing repairs change an existing
  database. Failed retries reuse and protect the same snapshot. After startup
  work succeeds, the newest 3 are retained; current databases start without one.
- **Scheduled backups** (`VACUUM INTO`, transactionally consistent) run every 6
  hours, keeping the newest 14, in `$KINETIX_DATA_DIR/backups`. A `RESTORE.txt`
  documents the procedure.

Restore:

```bash
sudo systemctl stop kinetix
cp /var/lib/kinetix/backups/kinetix-<stamp>.db /var/lib/kinetix/data/kinetix.db
rm -f /var/lib/kinetix/data/kinetix.db-wal /var/lib/kinetix/data/kinetix.db-shm
sudo chown kinetix:kinetix /var/lib/kinetix/data/kinetix.db
sudo systemctl start kinetix
```

> Editing an already-applied migration in place changes its checksum and aborts
> startup. To change schema, add a **new** migration (or reset the DB in dev).

## Upgrades

Migrations run automatically on startup. Deploy the new binary, restart, and
watch the logs for migration errors.

## Docker

See [Docker](Docker). `docker-compose.yml` runs Kinetix with a persistent volume
and an optional cloudflared service.

## Health & alerting

- Point your monitor at `GET /healthz` (HTTP 200 = serviceable).
- Set `KINETIX_ALERT_WEBHOOK_URL` for alerts. See [Observability](Observability).

## Dev-only flags

`KINETIX_ALLOW_PRIVATE_UPSTREAMS` and `KINETIX_ALLOW_INSECURE_TLS` relax the SSRF
and TLS guards. They are **visibly-marked development modes** and must not be
enabled in production.
