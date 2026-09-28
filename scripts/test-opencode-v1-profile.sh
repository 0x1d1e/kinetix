#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

OPENCODE_VERSION="1.18.33"
CLI_DIR="$ROOT_DIR/target/opencode-v1-cli"
OPENCODE_BIN="$CLI_DIR/node_modules/.bin/opencode"

if [[ ! -x "$OPENCODE_BIN" ]] || [[ "$("$OPENCODE_BIN" --version 2>/dev/null || true)" != "$OPENCODE_VERSION" ]]; then
  mkdir -p "$CLI_DIR"
  npm install \
    --prefix "$CLI_DIR" \
    --ignore-scripts \
    --no-audit \
    --no-fund \
    --package-lock=false \
    "opencode-ai@$OPENCODE_VERSION"
  node "$CLI_DIR/node_modules/opencode-ai/postinstall.mjs"
fi

KINETIX_OPENCODE_V1_BIN="$OPENCODE_BIN" \
  cargo test --lib \
    client_profiles::tests::open_code_v1_profile_sends_selected_model_to_kinetix_stub \
    -- --ignored --exact
