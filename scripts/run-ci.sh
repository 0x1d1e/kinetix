#!/usr/bin/env bash
# Local replica of .github/workflows/ci.yml. Runs the format, dashboard,
# rust, and dependency-policy jobs with the same dependency graph as CI
# (rust needs format + dashboard to finish first; the rest run in parallel).
#
#   scripts/run-ci.sh              # full gate, mirrors ci.yml on push
#   scripts/run-ci.sh --skip-deps  # skip the dependency-policy (cargo-deny) job
#                               # (in ci.yml this job only runs on push, not PRs)
set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"

SKIP_DEPS=false
for arg in "$@"; do
  case "$arg" in
    --skip-deps)
      SKIP_DEPS=true
      ;;
    -h|--help)
      printf 'Usage: %s [--skip-deps]\n\n' "$0"
      printf 'Mirrors .github/workflows/ci.yml locally:\n'
      printf '  format             cargo fmt --all -- --check + documentation checks\n'
      printf '  dashboard          npm ci + tsc --noEmit + npm run build\n'
      printf '  rust               clippy + test + OpenCode v1 + build + compat-matrix (needs format, dashboard)\n'
      printf '  dependency-policy  cargo-deny --all-features check (push-only in CI)\n\n'
      printf '  --skip-deps   Skip the dependency-policy job.\n'
      exit 0
      ;;
    *)
      printf 'Unknown argument: %s\n' "$arg" >&2
      exit 1
      ;;
  esac
done

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
  GREEN=$'\033[32m'
  RED=$'\033[31m'
  YELLOW=$'\033[33m'
  CYAN=$'\033[36m'
  DIM=$'\033[2m'
  RESET=$'\033[0m'
else
  GREEN=""
  RED=""
  YELLOW=""
  CYAN=""
  DIM=""
  RESET=""
fi

LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kinetix-ci.XXXXXX")"

cleanup() {
  rm -rf "$LOG_DIR"
}

interrupt() {
  printf '\n%sCI interrupted%s\n' "$RED" "$RESET"
  jobs -p | xargs -r kill 2>/dev/null || true
  wait 2>/dev/null || true
  exit 130
}

trap cleanup EXIT
trap interrupt INT TERM

format_duration() {
  local seconds="$1"
  printf '%dm %02ds' "$((seconds / 60))" "$((seconds % 60))"
}

run_step() {
  local label="$1"
  shift
  printf '\n==> %s\n' "$label"
  "$@"
}

format_job() {
  run_step "cargo fmt --check" cargo fmt --all -- --check &&
    run_step "documentation checks" bash scripts/check-docs.sh
}

dashboard_job() {
  (
    cd dashboard || exit 1
    run_step "npm ci" npm ci &&
      run_step "tsc --noEmit" npx tsc --noEmit &&
      run_step "npm run build" npm run build
  )
}

# Mirrors the rust job: needs the dashboard/dist bundle on disk (CI downloads
# it as an artifact; here it's already in place from dashboard_job) since
# it's embedded into the binary via rust-embed.
rust_job() {
  run_step "cargo clippy --all-targets" cargo clippy --all-targets &&
    run_step "cargo test --quiet" cargo test --quiet &&
    run_step "stable OpenCode v1 profile compatibility" bash scripts/test-opencode-v1-profile.sh &&
    run_step "cargo build --quiet --bin kinetix" cargo build --quiet --bin kinetix &&
    run_step "compat-matrix.sh" env \
      KINETIX_BIN="target/debug/kinetix" \
      SYN_TOKENS="4" \
      SYN_DELAY_MS="0" \
      bash scripts/compat-matrix.sh
}

dependency_policy_job() {
  # ci.yml's cargo-deny-action passes arguments (--all-features) BEFORE the
  # command (check), i.e. `cargo-deny --all-features check`.
  if command -v cargo-deny >/dev/null 2>&1; then
    run_step "cargo-deny --all-features check" cargo-deny --all-features check
  else
    printf 'cargo-deny not installed; skipping (install: cargo install cargo-deny --locked)\n'
  fi
}

declare -A PIDS
declare -A START_TS

run_job() {
  local name="$1"
  local fn="$2"
  local log="$LOG_DIR/$name.log"
  local result="$LOG_DIR/$name.result"

  (
    local start end rc duration
    start="$(date +%s)"
    printf '%s●%s %-18s started\n' "$CYAN" "$RESET" "$name"

    if "$fn" >"$log" 2>&1; then
      rc=0
    else
      rc=$?
    fi

    end="$(date +%s)"
    duration="$((end - start))"
    printf '%s %s\n' "$rc" "$duration" >"$result"

    if (( rc == 0 )); then
      printf '%s✓%s %-18s passed  %s%s%s\n' \
        "$GREEN" "$RESET" "$name" "$DIM" "$(format_duration "$duration")" "$RESET"
    else
      printf '%s✗%s %-18s failed  %s%s%s\n' \
        "$RED" "$RESET" "$name" "$DIM" "$(format_duration "$duration")" "$RESET"
    fi
  ) &

  PIDS["$name"]="$!"
}

skip_job() {
  local name="$1"
  local reason="$2"
  printf '%s○%s %-18s skipped %s(%s)%s\n' "$YELLOW" "$RESET" "$name" "$DIM" "$reason" "$RESET"
  printf 'skip 0\n' >"$LOG_DIR/$name.result"
  printf '%s\n' "$reason" >"$LOG_DIR/$name.log"
}

printf '%sLocal CI%s\n' "$CYAN" "$RESET"
printf 'Repo: %s\n\n' "$ROOT_DIR"

JOBS=(format dashboard rust)
if [[ "$SKIP_DEPS" == false ]]; then
  JOBS+=(dependency-policy)
  printf 'Running format, dashboard, and dependency-policy in parallel; rust starts once format + dashboard pass...\n\n'
else
  printf 'Running format and dashboard in parallel; rust starts once both pass... (dependency-policy skipped)\n\n'
fi

# Wave 1: jobs with no dependencies in ci.yml.
run_job format format_job
run_job dashboard dashboard_job
if [[ "$SKIP_DEPS" == false ]]; then
  run_job dependency-policy dependency_policy_job
fi

# rust needs [format, dashboard] in ci.yml.
wait "${PIDS[format]}"
wait "${PIDS[dashboard]}"

read -r format_rc _ <"$LOG_DIR/format.result"
read -r dashboard_rc _ <"$LOG_DIR/dashboard.result"

if [[ "$format_rc" == "0" && "$dashboard_rc" == "0" ]]; then
  run_job rust rust_job
else
  skip_job rust "format and/or dashboard failed"
fi

# Wait for whatever's still running (rust, dependency-policy).
wait "${PIDS[rust]:-}" 2>/dev/null || true
if [[ "$SKIP_DEPS" == false ]]; then
  wait "${PIDS[dependency-policy]}"
fi

printf '\n%sSummary%s\n' "$CYAN" "$RESET"
printf '%-18s %-8s %s\n' "Job" "Status" "Duration"
printf '%-18s %-8s %s\n' "------------------" "-------" "--------"

overall=0
for name in "${JOBS[@]}"; do
  read -r rc duration <"$LOG_DIR/$name.result"
  if [[ "$rc" == "skip" ]]; then
    printf '%-18s %s○ skipped%s\n' "$name" "$YELLOW" "$RESET"
  elif (( rc == 0 )); then
    printf '%-18s %s✓ passed%s %s\n' "$name" "$GREEN" "$RESET" "$(format_duration "$duration")"
  else
    printf '%-18s %s✗ failed%s %s\n' "$name" "$RED" "$RESET" "$(format_duration "$duration")"
    overall=1
  fi
done

if (( overall != 0 )); then
  printf '\n%sFailed job logs%s\n' "$RED" "$RESET"
  for name in "${JOBS[@]}"; do
    read -r rc _ <"$LOG_DIR/$name.result"
    if [[ "$rc" != "skip" ]] && (( rc != 0 )); then
      printf '\n===== %s =====\n' "$name"
      cat "$LOG_DIR/$name.log"
    fi
  done
  printf '\n%sCI failed%s\n' "$RED" "$RESET"
  exit 1
fi

printf '\n%s✓ CI passed%s\n' "$GREEN" "$RESET"
