#!/usr/bin/env bash
# scripts/release-local.sh
# Build releases locally for x86_64-unknown-linux-gnu and aarch64-unknown-linux-gnu,
# package tarballs, compute SHA256SUMS, and optionally publish directly to GitHub Releases.
#
# Usage:
#   scripts/release-local.sh <tag> [--publish] [--draft] [--pi-acceptance <file>]
# Example:
#   scripts/release-local.sh v0.1.0 --publish --pi-acceptance acceptance-artifacts/<stamp>/pi-acceptance.json
#
# Publishing requires a passing real-Pi acceptance artifact for this release
# candidate (see docs/pi-compatibility.md). It is verified and attached to the
# release. --skip-pi-acceptance publishes without it and says so loudly.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$ROOT_DIR/target/dist"

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
err() { printf '\033[1;31m==>\033[0m %s\n' "$*" >&2; exit 1; }

if [ $# -lt 1 ]; then
  echo "Usage: $0 <tag> [--publish] [--draft]"
  echo "  <tag>        Release tag, e.g. v0.1.0"
  echo "  --publish    Upload artifacts directly to GitHub Release using gh CLI"
  echo "  --draft      Create release as draft when publishing"
  echo "  --pi-acceptance <file>  Passing pi-acceptance.json for this release candidate"
  echo "  --skip-pi-acceptance    Publish without the real-Pi acceptance artifact"
  exit 1
fi

TAG="$1"
shift

PUBLISH=0
DRAFT_FLAG=""
PI_ACCEPTANCE=""
SKIP_PI_ACCEPTANCE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --publish) PUBLISH=1; shift ;;
    --draft) DRAFT_FLAG="--draft"; shift ;;
    --pi-acceptance)
      [ $# -ge 2 ] || err "--pi-acceptance needs a file"
      PI_ACCEPTANCE="$2"; shift 2 ;;
    --skip-pi-acceptance) SKIP_PI_ACCEPTANCE=1; shift ;;
    *) err "Unknown option: $1" ;;
  esac
done

if ! printf '%s' "$TAG" | grep -qE '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
  err "Tag must match vX.Y.Z exactly (got '$TAG')"
fi

# Release-candidate Pi acceptance gate, checked before the build. The artifact
# comes from a scripts/release-client-acceptance.sh pi run against this build.
if [ -n "$PI_ACCEPTANCE" ]; then
  [ -f "$PI_ACCEPTANCE" ] || err "pi acceptance artifact not found: $PI_ACCEPTANCE"
  python3 - "$PI_ACCEPTANCE" "${TAG#v}" <<'PY' || err "pi acceptance artifact does not pass for $TAG"
import json
import sys

report = json.load(open(sys.argv[1]))
version = sys.argv[2]
if report.get("schema") != "kinetix.pi-acceptance.v1":
    raise SystemExit(f"unexpected schema: {report.get('schema')}")
if report.get("result") != "pass":
    raise SystemExit("pi acceptance result is not pass")
if version not in str(report.get("kinetix_version", "")).split():
    raise SystemExit(f"artifact Kinetix version {report.get('kinetix_version')!r} is not {version}")
print(f"pi acceptance ok: Pi {report.get('pi_version')}, {len(report.get('cases', []))} case(s)")
PY
  PI_ACCEPTANCE="$(cd "$(dirname "$PI_ACCEPTANCE")" && pwd)/$(basename "$PI_ACCEPTANCE")"
elif [ "$PUBLISH" -eq 1 ]; then
  [ "$SKIP_PI_ACCEPTANCE" -eq 1 ] || err "publishing needs --pi-acceptance <file> (or --skip-pi-acceptance)"
  printf '\033[1;33m==>\033[0m %s\n' "publishing $TAG WITHOUT real-Pi acceptance evidence" >&2
fi

cd "$ROOT_DIR"

command -v git >/dev/null 2>&1 || err "git is required"
command -v cargo >/dev/null 2>&1 || err "cargo is required"
command -v rustup >/dev/null 2>&1 || err "rustup is required"
command -v npm >/dev/null 2>&1 || err "npm is required"
command -v tar >/dev/null 2>&1 || err "tar is required"
command -v sha256sum >/dev/null 2>&1 || err "sha256sum is required"

git rev-parse --is-inside-work-tree >/dev/null 2>&1 || err "must be run from a git checkout"

if [ "$PUBLISH" -eq 1 ]; then
  command -v gh >/dev/null 2>&1 || err "gh CLI is required to publish"
  gh auth status >/dev/null 2>&1 || err "gh CLI is not authenticated"

  log "Refreshing remote tags before resolving $TAG..."
  git fetch --tags origin
fi

TAG_REF="refs/tags/$TAG"
TAG_EXISTS=0
if git rev-parse --verify --quiet "$TAG_REF^{commit}" >/dev/null; then
  TAG_EXISTS=1
  SOURCE_SHA="$(git rev-parse "$TAG_REF^{commit}")"
  log "Building existing tag $TAG at $SOURCE_SHA"
else
  if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
    err "working tree is not clean; commit or stash changes before building a new release tag"
  fi
  SOURCE_SHA="$(git rev-parse HEAD)"
  log "Tag $TAG does not exist yet; building clean HEAD at $SOURCE_SHA"
fi

PACKAGE_VERSION="$(
  git show "$SOURCE_SHA:Cargo.toml" |
    awk -F'"' '/^version[[:space:]]*=/{print $2; exit}'
)"
[ -n "$PACKAGE_VERSION" ] || err "could not read package version from Cargo.toml at $SOURCE_SHA"
if [ "$TAG" != "v$PACKAGE_VERSION" ]; then
  err "release tag $TAG does not match Cargo package version $PACKAGE_VERSION"
fi

WORK_PARENT="$(mktemp -d)"
BUILD_ROOT="$WORK_PARENT/source"
WORKTREE_ADDED=0

cleanup() {
  if [ "$WORKTREE_ADDED" -eq 1 ]; then
    git -C "$ROOT_DIR" worktree remove --force "$BUILD_ROOT" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK_PARENT"
}
trap cleanup EXIT

git worktree add --detach "$BUILD_ROOT" "$SOURCE_SHA" >/dev/null
WORKTREE_ADDED=1

TARGETS=("x86_64-unknown-linux-gnu" "aarch64-unknown-linux-gnu")

# 1. Build dashboard frontend from the exact source commit that will be released.
log "Installing and building embedded dashboard frontend..."
(
  cd "$BUILD_ROOT/dashboard"
  npm ci
  npm run build
)

rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

# 2. Build each target from the same detached source worktree.
for TARGET in "${TARGETS[@]}"; do
  log "Building binary for target: $TARGET"

  if [ "$TARGET" = "x86_64-unknown-linux-gnu" ]; then
    rustup target add "$TARGET" >/dev/null 2>&1 || true
    (
      cd "$BUILD_ROOT"
      cargo build --release --locked --target "$TARGET"
    )
  else
    if command -v cross >/dev/null 2>&1; then
      log "Cross-compiling $TARGET via cross..."
      (
        cd "$BUILD_ROOT"
        cross build --release --locked --target "$TARGET"
      )
    else
      err "cross command not found. Install cross to build aarch64."
    fi
  fi

  BIN_SRC="$BUILD_ROOT/target/$TARGET/release/kinetix"
  [ -f "$BIN_SRC" ] || err "Built binary not found at $BIN_SRC"

  ARCHIVE_NAME="kinetix-$TAG-$TARGET.tar.gz"
  ARCHIVE_PATH="$DIST_DIR/$ARCHIVE_NAME"

  log "Creating archive: $ARCHIVE_NAME"
  TMP_STAGE="$WORK_PARENT/stage-$TARGET"
  mkdir -p "$TMP_STAGE"
  cp "$BIN_SRC" "$TMP_STAGE/kinetix"
  chmod 0755 "$TMP_STAGE/kinetix"
  tar -czf "$ARCHIVE_PATH" -C "$TMP_STAGE" kinetix

  (
    cd "$DIST_DIR"
    sha256sum "$ARCHIVE_NAME" > "$ARCHIVE_NAME.sha256"
  )
done

# 3. Create consolidated SHA256SUMS file.
log "Generating canonical SHA256SUMS..."
(
  cd "$DIST_DIR"
  shopt -s nullglob
  ASSETS=(kinetix-"$TAG"-*.tar.gz)
  [ "${#ASSETS[@]}" -gt 0 ] || err "no release assets were produced"
  sha256sum "${ASSETS[@]}" > SHA256SUMS
)

log "Release artifacts prepared in $DIST_DIR from source $SOURCE_SHA:"
ls -lh "$DIST_DIR"

# 4. Attach the release-candidate Pi acceptance evidence verified above.
rm -f "$DIST_DIR/pi-acceptance.json"
if [ -n "$PI_ACCEPTANCE" ]; then
  cp "$PI_ACCEPTANCE" "$DIST_DIR/pi-acceptance.json"
fi

# 5. Optional publish via gh. Create a new tag only after every artifact succeeds.
if [ "$PUBLISH" -eq 1 ]; then
  log "Publishing release $TAG to GitHub..."

  if [ "$TAG_EXISTS" -eq 0 ]; then
    log "Creating git tag $TAG at $SOURCE_SHA..."
    git tag -a "$TAG" "$SOURCE_SHA" -m "Release $TAG"
  fi

  TAG_SHA="$(git rev-parse "$TAG_REF^{commit}")"
  if [ "$TAG_SHA" != "$SOURCE_SHA" ]; then
    err "tag $TAG resolves to $TAG_SHA, but artifacts were built from $SOURCE_SHA"
  fi

  log "Pushing tag $TAG to origin..."
  git push origin "refs/tags/$TAG"

  log "Ensuring GitHub Release exists..."
  if ! gh release view "$TAG" >/dev/null 2>&1; then
    gh release create "$TAG" --title "Kinetix $TAG" --generate-notes $DRAFT_FLAG
  fi

  log "Uploading artifacts to release $TAG..."
  shopt -s nullglob
  RELEASE_ASSETS=(
    "$DIST_DIR"/kinetix-"$TAG"-*.tar.gz
    "$DIST_DIR"/kinetix-"$TAG"-*.sha256
    "$DIST_DIR"/SHA256SUMS
  )
  if [ -f "$DIST_DIR/pi-acceptance.json" ]; then
    RELEASE_ASSETS+=("$DIST_DIR/pi-acceptance.json")
  fi
  gh release upload "$TAG" "${RELEASE_ASSETS[@]}" --clobber

  log "Release $TAG published successfully!"
  gh release view "$TAG"
fi
