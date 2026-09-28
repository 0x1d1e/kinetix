#!/usr/bin/env bash
# Manual release acceptance against a real Kinetix deployment.
# Intentionally NOT called from normal PR/local CI: these sessions can consume
# real provider quota and depend on installed external clients/toolchains.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLIENT="${1:-all}"
BASE="${KINETIX_BASE:-}"
KEY="${KINETIX_KEY:-}"
ADMIN_TOKEN="${KINETIX_ADMIN_TOKEN:-}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACTS="${KINETIX_ACCEPT_ARTIFACT_DIR:-$ROOT/acceptance-artifacts/$STAMP}"
WORK="$(mktemp -d)"
SUMMARY="$ARTIFACTS/summary.tsv"
PROXY_PID=""
PROXY_BASE=""
TRACE=""
CASE_ID=""

PI_SAME_MODEL="${KINETIX_ACCEPT_PI_SAME_MODEL:-}"
PI_TRANSLATED_MODEL="${KINETIX_ACCEPT_PI_TRANSLATED_MODEL:-}"
PI_FALLBACK_MODEL="${KINETIX_ACCEPT_PI_FALLBACK_MODEL:-}"
PI_AFFINITY_MODEL="${KINETIX_ACCEPT_PI_AFFINITY_MODEL:-}"

CLAUDE_SAME_MODEL="${KINETIX_ACCEPT_CLAUDE_SAME_MODEL:-}"
CLAUDE_TRANSLATED_MODEL="${KINETIX_ACCEPT_CLAUDE_TRANSLATED_MODEL:-}"
CLAUDE_FALLBACK_MODEL="${KINETIX_ACCEPT_CLAUDE_FALLBACK_MODEL:-}"
CLAUDE_AFFINITY_MODEL="${KINETIX_ACCEPT_CLAUDE_AFFINITY_MODEL:-}"

RESPONSES_OPENAI_MODEL="${KINETIX_ACCEPT_RESPONSES_OPENAI_MODEL:-}"
RESPONSES_GEMINI_MODEL="${KINETIX_ACCEPT_RESPONSES_GEMINI_MODEL:-}"
RESPONSES_ANTHROPIC_MODEL="${KINETIX_ACCEPT_RESPONSES_ANTHROPIC_MODEL:-}"
RESPONSES_FALLBACK_MODEL="${KINETIX_ACCEPT_RESPONSES_FALLBACK_MODEL:-}"
RESPONSES_AFFINITY_MODEL="${KINETIX_ACCEPT_RESPONSES_AFFINITY_MODEL:-}"

mkdir -p "$ARTIFACTS"
printf 'profile\tscenario\tmodel\tstatus\tcategory\tversion\n' >"$SUMMARY"

cleanup() {
  if [ -n "${PROXY_PID:-}" ]; then
    kill "$PROXY_PID" 2>/dev/null || true
    wait "$PROXY_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

die() {
  echo "release acceptance: $*" >&2
  exit 2
}

[ -n "$BASE" ] || die "KINETIX_BASE is required"
[ -n "$KEY" ] || die "KINETIX_KEY is required"
BASE="${BASE%/}"

case "$CLIENT" in
  all|pi|claude|responses|plugin) ;;
  *) die "usage: $0 [all|pi|claude|responses|plugin]" ;;
esac

require_value() {
  local name="$1"
  local value="$2"
  [ -n "$value" ] || die "$name is required for the selected release acceptance matrix"
}

record_version() {
  local profile="$1"
  shift
  local out="$ARTIFACTS/$profile.version.txt"
  if "$@" >"$out" 2>&1; then
    head -n 1 "$out" | tr '\t\n' '  '
  else
    echo unavailable
  fi
}

classify() {
  local log="$1"
  local trace="$2"
  local scenario="$3"
  local combined="$WORK/classify.txt"
  cat "$log" "$trace" >"$combined" 2>/dev/null || true

  if grep -Eqi '401|403|unauthori|authentication|invalid.*key' "$combined"; then
    echo auth
  elif grep -Eqi '404|model.*not found|unknown model|no.*model' "$combined"; then
    echo model
  elif grep -Eqi 'timed out|timeout|connection refused|connect error|dns|network|RemoteDisconnected' "$combined"; then
    echo transport
  elif grep -Eqi 'cannot be translated|translated subset|translation|portability|no canonical cross-format' "$combined"; then
    echo translation
  elif [ "$scenario" = "fallback" ] || [ "$scenario" = "affinity" ] || \
       grep -Eqi 'no eligible target|no healthy|all targets|routing|route.*failed' "$combined"; then
    echo routing
  elif grep -Eqi '"response_status": (429|50[234])|upstream|rate.?limit|quota' "$combined"; then
    echo upstream
  elif grep -Eqi '"response_status": (400|422)|invalid_request|protocol|SSE|stream' "$combined"; then
    echo frontend
  else
    echo client
  fi
}

prepare_project() {
  local dir="$WORK/project-$CASE_ID"
  mkdir -p "$dir"
  printf 'alpha-sentinel\n' >"$dir/acceptance-a.txt"
  printf 'beta-sentinel\n' >"$dir/acceptance-b.txt"
  python3 - "$dir/pixel.png" <<'PY'
import base64
import pathlib
import sys
pathlib.Path(sys.argv[1]).write_bytes(base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wlq9mQAAAAASUVORK5CYII="
))
PY
  echo "$dir"
}

start_proxy() {
  local name="$1"
  local port_file="$WORK/$name.port"
  TRACE="$ARTIFACTS/$name.trace.jsonl"
  rm -f "$port_file" "$TRACE"
  python3 "$ROOT/scripts/release-client-proxy.py" \
    --upstream "$BASE" \
    --log "$TRACE" \
    --port-file "$port_file" \
    >"$ARTIFACTS/$name.proxy.log" 2>&1 &
  PROXY_PID=$!

  local i
  for i in $(seq 1 100); do
    if [ -s "$port_file" ]; then
      PROXY_BASE="http://127.0.0.1:$(cat "$port_file")"
      return 0
    fi
    if ! kill -0 "$PROXY_PID" 2>/dev/null; then
      cat "$ARTIFACTS/$name.proxy.log" >&2 || true
      return 1
    fi
    sleep 0.05
  done
  echo "acceptance evidence proxy failed to start" >&2
  return 1
}

stop_proxy() {
  if [ -n "${PROXY_PID:-}" ]; then
    kill "$PROXY_PID" 2>/dev/null || true
    wait "$PROXY_PID" 2>/dev/null || true
    PROXY_PID=""
  fi
}

verify_trace() {
  local trace="$1"
  local require_fallback="$2"
  local require_affinity="$3"
  local require_multi_tools="$4"
  python3 - "$trace" "$require_fallback" "$require_affinity" "$require_multi_tools" <<'PY'
import collections
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
require_fallback = sys.argv[2] == "1"
require_affinity = sys.argv[3] == "1"
require_multi = sys.argv[4] == "1"

records = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
inference = [
    row for row in records
    if row.get("path", "").split("?", 1)[0] in {
        "/v1/chat/completions",
        "/v1/messages",
        "/v1/responses",
    }
]
if len(inference) < 2:
    raise SystemExit(f"expected a multi-turn client session, saw {len(inference)} inference request(s)")

non_streaming = [
    (row.get("path"), row.get("request_stream"), row.get("response_content_type"))
    for row in inference
    if row.get("request_stream") is not True
    or row.get("response_content_type", "").split(";", 1)[0].strip().lower()
    != "text/event-stream"
]
if non_streaming:
    raise SystemExit(
        "real-client inference turn was not streaming SSE: "
        + repr(non_streaming)
    )

if require_multi:
    tool_ids = {tool_id for row in inference for tool_id in row.get("tool_call_ids", [])}
    result_refs = sum(int(row.get("request_tool_results", 0)) for row in inference)
    if len(tool_ids) < 2:
        raise SystemExit(f"expected at least two distinct tool calls, saw {sorted(tool_ids)}")
    if result_refs < 2:
        raise SystemExit(f"expected at least two returned tool results, saw {result_refs}")

if require_fallback and not any(row.get("fallback") == "1" for row in inference):
    raise SystemExit("forced-fallback route never returned X-Kinetix-Fallback: 1")

if require_affinity:
    counts = collections.Counter(
        value
        for row in inference
        for value in row.get("session_headers", {}).values()
        if value
    )
    if not any(count >= 2 for count in counts.values()):
        raise SystemExit(f"no stable session-affinity header across client turns: {dict(counts)}")

print(
    f"trace ok: streaming_sse_requests={len(inference)} "
    f"tool_results={sum(int(row.get('request_tool_results', 0)) for row in inference)}"
)
PY
}

verify_affinity_routes() {
  local trace="$1"
  python3 - "$trace" "$BASE" "$ADMIN_TOKEN" <<'PY'
import json
import pathlib
import sys
import time
import urllib.error
import urllib.request

trace_path = pathlib.Path(sys.argv[1])
base = sys.argv[2].rstrip("/")
admin_token = sys.argv[3]
rows = [
    json.loads(line)
    for line in trace_path.read_text().splitlines()
    if line.strip()
]
opaque_ids = [
    row.get("opaque_route_id")
    for row in rows
    if row.get("opaque_route_id")
    and row.get("path", "").split("?", 1)[0] in {
        "/v1/chat/completions",
        "/v1/messages",
        "/v1/responses",
    }
]
if len(opaque_ids) < 2:
    raise SystemExit(f"need at least two route traces for affinity proof, saw {len(opaque_ids)}")

final_targets = []
for opaque_id in opaque_ids:
    url = f"{base}/admin/api/route-traces/{opaque_id}"
    last_error = None
    for _ in range(20):
        request = urllib.request.Request(
            url,
            headers={"x-kinetix-admin-token": admin_token},
        )
        try:
            with urllib.request.urlopen(request, timeout=15) as response:
                trace = json.loads(response.read())
            target = trace.get("final_target")
            if not target:
                raise SystemExit(f"route trace {opaque_id} has no final_target")
            final_targets.append(target)
            break
        except urllib.error.HTTPError as error:
            last_error = error
            if error.code != 404:
                raise
            time.sleep(0.1)
    else:
        raise SystemExit(f"route trace {opaque_id} unavailable: {last_error}")

if len(set(final_targets)) != 1:
    raise SystemExit(f"session affinity changed target across turns: {final_targets}")
print(f"affinity ok: {len(final_targets)} turns -> {final_targets[0]}")
PY
}

run_pi() {
  local model="$1"
  command -v pi >/dev/null || return 127
  local project agent
  project="$(prepare_project)"
  agent="$WORK/pi-agent-$CASE_ID"
  mkdir -p "$agent"
  KINETIX_BASE="$BASE" \
  KINETIX_ADMIN_TOKEN="$ADMIN_TOKEN" \
  KINETIX_KEY="$KEY" \
    python3 "$ROOT/scripts/release-client-pi-profile.py" "$model" "$PROXY_BASE" "$agent"
  (
    cd "$project"
    PI_CODING_AGENT_DIR="$agent" \
    KINETIX_API_KEY="$KEY" \
      pi --provider kinetix --model "$model" --thinking high --mode json \
      @pixel.png \
      "Use two separate file-tool calls: first read acceptance-a.txt, then make a second tool call to read acceptance-b.txt. Report alpha-sentinel and beta-sentinel exactly, describe the attached image, then finish normally."
  )
}

run_claude() {
  local model="$1"
  command -v claude >/dev/null || return 127
  local project
  project="$(prepare_project)"
  (
    cd "$project"
    ANTHROPIC_BASE_URL="$PROXY_BASE" \
    ANTHROPIC_API_KEY="$KEY" \
    ANTHROPIC_AUTH_TOKEN="" \
      claude -p \
      --model "$model" \
      --output-format stream-json \
      --verbose \
      "Use two separate file-tool calls: first read acceptance-a.txt, then make a second tool call to read acceptance-b.txt. Report alpha-sentinel and beta-sentinel exactly, then finish normally."
  )
}

run_responses() {
  local model="$1"
  command -v codex >/dev/null || return 127
  local project home session
  project="$(prepare_project)"
  home="$WORK/codex-home-$CASE_ID"
  session="kinetix-release-$STAMP-$CASE_ID"
  mkdir -p "$home"
  cat >"$home/config.toml" <<EOF
model = "$model"
model_provider = "kinetix"
model_reasoning_effort = "high"
approval_policy = "never"
sandbox_mode = "workspace-write"

[model_providers.kinetix]
name = "Kinetix"
base_url = "$PROXY_BASE/v1"
env_key = "KINETIX_ACCEPT_KEY"
wire_api = "responses"
requires_openai_auth = false
http_headers = { "X-Kinetix-Session" = "$session" }
EOF
  (
    cd "$project"
    CODEX_HOME="$home" \
    KINETIX_ACCEPT_KEY="$KEY" \
      codex exec --json --skip-git-repo-check \
      "Use two separate file-tool calls: first read acceptance-a.txt, then make a second tool call to read acceptance-b.txt. Report alpha-sentinel and beta-sentinel exactly, then finish normally."
  )
}

run_client_case() {
  local profile="$1"
  local scenario="$2"
  local model="$3"
  local version="$4"
  local require_fallback="$5"
  local require_affinity="$6"
  local runner="$7"
  CASE_ID="$profile-$scenario"
  local log="$ARTIFACTS/$CASE_ID.jsonl"
  local verify_log="$ARTIFACTS/$CASE_ID.verify.txt"

  echo "==> $profile / $scenario / $model ($version)"
  if ! start_proxy "$CASE_ID"; then
    printf '%s\t%s\t%s\tFAIL\ttransport\t%s\n' "$profile" "$scenario" "$model" "$version" >>"$SUMMARY"
    return 1
  fi

  local rc=0
  "$runner" "$model" >"$log" 2>&1 || rc=$?
  stop_proxy

  if [ "$rc" -eq 0 ] && { ! grep -q 'alpha-sentinel' "$log" || ! grep -q 'beta-sentinel' "$log"; }; then
    echo "grounded sentinel missing" >"$verify_log"
    rc=1
  fi
  if [ "$rc" -eq 0 ] && ! verify_trace "$TRACE" "$require_fallback" "$require_affinity" 1 >"$verify_log" 2>&1; then
    rc=1
  fi
  if [ "$rc" -eq 0 ] && [ "$require_affinity" = "1" ] && ! verify_affinity_routes "$TRACE" >>"$verify_log" 2>&1; then
    rc=1
  fi

  if [ "$rc" -eq 0 ]; then
    printf '%s\t%s\t%s\tPASS\t-\t%s\n' "$profile" "$scenario" "$model" "$version" >>"$SUMMARY"
    echo "    PASS"
    return 0
  fi

  local category
  category="$(classify "$log" "$TRACE" "$scenario")"
  if [ -s "$verify_log" ] && grep -Eqi 'fallback|affinity|session' "$verify_log"; then
    category="routing"
  fi
  printf '%s\t%s\t%s\tFAIL\t%s\t%s\n' "$profile" "$scenario" "$model" "$category" "$version" >>"$SUMMARY"
  echo "    FAIL [$category] -- see $log, $TRACE, $verify_log" >&2
  return 1
}

run_claude_count_tokens() {
  local model="$1"
  CASE_ID="claude-count_tokens"
  local log="$ARTIFACTS/$CASE_ID.jsonl"
  echo "==> claude / count_tokens / $model"
  if ! start_proxy "$CASE_ID"; then
    return 1
  fi
  local rc=0
  PROXY_BASE="$PROXY_BASE" KINETIX_ACCEPT_KEY="$KEY" KINETIX_ACCEPT_MODEL="$model" \
    python3 - >"$log" 2>&1 <<'PY' || rc=$?
import json
import os
import urllib.request

payload = {
    "model": os.environ["KINETIX_ACCEPT_MODEL"],
    "system": "release acceptance token count",
    "messages": [{"role": "user", "content": "count this"}],
    "tools": [{"name": "read_file", "input_schema": {"type": "object"}}],
}
request = urllib.request.Request(
    os.environ["PROXY_BASE"].rstrip("/") + "/v1/messages/count_tokens",
    data=json.dumps(payload).encode(),
    headers={
        "content-type": "application/json",
        "x-api-key": os.environ["KINETIX_ACCEPT_KEY"],
        "anthropic-version": "2023-06-01",
    },
)
with urllib.request.urlopen(request, timeout=30) as response:
    data = json.loads(response.read())
    mode = response.headers.get("x-kinetix-token-count")
if not isinstance(data.get("input_tokens"), int) or data["input_tokens"] <= 0:
    raise SystemExit(f"invalid count_tokens response: {data}")
if mode != "exact":
    raise SystemExit(f"same-format Anthropic count_tokens was not exact: {mode}")
print(json.dumps({"input_tokens": data["input_tokens"], "mode": mode}))
PY
  stop_proxy
  return "$rc"
}

run_plugin() {
  local package="${KINETIX_PLUGIN_E2E_PACKAGE:-}"
  [ -n "$package" ] || { echo "KINETIX_PLUGIN_E2E_PACKAGE is required for plugin acceptance" >&2; return 2; }
  [ -f "$package" ] || { echo "plugin package does not exist: $package" >&2; return 2; }
  (
    cd "$ROOT"
    KINETIX_PLUGIN_E2E_PACKAGE="$package" cargo test --test plugin_e2e -- --nocapture
  )
}

failures=0

if [ "$CLIENT" = all ] || [ "$CLIENT" = pi ]; then
  require_value KINETIX_ADMIN_TOKEN "$ADMIN_TOKEN"
  require_value KINETIX_ACCEPT_PI_SAME_MODEL "$PI_SAME_MODEL"
  require_value KINETIX_ACCEPT_PI_TRANSLATED_MODEL "$PI_TRANSLATED_MODEL"
  require_value KINETIX_ACCEPT_PI_FALLBACK_MODEL "$PI_FALLBACK_MODEL"
  require_value KINETIX_ACCEPT_PI_AFFINITY_MODEL "$PI_AFFINITY_MODEL"
  command -v pi >/dev/null || die "pi is not installed"
  version="$(record_version pi pi --version)"
  run_client_case pi same-format "$PI_SAME_MODEL" "$version" 0 0 run_pi || failures=$((failures + 1))
  run_client_case pi translated "$PI_TRANSLATED_MODEL" "$version" 0 0 run_pi || failures=$((failures + 1))
  run_client_case pi fallback "$PI_FALLBACK_MODEL" "$version" 1 0 run_pi || failures=$((failures + 1))
  run_client_case pi affinity "$PI_AFFINITY_MODEL" "$version" 0 1 run_pi || failures=$((failures + 1))
fi

if [ "$CLIENT" = all ] || [ "$CLIENT" = claude ]; then
  require_value KINETIX_ADMIN_TOKEN "$ADMIN_TOKEN"
  require_value KINETIX_ACCEPT_CLAUDE_SAME_MODEL "$CLAUDE_SAME_MODEL"
  require_value KINETIX_ACCEPT_CLAUDE_TRANSLATED_MODEL "$CLAUDE_TRANSLATED_MODEL"
  require_value KINETIX_ACCEPT_CLAUDE_FALLBACK_MODEL "$CLAUDE_FALLBACK_MODEL"
  require_value KINETIX_ACCEPT_CLAUDE_AFFINITY_MODEL "$CLAUDE_AFFINITY_MODEL"
  command -v claude >/dev/null || die "claude is not installed"
  version="$(record_version claude claude --version)"
  run_client_case claude same-format "$CLAUDE_SAME_MODEL" "$version" 0 0 run_claude || failures=$((failures + 1))
  run_client_case claude translated "$CLAUDE_TRANSLATED_MODEL" "$version" 0 0 run_claude || failures=$((failures + 1))
  run_client_case claude fallback "$CLAUDE_FALLBACK_MODEL" "$version" 1 0 run_claude || failures=$((failures + 1))
  run_client_case claude affinity "$CLAUDE_AFFINITY_MODEL" "$version" 0 1 run_claude || failures=$((failures + 1))
  if run_claude_count_tokens "$CLAUDE_SAME_MODEL"; then
    printf 'claude\tcount_tokens\t%s\tPASS\t-\t%s\n' "$CLAUDE_SAME_MODEL" "$version" >>"$SUMMARY"
    echo "    PASS"
  else
    category="$(classify "$ARTIFACTS/claude-count_tokens.jsonl" "$TRACE" count_tokens)"
    printf 'claude\tcount_tokens\t%s\tFAIL\t%s\t%s\n' "$CLAUDE_SAME_MODEL" "$category" "$version" >>"$SUMMARY"
    failures=$((failures + 1))
  fi
fi

if [ "$CLIENT" = all ] || [ "$CLIENT" = responses ]; then
  require_value KINETIX_ADMIN_TOKEN "$ADMIN_TOKEN"
  require_value KINETIX_ACCEPT_RESPONSES_OPENAI_MODEL "$RESPONSES_OPENAI_MODEL"
  require_value KINETIX_ACCEPT_RESPONSES_GEMINI_MODEL "$RESPONSES_GEMINI_MODEL"
  require_value KINETIX_ACCEPT_RESPONSES_ANTHROPIC_MODEL "$RESPONSES_ANTHROPIC_MODEL"
  require_value KINETIX_ACCEPT_RESPONSES_FALLBACK_MODEL "$RESPONSES_FALLBACK_MODEL"
  require_value KINETIX_ACCEPT_RESPONSES_AFFINITY_MODEL "$RESPONSES_AFFINITY_MODEL"
  command -v codex >/dev/null || die "codex is not installed"
  version="$(record_version responses codex --version)"
  # Responses has no native Responses upstream passthrough in Kinetix v1; all
  # three built-in provider paths are translation paths by design.
  run_client_case responses translated-openai "$RESPONSES_OPENAI_MODEL" "$version" 0 0 run_responses || failures=$((failures + 1))
  run_client_case responses translated-gemini "$RESPONSES_GEMINI_MODEL" "$version" 0 0 run_responses || failures=$((failures + 1))
  run_client_case responses translated-anthropic "$RESPONSES_ANTHROPIC_MODEL" "$version" 0 0 run_responses || failures=$((failures + 1))
  run_client_case responses fallback "$RESPONSES_FALLBACK_MODEL" "$version" 1 0 run_responses || failures=$((failures + 1))
  run_client_case responses affinity "$RESPONSES_AFFINITY_MODEL" "$version" 0 1 run_responses || failures=$((failures + 1))
fi

if [ "$CLIENT" = plugin ]; then
  version="cargo $(cargo --version 2>/dev/null || echo unavailable)"
  CASE_ID="plugin-real-guest"
  log="$ARTIFACTS/$CASE_ID.log"
  if run_plugin >"$log" 2>&1; then
    printf 'plugin\treal-guest\t%s\tPASS\t-\t%s\n' "${KINETIX_PLUGIN_E2E_PACKAGE:-}" "$version" >>"$SUMMARY"
  else
    printf 'plugin\treal-guest\t%s\tFAIL\tclient\t%s\n' "${KINETIX_PLUGIN_E2E_PACKAGE:-}" "$version" >>"$SUMMARY"
    failures=$((failures + 1))
  fi
elif [ "$CLIENT" = all ] && [ -n "${KINETIX_PLUGIN_E2E_PACKAGE:-}" ]; then
  version="cargo $(cargo --version 2>/dev/null || echo unavailable)"
  CASE_ID="plugin-real-guest"
  log="$ARTIFACTS/$CASE_ID.log"
  if run_plugin >"$log" 2>&1; then
    printf 'plugin\treal-guest\t%s\tPASS\t-\t%s\n' "$KINETIX_PLUGIN_E2E_PACKAGE" "$version" >>"$SUMMARY"
  else
    printf 'plugin\treal-guest\t%s\tFAIL\tclient\t%s\n' "$KINETIX_PLUGIN_E2E_PACKAGE" "$version" >>"$SUMMARY"
    failures=$((failures + 1))
  fi
else
  echo "==> plugin: SKIP (set KINETIX_PLUGIN_E2E_PACKAGE to include real .kxp acceptance)"
fi

echo
column -t -s $'\t' "$SUMMARY" 2>/dev/null || cat "$SUMMARY"
echo "Artifacts: $ARTIFACTS"

[ "$failures" -eq 0 ] || exit 1
