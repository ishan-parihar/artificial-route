#!/usr/bin/env bash
# Proves the two errors the user reported are gone, in a real Codex session.
#
#   1. "Hook failed — unsupported plugin entry type: …/node_modules/.bin/comment-checker"
#      Codex fingerprints the plugin bundle before running a hook, and the
#      walk rejects any symlink. A bundle with npm workspace symlinks
#      therefore fails the fingerprint and every hook in it is skipped.
#
#   2. "Missing environment variable: `AROUTE_API_KEY`."
#      Codex resolves the provider's `env_key` from the process
#      environment. The key lives in a file the shell sources, so a
#      session started any other way has no key and the turn fails before
#      it reaches the router.
#
# The check runs a real turn and looks for both messages in its output.
#
# The environment variable is *removed* for the run, not exported. Exporting
# it is the condition that hid the bug: a shell that sources the key file
# has the key, so the error never appears there, and the error appears only
# in the sessions that do not. Removing it here is the whole point of the
# check — with the command-backed auth in the config, the turn must work
# with no key in the environment at all.
set -uo pipefail

REPO=/home/ishanp/Documents/github/my-projects/agentic-utility/proxy/artificial-route
BIN="$REPO/target/release/aroute"

fail=0
ok()  { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }

cd "$REPO"

# The key must not be in the environment of the turn. `env -u` removes it
# from the child process only; this shell keeps its own copy.
unset AROUTE_API_KEY

# The config has to carry the command-backed auth, or the turn has nothing
# to authenticate with.
if grep -q '^\[model_providers\.aroute\.auth\]' "$HOME/.codex/config.toml" 2>/dev/null; then
  ok "the config carries [model_providers.aroute.auth]"
else
  bad "the config has no [model_providers.aroute.auth]: run 'aroute codex --auth-command'"
fi

# Codex refuses a provider that carries both `env_key` and `auth`, and the
# whole config then fails to load.
if awk '/^\[model_providers\.aroute\]/{inside=1; next} /^\[/{inside=0} inside && /^env_key/{found=1} END{exit found?0:1}' \
  "$HOME/.codex/config.toml" 2>/dev/null; then
  bad "the provider block still has env_key, which Codex refuses beside auth"
else
  ok "the provider block has no env_key beside the auth table"
fi

# The command must be a program Codex can run, with its arguments beside it.
if grep -q '^command = "aroute"$' "$HOME/.codex/config.toml" 2>/dev/null \
  && grep -q '^args = \["token"\]$' "$HOME/.codex/config.toml" 2>/dev/null; then
  ok "the auth command is a runnable program with its arguments beside it"
else
  bad "the auth command is not 'aroute' with args [\"token\"]"
fi

# The program the config names must be on PATH and must print a token.
if command -v aroute > /dev/null 2>&1; then
  ok "the 'aroute' program is on PATH"
else
  bad "the 'aroute' program is not on PATH"
fi

token=$(env -u AROUTE_API_KEY aroute token 2>/dev/null)
if [ -n "$token" ]; then
  ok "the auth command prints a token with no key in the environment"
else
  bad "the auth command printed no token with no key in the environment"
fi

out=$(env -u AROUTE_API_KEY timeout 600 codex exec -m large-stack \
  'Reply with exactly: aroute-ok' < /dev/null 2>&1)
rc=$?

echo "--- the turn's output ---"
printf '%s\n' "$out" | tail -15
echo "--- end ---"

if [ "$rc" = 0 ]; then
  ok "the turn exited 0"
else
  bad "the turn exited $rc"
fi

if printf '%s' "$out" | grep -q "unsupported plugin entry type"; then
  bad "the plugin fingerprint still fails"
else
  ok "no plugin fingerprint error"
fi

if printf '%s' "$out" | grep -q "Hook failed"; then
  bad "a hook still failed"
else
  ok "no hook failed"
fi

if printf '%s' "$out" | grep -q "Missing environment variable"; then
  bad "the environment variable is still missing"
else
  ok "no missing environment variable"
fi

if printf '%s' "$out" | grep -q "provider auth command"; then
  bad "the auth command itself failed"
else
  ok "the auth command ran"
fi

if printf '%s' "$out" | grep -q "aroute-ok"; then
  ok "the model answered through the router"
else
  bad "the model did not answer: $out"
fi

echo
if [ "$fail" = 0 ]; then echo "CODEX_SESSION_CHECK: PASS"; else echo "CODEX_SESSION_CHECK: FAIL"; fi
exit $fail
