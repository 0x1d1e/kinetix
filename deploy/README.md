# Kinetix deployment runbook

Kinetix is a single Rust binary plus a SQLite database, reachable only through a
Cloudflare Tunnel. Nothing else needs to be installed.

## 1. Build

```sh
# The dashboard bundle is embedded into the binary by rust-embed, so build it
# first. scripts/build-dashboard.sh does both steps.
scripts/build-dashboard.sh
# -> target/release/kinetix
```

Reproducible builds: CI builds on Linux x86_64/aarch64.

## 2. Install

```sh
sudo useradd --system --home /var/lib/kinetix --create-home kinetix
sudo install -d -o kinetix -g kinetix /etc/kinetix /var/lib/kinetix
sudo install -m 0755 target/release/kinetix /usr/local/bin/kinetix
sudo install -m 0644 deploy/kinetix.service /etc/systemd/system/kinetix.service
sudo install -m 0600 .env.example /etc/kinetix/kinetix.env   # then edit it
sudo systemctl daemon-reload
sudo systemctl enable --now kinetix
```

`KINETIX_DATA_DIR` (default `/var/lib/kinetix`) holds the database and
`backups/`. The unit runs as the unprivileged `kinetix` user with
`ProtectSystem=strict`; only that directory is writable.

## 3. Cloudflare Tunnel + Access

```sh
cloudflared tunnel create kinetix
cloudflared tunnel route dns kinetix api.example.com
cloudflared tunnel route dns kinetix admin.example.com   # separate admin hostname
```

Ingress: the **API hostname** points at `http://127.0.0.1:8080`; the **admin
hostname** also points at `127.0.0.1:8080` but is protected by a Cloudflare
Access application. Set `KINETIX_CF_ACCESS_AUD` and
`KINETIX_CF_ACCESS_TEAM_DOMAIN` so Kinetix additionally validates the Access
JWT; until then the admin API requires the admin-token session cookie.

```yaml
# ~/.cloudflared/config.yml
tunnel: kinetix
credentials-file: /etc/cloudflared/kinetix.json
ingress:
  - hostname: api.example.com
    service: http://127.0.0.1:8080
  - hostname: admin.example.com
    service: http://127.0.0.1:8080
  - service: http_status:404
```

## 4. Backups and restore

Backups are written to `$KINETIX_DATA_DIR/backups`:

- `kinetix-pre-migration-<ts>-<uuid>.db`: written before startup work that
  will change an existing database. Failed retries reuse and protect the same
  snapshot. Pending snapshots for databases sharing this directory are protected
  and excluded from the three-snapshot limit.
- `kinetix-<ts>.db`: written every 6h via `VACUUM INTO` (consistent while live);
  14-file retention.

Restore:

```sh
sudo systemctl stop kinetix
sudo cp /var/lib/kinetix/backups/kinetix-<ts>.db /var/lib/kinetix/kinetix.db
sudo rm -f /var/lib/kinetix/kinetix.db-wal /var/lib/kinetix/kinetix.db-shm
sudo chown kinetix:kinetix /var/lib/kinetix/kinetix.db
sudo systemctl start kinetix
```

To move configuration between instances, use the admin API export/import
(`GET /admin/api/config/export`, `POST /admin/api/config/import`) rather than
copying the database.

## 5. Health and alerting

- External uptime probe: `GET /healthz` returns 200 while the **data plane** is
  serviceable and reports `control_plane: ok|degraded` in the body.
- Metrics: `GET /admin/api/metrics` (Prometheus, admin-only).
- Alerts: set `KINETIX_ALERT_WEBHOOK_URL` to receive edge-triggered JSON alerts.

## 6. Upgrades

1. `git pull && scripts/build-dashboard.sh`
2. `sudo install -m 0755 target/release/kinetix /usr/local/bin/kinetix`
3. `sudo systemctl restart kinetix`

Migrations run automatically on start. A pre-migration snapshot is created
when pending migrations or pricing repairs will change an existing database;
current databases start without one. If a migration was edited in place, the
applied-migration checksum changes and startup aborts; reset the database or
restore a backup.

## 7. Local development

`KINETIX_ALLOW_PRIVATE_UPSTREAMS=true` and `KINETIX_ALLOW_INSECURE_TLS=true`
enable localhost/plain-HTTP upstreams. Both are visibly-marked development
modes and must not be enabled in production.
