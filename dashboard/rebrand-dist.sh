#!/usr/bin/env bash
# rebrand-dist.sh — regenerate the rebranded Artificial Route dashboard dist
# from an OmniRoute standalone build, without shipping OmniRoute source.
#
# Pipeline: copy standalone essentials → targeted manifest patches → general
# display-text rebrand → node --check parse gate (catches any identifier the
# regex grazes) → string-anchored fallback for any file the gate rejects →
# boot smoke.
#
# Prerequisites (run once, read-only w.r.t. OmniRoute source):
#   cd ../OmniRoute && npm install && npm run build
# That emits .build/next/standalone — the compiled server + UI, no source
# needed at runtime. This script then copies and rebrands it.
#
# What is rebranded: every user-visible "OmniRoute" display string — page
# titles, i18n sentences, the PWA manifest, service-worker notifications,
# SVG brand text, the OpenAPI description.
# What is deliberately NOT rebranded (internal identifiers; renaming them
# in compiled JS risks runtime breakage for zero visible gain):
#   - lowercase omniroute (localStorage keys, env hints, API-key prefix
#     sk_omniroute, UA string OmniRoute/1.x, x-omniroute-* headers)
#   - @omniroute/* module namespace (must match node_modules layout)
#   - OMNIROUTE_* env var names (must match server.js + assembled scripts)
#   - ~/.omniroute default data dir (override with DATA_DIR)
#   - GitHub repo URLs (diegosouzapw/OmniRoute) and X-OmniRoute-* header names

set -euo pipefail

AR_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OMNI_ROOT="${OMNI_ROOT:-$AR_ROOT/../OmniRoute}"
SRC="$OMNI_ROOT/.build/next/standalone"
DST="$AR_ROOT/dashboard/dist"
PORT="${AR_DASH_PORT:-20149}"
DATA_DIR="${AR_DASH_DATA:-/tmp/ar-dashboard-data}"

log() { printf '[rebrand] %s\n' "$*"; }
die() { printf '[rebrand] FATAL: %s\n' "$*" >&2; exit 1; }

command -v node >/dev/null || die "node not found on PATH"
[ -f "$SRC/server.js" ] || die "no standalone build at $SRC — run: (cd $OMNI_ROOT && npm install && npm run build)"

# ── 1. Copy essentials (server + compiled UI + runtime deps; no source) ──────
log "copying standalone essentials → $DST"
rm -rf "$DST"
mkdir -p "$DST"
cp "$SRC/server.js" "$SRC/package.json" "$DST/"
cp -r "$SRC/public" "$DST/public"
cp -r "$SRC/node_modules" "$DST/node_modules"
cp -r "$SRC/.build" "$DST/.build"
[ -d "$OMNI_ROOT/src/lib/db/migrations" ] && cp -r "$OMNI_ROOT/src/lib/db/migrations" "$DST/migrations"

# ── 2. Targeted PWA manifest patches (before the general pass so short_name
#       becomes "aroute" rather than the long form) ───────────────────────────
log "patching PWA manifest"
mapfile -t MANIFEST_FILES < <(grep -rl 'OmniRoute AI' "$DST/.build/next/server" --include='*.js' 2>/dev/null || true)
MANIFEST_FILES+=("$DST/.build/next/server/app/manifest.webmanifest.body")
for f in "${MANIFEST_FILES[@]}"; do
  [ -f "$f" ] || continue
  perl -pi -e '
    s/OmniRoute AI 网关/Artificial Route/g;
    s/short_name:"OmniRoute"/short_name:"aroute"/g;
    s/"short_name":"OmniRoute"/"short_name":"aroute"/g;
    s/OmniRoute 是一个面向多提供者 LLM 的 AI 网关。一个端点连接您所有的 AI 提供者。/Multi-provider LLM gateway. One endpoint connects all your providers./g;
    s/OmniRoute Dashboard/Artificial Route Dashboard/g;
  ' "$f"
done

# ── 3. General display-text rebrand ──────────────────────────────────────────
# Guard: not inside identifiers (chars before/after), not in URLs (slash),
# not a package name (@), not X-OmniRoute-* headers (hyphen-before).
# Both sides: [A-Za-z0-9_] excludes identifier continuation/leading chars,
# \/ excludes URL segments, lookbehind @- excludes namespaces and headers.
GENERAL='s/(?<![A-Za-z0-9_\/@-])OmniRoute(?![A-Za-z0-9_\/])/Artificial Route/g'
# Fallback for files the parse gate rejects: only quote-anchored string forms.
ANCHORED='s/(["\x27\x60])OmniRoute(["\x27\x60])/${1}Artificial Route${2}/g;
          s/(["\x27\x60])OmniRoute(?![A-Za-z0-9_\/])/${1}Artificial Route/g;
          s/(?<![A-Za-z0-9_\/@-])OmniRoute(?=["\x27\x60.,;:!?—–…-])/Artificial Route/g'

# openapi.yaml gets the hyphen-safe variant so X-OmniRoute-* header names survive.
perl -pi -e 's/(?<![A-Za-z0-9_\/@-])OmniRoute(?![A-Za-z0-9_\/])/Artificial Route/g' "$DST/public/openapi.yaml"

# ── 4. Parse gate: baseline pass-set, patch, re-check ───────────────────────
log "baseline node --check over compiled JS"
mapfile -t JS_FILES < <(find "$DST/.build" "$DST/public" -type d -name node_modules -prune -o \
  -type f -name '*.js' ! -name '*.map' -print)
PASS=/tmp/ar-rebrand-pass.$$; FAIL=/tmp/ar-rebrand-fail.$$; NOCHECK=/tmp/ar-rebrand-nocheck.$$
trap 'rm -f "$PASS" "$FAIL" "$NOCHECK"' EXIT
: >"$PASS"; : >"$NOCHECK"
for f in "${JS_FILES[@]}"; do
  if node --check "$f" >/dev/null 2>&1; then echo "$f" >>"$PASS"; else echo "$f" >>"$NOCHECK"; fi
done
log "baseline: $(wc -l <"$PASS") parse, $(wc -l <"$NOCHECK") don't (ESM etc. — anchored patch only)"

log "applying general rebrand"
find "$DST/.build" -type d -name node_modules -prune -o -type f \
  \( -name '*.js' -o -name '*.mjs' -o -name '*.cjs' -o -name '*.json' \
     -o -name '*.html' -o -name '*.css' -o -name '*.body' \) \
  ! -name '*.map' ! -name '*.nft.json' -print0 | xargs -0 perl -pi -e "$GENERAL"
find "$DST/public" -type f \( -name '*.js' -o -name '*.html' -o -name '*.css' -o -name '*.svg' \) \
  -print0 | xargs -0 perl -pi -e "$GENERAL"

log "re-checking parse gate"
: >"$FAIL"
while IFS= read -r f; do
  node --check "$f" >/dev/null 2>&1 || echo "$f" >>"$FAIL"
done <"$PASS"

if [ -s "$FAIL" ]; then
  log "gate rejected $(wc -l <"$FAIL") file(s) — restoring originals, applying anchored patch"
  while IFS= read -r f; do
    rel="${f#"$DST"/}"
    cp "$SRC/$rel" "$f"
    perl -pi -e "$ANCHORED" "$f"
    node --check "$f" >/dev/null 2>&1 || die "still unparsable after anchored patch: $f"
  done <"$FAIL"
fi

# Files that never parsed as originals (ESM): anchored patch only.
if [ -s "$NOCHECK" ]; then
  log "anchored patch for $(wc -l <"$NOCHECK") non-CJS file(s)"
  while IFS= read -r f; do
    perl -pi -e "$ANCHORED" "$f"
  done <"$NOCHECK"
fi

# Re-apply targeted manifest patch to any file the general pass may have
# touched first (order is: targeted → general, so this is a no-op safety net).
log "done patching"

# ── 5. Boot smoke ────────────────────────────────────────────────────────────
SMOKE_UNIT="ar-rebrand-smoke.$$"
mkdir -p "$DATA_DIR"
# A stale squatter on the port makes every smoke verdict a lie (polling hits the
# wrong server), so when the operator did not pin a port, ask the OS for a free
# one instead of guessing.
if [ -z "${AR_DASH_PORT:-}" ]; then
  PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
fi
log "boot smoke on :$PORT"
systemd-run --user --collect --unit="$SMOKE_UNIT" \
  --property=WorkingDirectory="$DST" \
  --setenv=PORT="$PORT" --setenv=HOSTNAME=127.0.0.1 \
  --setenv=DATA_DIR="$DATA_DIR" \
  node "$DST/server.js" >/dev/null 2>&1 || die "systemd-run failed"
trap "systemctl --user stop '$SMOKE_UNIT' >/dev/null 2>&1 || true; rm -f '$PASS' '$FAIL' '$NOCHECK'" EXIT
ok=0
for _ in $(seq 1 30); do
  if curl -sf -m 3 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ "$ok" = 1 ] || { journalctl --user -u "$SMOKE_UNIT" -n 30 --no-pager >&2 || true; die "dashboard did not come up"; }

MANIFEST_NAME=$(curl -sf -m 5 "http://127.0.0.1:$PORT/manifest.webmanifest" | python3 -c 'import json,sys; print(json.load(sys.stdin)["name"])' 2>/dev/null || echo MISSING)
[ "$MANIFEST_NAME" = "Artificial Route" ] || die "manifest name is '$MANIFEST_NAME', expected 'Artificial Route'"

LOGIN=$(curl -sf -m 10 "http://127.0.0.1:$PORT/login" 2>/dev/null || true)
case "$LOGIN" in
  *"Artificial Route"*) : ;;
  *) die "login page does not mention Artificial Route" ;;
esac

systemctl --user stop "$SMOKE_UNIT" >/dev/null 2>&1 || true
log "OK — dist rebuilt at $DST ($(du -sh "$DST" | cut -f1)), manifest=$MANIFEST_NAME"
