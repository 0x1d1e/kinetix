#!/usr/bin/env bash
# Kinetix uninstaller (Linux) — the inverse of install.sh.
#
#   curl -fsSL https://raw.githubusercontent.com/0x1d1e/kinetix/main/uninstall.sh | bash
#
# Removes the XDG config/data/state directories (the database, exports, backups,
# master key, and admin password) and, optionally, the `kinetix` binary itself.
# Because it is destructive it prints exactly what it will remove and asks for
# confirmation unless you pass -y/--yes.
#
# You can also uninstall from the binary directly:
#   kinetix uninstall [--yes] [--remove-binary] [--keep-data] [--dry-run]
#
# Environment overrides (same as install.sh):
#   KINETIX_PREFIX    install prefix to clean (default: ~/.local)
#   KINETIX_HOME      a single root holding config/ data/ state/ (overrides XDG)
#   KINETIX_CONFIG_DIR / KINETIX_DATA_DIR / KINETIX_STATE_DIR
set -euo pipefail

PREFIX="${KINETIX_PREFIX:-$HOME/.local}"
BIN="$PREFIX/bin/kinetix"

CONFIRM=1
REMOVE_BINARY=1
KEEP_DATA=0
DRY_RUN=0

for arg in "$@"; do
  case "$arg" in
    -y|--yes)          CONFIRM=0 ;;
    --remove-binary)   REMOVE_BINARY=1 ;;
    --keep-binary)     REMOVE_BINARY=0 ;;
    --keep-data)       KEEP_DATA=1 ;;
    --dry-run)         DRY_RUN=1 ;;
    -h|--help)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

# Resolve the state directories the same way the binary does.
if [ -n "${KINETIX_HOME:-}" ]; then
  CONFIG_DIR="$KINETIX_HOME/config"
  DATA_DIR="$KINETIX_HOME/data"
  STATE_DIR="$KINETIX_HOME/state"
else
  CONFIG_DIR="${KINETIX_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/kinetix}"
  DATA_DIR="${KINETIX_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/kinetix}"
  STATE_DIR="${KINETIX_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/kinetix}"
fi

targets=("$CONFIG_DIR" "$STATE_DIR")
[ "$KEEP_DATA" -eq 0 ] && targets+=("$DATA_DIR")

log "Kinetix uninstall"
for t in "${targets[@]}"; do
  [ -e "$t" ] && printf '  [remove] %s\n' "$t" || printf '  [absent] %s\n' "$t"
done
if [ "$REMOVE_BINARY" -eq 1 ]; then
  [ -e "$BIN" ] && printf '  [remove] %s\n' "$BIN" || printf '  [absent] %s\n' "$BIN"
fi

if [ "$DRY_RUN" -eq 1 ]; then
  log "Dry run — nothing was deleted."
  exit 0
fi

if [ "$CONFIRM" -eq 1 ]; then
  printf '\nDelete the items above? This cannot be undone. [y/N] '
  read -r answer </dev/tty || answer=""
  case "$answer" in
    y|Y|yes|YES) : ;;
    *) log "Aborted."; exit 0 ;;
  esac
fi

for t in "${targets[@]}"; do
  if [ -e "$t" ]; then
    rm -rf -- "$t" && log "removed $t"
  fi
done

if [ "$REMOVE_BINARY" -eq 1 ] && [ -e "$BIN" ]; then
  rm -f -- "$BIN" && log "removed $BIN"
fi

log "Kinetix removed. Restart your shell if PATH still references the binary."
