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
OMNI_ROOT="${OMNI_ROOT:-$AR_ROOT/../OmniRoute}"

[ -d "$OMNI_ROOT/.git" ] || { echo "no OmniRoute checkout at $OMNI_ROOT" >&2; exit 1; }

echo "[update-dist] pulling $OMNI_ROOT"
git -C "$OMNI_ROOT" pull --ff-only --autostash

echo "[update-dist] installing deps + building"
( cd "$OMNI_ROOT" && npm ci && npm run build )

echo "[update-dist] rebranding into $AR_ROOT/dashboard/dist"
"$AR_ROOT/dashboard/rebrand-dist.sh"
