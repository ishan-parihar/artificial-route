#!/usr/bin/env bash
# One-command gate credential management for this machine.
#
# The problem it solves: the gate token used to be copied by hand into three
# harness configs, so every rotation was four edits and a 401 if one was
# missed. Here the token lives in ONE file and this script regenerates every
# consumer from it.
#
#   ./scripts/gate-token.sh mint          # mint a non-expiring token and push it everywhere
#   ./scripts/gate-token.sh write <jwt>   # push an existing token everywhere
#   ./scripts/gate-token.sh verify        # check that every consumer carries the current token
#   ./scripts/gate-token.sh show          # print where the token lives (not the token)
#
# Consumers, and how each one reads it:
#   opencode  ~/.config/opencode/opencode.json   {file:<canonical>}  — read at request time
#   omp       ~/.omp/agent/models.yml             apiKey: <jwt>      — inline, regenerated here
#   jcode     ~/.jcode/provider-omniroute.env     OMNIROUTE_API_KEY=<jwt> — inline, regenerated here
#   claude    ~/.claude/settings.json             env.ANTHROPIC_AUTH_TOKEN=<jwt> — inline
#
# Why some are inline: opencode interpolates a file into its config, but omp,
# jcode and claude take a literal value. Rather than teach three harnesses a
# new syntax, this script owns the derived copies. A consumer that supports
# `{file:…}` should use the canonical path instead of a generated copy.
#
# Claude Code reads ANTHROPIC_AUTH_TOKEN from settings.json `env`, but it has
# no `{file:…}` form — the value is taken literally. A stale copy there makes
# every session 401 with "this token is not valid", which reads as a gateway
# fault rather than a credential drift, so it is managed here too.
set -euo pipefail

CANONICAL="${AR_GATE_TOKEN_FILE:-$HOME/.config/ar/racknerd-gate-token.jwt}"
OPENCODE_JSON="${AR_OPENCODE_JSON:-$HOME/.config/opencode/opencode.json}"
OMP_MODELS="${AR_OMP_MODELS:-$HOME/.omp/agent/models.yml}"
# jcode resolves a profile's `env_file` against its **config home**, which on
# this box is `~/.config/jcode/` — not `~/.jcode/`, where the config file
# itself lives. The two drifted: the token was written to `~/.jcode/` and the
# file jcode actually read, `~/.config/jcode/provider-omniroute.env`, kept an
# old `sk-omni-` Dashboard key. The symptom was a 401
# `credential_rejected` on every jcode request while `gate-token.sh verify`
# reported "ok" — because verify checked the file it wrote, not the file
# jcode read.
#
# So: write the one jcode reads, and keep the other in step so a consumer
# that resolves either way finds a live token.
JCODE_ENV="${AR_JCODE_ENV:-$HOME/.config/jcode/provider-omniroute.env}"
JCODE_ENV_LEGACY="${AR_JCODE_ENV_LEGACY:-$HOME/.jcode/provider-omniroute.env}"
CLAUDE_SETTINGS="${AR_CLAUDE_SETTINGS:-$HOME/.claude/settings.json}"
AROUTE_BIN="${AROUTE_BIN:-$HOME/.local/bin/aroute}"
KEY_ID="${AR_GATE_KEY_ID:-racknerd-harness}"

die() { printf 'gate-token: %s\n' "$1" >&2; exit 1; }

# Writes the token to every consumer. Idempotent, and safe to run when only
# one consumer is out of date: each is rewritten from the token it is given.
push() {
  local token="$1"
  [ -n "$token" ] || die "empty token"
  printf '%s' "$token" > "$CANONICAL"
  chmod 600 "$CANONICAL"

  if [ -f "$OPENCODE_JSON" ]; then
    python3 - "$OPENCODE_JSON" "$CANONICAL" <<'PY'
import json, sys
path, canonical = sys.argv[1], sys.argv[2]
with open(path) as fh:
    cfg = json.load(fh)
provider = cfg.setdefault("provider", {}).setdefault("ar", {})
provider.setdefault("options", {})["apiKey"] = "{file:%s}" % canonical
with open(path, "w") as fh:
    json.dump(cfg, fh, indent=2)
    fh.write("\n")
PY
  fi

  if [ -f "$OMP_MODELS" ]; then
    python3 - "$OMP_MODELS" "$token" <<'PY'
import sys
path, token = sys.argv[1], sys.argv[2]
lines = open(path).read().split("\n")
out, inside = [], False
for line in lines:
    if line.strip() == "aroute:":
        inside = True
    elif inside and line and not line.startswith("    "):
        inside = False
    if inside and line.strip().startswith("apiKey:"):
        line = "    apiKey: %s" % token
    out.append(line)
open(path, "w").write("\n".join(out))
PY
  fi

  # Both jcode paths are written: the one jcode reads and the legacy one.
  # The write is unconditional rather than guarded on the file existing —
  # a missing env file is the state that produced the 401, and skipping it
  # is how the drift survived a `verify` that said "ok".
  for env_file in "$JCODE_ENV" "$JCODE_ENV_LEGACY"; do
    if [ "$(dirname "$env_file")" = "$HOME/.jcode" ] && [ ! -d "$HOME/.jcode" ]; then
      continue
    fi
    printf 'OMNIROUTE_API_KEY=%s\n' "$token" > "$env_file"
    chmod 600 "$env_file"
  done

  if [ -f "$CLAUDE_SETTINGS" ]; then
    python3 - "$CLAUDE_SETTINGS" "$token" <<'PY'
import json, sys
path, token = sys.argv[1], sys.argv[2]
with open(path) as fh:
    cfg = json.load(fh)
env = cfg.setdefault("env", {})
# Only touch a settings file that is actually pointed at this gateway: a
# Claude Code install talking to first-party Anthropic has no auth token here
# and must not be given one.
if env.get("ANTHROPIC_BASE_URL"):
    env["ANTHROPIC_AUTH_TOKEN"] = token
    with open(path, "w") as fh:
        json.dump(cfg, fh, indent=2)
        fh.write("\n")
    os_mode = 0o600
    import os
    os.chmod(path, os_mode)
PY
  fi
}

case "${1:-}" in
  mint)
    # The workstation holds no gate master (its ar.env has none), so a local
    # `aroute keys mint` cannot sign and dies with "mint produced no token".
    # The VPS holds the master and the credential store, so mint THERE and
    # bring the token home. Set AR_GATE_MINT_SSH to override the host.
    mint_ssh="${AR_GATE_MINT_SSH:-racknerd}"
    if [ -x "$AROUTE_BIN" ] && "$AROUTE_BIN" keys mint --key-id "$KEY_ID" --ttl-days 0 2>/dev/null | grep -q '^token: '; then
      token="$("$AROUTE_BIN" keys mint --key-id "$KEY_ID" --ttl-days 0 | awk '/^token: /{print $2}')"
      [ -n "$token" ] || die "mint produced no token"
    else
      token="$(ssh "$mint_ssh" 'export AR_MASTER_KEY=$(grep -E "^AR_MASTER_KEY=" ~/.config/ar/ar.env | cut -d= -f2); export AR_HTTP_MASTER_KEY=$(grep -E "^AR_HTTP_MASTER_KEY=" ~/.config/ar/ar.env | cut -d= -f2); ~/.local/bin/aroute --config ~/.config/ar/config.yaml keys mint --key-id '"$KEY_ID"' --ttl-days 0' 2>/dev/null | awk '/^token: /{print $2}')"
      [ -n "$token" ] || die "mint produced no token (local mint failed and the VPS mint over $mint_ssh failed too)"
    fi
    push "$token"
    printf 'gate-token: minted %s (%s) and pushed to every consumer\n' "$KEY_ID" "$CANONICAL"
    ;;
  write)
    [ -n "${2:-}" ] || die "usage: $0 write <jwt>"
    push "$2"
    printf 'gate-token: pushed the given token to every consumer\n'
    ;;
  verify)
    [ -f "$CANONICAL" ] || die "no canonical token file at $CANONICAL"
    token="$(cat "$CANONICAL")"
    printf 'canonical : %s (%s bytes)\n' "$CANONICAL" "${#token}"
    for f in "$OPENCODE_JSON" "$OMP_MODELS" "$JCODE_ENV" "$JCODE_ENV_LEGACY" "$CLAUDE_SETTINGS"; do
      if [ ! -f "$f" ]; then
        # The legacy jcode path is optional: it only exists because the two
        # homes drifted. Its absence is not a failure.
        if [ "$f" = "$JCODE_ENV_LEGACY" ]; then
          printf '  %-44s (absent, optional)\n' "$f"
          continue
        fi
        printf '  %-44s (absent)\n' "$f"
        continue
      fi
      if grep -qF "$token" "$f"; then
        printf '  %-44s ok\n' "$f"
      elif [ "$f" = "$OPENCODE_JSON" ] && grep -qF "{file:$CANONICAL}" "$f"; then
        printf '  %-44s ok (reads the canonical file)\n' "$f"
      else
        printf '  %-44s STALE\n' "$f"
      fi
    done
    printf 'remote    : '
    if curl -fsS -m 15 -H "Authorization: Bearer $token" \
        https://aroute.ishanparihar.com/v1/models >/dev/null 2>&1; then
      printf 'the endpoint accepts the canonical token\n'
    else
      printf 'the endpoint REJECTED the canonical token\n'
    fi
    ;;
  show)
    printf 'canonical : %s\n' "$CANONICAL"
    printf 'opencode  : %s\n' "$OPENCODE_JSON"
    printf 'omp       : %s\n' "$OMP_MODELS"
    printf 'jcode     : %s\n' "$JCODE_ENV"
    printf 'jcode (legacy): %s\n' "$JCODE_ENV_LEGACY"
  printf 'claude    : %s\n' "$CLAUDE_SETTINGS"
    printf 'key id    : %s\n' "$KEY_ID"
    ;;
  *)
    sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
