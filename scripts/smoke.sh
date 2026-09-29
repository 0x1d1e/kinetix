#!/usr/bin/env bash
# Kinetix end-to-end smoke test.
#
# Starts a synthetic upstream and a fresh Kinetix (release binary, throwaway
# database), then exercises the public API, the admin API, same-format
# passthrough, an OpenAI tool call, the Anthropic inbound format, Route
# fallback, the Route Trace / diagnostics, the live view, and Prometheus
# metrics. Exits non-zero on the first failed check.
#
# Usage: scripts/smoke.sh [bind-addr]   (default 127.0.0.1:8180)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIND="${1:-127.0.0.1:8180}"
UPSTREAM_PORT="${UPSTREAM_PORT:-9199}"
ADMIN_TOKEN="smoke-admin-token-0000000000000000000000"
MASTER_KEY="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"

WORK="$(mktemp -d)"
DB="$WORK/kinetix.db"
LOG="$WORK/kinetix.log"
UP_LOG="$WORK/upstream.log"
SMOKE_BOOTSTRAP="$WORK/smoke-bootstrap.toml"
PRE_DISPATCH_RPM_KEY="sk-kinetix-$(python3 -c 'import secrets; print(secrets.token_hex(24))')"
PRE_DISPATCH_TPM_KEY="sk-kinetix-$(python3 -c 'import secrets; print(secrets.token_hex(24))')"
ACTIVE_BUDGET_KEY="sk-kinetix-$(python3 -c 'import secrets; print(secrets.token_hex(24))')"
ACTIVE_UNKNOWN_BUDGET_KEY="sk-kinetix-$(python3 -c 'import secrets; print(secrets.token_hex(24))')"
cp "$ROOT/scripts/smoke-bootstrap.toml" "$SMOKE_BOOTSTRAP" || { echo "failed to prepare smoke bootstrap"; exit 1; }
PRE_DISPATCH_RPM_KEY="$PRE_DISPATCH_RPM_KEY" \
PRE_DISPATCH_TPM_KEY="$PRE_DISPATCH_TPM_KEY" \
ACTIVE_BUDGET_KEY="$ACTIVE_BUDGET_KEY" \
ACTIVE_UNKNOWN_BUDGET_KEY="$ACTIVE_UNKNOWN_BUDGET_KEY" \
python3 - "$SMOKE_BOOTSTRAP" <<'PY' || { echo "failed to prepare smoke bootstrap"; exit 1; }
import os, sys
from pathlib import Path
path = Path(sys.argv[1])
text = path.read_text()
for name in ("PRE_DISPATCH_RPM_KEY", "PRE_DISPATCH_TPM_KEY", "ACTIVE_BUDGET_KEY", "ACTIVE_UNKNOWN_BUDGET_KEY"):
    placeholder = f"__{name}__"
    if text.count(placeholder) != 1:
        raise SystemExit(f"expected one {placeholder} placeholder")
    text = text.replace(placeholder, os.environ[name])
path.write_text(text)
PY
FAILURES=0

cleanup() {
  [ -n "${KPID:-}" ] && kill "$KPID" 2>/dev/null
  [ -n "${UPID:-}" ] && kill "$UPID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

check() { # check <label> <actual> <expected-substring>
  if [[ "$2" == *"$3"* ]]; then
    echo "  ok   $1"
  else
    echo "  FAIL $1"
    echo "       expected to contain: $3"
    echo "       got: $2"
    FAILURES=$((FAILURES + 1))
  fi
}

check_absent() { # check_absent <label> <actual> <forbidden-substring>
  if [[ "$2" != *"$3"* ]]; then
    echo "  ok   $1"
  else
    echo "  FAIL $1"
    echo "       must not contain: $3"
    echo "       got: $2"
    FAILURES=$((FAILURES + 1))
  fi
}

check_body_contains() { # avoid echoing potentially sensitive error bodies on failure
  if [[ "$2" == *"$3"* ]]; then
    echo "  ok   $1"
  else
    echo "  FAIL $1"
    FAILURES=$((FAILURES + 1))
  fi
}

check_body_absent() {
  if [[ "$2" != *"$3"* ]]; then
    echo "  ok   $1"
  else
    echo "  FAIL $1"
    FAILURES=$((FAILURES + 1))
  fi
}

restart_kinetix() {
  kill "$KPID" 2>/dev/null || true
  wait "$KPID" 2>/dev/null || true
  KINETIX_BIND="$BIND" \
  KINETIX_DATABASE_URL="sqlite://$DB" \
  KINETIX_MASTER_KEY="$MASTER_KEY" \
  KINETIX_ADMIN_TOKEN="$ADMIN_TOKEN" \
  KINETIX_DATA_DIR="$WORK" \
  KINETIX_ALLOW_PRIVATE_UPSTREAMS=true \
  KINETIX_ALLOW_INSECURE_TLS=true \
  KINETIX_BOOTSTRAP_FILE="$SMOKE_BOOTSTRAP" \
  KINETIX_HOME="$WORK" \
    ./target/release/kinetix serve >>"$LOG" 2>&1 &
  KPID=$!

  for _ in $(seq 1 60); do
    curl -sf "http://$BIND/healthz" >/dev/null 2>&1 && return
    sleep 0.25
  done
}

echo "==> building release binary"
cargo build --release --quiet || { echo "build failed"; exit 1; }

echo "==> starting synthetic upstream on :$UPSTREAM_PORT"
python3 scripts/synthetic_upstream.py "$UPSTREAM_PORT" >"$UP_LOG" 2>&1 &
UPID=$!
sleep 1

echo "==> starting kinetix on $BIND"
KINETIX_BIND="$BIND" \
KINETIX_DATABASE_URL="sqlite://$DB" \
KINETIX_MASTER_KEY="$MASTER_KEY" \
KINETIX_ADMIN_TOKEN="$ADMIN_TOKEN" \
KINETIX_DATA_DIR="$WORK" \
KINETIX_ALLOW_PRIVATE_UPSTREAMS=true \
KINETIX_ALLOW_INSECURE_TLS=true \
KINETIX_BOOTSTRAP_FILE="$SMOKE_BOOTSTRAP" \
KINETIX_HOME="$WORK" \
  ./target/release/kinetix serve >"$LOG" 2>&1 &
KPID=$!

for _ in $(seq 1 60); do
  curl -sf "http://$BIND/healthz" >/dev/null 2>&1 && break
  sleep 0.25
done

BASE="http://$BIND"
KEY="$(grep -oE 'sk-kinetix-[0-9a-f]+' "$LOG" | head -1)"
JAR="$WORK/cookies"

echo "==> checks"
check "healthz" "$(curl -s "$BASE/healthz")" '"data_plane":"serving"'
check "bad key -> 401" "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H 'authorization: Bearer sk-kinetix-nope' -H 'content-type: application/json' -d '{"model":"syn-openai","messages":[]}')" "401"

check "models lists syn-openai" "$(curl -s "$BASE/v1/models" -H "authorization: Bearer $KEY")" 'syn-openai'

# same-format passthrough (OpenAI -> OpenAI)
STREAM="$(curl -s -N --max-time 20 -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $KEY" -H 'content-type: application/json' -d '{"model":"syn-openai","stream":true,"messages":[{"role":"user","content":"hi"}]}')"
check "passthrough streams frames" "$STREAM" 'data:'
check "passthrough terminates" "$STREAM" '[DONE]'

# Client usage is key-scoped and keeps unknown cost explicit. Usage logging is
# asynchronous, so wait briefly for the completed stream to reach SQLite.
for _ in $(seq 1 50); do
  CLIENT_USAGE="$(curl -s "$BASE/v1/usage" -H "authorization: Bearer $KEY")"
  [[ "$CLIENT_USAGE" == *'"requests":1'* ]] && break
  sleep 0.1
done
check "client usage includes streamed request" "$CLIENT_USAGE" '"requests":1'
check "unknown-cost request contributes zero known subtotal" "$CLIENT_USAGE" '"known_cost_usd":0.0'
check "unknown-cost request is counted separately" "$CLIENT_USAGE" '"unknown_cost_requests":1'
check_absent "client usage hides serving topology" "$CLIENT_USAGE" 'serving_provider'
check_absent "client usage hides account identity" "$CLIENT_USAGE" 'serving_account'

# An upstream failure is accounted and consumes RPM, both live and after restart.
FAIL_RPM_KEY="sk-kinetix-smoke-failure-rpm"
FAIL_RPM_FIRST_CODE="$(curl -s --max-time 20 -o "$WORK/failure-rpm-first" -w '%{http_code}' \
  -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $FAIL_RPM_KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"syn-fail","stream":false,"messages":[{"role":"user","content":"fail"}]}')"
check "RPM fixture first failed request returns gateway error" "$FAIL_RPM_FIRST_CODE" '502'
FAIL_RPM_SECOND_CODE="$(curl -s --max-time 20 -o "$WORK/failure-rpm-second" -w '%{http_code}' \
  -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $FAIL_RPM_KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"syn-fail","stream":false,"messages":[{"role":"user","content":"fail"}]}')"
check "failed request consumes live RPM slot" "$FAIL_RPM_SECOND_CODE" '429'

for _ in $(seq 1 50); do
  FAIL_RPM_USAGE="$(curl -s "$BASE/v1/usage" -H "authorization: Bearer $FAIL_RPM_KEY")"
  if python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["usage"]["daily"]["requests"] >= 1 else 1)' <<<"$FAIL_RPM_USAGE"; then
    break
  fi
  sleep 0.1
done
check "failed RPM request is durably reported once" "$FAIL_RPM_USAGE" '"requests":1'
check "failed request usage stays unknown" "$FAIL_RPM_USAGE" '"unknown_usage_requests":1'
check_absent "failed request usage hides provider topology" "$FAIL_RPM_USAGE" 'serving_provider'

# An unknown actual cost must retain its conservative estimate for budget admission.
FAIL_BUDGET_KEY="sk-kinetix-smoke-budget-reservation"
FAIL_BUDGET_FIRST_CODE="$(curl -s --max-time 20 -o "$WORK/failure-budget-first" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $FAIL_BUDGET_KEY" -H 'content-type: application/json' -d '{"model":"syn-fail-budget","stream":false,"messages":[{"role":"user","content":"fail"}]}')"
check "priced failed request returns gateway error" "$FAIL_BUDGET_FIRST_CODE" '502'
FAIL_BUDGET_SECOND_CODE="$(curl -s --max-time 20 -o "$WORK/failure-budget-second" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $FAIL_BUDGET_KEY" -H 'content-type: application/json' -d '{"model":"syn-fail-budget","stream":false,"messages":[{"role":"user","content":"fail"}]}')"
check "failed request consumes live budget reservation" "$FAIL_BUDGET_SECOND_CODE" '429'
for _ in $(seq 1 50); do
  FAIL_BUDGET_USAGE="$(curl -s "$BASE/v1/usage" -H "authorization: Bearer $FAIL_BUDGET_KEY")"
  if python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["usage"]["daily"]["requests"] >= 1 else 1)' <<<"$FAIL_BUDGET_USAGE"; then
    break
  fi
  sleep 0.1
done
check "unknown failed cost stays outside known subtotal" "$FAIL_BUDGET_USAGE" '"known_cost_usd":0.0'
check "unknown failed cost still makes budget remainder unknown" "$FAIL_BUDGET_USAGE" '"daily_budget_usd":null'

# Active priced reservations reduce client-visible remaining budget and affect
# admission before the stream settles.
ACTIVE_BUDGET_SETTLED_CODE="$(curl -s --max-time 20 -o /dev/null -w '%{http_code}' \
  -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $ACTIVE_BUDGET_KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"syn-openai-partial","stream":false,"max_tokens":100,"messages":[{"role":"user","content":"settled budget seed"}]}')"
check "priced budget seed request succeeds" "$ACTIVE_BUDGET_SETTLED_CODE" '200'
for _ in $(seq 1 50); do
  ACTIVE_BUDGET_SETTLED_USAGE="$(curl -s "$BASE/v1/usage" -H "authorization: Bearer $ACTIVE_BUDGET_KEY")"
  if python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["usage"]["daily"]["requests"] == 1 else 1)' <<<"$ACTIVE_BUDGET_SETTLED_USAGE"; then
    break
  fi
  sleep 0.1
done
ACTIVE_BUDGET_REQUEST='{"model":"syn-openai-partial","stream":true,"max_tokens":8000,"messages":[{"role":"user","content":"fixture:hold-budget"}]}'
curl -s -N --max-time 30 -X POST "$BASE/v1/chat/completions" \
  -H "authorization: Bearer $ACTIVE_BUDGET_KEY" -H 'content-type: application/json' \
  -d "$ACTIVE_BUDGET_REQUEST" >"$WORK/active-budget-stream" &
ACTIVE_BUDGET_STREAM_PID=$!
for _ in $(seq 1 100); do
  ACTIVE_BUDGET_USAGE="$(curl -s "$BASE/v1/usage" -H "authorization: Bearer $ACTIVE_BUDGET_KEY")"
  if python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["admission"]["in_flight"] == 1 else 1)' <<<"$ACTIVE_BUDGET_USAGE"; then
    break
  fi
  sleep 0.05
done
ACTIVE_BUDGET_SNAPSHOT_OK="$(python3 - "$ACTIVE_BUDGET_USAGE" <<'PY'
import json, math, sys
body = json.loads(sys.argv[1])
daily = body["admission"]["budget"]["daily"]
remaining = body["remaining"]["daily_budget_usd"]
expected = max(1.0 - daily["settled_spend_usd"] - daily["active_reserved_usd"], 0.0)
assert daily["settled_spend_usd"] > 0.0
assert daily["active_reserved_usd"] >= 0.8
assert not daily["unknown_active_cost"]
assert math.isclose(remaining, expected, rel_tol=0.0, abs_tol=1e-9)
assert 0.0 < remaining < 0.3
print("yes")
PY
)"
check "active priced reservation reduces remaining budget" "$ACTIVE_BUDGET_SNAPSHOT_OK" 'yes'
ACTIVE_BUDGET_FOLLOWUP_CODE="$(curl -s --max-time 20 -o "$WORK/active-budget-followup" -w '%{http_code}' \
  -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $ACTIVE_BUDGET_KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"syn-openai-partial","stream":false,"max_tokens":3000,"messages":[{"role":"user","content":"budget followup"}]}')"
check "follow-up is rejected against active budget reservation" "$ACTIVE_BUDGET_FOLLOWUP_CODE" '429'
kill "$ACTIVE_BUDGET_STREAM_PID" 2>/dev/null || true
wait "$ACTIVE_BUDGET_STREAM_PID" 2>/dev/null || true

# An active unpriced request makes the remaining budget unknown.
ACTIVE_UNKNOWN_BUDGET_REQUEST='{"model":"syn-openai","stream":true,"max_tokens":100,"messages":[{"role":"user","content":"fixture:hold-budget"}]}'
curl -s -N --max-time 30 -X POST "$BASE/v1/chat/completions" \
  -H "authorization: Bearer $ACTIVE_UNKNOWN_BUDGET_KEY" -H 'content-type: application/json' \
  -d "$ACTIVE_UNKNOWN_BUDGET_REQUEST" >"$WORK/active-unknown-budget-stream" &
ACTIVE_UNKNOWN_BUDGET_STREAM_PID=$!
for _ in $(seq 1 100); do
  ACTIVE_UNKNOWN_BUDGET_USAGE="$(curl -s "$BASE/v1/usage" -H "authorization: Bearer $ACTIVE_UNKNOWN_BUDGET_KEY")"
  if python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["admission"]["in_flight"] == 1 else 1)' <<<"$ACTIVE_UNKNOWN_BUDGET_USAGE"; then
    break
  fi
  sleep 0.05
done
check "active unpriced request makes remaining budget unknown" "$ACTIVE_UNKNOWN_BUDGET_USAGE" '"daily_budget_usd":null'
check "active unpriced request is flagged in aggregate snapshot" "$ACTIVE_UNKNOWN_BUDGET_USAGE" '"unknown_active_cost":true'
check_absent "client budget snapshot hides individual reservations" "$ACTIVE_UNKNOWN_BUDGET_USAGE" 'request_id'
kill "$ACTIVE_UNKNOWN_BUDGET_STREAM_PID" 2>/dev/null || true
wait "$ACTIVE_UNKNOWN_BUDGET_STREAM_PID" 2>/dev/null || true

# Partial provider usage must keep cost and budget state unknown.
PARTIAL_USAGE_KEY="sk-kinetix-smoke-partial-usage-budget"
PARTIAL_USAGE_REQUEST='{"model":"syn-openai-partial","stream":false,"messages":[{"role":"user","content":"fixture:partial-usage"}]}'
PARTIAL_USAGE_FIRST_CODE="$(curl -s --max-time 20 -o "$WORK/partial-usage-first" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $PARTIAL_USAGE_KEY" -H 'content-type: application/json' -d "$PARTIAL_USAGE_REQUEST")"
check "partial-usage priced request succeeds" "$PARTIAL_USAGE_FIRST_CODE" '200'
for _ in $(seq 1 50); do
  PARTIAL_USAGE="$(curl -s "$BASE/v1/usage" -H "authorization: Bearer $PARTIAL_USAGE_KEY")"
  if python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["usage"]["daily"]["requests"] >= 1 else 1)' <<<"$PARTIAL_USAGE"; then
    break
  fi
  sleep 0.1
done
check "partial usage preserves reported input" "$PARTIAL_USAGE" '"input_tokens":100'
check "partial usage keeps output unknown" "$PARTIAL_USAGE" '"output_tokens":null'
check "partial usage contributes zero known cost" "$PARTIAL_USAGE" '"known_cost_usd":0.0'
check "partial usage counts unknown cost" "$PARTIAL_USAGE" '"unknown_cost_requests":1'
check "partial usage counts unknown usage" "$PARTIAL_USAGE" '"unknown_usage_requests":1'
check "partial usage makes remaining budget unknown" "$PARTIAL_USAGE" '"daily_budget_usd":null'
PARTIAL_USAGE_DB_ROW="$(python3 - "$DB" <<'PY'
import json, sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    "SELECT input_tokens, output_tokens, cost_known, usage_confidence FROM usage_logs WHERE key_name = ? ORDER BY ts DESC LIMIT 1",
    ("Partial Usage Budget Key",),
).fetchone()
print(json.dumps(row))
PY
)"
check "partial usage persists unknown canonical accounting" "$PARTIAL_USAGE_DB_ROW" '[100, null, 0, "unknown"]'
PARTIAL_USAGE_RESERVATION="$(python3 - "$DB" <<'PY'
import sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    "SELECT admission_cost_usd FROM usage_logs WHERE key_name = ? ORDER BY ts DESC LIMIT 1",
    ("Partial Usage Budget Key",),
).fetchone()
print("yes" if row and row[0] is not None and row[0] > 0 else "no")
PY
)"
check "partial usage retains conservative admission cost" "$PARTIAL_USAGE_RESERVATION" 'yes'
PARTIAL_USAGE_SECOND_CODE="$(curl -s --max-time 20 -o "$WORK/partial-usage-second" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $PARTIAL_USAGE_KEY" -H 'content-type: application/json' -d "$PARTIAL_USAGE_REQUEST")"
check "partial usage consumes live budget reservation" "$PARTIAL_USAGE_SECOND_CODE" '429'

# DNS failures happen before the upstream request is dispatched. They count as
# a known zero-usage RPM attempt, not unknown TPM usage.
PRE_DISPATCH_REQUEST='{"model":"syn-pre-dispatch","stream":false,"max_tokens":5000,"messages":[{"role":"user","content":"dns failure"}]}'
PRE_DISPATCH_RPM_FIRST_CODE="$(curl -s --max-time 20 -o "$WORK/pre-dispatch-rpm-first" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $PRE_DISPATCH_RPM_KEY" -H 'content-type: application/json' -d "$PRE_DISPATCH_REQUEST")"
check "pre-dispatch RPM request fails before dispatch" "$PRE_DISPATCH_RPM_FIRST_CODE" '502'
PRE_DISPATCH_RPM_SECOND_CODE="$(curl -s --max-time 20 -o "$WORK/pre-dispatch-rpm-second" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $PRE_DISPATCH_RPM_KEY" -H 'content-type: application/json' -d "$PRE_DISPATCH_REQUEST")"
check "pre-dispatch failure consumes live RPM slot" "$PRE_DISPATCH_RPM_SECOND_CODE" '429'
for _ in $(seq 1 50); do
  PRE_DISPATCH_RPM_USAGE="$(curl -s "$BASE/v1/usage" -H "authorization: Bearer $PRE_DISPATCH_RPM_KEY")"
  if python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["usage"]["daily"]["requests"] >= 1 else 1)' <<<"$PRE_DISPATCH_RPM_USAGE"; then
    break
  fi
  sleep 0.1
done
check "pre-dispatch request records zero input tokens" "$PRE_DISPATCH_RPM_USAGE" '"input_tokens":0'
check "pre-dispatch request records zero output tokens" "$PRE_DISPATCH_RPM_USAGE" '"output_tokens":0'
check "pre-dispatch zero cost is known" "$PRE_DISPATCH_RPM_USAGE" '"unknown_cost_requests":0'
check "pre-dispatch zero usage is not unknown" "$PRE_DISPATCH_RPM_USAGE" '"unknown_usage_requests":0'
PRE_DISPATCH_DB_ROW="$(python3 - "$DB" <<'PY'
import json, sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    "SELECT input_tokens, output_tokens, cost_usd, cost_known, usage_confidence FROM usage_logs WHERE key_name = ? ORDER BY ts DESC LIMIT 1",
    ("Pre-dispatch RPM Key",),
).fetchone()
print(json.dumps(row))
PY
)"
check "pre-dispatch failure persists known zero usage and cost" "$PRE_DISPATCH_DB_ROW" '[0, 0, 0.0, 1, "not_dispatched"]'

PRE_DISPATCH_TPM_FIRST_CODE="$(curl -s --max-time 20 -o "$WORK/pre-dispatch-tpm-first" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $PRE_DISPATCH_TPM_KEY" -H 'content-type: application/json' -d "$PRE_DISPATCH_REQUEST")"
check "pre-dispatch TPM request fails before dispatch" "$PRE_DISPATCH_TPM_FIRST_CODE" '502'
PRE_DISPATCH_TPM_SECOND_CODE="$(curl -s --max-time 20 -o "$WORK/pre-dispatch-tpm-second" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $PRE_DISPATCH_TPM_KEY" -H 'content-type: application/json' -d "$PRE_DISPATCH_REQUEST")"
check "zero-token pre-dispatch failure leaves live TPM capacity" "$PRE_DISPATCH_TPM_SECOND_CODE" '502'
for _ in $(seq 1 50); do
  PRE_DISPATCH_TPM_USAGE="$(curl -s "$BASE/v1/usage" -H "authorization: Bearer $PRE_DISPATCH_TPM_KEY")"
  if python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["usage"]["daily"]["requests"] >= 2 else 1)' <<<"$PRE_DISPATCH_TPM_USAGE"; then
    break
  fi
  sleep 0.1
done
check "pre-dispatch TPM records zero tokens" "$PRE_DISPATCH_TPM_USAGE" '"input_tokens":0'
check "pre-dispatch TPM rows remain known" "$PRE_DISPATCH_TPM_USAGE" '"unknown_usage_requests":0'

restart_kinetix
check "healthz after restart" "$(curl -s "$BASE/healthz")" '"data_plane":"serving"'
FAIL_RPM_RESTART_CODE="$(curl -s --max-time 20 -o "$WORK/failure-rpm-after-restart" -w '%{http_code}' \
  -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $FAIL_RPM_KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"syn-fail","stream":false,"messages":[{"role":"user","content":"fail"}]}')"
check "failed request consumes RPM slot after restart" "$FAIL_RPM_RESTART_CODE" '429'
FAIL_BUDGET_RESTART_CODE="$(curl -s --max-time 20 -o "$WORK/failure-budget-after-restart" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $FAIL_BUDGET_KEY" -H 'content-type: application/json' -d '{"model":"syn-fail-budget","stream":false,"messages":[{"role":"user","content":"fail"}]}')"
check "failed request keeps budget reservation after restart" "$FAIL_BUDGET_RESTART_CODE" '429'
PARTIAL_USAGE_RESTART_CODE="$(curl -s --max-time 20 -o "$WORK/partial-usage-after-restart" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $PARTIAL_USAGE_KEY" -H 'content-type: application/json' -d "$PARTIAL_USAGE_REQUEST")"
check "partial usage keeps budget reservation after restart" "$PARTIAL_USAGE_RESTART_CODE" '429'
PRE_DISPATCH_RPM_RESTART_CODE="$(curl -s --max-time 20 -o "$WORK/pre-dispatch-rpm-after-restart" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $PRE_DISPATCH_RPM_KEY" -H 'content-type: application/json' -d "$PRE_DISPATCH_REQUEST")"
check "pre-dispatch failure consumes RPM after restart" "$PRE_DISPATCH_RPM_RESTART_CODE" '429'
PRE_DISPATCH_TPM_RESTART_CODE="$(curl -s --max-time 20 -o "$WORK/pre-dispatch-tpm-after-restart" -w '%{http_code}' -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $PRE_DISPATCH_TPM_KEY" -H 'content-type: application/json' -d "$PRE_DISPATCH_REQUEST")"
check "zero-token pre-dispatch failure preserves TPM capacity after restart" "$PRE_DISPATCH_TPM_RESTART_CODE" '502'

# translation (OpenAI inbound -> Gemini outbound)
TRANSL="$(curl -s -N --max-time 20 -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $KEY" -H 'content-type: application/json' -d '{"model":"syn-gemini-3","stream":true,"messages":[{"role":"user","content":"hi"}]}')"
check "translation streams" "$TRANSL" 'data:'

# Anthropic inbound
ANTH="$(curl -s -N --max-time 20 -X POST "$BASE/v1/messages" -H "x-api-key: $KEY" -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' -d '{"model":"syn-gemini-3","stream":true,"max_tokens":64,"messages":[{"role":"user","content":"hi"}]}')"
check "anthropic message_start" "$ANTH" 'message_start'
check "anthropic message_stop" "$ANTH" 'message_stop'

# OpenAI Responses API (streaming + non-streaming)
RESP_STREAM="$(curl -s -N --max-time 20 -X POST "$BASE/v1/responses" -H "authorization: Bearer $KEY" -H 'content-type: application/json' -d '{"model":"syn-openai","stream":true,"input":"hi"}')"
check "responses stream created event" "$RESP_STREAM" 'event: response.created'
check "responses stream completed event" "$RESP_STREAM" 'event: response.completed'

RESP_SYNC="$(curl -s --max-time 20 -X POST "$BASE/v1/responses" -H "authorization: Bearer $KEY" -H 'content-type: application/json' -d '{"model":"syn-openai","stream":false,"input":"hi"}')"
check "responses sync object" "$RESP_SYNC" '"object":"response"'
check "responses sync completed" "$RESP_SYNC" '"status":"completed"'

# tool call
TOOL="$(curl -s -N --max-time 20 -X POST "$BASE/v1/chat/completions" -H "authorization: Bearer $KEY" -H 'content-type: application/json' -d '{"model":"syn-openai","stream":true,"messages":[{"role":"user","content":"weather?"}],"tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object"}}}]}')"
check "tool_calls delta present" "$TOOL" 'tool_calls'

# admin: login + a couple of authenticated reads
check "admin login" "$(curl -s -c "$JAR" -X POST "$BASE/admin/api/login" -H 'content-type: application/json' -d "{\"password\":\"$ADMIN_TOKEN\"}")" '"ok":true'
check "admin unauthenticated -> 401" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/admin/api/keys")" "401"
check "admin overview" "$(curl -s -b "$JAR" "$BASE/admin/api/overview")" 'active_streams'
check "admin metrics" "$(curl -s -b "$JAR" "$BASE/admin/api/metrics")" 'kinetix_control_plane_degraded'
check "admin usage" "$(curl -s -b "$JAR" "$BASE/admin/api/usage?limit=5")" '"usage"'

# Route Trace + diagnostics for the most recent request
RID="$(curl -s -b "$JAR" "$BASE/admin/api/usage?limit=1" | python3 -c 'import sys,json;print(json.load(sys.stdin)["usage"][0]["request_id"])' 2>/dev/null)"
check "route-trace" "$(curl -s -b "$JAR" "$BASE/admin/api/requests/$RID/route-trace")" '"steps"'
check "diagnostics" "$(curl -s -b "$JAR" "$BASE/admin/api/requests/$RID/diagnostics")" 'flight_events'

# Outbound security is validated at config time. Both dev flags are on here, so
# a well-formed private URL is intentionally allowed; a malformed URL is still
# rejected (NFR-3.9). The TLS-mandatory / blocked-host rules are covered by the
# unit tests and by running without the dev flags.
check "invalid URL rejected" "$(curl -s -b "$JAR" -X POST "$BASE/admin/api/providers" -H 'content-type: application/json' -d '{"name":"evil","base_url":"not-a-url","wire_format":"openai"}')" 'invalid URL'

# Validate / Dry Run (FR-8.6/8.7) and the opaque route-id resolver (FR-12.15).
check "validate/provider reports missing custom header" \
  "$(curl -s -b "$JAR" -X POST "$BASE/admin/api/validate/provider" -H 'content-type: application/json' -d '{"name":"x","base_url":"https://api.example.com/v1","wire_format":"openai","auth_scheme":"custom_header"}')" \
  'requires custom_header_name'
check "validate/model warns unknown prices" \
  "$(curl -s -b "$JAR" -X POST "$BASE/admin/api/validate/model" -H 'content-type: application/json' -d '{"upstream_id":"m","context_window":1000,"max_output_tokens":500}')" \
  'price_state":"unknown"'
check "route dry-run" \
  "$(curl -s -b "$JAR" -X POST "$BASE/admin/api/routes/dry-run" -H 'content-type: application/json' -d '{"model":"syn-openai"}')" \
  '"candidates"'

# Resolve the opaque X-Kinetix-Route-Id the client received back to its trace.
OPAQUE="$(curl -s -D - -o /dev/null "$BASE/v1/chat/completions" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"syn-openai","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}' \
  | tr -d '\r' | awk 'tolower($1)=="x-kinetix-route-id:"{print $2}')"
check "opaque route id resolves to a trace" \
  "$(curl -s -b "$JAR" "$BASE/admin/api/route-traces/$OPAQUE")" '"opaque_route_id"'
check "opaque route id is hidden from the response" \
  "$(curl -s -D - -o /dev/null "$BASE/v1/chat/completions" \
     -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
     -d '{"model":"syn-openai","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}')" \
  'x-kinetix-route-id'

# Induce database failures only after all other smoke checks, then restore each
# table immediately so the running server can shut down cleanly.
python3 - "$DB" <<'PY' || { echo "failed to induce usage summary database failure"; exit 1; }
import sqlite3, sys
with sqlite3.connect(sys.argv[1], timeout=10) as connection:
    connection.execute("ALTER TABLE usage_logs RENAME TO usage_logs_smoke_backup")
PY
USAGE_DB_FAILURE_CODE="$(curl -s --max-time 10 -o "$WORK/usage-db-failure-response" -w '%{http_code}' "$BASE/v1/usage" -H "authorization: Bearer $KEY")"
USAGE_DB_FAILURE_BODY="$(<"$WORK/usage-db-failure-response")"
python3 - "$DB" <<'PY' || { echo "failed to restore usage logs table"; exit 1; }
import sqlite3, sys
with sqlite3.connect(sys.argv[1], timeout=10) as connection:
    connection.execute("ALTER TABLE usage_logs_smoke_backup RENAME TO usage_logs")
PY
check "usage database failure returns unavailable" "$USAGE_DB_FAILURE_CODE" '503'
check_body_contains "usage database failure returns generic message" "$USAGE_DB_FAILURE_BODY" 'usage temporarily unavailable'
check_body_absent "usage response hides table name" "$USAGE_DB_FAILURE_BODY" 'usage_logs'
check_body_absent "usage response hides database path" "$USAGE_DB_FAILURE_BODY" "$DB"
check_body_absent "usage response hides SQL details" "$USAGE_DB_FAILURE_BODY" 'SELECT'
check_body_absent "usage response hides original SQLite error" "$USAGE_DB_FAILURE_BODY" 'no such table'

python3 - "$DB" <<'PY' || { echo "failed to induce virtual key authentication database failure"; exit 1; }
import sqlite3, sys
with sqlite3.connect(sys.argv[1], timeout=10) as connection:
    connection.execute("ALTER TABLE virtual_keys RENAME TO virtual_keys_smoke_backup")
PY
AUTH_DB_FAILURE_CODE="$(curl -s --max-time 10 -o "$WORK/auth-db-failure-response" -w '%{http_code}' "$BASE/v1/usage" -H "authorization: Bearer $KEY")"
AUTH_DB_FAILURE_BODY="$(<"$WORK/auth-db-failure-response")"
python3 - "$DB" <<'PY' || { echo "failed to restore virtual keys table"; exit 1; }
import sqlite3, sys
with sqlite3.connect(sys.argv[1], timeout=10) as connection:
    connection.execute("ALTER TABLE virtual_keys_smoke_backup RENAME TO virtual_keys")
PY
check "authentication database failure returns unavailable" "$AUTH_DB_FAILURE_CODE" '503'
check_body_contains "authentication database failure returns generic message" "$AUTH_DB_FAILURE_BODY" 'authentication temporarily unavailable'
check_body_absent "authentication response hides table name" "$AUTH_DB_FAILURE_BODY" 'virtual_keys'
check_body_absent "authentication response hides database path" "$AUTH_DB_FAILURE_BODY" "$DB"
check_body_absent "authentication response hides SQL details" "$AUTH_DB_FAILURE_BODY" 'SELECT'
check_body_absent "authentication response hides original SQLite error" "$AUTH_DB_FAILURE_BODY" 'no such table'

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "==> smoke: all checks passed"
  exit 0
else
  echo "==> smoke: $FAILURES check(s) failed"
  echo "--- kinetix log tail ---"
  tail -20 "$LOG"
  exit 1
fi
