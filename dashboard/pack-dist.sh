#!/usr/bin/env bash
# pack-dist.sh — pack the rebranded dashboard dist into the release tarball
# and its checksum (docs/41 §4).
#
# The measured recipe: the dist is 1.6 GiB raw, but `tar.gz` WITHOUT
# `node_modules` is 277 MB — the artifact is shippable as a release asset at
# that size, and `npm ci --omit=dev` runs once on the host where npm can
# verify the tree. The `--with-deps` flag includes `node_modules` (≈330 MB)
# instead, for a host where the 2-vCPU `npm ci` is judged too slow.
#
# The tarball carries `patch-base-path.mjs` (vendored by rebrand-dist.sh) so
# `install.sh --dashboard-src <tarball>` can convert a root-built dist to the
# `/dash` subpath at install time — the one-way patch the operator cannot
# run later without the file.
#
# Usage:
#   dashboard/pack-dist.sh [--with-deps] [--out <dir>]
# Env: AR_BASE_PATH (default /dash) — recorded in the tarball name only; the
#      bake happened at build time and the sentinel file records it inside.
set -euo pipefail

AR_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DST="$AR_ROOT/dashboard/dist"
BASE_PATH="${AR_BASE_PATH:-/dash}"
OUT_DIR="${AR_OUT:-$AR_ROOT/target/dist}"
WITH_DEPS=0

while [ $# -gt 0 ]; do
  case "$1" in
    --with-deps) WITH_DEPS=1 ;;
    --out) OUT_DIR="$2"; shift ;;
    *) echo "pack-dist: unknown flag $1" >&2; exit 2 ;;
  esac
  shift
done

[ -f "$DST/server.js" ] || { echo "pack-dist: no dist at $DST — run dashboard/rebrand-dist.sh first" >&2; exit 1; }
[ -f "$DST/patch-base-path.mjs" ] || { echo "pack-dist: the dist has no patch-base-path.mjs — rebrand-dist.sh did not complete" >&2; exit 1; }

# The version the tarball name carries: the repo's, so an asset maps to the
# release that shipped it.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$AR_ROOT/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || { echo "pack-dist: cannot read the version from Cargo.toml" >&2; exit 1; }

SUFFIX="v$VERSION"
[ "$WITH_DEPS" = 1 ] && SUFFIX="$SUFFIX-deps"
TARBALL="$OUT_DIR/aroute-dashboard-$SUFFIX.tar.gz"

mkdir -p "$OUT_DIR"
rm -f "$TARBALL" "$TARBALL.sha256"

echo "[pack-dist] packing $DST -> $TARBALL"
if [ "$WITH_DEPS" = 1 ]; then
  TAR_ARGS=(--exclude=old --exclude=.old)
else
  TAR_ARGS=(--exclude=node_modules --exclude=old --exclude=.old)
fi
# -C "$DST" so the tarball's members are dist-rooted: extracting yields
# server.js, package.json, .build/, public/, migrations/ directly — the shape
# install.sh's `install_dashboard` expects (it extracts into
# ${DASH_DEST}.new and then checks for server.js inside it).
(cd "$DST" && tar -czf "$TARBALL" "${TAR_ARGS[@]}" \
  server.js package.json $( [ -f package-lock.json ] && echo package-lock.json ) \
  $( [ -f .npmrc ] && echo .npmrc ) \
  .build public migrations \
  peer-stamp-launcher.cjs patch-base-path.mjs \
  $( [ -f BUILD_OMNIROUTE_BASE_PATH ] && echo BUILD_OMNIROUTE_BASE_PATH ) \
  $( for f in *.mjs *.cjs; do [ -f "$f" ] && echo "$f"; done ) \
  $( [ -f dev/run-standalone.mjs ] && echo dev/run-standalone.mjs ) \
  $( [ -d build ] && echo build ) \
)

# The checksum is published beside the tarball; install.sh verifies it when
# the operator downloads both. `.sha256` (not SHA256SUMS) so a release can
# carry one asset per file without a merge step.
sha256sum "$TARBALL" > "$TARBALL.sha256"

SIZE=$(du -h "$TARBALL" | cut -f1)
echo "[pack-dist] OK — $TARBALL ($SIZE)"
echo "[pack-dist] checksum: $TARBALL.sha256"
echo "[pack-dist] install with: install.sh --with-dashboard --dashboard-src $TARBALL"
