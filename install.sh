#!/usr/bin/env bash
# Kinetix installer (Linux x86_64 / aarch64).
#
#   curl -fsSL https://raw.githubusercontent.com/0x1d1e/kinetix/main/install.sh | bash
#
# It installs the `kinetix` binary to ~/.local/bin. Run `kinetix init` when you
# are ready to create local state and generate the dashboard admin password.
# Everything is then configurable through the CLI — no .env file required.
#
# Environment overrides:
#   KINETIX_VERSION   release tag or branch to install (default: latest release, fallback: main)
#   KINETIX_PREFIX    install prefix (default: ~/.local)
#   KINETIX_REPO      git / release repository URL (default: https://github.com/0x1d1e/kinetix)
set -euo pipefail

REPO="${KINETIX_REPO:-https://github.com/0x1d1e/kinetix}"
REPO="${REPO%/}"
PREFIX="${KINETIX_PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
err() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) : ;;
  *) err "this installer currently supports Linux only (detected $(uname -s))" ;;
esac
case "$(uname -m)" in
  x86_64|aarch64) : ;;
  *) err "unsupported architecture $(uname -m) (need x86_64 or aarch64)" ;;
esac

command -v git   >/dev/null 2>&1 || :  # git only needed for the source-build fallback

case "$(uname -m)" in
  x86_64)  RUST_TARGET="x86_64-unknown-linux-gnu" ;;
  aarch64) RUST_TARGET="aarch64-unknown-linux-gnu" ;;
esac

# Resolve target version: prefer the latest release tag if unspecified.
if [ -z "${KINETIX_VERSION:-}" ] || [ "${KINETIX_VERSION:-}" = "latest" ]; then
  LATEST_URL="$(curl -fsSL -o /dev/null -w "%{url_effective}" "${REPO}/releases/latest" 2>/dev/null || :)"
  LATEST_TAG="${LATEST_URL##*/}"
  if printf '%s' "$LATEST_TAG" | grep -qE '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
    VERSION="$LATEST_TAG"
  else
    VERSION="main"
  fi
else
  VERSION="$KINETIX_VERSION"
fi

# --- Prefer a prebuilt release binary (no toolchain needed) ----------------
# When VERSION looks like a release tag (vX.Y.Z) we fetch the matching
# asset from GitHub Releases; otherwise we fall back to building from source.
installed=0
if printf '%s' "$VERSION" | grep -qE '^v[0-9]+\.[0-9]+\.[0-9]+'; then
  ASSET="kinetix-${VERSION}-${RUST_TARGET}.tar.gz"
  URL="${REPO}/releases/download/${VERSION}/${ASSET}"
  SUMS_URL="${REPO}/releases/download/${VERSION}/SHA256SUMS"
  log "Downloading prebuilt binary ($ASSET)"

  DL_DIR="$(mktemp -d)"
  dl_cleanup() { rm -rf "$DL_DIR"; }

  if curl -fsSL "$URL" -o "$DL_DIR/$ASSET" 2>/dev/null; then
    # Verify SHA256 checksum if SHA256SUMS asset exists on release
    if curl -fsSL "$SUMS_URL" -o "$DL_DIR/SHA256SUMS" 2>/dev/null; then
      EXPECTED="$(grep -E "[[:space:]]${ASSET}$" "$DL_DIR/SHA256SUMS" 2>/dev/null | awk '{print $1}' || :)"
      if [ -n "$EXPECTED" ]; then
        if command -v sha256sum >/dev/null 2>&1; then
          ACTUAL="$(sha256sum "$DL_DIR/$ASSET" | awk '{print $1}')"
        elif command -v shasum >/dev/null 2>&1; then
          ACTUAL="$(shasum -a 256 "$DL_DIR/$ASSET" | awk '{print $1}')"
        else
          ACTUAL=""
        fi
        if [ -n "$ACTUAL" ]; then
          if [ "$ACTUAL" != "$EXPECTED" ]; then
            dl_cleanup
            err "Checksum mismatch for $ASSET! Expected $EXPECTED, got $ACTUAL"
          fi
          log "Verified SHA256 checksum: $ACTUAL"
        fi
      fi
    fi

    mkdir -p "$BIN_DIR"
    if ! tar -xzf "$DL_DIR/$ASSET" -C "$BIN_DIR" kinetix 2>/dev/null; then
      tar -xzf "$DL_DIR/$ASSET" -C "$DL_DIR"
      install -m 0755 "$DL_DIR/kinetix" "$BIN_DIR/kinetix"
    fi
    chmod 0755 "$BIN_DIR/kinetix"
    dl_cleanup
    log "Installed prebuilt $BIN_DIR/kinetix"
    installed=1
  else
    dl_cleanup
    log "No prebuilt asset found for $VERSION; building from source instead"
  fi
fi

if [ "$installed" -eq 0 ]; then
  command -v cargo >/dev/null 2>&1 || err "cargo (Rust) is required to build Kinetix; install from https://rustup.rs (or install a released tag: KINETIX_VERSION=vX.Y.Z)"
  command -v git   >/dev/null 2>&1 || err "git is required to build Kinetix from source"
  command -v node  >/dev/null 2>&1 || err "Node.js is required to build the embedded dashboard from source"
  command -v npm   >/dev/null 2>&1 || err "npm is required to build the embedded dashboard from source"

  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT

  log "Fetching Kinetix ($VERSION)"
  git clone --depth 1 --branch "$VERSION" "$REPO" "$WORK/kinetix" 2>/dev/null \
    || git clone --depth 1 "$REPO" "$WORK/kinetix"

  cd "$WORK/kinetix"

  log "Building embedded dashboard"
  (
    cd dashboard
    npm ci
    npm run build
  )
  [ -f dashboard/dist/index.html ] || err "dashboard build did not produce dashboard/dist/index.html"

  log "Building release binary (this may take a few minutes)"
  cargo build --release --locked 2>/dev/null || cargo build --release

  mkdir -p "$BIN_DIR"
  install -m 0755 target/release/kinetix "$BIN_DIR/kinetix"
  log "Installed $BIN_DIR/kinetix"
fi

# Ensure ~/.local/bin is on PATH for future shells.
if ! printf '%s' ":$PATH:" | grep -q ":$BIN_DIR:"; then
  case "${SHELL:-}" in
    */zsh)  RC="$HOME/.zshrc" ;;
    */bash) RC="$HOME/.bashrc" ;;
    *)      RC="$HOME/.profile" ;;
  esac
  if [ -f "$RC" ] && ! grep -q "$BIN_DIR" "$RC" 2>/dev/null; then
    printf '\nexport PATH="%s:$PATH"\n' "$BIN_DIR" >> "$RC"
    log "Added $BIN_DIR to PATH in $RC (restart your shell or run: export PATH=\"$BIN_DIR:\$PATH\")"
  fi
fi

cat <<EOF

Kinetix is installed. To create local state and generate the dashboard admin password, run:
  kinetix init

Then configure an upstream provider, register a model, create a virtual key, and start the proxy:
  kinetix provider add --help
  kinetix model add --help
  kinetix key create --help
  kinetix serve

Run \`kinetix --help\` for the full command surface.
EOF
