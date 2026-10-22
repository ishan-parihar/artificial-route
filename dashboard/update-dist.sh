#!/usr/bin/env bash
# update-dist.sh — pull the upstream OmniRoute front-end, rebuild it, and
# regenerate the vendored rebranded dist.
#
# The dashboard bundle is a build artifact of ../OmniRoute: dist/ is
# gitignored in this repo, so "more commits in the front end" arrive by
# pulling upstream and re-running the rebrand pipeline — this script chains
# those three steps. Run it from anywhere.
set -euo pipefail

AR_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# The upstream checkout lives in the sibling `external-repos` tree, not beside
# this repo. The old default (`$AR_ROOT/../OmniRoute`) pointed at a path that
# does not exist, so the whole pipeline died at its first step (docs/41 §4,
# gap B4). The default now names the real location; an explicit OMNI_ROOT
# still wins, and it is EXPORTED so the rebrand script — which re-derives its
# own default — cannot diverge from this one.
OMNI_ROOT="${OMNI_ROOT:-$AR_ROOT/../external-repos/OmniRoute}"
export OMNI_ROOT

# The subpath the public dashboard is served under. `next.config.mjs:111`
# bakes `basePath` from `OMNIROUTE_BASE_PATH` at BUILD time, so this must be
# set for the build step too — not just at launch. `/dash` is the path mount
# docs/41 §1 chose; an empty value produces a root-built dist, which is what
# `aroute dashboard` on a local box wants.
export OMNIROUTE_BASE_PATH="${AR_BASE_PATH:-/dash}"

[ -d "$OMNI_ROOT/.git" ] || { echo "no OmniRoute checkout at $OMNI_ROOT" >&2; exit 1; }

echo "[update-dist] pulling $OMNI_ROOT"
git -C "$OMNI_ROOT" pull --ff-only --autostash

echo "[update-dist] installing deps + building"
( cd "$OMNI_ROOT" && npm ci && npm run build )

echo "[update-dist] rebranding into $AR_ROOT/dashboard/dist"
"$AR_ROOT/dashboard/rebrand-dist.sh"
