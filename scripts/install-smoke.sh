#!/usr/bin/env bash
# Exercise install.sh for both prebuilt release downloads and source-build fallbacks.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

SOURCE="$WORK/source"
FAKE_BIN="$WORK/bin"
HOME_DIR="$WORK/home"
PREFIX="$WORK/prefix"
PREBUILT_DIR="$WORK/mock-release"
LOG="$WORK/order.log"
INIT_LOG="$WORK/init.log"

mkdir -p "$SOURCE/dashboard" "$FAKE_BIN" "$HOME_DIR" "$PREBUILT_DIR" "$WORK/stage"
printf '{}\n' > "$SOURCE/dashboard/package-lock.json"

case "$(uname -m)" in
  x86_64)  RUST_TARGET="x86_64-unknown-linux-gnu" ;;
  aarch64) RUST_TARGET="aarch64-unknown-linux-gnu" ;;
esac
ASSET_NAME="kinetix-v9.9.9-${RUST_TARGET}.tar.gz"

cat > "$WORK/stage/kinetix" <<'EOF_BIN'
#!/usr/bin/env bash
if [ "${1:-}" = "init" ]; then
  printf 'init\n' >> "$INSTALL_TEST_INIT_LOG"
  exit 0
fi
printf 'prebuilt-binary-v9.9.9\n'
EOF_BIN
chmod +x "$WORK/stage/kinetix"

tar -czf "$PREBUILT_DIR/$ASSET_NAME" -C "$WORK/stage" kinetix
(
  cd "$PREBUILT_DIR"
  sha256sum "$ASSET_NAME" > SHA256SUMS
)

cat > "$FAKE_BIN/curl" <<SH
#!/usr/bin/env bash
set -euo pipefail
out=""
wformat=""
url=""
while [ "\$#" -gt 0 ]; do
  case "\$1" in
    -o) out="\$2"; shift 2 ;;
    -w) wformat="\$2"; shift 2 ;;
    -fsSL|-sSL|-fSL) shift ;;
    mock://*) url="\$1"; shift ;;
    *) shift ;;
  esac
done

if [ "\$url" = "mock://repo/releases/latest" ]; then
  if [ -n "\$wformat" ]; then
    printf 'mock://repo/releases/tag/v9.9.9'
  fi
  exit 0
elif [ "\$url" = "mock://repo/releases/download/v9.9.9/$ASSET_NAME" ]; then
  cp "$PREBUILT_DIR/$ASSET_NAME" "\$out"
  exit 0
elif [ "\$url" = "mock://repo/releases/download/v9.9.9/SHA256SUMS" ]; then
  cp "$PREBUILT_DIR/SHA256SUMS" "\$out"
  exit 0
fi

if command -v /usr/bin/curl >/dev/null 2>&1; then
  exec /usr/bin/curl "\$@"
fi
exit 1
SH

cat > "$FAKE_BIN/node" <<'SH'
#!/usr/bin/env bash
exit 0
SH

cat > "$FAKE_BIN/npm" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [ "$#" -eq 1 ] && [ "$1" = "ci" ]; then
  printf 'npm ci\n' >> "$INSTALL_TEST_LOG"
  exit 0
fi
if [ "$#" -eq 2 ] && [ "$1" = "run" ] && [ "$2" = "build" ]; then
  printf 'npm run build\n' >> "$INSTALL_TEST_LOG"
  mkdir -p dist
  printf '<!doctype html>\n' > dist/index.html
  exit 0
fi
exit 2
SH

cat > "$FAKE_BIN/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo %s\n' "$*" >> "$INSTALL_TEST_LOG"
[ -f dashboard/dist/index.html ] || {
  echo "cargo ran before dashboard/dist/index.html existed" >&2
  exit 42
}
mkdir -p target/release
cat > target/release/kinetix <<'EOF_BIN'
#!/usr/bin/env bash
if [ "${1:-}" = "init" ]; then
  printf 'init\n' >> "$INSTALL_TEST_INIT_LOG"
fi
exit 0
EOF_BIN
chmod +x target/release/kinetix
SH

chmod +x "$FAKE_BIN/curl" "$FAKE_BIN/node" "$FAKE_BIN/npm" "$FAKE_BIN/cargo"

# --- Test 1: Default install prefers prebuilt binary ------------------------
install_output="$(INSTALL_TEST_LOG="$LOG" \
INSTALL_TEST_INIT_LOG="$INIT_LOG" \
HOME="$HOME_DIR" \
PATH="$FAKE_BIN:$PATH" \
KINETIX_REPO="mock://repo" \
KINETIX_PREFIX="$PREFIX" \
  "$ROOT/install.sh")"

[ -x "$PREFIX/bin/kinetix" ] || {
  echo "prebuilt installer did not install an executable binary" >&2
  exit 1
}

output="$("$PREFIX/bin/kinetix")"
[ "$output" = "prebuilt-binary-v9.9.9" ] || {
  echo "expected prebuilt binary, got: $output" >&2
  exit 1
}

[ ! -f "$LOG" ] || {
  echo "prebuilt install should not run cargo/npm build steps:" >&2
  cat "$LOG" >&2
  exit 1
}
[ ! -f "$INIT_LOG" ] || {
  echo "installer must not run kinetix init automatically:" >&2
  cat "$INIT_LOG" >&2
  exit 1
}
printf '%s\n' "$install_output" | grep -F "kinetix init" >/dev/null || {
  echo "installer should tell the user to run kinetix init" >&2
  exit 1
}

echo "installer prebuilt download smoke passed"

# --- Test 2: Explicit source build fallback when VERSION=main --------------
rm -f "$LOG" "$INIT_LOG"
rm -rf "$PREFIX"
mkdir -p "$PREFIX"

(
  cd "$SOURCE"
  git init -q -b main
  git config user.name "Kinetix installer test"
  git config user.email "installer-test@example.invalid"
  git add .
  git commit -qm "fixture"
)

INSTALL_TEST_LOG="$LOG" \
INSTALL_TEST_INIT_LOG="$INIT_LOG" \
HOME="$HOME_DIR" \
PATH="$FAKE_BIN:$PATH" \
KINETIX_REPO="$SOURCE" \
KINETIX_VERSION=main \
KINETIX_PREFIX="$PREFIX" \
  "$ROOT/install.sh" >/dev/null

[ -x "$PREFIX/bin/kinetix" ] || {
  echo "installer did not install an executable binary" >&2
  exit 1
}

expected="$(cat <<'EOF_EXPECTED'
npm ci
npm run build
cargo build --release --locked
EOF_EXPECTED
)"
actual="$(head -n 3 "$LOG")"
[ "$actual" = "$expected" ] || {
  echo "unexpected source-build order:" >&2
  cat "$LOG" >&2
  exit 1
}
[ ! -f "$INIT_LOG" ] || {
  echo "source-build installer must not run kinetix init automatically:" >&2
  cat "$INIT_LOG" >&2
  exit 1
}

echo "installer source-build smoke passed"
