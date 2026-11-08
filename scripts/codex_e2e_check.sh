#!/usr/bin/env bash
# The acceptance the Codex fix has to meet: a real MCP tool dispatch through
# Codex, with no profile and no flags, and the usage landing on the router's
# ledger. A smoke reply is not enough — the tool has to actually run.
set -uo pipefail

BIN=/home/ishanp/Documents/github/my-projects/agentic-utility/proxy/artificial-route/target/release/aroute
LEDGER="$HOME/.config/ar/usage.sqlite"
WORK=$(mktemp -d "${TMPDIR:-/var/tmp}/codex-e2e.XXXXXX")

fail=0
ok()  { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }

# The base config points Codex at the VPS router, so the VPS is where the rows
# land — the local ledger is a different install's and does not move.
SSH="ssh -p 25897 -o ConnectTimeout=15 -o BatchMode=yes nerd@107.173.144.77"

# The last row id before the run, so the new row is identifiable.
before=$($SSH 'sqlite3 ~/.config/ar/usage.sqlite "select coalesce(max(id),0) from usage;"' 2>/dev/null || echo 0)

# A prompt that must call a real MCP tool, not answer from memory.
PROMPT='Use the sourcehound MCP server to search for news about the Rust foundation. Call the tool. Then answer with only the number of results you got.'

# Codex refuses to run outside a trusted directory, so the run happens in the
# repo, which the base config already trusts.
cd /home/ishanp/Documents/github/my-projects/agentic-utility/proxy/artificial-route

# The key is *removed* from the environment rather than exported into it.
# Exporting it is the condition that hid the bug: a shell that sources the
# key file has the key, so the missing-variable error never appears there.
# The router's credential comes from Codex's command-backed auth instead,
# and a turn has to work with no key in the environment at all.
unset AROUTE_API_KEY

out=$(env -u AROUTE_API_KEY timeout 600 codex exec -m large-stack "$PROMPT" < /dev/null 2>&1)
rc=$?

if [ "$rc" = 0 ]; then
  ok "plain 'codex exec -m large-stack' exited 0"
else
  bad "plain 'codex exec -m large-stack' exited $rc: $out"
fi

# The tool must have run: Codex prints the server and tool names.
if printf '%s' "$out" | grep -qi "sourcehound"; then
  ok "the sourcehound MCP tool was invoked"
else
  bad "the sourcehound MCP tool was not invoked: $out"
fi

if printf '%s' "$out" | grep -qi "not supported when using Codex with a ChatGPT account"; then
  bad "the request went to OpenAI's own backend, not the router"
else
  ok "the request did not go to OpenAI's ChatGPT backend"
fi

if printf '%s' "$out" | grep -qi "unsupported tool type"; then
  bad "the router refused the request's tool set"
else
  ok "the router accepted the request's tool set"
fi

# The turn ran with no key in the environment, so the credential came from
# Codex's command-backed auth. If the auth command itself failed, the turn
# would not have reached the router at all.
if printf '%s' "$out" | grep -qi "provider auth command"; then
  bad "the auth command failed"
else
  ok "the turn authenticated with no key in the environment"
fi

# The usage must land on the router's ledger, after the `before` row.
after=$($SSH 'sqlite3 ~/.config/ar/usage.sqlite "select coalesce(max(id),0) from usage;"' 2>/dev/null || echo 0)
if [ "$after" -gt "$before" ]; then
  ok "the router wrote $((after - before)) new ledger row(s)"
else
  bad "no new ledger row (before=$before after=$after)"
fi

newrows=$($SSH "sqlite3 ~/.config/ar/usage.sqlite \"select count(*) from usage where id > $before and combo='large-stack';\"" 2>/dev/null || echo 0)
if [ "$newrows" -gt 0 ]; then
  ok "$newrows new large-stack row(s) on the router's ledger"
else
  bad "no new large-stack row on the router's ledger"
fi

# The row's token count has to match what Codex reported, or the traffic went
# somewhere else. Codex prints the label and the number on separate lines.
reported=$(printf '%s\n' "$out" | grep -A1 -E '^tokens used$' | tail -1 | grep -oE '[0-9,]+' | tr -d ,)
ledger_tokens=$($SSH "sqlite3 ~/.config/ar/usage.sqlite \"select coalesce(sum(total_tokens),0) from usage where id > $before and combo='large-stack';\"" 2>/dev/null || echo 0)
if [ -n "$reported" ] && [ "$ledger_tokens" != "0" ]; then
  # The ledger records each upstream call the turn made, and Codex's number
  # is the whole turn, so the ledger's total is at least the reported one.
  if [ "$ledger_tokens" -ge "$reported" ]; then
    ok "the ledger's new rows carry $ledger_tokens tokens (Codex reported $reported)"
  else
    bad "the ledger's new rows carry $ledger_tokens tokens, fewer than Codex reported $reported"
  fi
else
  bad "could not line up the token counts (codex=$reported ledger=$ledger_tokens)"
fi

echo
echo "--- codex output (tail) ---"
printf '%s\n' "$out" | tail -20
echo
if [ "$fail" = 0 ]; then echo "CODEX_E2E: PASS"; else echo "CODEX_E2E: FAIL"; fi
exit $fail
