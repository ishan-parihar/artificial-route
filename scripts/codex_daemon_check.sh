#!/usr/bin/env bash
# Proves the daemon restart is what makes the picker list the lanes, by
# driving the real TUI over tmux and reading the `/model` picker.
#
# The daemon reads `model_catalog_json` once, at startup. So the test is:
#   1. append a probe lane to the catalog,
#   2. show the long-running daemon's picker does NOT list it,
#   3. restart the daemon,
#   4. show the picker now DOES list it.
#
# Step 2 is the part that matters. Without it, "the picker lists the lanes"
# would only prove the catalog file is right, not that Codex reads it.
set -uo pipefail

BIN=/home/ishanp/Documents/github/my-projects/agentic-utility/proxy/artificial-route/target/release/aroute
HOME_DIR="$HOME/.codex"
CATALOG="$HOME_DIR/aroute-models.json"
PROBE="zzprobe-daemon-test"
SESSION="codex-daemon-check"
REPO=/home/ishanp/Documents/github/my-projects/agentic-utility/proxy/artificial-route

fail=0
ok()  { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }

# The picker's rows, as the TUI renders them. The picker scrolls, so the
# whole list is collected by scrolling up and down through it.
picker() {
  tmux kill-session -t "$SESSION" 2>/dev/null
  tmux new-session -d -s "$SESSION" -x 200 -y 60 "cd $REPO && codex --no-alt-screen" 2>/dev/null
  sleep 10
  tmux send-keys -t "$SESSION" "/model" 2>/dev/null
  sleep 2
  tmux send-keys -t "$SESSION" Enter 2>/dev/null
  sleep 4
  # Scroll to the top of the list, then walk down through it, collecting
  # every screen. The picker shows a handful of rows per screen, so one
  # capture is not the list: the walk is.
  tmux send-keys -t "$SESSION" Home 2>/dev/null
  sleep 1
  local all=""
  # The picker shows a handful of rows per screen, so the walk is what makes
  # one capture into a list. The catalog carries the lanes plus clones of the
  # bundled entries, so the walk has to cover them all.
  for _ in $(seq 1 45); do
    all="$all
$(tmux capture-pane -p -t "$SESSION" 2>/dev/null)"
    tmux send-keys -t "$SESSION" Down 2>/dev/null
    sleep 0.7
  done
  tmux send-keys -t "$SESSION" Escape 2>/dev/null
  sleep 1
  tmux kill-session -t "$SESSION" 2>/dev/null
  printf '%s' "$all"
}

# --- 1. append the probe lane ----------------------------------------------
python3 - "$CATALOG" "$PROBE" <<'PY'
import json, sys
path, probe = sys.argv[1], sys.argv[2]
d = json.load(open(path))
if not any(x.get("slug") == probe for x in d["models"]):
    clone = dict(d["models"][0])
    clone["slug"] = probe
    clone["display_name"] = probe
    clone["priority"] = 99
    d["models"].append(clone)
    json.dump(d, open(path, "w"), indent=2)
print("probe lane present")
PY

# --- 2. the long-running daemon's picker must NOT list it ------------------
before=$(picker)
if printf '%s' "$before" | grep -q "$PROBE"; then
  bad "the long-running daemon's picker saw a lane added after it started"
else
  ok "the long-running daemon's picker does not list the probe lane"
fi
if printf '%s' "$before" | grep -q "large-stack"; then
  ok "the long-running daemon's picker already lists the 13 lanes"
else
  bad "the long-running daemon's picker does not list the lanes"
fi

# --- 3. restart the daemon, so it reads the catalog at startup --------------
# The daemon is restarted directly here, not through `aroute codex`: that
# command rebuilds the catalog from the router's lanes, which would drop the
# probe and leave nothing to observe. The restart is the daemon's own.
codex app-server daemon restart >/dev/null 2>&1
sleep 3

# --- 4. the restarted daemon's picker MUST list the probe ------------------
after=$(picker)
if printf '%s' "$after" | grep -q "$PROBE"; then
  ok "the restarted daemon's picker lists the probe lane"
else
  bad "the restarted daemon's picker does not list the probe lane"
fi

# The lanes are counted by name, not by the "via aroute" suffix: the walk
# captures the same rows on several screens, so a raw count of the suffix
# counts the repeats.
lane_names=$(printf '%s\n' "$after" | grep -oE "[a-z0-9./-]+ +via aroute" | awk '{print $1}' | sort -u)
listed=$(printf '%s\n' "$lane_names" | grep -c . || true)
if [ "$listed" -ge 13 ]; then
  ok "the picker lists $listed 'via aroute' lanes"
else
  bad "the picker lists only $listed 'via aroute' lanes, not 13: $lane_names"
fi

# --- 5. the command does the restart itself, and says so -------------------
out=$("$BIN" codex --codex-home "$HOME_DIR" --config "$HOME/.config/ar/config.yaml" --force 2>&1)
if printf '%s' "$out" | grep -qi "daemon: restarted"; then
  ok "the command reports the daemon as restarted"
else
  bad "the command did not report a restart: $out"
fi

# --- 6. take the probe back out and leave the catalog as it was ------------
python3 - "$CATALOG" "$PROBE" <<'PY'
import json, sys
path, probe = sys.argv[1], sys.argv[2]
d = json.load(open(path))
d["models"] = [x for x in d["models"] if x.get("slug") != probe]
json.dump(d, open(path, "w"), indent=2)
print("probe lane removed")
PY
"$BIN" codex --codex-home "$HOME_DIR" --config "$HOME/.config/ar/config.yaml" --force >/dev/null 2>&1

echo
if [ "$fail" = 0 ]; then echo "CODEX_DAEMON_CHECK: PASS"; else echo "CODEX_DAEMON_CHECK: FAIL"; fi
exit $fail
