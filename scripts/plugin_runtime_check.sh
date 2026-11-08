#!/usr/bin/env bash
# Proves the omo plugin's runtime does not need `node_modules`.
#
# The reason this matters: Codex's plugin-content fingerprint walks the
# plugin root and rejects every symlink, so a bundle with `node_modules`
# symlinks cannot be fingerprinted and every hook in it is skipped. The
# question is therefore not "can the symlinks be removed" but "does
# anything need them".
#
# The test is: move `node_modules` aside, then run every runtime entry
# point the plugin declares — every hook command and every MCP server — and
# see whether they still work. If they do, the tree is npm's workspace
# plumbing, not runtime code.
set -uo pipefail

# The plugin version is not written down here: the plugin auto-updates, and a
# script that names a version tests a version that may no longer be
# installed. The newest version in the cache is the one Codex loads.
CACHE="$HOME/.codex/plugins/cache/sisyphuslabs/omo"
ROOT=$(ls -d "$CACHE"/*/ 2>/dev/null | sort -V | tail -1)
ROOT="${ROOT%/}"
STASH="$ROOT/.node_modules-stashed"

fail=0
ok()  { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }

if [ -z "$ROOT" ] || [ ! -d "$ROOT" ]; then
  bad "no plugin version found under $CACHE"
  exit 1
fi

echo "--- plugin version: ${ROOT##*/} ---"

if [ ! -d "$ROOT/node_modules" ]; then
  # The plugin shipped without `node_modules`, which is the healthy shape:
  # there is no npm plumbing to need, and nothing for Codex's fingerprint
  # walk to reject.
  ok "the plugin has no node_modules, so there is no symlink plumbing to need"
  echo
  echo "PLUGIN_RUNTIME_CHECK: PASS"
  exit 0
fi

restore() {
  if [ -d "$STASH" ] && [ ! -e "$ROOT/node_modules" ]; then
    mv "$STASH" "$ROOT/node_modules"
    echo "node_modules restored"
  fi
}
trap restore EXIT

echo "--- stashing node_modules ---"
mv "$ROOT/node_modules" "$STASH"

# --- every hook command the plugin declares --------------------------------
# The commands are read from the hook files themselves, so a new hook is
# covered without editing this script.
echo "--- hook commands ---"
failed_hooks=0
total_hooks=0
for hookfile in "$ROOT"/hooks/*.json; do
  [ -e "$hookfile" ] || continue
  while IFS= read -r cmd; do
    [ -n "$cmd" ] || continue
    total_hooks=$((total_hooks + 1))
    # The command is `node <script> <hook event>`. Substitute PLUGIN_ROOT, then
    # run it the way Codex runs it: with the event on stdin. The script's own
    # `--help` is not a substitute — it exits 1 for some of these CLIs even
    # when everything they need is present, so a probe built on it reports a
    # missing dependency that is not there.
    #
    # The event is the command's own last argument, not a fixed one: a
    # `post-tool-use` CLI does not answer a `user-prompt-submit` event, and a
    # probe that asks the wrong question reports a dependency that is not
    # missing.
    resolved=${cmd//\$\{PLUGIN_ROOT\}/$ROOT}
    script=${resolved#node }
    script=${script%% *}
    # The plugin quotes the path, and the quotes are part of the token the
    # shell would have removed.
    script=${script#\"}
    script=${script%\"}
    # The event is what follows the script path.
    event=${resolved##* }
    if printf '%s\n' "{\"hook_event_name\":\"$event\",\"prompt\":\"check\"}" \
      | timeout 20 node "$script" hook "$event" >/dev/null 2>&1; then
      :
    else
      echo "  hook command failed: $cmd"
      failed_hooks=$((failed_hooks + 1))
    fi
  done < <(python3 - "$hookfile" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
for groups in (d.get("hooks") or {}).values():
    for group in groups or []:
        for h in group.get("hooks") or []:
            cmd = h.get("command")
            if cmd:
                print(cmd)
PY
)
done
if [ "$failed_hooks" -eq 0 ]; then
  ok "all $total_hooks hook commands run without node_modules"
else
  bad "$failed_hooks of $total_hooks hook commands need node_modules"
fi

# --- every MCP server the plugin declares ----------------------------------
echo "--- MCP servers ---"
failed_mcp=0
total_mcp=0
while IFS= read -r entry; do
  [ -n "$entry" ] || continue
  total_mcp=$((total_mcp + 1))
  # A stdio MCP server answers a request on stdin; give it one and see
  # whether it replies rather than crashing on a missing module.
  out=$(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"check","version":"1"}}}' \
    | timeout 20 bash -c "cd $ROOT && $entry" 2>&1)
  if printf '%s' "$out" | grep -qE "Cannot find module|ERR_MODULE_NOT_FOUND|ERR_MODULE"; then
    echo "  MCP server needs node_modules: $entry"
    failed_mcp=$((failed_mcp + 1))
  fi
done < <(python3 - "$ROOT/.mcp.json" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
for name, spec in (d.get("mcpServers") or {}).items():
    cmd = spec.get("command")
    args = spec.get("args") or []
    if cmd and args:
        print(" ".join([cmd] + args))
PY
)
if [ "$failed_mcp" -eq 0 ]; then
  ok "all $total_mcp MCP servers start without node_modules"
else
  bad "$failed_mcp of $total_mcp MCP servers need node_modules"
fi

echo
if [ "$fail" = 0 ]; then echo "PLUGIN_RUNTIME_CHECK: PASS"; else echo "PLUGIN_RUNTIME_CHECK: FAIL"; fi
exit $fail
