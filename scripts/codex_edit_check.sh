#!/usr/bin/env bash
# Proves the two behaviours skunk owed, against the real binary:
#   1. a foreign model_catalog_json is PRESERVED, and the report says the
#      lanes may not appear.
#   2. the editor is byte-exact: CRLF stays CRLF, and a file with no
#      trailing newline does not gain one.
set -uo pipefail

BIN=/home/ishanp/Documents/github/my-projects/agentic-utility/proxy/artificial-route/target/release/aroute
ROUTER_CONFIG="$HOME/.config/ar/config.yaml"
ROOT=$(mktemp -d "${TMPDIR:-/var/tmp}/codex-edit-check.XXXXXX")
mkdir -p "$ROOT/foreign" "$ROOT/crlf" "$ROOT/nonewline"

fail=0
ok()   { echo "PASS: $1"; }
bad()  { echo "FAIL: $1"; fail=1; }

# --- 1. a foreign catalog path is preserved and reported loudly -------------
cd "$ROOT/foreign"
printf '%s' 'model_catalog_json = "/home/operator/their-own-models.json"
' > config.toml
out=$("$BIN" codex --codex-home "$ROOT/foreign" --no-daemon-restart \
      --config "$ROUTER_CONFIG" 2>&1)
if grep -q 'their-own-models.json' config.toml; then
  ok "the operator's catalog path survived"
else
  bad "the operator's catalog path was overwritten: $(cat config.toml)"
fi
if printf '%s' "$out" | grep -qi "may not appear"; then
  ok "the report says the lanes may not appear"
else
  bad "the report did not say the lanes may not appear: $out"
fi
if printf '%s' "$out" | grep -qi "NOT at"; then
  ok "the report says where it does point instead"
else
  bad "the report did not say where it points: $out"
fi

# --- 2. CRLF survives -------------------------------------------------------
cd "$ROOT/crlf"
printf 'model_reasoning_effort = "medium"\r\n\r\n[tui]\r\nscreen_reader_detection_done = true\r\n' > config.toml
"$BIN" codex --codex-home "$ROOT/crlf" --no-daemon-restart \
  --config "$ROUTER_CONFIG" >/dev/null 2>&1
if [ "$(grep -c $'\r$' config.toml)" = "$(grep -c '' config.toml)" ]; then
  ok "every line still ends with CRLF"
else
  bad "the CRLF file was normalised: $(od -c config.toml | head -5)"
fi
if grep -q 'screen_reader_detection_done = true' config.toml; then
  ok "the operator's own table survived"
else
  bad "the operator's own table was lost"
fi

# --- 3. no trailing newline is added ---------------------------------------
cd "$ROOT/nonewline"
printf 'model_reasoning_effort = "medium"' > config.toml
"$BIN" codex --codex-home "$ROOT/nonewline" --no-daemon-restart \
  --config "$ROUTER_CONFIG" >/dev/null 2>&1
if [ -n "$(tail -c 1 config.toml)" ]; then
  ok "the file still does not end with a newline"
else
  bad "a trailing newline was added: $(od -c config.toml | tail -3)"
fi

# --- 4. the fresh-home path still works, with the provider selected ---------
cd "$ROOT/foreign"
if grep -q '^model_provider = "aroute"' config.toml; then
  ok "model_provider still selects aroute"
else
  bad "model_provider was not set: $(cat config.toml)"
fi
if grep -q '^web_search = "disabled"' config.toml; then
  ok "web_search is disabled"
else
  bad "web_search was not disabled: $(cat config.toml)"
fi
if grep -q '^\[model_providers.aroute\]' config.toml; then
  ok "the aroute provider block exists"
else
  bad "the aroute provider block is missing: $(cat config.toml)"
fi

echo
if [ "$fail" = 0 ]; then echo "CODEX_EDIT_CHECK: PASS"; else echo "CODEX_EDIT_CHECK: FAIL"; fi
exit $fail
