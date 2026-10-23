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
# The upstream checkout lives in the sibling `external-repos` tree, not beside
# this repo. The old default (`$AR_ROOT/../OmniRoute`) pointed at a path that
# does not exist, so the pipeline died at its first step (docs/41 §4, gap B4).
OMNI_ROOT="${OMNI_ROOT:-$AR_ROOT/../external-repos/OmniRoute}"
SRC="$OMNI_ROOT/.build/next/standalone"
DST="$AR_ROOT/dashboard/dist"
PORT="${AR_DASH_PORT:-20149}"
DATA_DIR="${AR_DASH_DATA:-/tmp/ar-dashboard-data}"
# The subpath the bundle is served under. Baked at BUILD time (the dist's own
# `BUILD_OMNIROUTE_BASE_PATH` sentinel records it), and it must match the
# runtime env exactly — the standalone server refuses to start when the two
# disagree. `/dash` is the path mount docs/41 §1 chose for the aroute
# hostname; an empty value means a root-built dist, which is what a local
# operator run wants.
BASE_PATH="${AR_BASE_PATH:-/dash}"

log() { printf '[rebrand] %s\n' "$*"; }
die() { printf '[rebrand] FATAL: %s\n' "$*" >&2; exit 1; }

command -v node >/dev/null || die "node not found on PATH"
[ -f "$SRC/server.js" ] || die "no standalone build at $SRC — run: (cd $OMNI_ROOT && npm install && npm run build)"

# ── 1. Copy essentials (server + compiled UI + runtime deps; no source) ──────
log "copying standalone essentials → $DST"
rm -rf "$DST"
mkdir -p "$DST"
cp "$SRC/server.js" "$SRC/package.json" "$DST/"
# package-lock.json is REQUIRED in the dist: any host that resolves deps
# with `npm ci --omit=dev` (the --with-deps-less tarball shape) hard-fails
# without it. Upstream's own install posture rides along too: their .npmrc
# carries legacy-peer-deps=true (their tree has a real marked/marked-terminal
# peer conflict), fetch-retry hardening, and onnxruntime-node-install=skip
# (without it the postinstall pulls ~300 MB of CUDA binaries on linux/x64).
[ -f "$SRC/package-lock.json" ] && cp "$SRC/package-lock.json" "$DST/"
[ -f "$OMNI_ROOT/.npmrc" ] && cp "$OMNI_ROOT/.npmrc" "$DST/.npmrc"
# The standalone package.json carries upstream's full DEV scripts block, all
# pointing at scripts/ dirs that do not exist in a standalone dist. npm ci
# runs `postinstall` on every install and dies on the missing file, so the
# scripts block is stripped from the dist's copy: the standalone runtime
# never calls them, and npm ci resolves the tree purely from dependencies.
node -e '
  const fs = require("fs");
  const p = process.argv[1];
  const pkg = JSON.parse(fs.readFileSync(p, "utf8"));
  delete pkg.scripts;
  fs.writeFileSync(p, JSON.stringify(pkg, null, 2) + "\n");
' "$DST/package.json"
cp "$AR_ROOT/dashboard/peer-stamp-launcher.cjs" "$DST/"
cp -r "$SRC/public" "$DST/public"
cp -r "$SRC/node_modules" "$DST/node_modules"
cp -r "$SRC/.build" "$DST/.build"
[ -d "$OMNI_ROOT/src/lib/db/migrations" ] && cp -r "$OMNI_ROOT/src/lib/db/migrations" "$DST/migrations"

# ── 1.5 The upstream launch family and the base-path patcher ride along ──────
# Upstream's own `assembleStandalone` co-locates `server-ws.mjs` (the
# WS-aware wrapper that installs the peer-stamp), `peer-stamp.mjs` (the
# stamping helper WITH the Cloudflare-edge CIDRs and
# OMNIROUTE_TRUSTED_PROXIES handling — gaps A5/B2/B3 in docs/41 §6), and
# `dev/run-standalone.mjs` (the entrypoint that picks server-ws over the
# bare server.js). A v3.8.52-era source tree does not carry them, so each
# copy is guarded: the file lands when upstream shipped it, and the
# vendored `peer-stamp-launcher.cjs` remains the fallback entry otherwise
# (`crates/ar-cli/src/dashboard.rs` picks `server-ws.mjs` first).
#
# The base-path patcher (`patch-base-path.mjs`, upstream's
# `scripts/docker/patch-standalone-base-path.mjs` + the
# `BUILD_OMNIROUTE_BASE_PATH` sentinel contract from
# `ensure-docker-base-path.mjs`) is REQUIRED in the dist even though the
# rebuild bakes `/dash`: the vendored pipeline drops `scripts/docker/` from
# the copy, the installer runs the patcher once on an installed dist, and a
# root-built dist (the local-operator shape) cannot be converted without it.
log "copying upstream launch family + base-path patcher"
# The whole top-level launch family rides along: server-ws.mjs imports the
# guard modules (h2c-upgrade-guard.cjs, webdav-handler.mjs, tls-options.mjs,
# …) by relative path, and upstream keeps adding them — enumerating the set
# breaks on every upstream bump. Every top-level *.mjs/*.cjs is copied; the
# stray config files (next/eslint/postcss/prettier) are inert at runtime and
# a few KB. The vendored peer-stamp-launcher.cjs stays the fallback entry
# for a v3.8.52-era dist that carried none of these.
for f in "$SRC"/*.mjs "$SRC"/*.cjs; do
  [ -f "$f" ] && cp "$f" "$DST/"
done
[ -f "$SRC/dev/run-standalone.mjs" ] && { mkdir -p "$DST/dev"; cp "$SRC/dev/run-standalone.mjs" "$DST/dev/run-standalone.mjs"; }
# run-standalone.mjs imports ../build/{runtime-env,bootstrap-env}.mjs (port
# resolution, heap sizing, .env bootstrap) — the whole build/ dir rides along.
[ -d "$SRC/build" ] && { mkdir -p "$DST/build"; cp -r "$SRC/build/." "$DST/build/"; }
node "$AR_ROOT/dashboard/vendor-base-path-patcher.cjs" "$OMNI_ROOT" "$DST/patch-base-path.mjs" \
  || die "could not vendor the base-path patcher"
# The sentinel records what the bundle was BUILT for. The build writes it
# when upstream's assemble step runs; a dist rebuilt from this pipeline
# carries it, and the launcher (`aroute dashboard`) reads it to keep the
# runtime env in agreement with the bundle.
if [ -f "$SRC/BUILD_OMNIROUTE_BASE_PATH" ]; then
  cp "$SRC/BUILD_OMNIROUTE_BASE_PATH" "$DST/BUILD_OMNIROUTE_BASE_PATH"
fi

# ── 1.7 The call-log artifact worker must be a standalone file in the dist ──
# The compiled writer resolves its worker thread through four candidates
# (see dashboard/build-calllog-worker.mjs). The first is
# <moduleDir>/callLogArtifactWorker.js, where moduleDir is traced from the
# bundled writer chunk and lands under $DST/src/lib/usage. A dist that ships no
# `src/` tree — and no tsx for the .ts candidates — resolves NONE of them, so
# every artifact write died with "Cannot find module …callLogArtifactWorker.js"
# and the queue silently dropped full request/response bodies (71 journal lines
# in one session; the detail view then renders "missing" for every row).
#
# Shipping the FIRST candidate fixes it and leaves the tsx/execArgv paths
# unreachable, so the dist stays tsx-free. The worker's whole closure imports
# only node builtins plus one type-only open-sse import (erased by esbuild), so
# the bundle is self-contained: only node:fs/node:path/node:worker_threads stay
# external. Verified end-to-end: spawned as the writer spawns it
# (new Worker(pathToFileURL), no execArgv), it replies
# {relPath,sizeBytes,sha256} and writes the artifact at 0600 under
# $DATA_DIR/call_logs/<date>/.
log "compiling the call-log artifact worker into the dist"
# $OMNI_ROOT is passed through explicitly: the operator can redirect it, and
# everything else in this script (the copy, the base-path patcher) reads that
# same tree. Deriving the path inside the packager instead would let the worker
# be compiled from a different checkout than the dist it lands in.
node "$AR_ROOT/dashboard/build-calllog-worker.mjs" "$OMNI_ROOT" \
  "$DST/src/lib/usage/callLogArtifactWorker.js" \
  || die "could not compile the call-log artifact worker"
[ -s "$DST/src/lib/usage/callLogArtifactWorker.js" ] \
  || die "the call-log artifact worker bundle is missing or empty"
node --check "$DST/src/lib/usage/callLogArtifactWorker.js" \
  || die "the call-log artifact worker bundle is not parseable CJS"

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

# ── 2.5 Client env-shim patch: the browser must learn the base path ─────────
# The client bundle resolves its basePath from `process.env` at RUNTIME
# (`BasePathNetworkProvider` → `installBasePathFetch` reads
# NEXT_PUBLIC_OMNIROUTE_BASE_PATH || OMNIROUTE_BASE_PATH from the Turbopack
# `process` shim). That shim ships `env:{}` — the build bakes basePath into
# the manifests and SOME literals, but NOT into the shim — so in the browser
# every root-relative `fetch("/api/…")` and `EventSource` stays on the ROOT
# and 404s on a subpath-served dist, which trips the amber
# "Server is experiencing issues" banner after two failed `/api/health/ping`
# polls (reproduced 2026-10-22: banner on, 14 root-path 404s; shim patched →
# banner gone, all calls hit `${basePath}/api/…`). Upstream's own patcher has
# `patchProcessEnvShim` for exactly this, but it only runs root→subpath; a
# dist BUILT for /dash never passes through it, so the pipeline does the
# equivalent splice here, unconditionally, for whatever the sentinel names.
log "patching the client process-shim env with the baked base path"
if [ -f "$DST/BUILD_OMNIROUTE_BASE_PATH" ]; then
  SHIM_BP="$(tr -d '[:space:]' < "$DST/BUILD_OMNIROUTE_BASE_PATH")"
else
  SHIM_BP=""
fi
if [ -n "$SHIM_BP" ] && [ "$SHIM_BP" != "/" ]; then
  SHIM_COUNT=$(node - "$DST/.build/next/static/chunks" "$SHIM_BP" <<'SHIM_EOF'
const fs = require("node:fs");
const path = require("node:path");
const [dir, bpRaw] = process.argv.slice(2);
const bp = JSON.stringify(bpRaw).slice(1, -1); // escape quotes/backslashes
let n = 0;
for (const f of fs.readdirSync(dir)) {
  if (!f.endsWith(".js")) continue;
  const p = path.join(dir, f);
  const before = fs.readFileSync(p, "utf8");
  // Mirrors upstream patchProcessEnvShim: the shim's `.env={}` gains the two
  // keys the app reads. `.env={}` is unique to the shim factory in practice;
  // the parse gate below re-checks every touched file.
  const after = before.replaceAll(
    ".env={}",
    `.env={OMNIROUTE_BASE_PATH:"${bp}",NEXT_PUBLIC_OMNIROUTE_BASE_PATH:"${bp}"}`,
  );
  if (after !== before) { fs.writeFileSync(p, after); n++; }
}
process.stdout.write(String(n));
SHIM_EOF
  )
  log "client env-shim patched in ${SHIM_COUNT} chunk(s) with basePath ${SHIM_BP}"
  [ "${SHIM_COUNT}" -ge 1 ] || die "the client process-shim (.env={}) was not found under $DST/.build/next/static/chunks — the base-path fix did not land"
fi

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

# ── 3.5 Sponsor/partner branding scrub ──────────────────────────────────────
# The upstream bundle ships sponsor surfaces (Kimi/Cheaper Inference banners,
# supporter badges/tooltips, supporter-offers copy). Scrubbed by
# dashboard/scrub-sponsors.cjs, which decodes each embedded `JSON.parse`
# payload with a real parser instead of a `[^}]*` regex (the regex stops at
# the first `}` — inside ICU plurals / nested objects — and corrupted all 134
# locale chunks). It also forces the banner render gates and the
# ProviderCard/ProviderPageHeader partner flags off, because the message
# fallbacks are hardcoded English sponsor copy.
log "scrubbing sponsor branding"
node "$AR_ROOT/dashboard/scrub-sponsors.cjs" "$DST"

# ── 4. Parse gate: baseline pass-set, patch, re-check ───────────────────────
log "baseline node --check over compiled JS"
mapfile -t JS_FILES < <(find "$DST/.build" "$DST/public" -type d -name node_modules -prune -o \
  -type f -name '*.js' ! -name '*.map' -print)
PASS=/tmp/ar-rebrand-pass.$$; FAIL=/tmp/ar-rebrand-fail.$$; NOCHECK=/tmp/ar-rebrand-nocheck.$$
trap 'rm -f "$PASS" "$FAIL" "$NOCHECK"' EXIT
: >"$PASS"; : >"$NOCHECK"
# Parallel: ~20k files × node --check is minutes serial, seconds fanned out
# over the cores. One line per file so the pass/no-check split stays exact.
printf '%s\n' "${JS_FILES[@]}" | xargs -P "$(nproc)" -I{} sh -c \
  'node --check "$1" >/dev/null 2>&1 && echo "$1" || true' _ {} \
  | sort > "$PASS.tmp.$$" || true
ALLF=/tmp/ar-rebrand-all.$$; printf '%s\n' "${JS_FILES[@]}" | sort > "$ALLF"
comm -23 "$ALLF" "$PASS.tmp.$$" > "$NOCHECK"
mv "$PASS.tmp.$$" "$PASS"; rm -f "$ALLF"
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
# Same parallel shape as the baseline pass; failures land in $FAIL.
xargs -P "$(nproc)" -I{} sh -c 'node --check "$1" >/dev/null 2>&1 || echo "$1"' _ {} < "$PASS" >> "$FAIL" || true

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
# ---------------------------------------------------------------------------
# ONNX runtime is opt-in, never shipped.
#
# The model-inference stack (onnxruntime-node/-web/-common, @huggingface,
# @atjsh/llmlingua-2, js-tiktoken) is ~450 MB of the ~2 GB dist and nothing on
# the boot path touches it: OmniRoute's vector memory is `enabled: false` by
# default (src/lib/memory/settings.ts) and the LLMLingua worker imports these
# dynamically under a fail-open contract, so an absent module degrades one
# opt-in feature instead of breaking the server. Verified by booting the pruned
# tree: /api/health, /api/combos, and completions all serve.
# `dashboard/enable-onnx.sh` fetches the stack into an installed dist when an
# operator actually turns those features on.
BEFORE_KB=$(du -sk "$DST" | cut -f1)
for pkg in onnxruntime-node onnxruntime-web onnxruntime-common @huggingface @atjsh js-tiktoken; do
  rm -rf "${DST:?}/node_modules/$pkg"
done
log "ONNX prune: $((BEFORE_KB - $(du -sk "$DST" | cut -f1))) KB lighter — dist is $(du -sh "$DST" | cut -f1)"

log "boot smoke on :$PORT (base path $BASE_PATH)"
systemd-run --user --collect --unit="$SMOKE_UNIT" \
  --property=WorkingDirectory="$DST" \
  --setenv=PORT="$PORT" --setenv=HOSTNAME=127.0.0.1 \
  --setenv=OMNIROUTE_HOSTNAME=127.0.0.1 \
  --setenv=DATA_DIR="$DATA_DIR" \
  node "$DST/dev/run-standalone.mjs" >/dev/null 2>&1 || die "systemd-run failed"
trap "systemctl --user stop '$SMOKE_UNIT' >/dev/null 2>&1 || true; rm -f '$PASS' '$FAIL' '$NOCHECK'" EXIT
ok=0
for _ in $(seq 1 30); do
  if curl -sf -m 3 "http://127.0.0.1:$PORT$BASE_PATH/healthz" >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ "$ok" = 1 ] || { journalctl --user -u "$SMOKE_UNIT" -n 30 --no-pager >&2 || true; die "dashboard did not come up"; }

MANIFEST_NAME=$(curl -sf -m 5 "http://127.0.0.1:$PORT$BASE_PATH/manifest.webmanifest" | python3 -c 'import json,sys; print(json.load(sys.stdin)["name"])' 2>/dev/null || echo MISSING)
[ "$MANIFEST_NAME" = "Artificial Route" ] || die "manifest name is '$MANIFEST_NAME', expected 'Artificial Route'"

LOGIN=$(curl -sf -m 10 "http://127.0.0.1:$PORT$BASE_PATH/login" 2>/dev/null || true)
case "$LOGIN" in
  *"Artificial Route"*) : ;;
  *) die "login page does not mention Artificial Route" ;;
esac

systemctl --user stop "$SMOKE_UNIT" >/dev/null 2>&1 || true

# ── 6. Security fingerprint gate (docs/41 §7 control 14) ───────────────────
# The dist this pipeline publishes must not carry the two advisory paths the
# 2026-10-07 build carried. Both checks are fingerprints OF THE VULNERABLE
# COMPILED SHAPE — not version guesses, and not identifier hunts (minifiers
# rename identifiers; they cannot rewrite string literals or restructure an
# inlined branch). Both validated in both directions 2026-10-22: each matches
# dashboard/dist (2026-10-07, v3.8.52) and neither matches the v3.8.53 rebuild.
#
#   GHSA-66vh-35g3-78qv — the Redis-hit short-circuit that authorized an API
#   key from the cache alone (.get on the `auth:api_key:` key, return BEFORE
#   the SQLite read). Upstream fix 709a00a2ca deleted the entire read path;
#   post-fix the key is only ever .set() after a SQLite confirmation or
#   .del() on revoke — a .get() on it IS the vulnerability.
#
#   GHSA-3fxv-j4h8-9mgg — the OAuth relay callback compared state only when
#   the callback supplied one, so a code with NO state was accepted as the
#   operator's active attempt (account-linking CSRF). Upstream fix
#   faed233501 inverted the check: when the active attempt carries a state,
#   the callback must echo exactly it. The fixed bundle inlines the inverted
#   shape and cannot match the loose pre-fix shape.
#
# A failure is fatal: the pipeline refuses to produce a publishable dist
# that carries a known-vulnerable auth path.
log "security fingerprint gate (GHSA-66vh, GHSA-3fxv)"
G1=$(grep -rlP 'auth:api_key:\$\{[a-zA-Z0-9_$]{1,4}\}.{0,16}\.get\(' "$DST/.build/next/server/chunks" 2>/dev/null | wc -l || true)
[ "$G1" -eq 0 ] || die "GHSA-66vh fingerprint present: the Redis-hit short-circuit (.get on auth:api_key:, return before SQLite) survives in $G1 chunk(s) — the upstream fix (709a00a2ca) did not reach this build"
G2=$(grep -rlP 'state&&[a-zA-Z]&&[a-zA-Z]!==.{0,60}errorStateMismatch' "$DST/.build/next/server/chunks" 2>/dev/null | wc -l || true)
[ "$G2" -eq 0 ] || die "GHSA-3fxv fingerprint present: the loose OAuth callback state check (received state required, missing supplied state accepted) survives in $G2 chunk(s) — the upstream fix (faed233501) did not reach this build"
log "both GHSA fingerprints clear"

# ── 7. Artificial closeout: the user-visible "omni" wording the passes
# above leave (key sentinel copy, repo refs) — docs/16 §4. Runs AFTER the
# general rebrand and AFTER the gates: its own boot smoke asserts the
# login page says "Artificial Route", which only the general pass
# produces, and its sk-artificial sentinel patch must see the final tree.
log "artificial closeout"
"$AR_ROOT/dashboard/patch-artificial.sh" "$DST"

log "OK — dist rebuilt at $DST ($(du -sh "$DST" | cut -f1)), manifest=$MANIFEST_NAME"
