#!/usr/bin/env bash
# patch-artificial.sh — close the user-visible "omni" wording the rebrand
# pipeline deliberately leaves (docs/14's retained table) plus the sweep
# findings in docs/16 §4. Applicable to ANY dist: the freshly rebranded
# dashboard/dist or the installed ~/.config/ar/dashboard.
#
# What it patches (display text only; every occurrence is a quoted string):
#   sk_omniroute      → sk_artificial   (sentinel shared by locale copy and
#                                        the CLI-config writers/detectors —
#                                        patching it everywhere keeps them
#                                        coherent; minted keys match by value)
#   sk-your-omniroute → sk-your-artificial
#   diegosouzapw/OmniRoute (bare repo refs, NOT deep links) →
#                                        ishan-parihar/artificial-route
#
# What it never touches: X-OmniRoute-* headers, OmniRoute/1.0 UA, OMNIROUTE_*
# env names, @omniroute/* namespace, i18n key NAMES, ~/.omniroute paths (real
# for browser-login-profiles), deep links into the upstream repo.
#
# Safety: same contract as rebrand-dist.sh — node --check baseline over
# exactly the files it patches, restore+quote-anchored on failure, boot smoke
# on an OS-picked free port with an isolated DATA_DIR, manifest + brand +
# sentinel-absence assertions. Idempotent: a fully patched dist is a no-op.

set -euo pipefail

AR_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="${1:-${AR_PATCH_DIST:-$AR_ROOT/dashboard/dist}}"
if [ ! -f "$DIST/server.js" ]; then
  DIST="$HOME/.config/ar/dashboard"
fi
[ -f "$DIST/server.js" ] || { echo "[patch] FATAL: no dist at $DIST (pass one as \$1)" >&2; exit 1; }
DATA_DIR="${AR_DASH_DATA:-/tmp/ar-dashboard-data}"

log() { printf '[patch] %s\n' "$*"; }
die() { printf '[patch] FATAL: %s\n' "$*" >&2; exit 1; }

command -v node >/dev/null || die "node not found on PATH"
command -v rg >/dev/null || die "ripgrep (rg) not found on PATH"

# ── 1. Find the files that need patching ─────────────────────────────────────
# rg skips hidden dirs by default (.build) and would honor any stray ignore
# files; --hidden --no-ignore plus an explicit node_modules prune is the
# honest walk.
TARGETS=$(rg --pcre2 -l --hidden --no-ignore -g '!node_modules/**' \
  'sk_omniroute|sk-your-omniroute|(?<![\w.@-])diegosouzapw/OmniRoute(?![\w/-])' \
  "$DIST/.build/next/server" "$DIST/public" 2>/dev/null || true)
if [ -z "$TARGETS" ]; then
  # A detection failure (bad regex, missing engine) would fake a clean dist,
  # so the no-op verdict is double-checked with a plain fixed-string scan.
  if rg -l --hidden --no-ignore -g '!node_modules/**' 'sk_omniroute' \
      "$DIST/.build/next/server" "$DIST/public" >/dev/null 2>&1; then
    die "target scan failed but the sentinel is present — detection is broken"
  fi
  log "nothing to patch — dist is already artificial ($DIST)"
  exit 0
fi
COUNT=$(printf '%s\n' "$TARGETS" | wc -l)
log "patching $COUNT file(s) in $DIST"

# ── 2. Patch with per-file backup and a parse gate ───────────────────────────
BACKUP="$(mktemp -d /tmp/ar-patch-artificial.XXXXXX)"
trap 'rm -rf "$BACKUP"' EXIT

patch_file() {
  perl -pi -e '
    s/sk_omniroute/sk_artificial/g;
    s/sk-your-omniroute/sk-your-artificial/g;
    s{(?<![\w.@-])diegosouzapw/OmniRoute(?![\w/-])}{ishan-parihar/artificial-route}g;
  ' "$1"
}

anchored_patch_file() {
  # Fallback for files the parse gate rejects: only quote-anchored forms.
  perl -pi -e '
    s/(["\x27`])sk_omniroute/${1}sk_artificial/g;
    s/(["\x27`])sk-your-omniroute/${1}sk-your-artificial/g;
    s/(["\x27`])diegosouzapw\/OmniRoute(?![\w\/-])/${1}ishan-parihar\/artificial-route/g;
  ' "$1"
}

while IFS= read -r f; do
  rel="${f#"$DIST/"}"
  cp "$f" "$BACKUP/$(printf '%s' "$rel" | tr / _)"
  patch_file "$f"
  if ! node --check "$f" >/dev/null 2>&1; then
    cp "$BACKUP/$(printf '%s' "$rel" | tr / _)" "$f"
    anchored_patch_file "$f"
    node --check "$f" >/dev/null 2>&1 || die "still unparsable after anchored patch: $rel"
    log "anchored patch: $rel"
  fi
done <<<"$TARGETS"
log "parse gate: $COUNT file(s) still parse"

# ── 3. Boot smoke on an OS-picked free port ──────────────────────────────────
SMOKE_UNIT="ar-patch-smoke.$$"
mkdir -p "$DATA_DIR"
if [ -n "${AR_PATCH_PORT:-}" ]; then PORT="$AR_PATCH_PORT"; else
  PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
fi

systemd-run --user --collect --unit="$SMOKE_UNIT" \
  --property=WorkingDirectory="$DIST" \
  --setenv=PORT="$PORT" --setenv=HOSTNAME=127.0.0.1 \
  --setenv=DATA_DIR="$DATA_DIR" \
  node "$DIST/peer-stamp-launcher.cjs" >/dev/null 2>&1 || die "systemd-run failed"
SMOKE_STARTED=1
trap 'rm -rf "$BACKUP"; [ -n "${SMOKE_STARTED:-}" ] && systemctl --user stop "$SMOKE_UNIT" >/dev/null 2>&1 || true' EXIT

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

# rg exits 1 on no-match; under pipefail that is a pipeline failure, so the
# assignment must be guarded — "no matches" is the success case here.
RESIDUAL=$(rg --pcre2 -l --hidden --no-ignore -g '!node_modules/**' \
  'sk_omniroute|sk-your-omniroute' "$DIST/.build/next/server" "$DIST/public" 2>/dev/null | wc -l) || RESIDUAL=0
[ "$RESIDUAL" = 0 ] || die "$RESIDUAL file(s) still carry the sentinel"

systemctl --user stop "$SMOKE_UNIT" >/dev/null 2>&1 || true
log "OK — dist is fully artificial ($DIST), manifest=$MANIFEST_NAME"
