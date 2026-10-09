#!/usr/bin/env bash
# Fetch the ONNX model-inference stack into an installed dashboard dist, on
# demand.
#
# The shipped dist deliberately leaves it out: onnxruntime-node (288 MB),
# onnxruntime-web (141 MB) and their companions are ~450 MB of weight for two
# features that are off unless an operator turns them on — OmniRoute's vector
# memory (`enabled: false` by default) and the LLMLingua compression engine
# (opt-in, fail-open). Without them the server boots and serves normally; with
# them the memory page and the real LLMLingua engine come alive.
#
#   dashboard/enable-onnx.sh [DIST]
#
# DIST defaults to ~/.config/ar/dashboard (where install.sh --with-dashboard
# puts it). Requires network access and npm; the dist must already exist.
# After fetching, restart the dashboard so the supervisor's Node child picks
# the new modules up.
set -euo pipefail

DIST="${1:-$HOME/.config/ar/dashboard}"
[ -f "$DIST/server.js" ] || {
  echo "error: no dashboard dist at $DIST" >&2
  echo "       install it first: install.sh --with-dashboard --dashboard-src <dist>" >&2
  exit 1
}
command -v npm >/dev/null 2>&1 || { echo "error: npm not found on PATH" >&2; exit 1; }

echo "fetching ONNX stack into $DIST (this is the ~450 MB the dist ships without) ..."
npm install --prefix "$DIST" --no-save --no-audit --no-fund \
  onnxruntime-node@1.30.0 \
  "@huggingface/transformers@^4.2.0" \
  "@atjsh/llmlingua-2@3.0.0" \
  "js-tiktoken@^1.0.20"

echo "done: $(du -sh "$DIST" | cut -f1) at $DIST"
echo "restart to pick it up: systemctl --user restart aroute-dashboard"
