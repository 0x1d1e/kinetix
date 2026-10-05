#!/usr/bin/env bash
# End-to-end coding-agent compatibility matrix runner.
# Starts synthetic upstream + Kinetix, then drives the full client matrix.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> Testing release acceptance streaming proxy"
python3 scripts/release_client_proxy_test.py

BIND="${1:-127.0.0.1:8186}"
UPSTREAM_PORT="${UPSTREAM_PORT:-9198}"
ADMIN_TOKEN="compat-admin-token-00000000000000000000"
MASTER_KEY="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"

WORK="$(mktemp -d)"
DB="$WORK/kinetix.db"
LOG="$WORK/kinetix.log"
UP_LOG="$WORK/upstream.log"

cleanup() {
  [ -n "${KPID:-}" ] && kill "$KPID" 2>/dev/null || :
  [ -n "${UPID:-}" ] && kill "$UPID" 2>/dev/null || :
  rm -rf "$WORK"
}
trap cleanup EXIT

if [ -n "${KINETIX_BIN:-}" ]; then
  BIN="$KINETIX_BIN"
  echo "==> Using prebuilt Kinetix binary: $BIN"
else
  BIN="$ROOT/target/release/kinetix"
  echo "==> Building release binary for local compatibility run"
  cargo build --release --quiet
fi

if [ ! -x "$BIN" ]; then
  echo "Kinetix binary is not executable: $BIN"
  exit 1
fi

echo "==> Starting synthetic upstream on :$UPSTREAM_PORT"
SYN_DELAY_MS=1 python3 scripts/synthetic_upstream.py "$UPSTREAM_PORT" >"$UP_LOG" 2>&1 &
UPID=$!
sleep 1

# Create bootstrap file pointing to this synthetic upstream port
BOOTSTRAP="$WORK/bootstrap.toml"
sed "s/9199/$UPSTREAM_PORT/g" "$ROOT/scripts/smoke-bootstrap.toml" > "$BOOTSTRAP"

echo "==> Starting Kinetix on $BIND"
KINETIX_BIND="$BIND" \
KINETIX_DATABASE_URL="sqlite://$DB" \
KINETIX_MASTER_KEY="$MASTER_KEY" \
KINETIX_ADMIN_TOKEN="$ADMIN_TOKEN" \
KINETIX_DATA_DIR="$WORK" \
KINETIX_ALLOW_PRIVATE_UPSTREAMS=true \
KINETIX_ALLOW_INSECURE_TLS=true \
KINETIX_BOOTSTRAP_FILE="$BOOTSTRAP" \
KINETIX_HOME="$WORK" \
KINETIX_IP_RATE_LIMIT_PER_MIN=0 \
  "$BIN" serve >"$LOG" 2>&1 &
KPID=$!

for _ in $(seq 1 60); do
  if curl -sf "http://$BIND/healthz" >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done

KEY="$(grep -oE 'sk-kinetix-[0-9a-f]+' "$LOG" | head -1)"
if [ -z "$KEY" ]; then
  echo "Failed to extract virtual key from Kinetix log"
  cat "$LOG"
  exit 1
fi

KEY_READY=false
for _ in $(seq 1 20); do
  if curl -fsS -H "Authorization: Bearer $KEY" "http://$BIND/v1/models" >/dev/null; then
    KEY_READY=true
    break
  fi
  sleep 0.25
done
if [ "$KEY_READY" != true ]; then
  echo "Bootstrap virtual key failed authentication"
  exit 1
fi

echo "==> Running compatibility matrix suite"
KINETIX_BASE="http://$BIND" KINETIX_KEY="$KEY" KINETIX_ADMIN_TOKEN="$ADMIN_TOKEN" python3 scripts/compat-matrix.py

echo "==> Running protocol v1 native/translated matrix"
KINETIX_BASE="http://$BIND" \
KINETIX_KEY="$KEY" \
KINETIX_RESTRICTED_MODELS_KEY="sk-kinetix-compat-restricted-models" \
  python3 scripts/protocol-v1-matrix.py

echo "==> Checking generated protocol compatibility docs"
python3 scripts/render-protocol-v1-compat.py --check
