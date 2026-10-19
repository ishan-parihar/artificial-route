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
#
# Why some are inline: opencode interpolates a file into its config, but omp
# and jcode take a literal value. Rather than teach two harnesses a new
# syntax, this script owns the two derived copies. A consumer that supports
# `{file:…}` should use the canonical path instead of a generated copy.
set -euo pipefail

CANONICAL="${AR_GATE_TOKEN_FILE:-$HOME/.config/ar/racknerd-gate-token.jwt}"
OPENCODE_JSON="${AR_OPENCODE_JSON:-$HOME/.config/opencode/opencode.json}"
OMP_MODELS="${AR_OMP_MODELS:-$HOME/.omp/agent/models.yml}"
JCODE_ENV="${AR_JCODE_ENV:-$HOME/.jcode/provider-omniroute.env}"
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

  if [ -f "$JCODE_ENV" ]; then
    printf 'OMNIROUTE_API_KEY=%s\n' "$token" > "$JCODE_ENV"
    chmod 600 "$JCODE_ENV"
  fi
}

case "${1:-}" in
  mint)
    [ -x "$AROUTE_BIN" ] || die "no aroute binary at $AROUTE_BIN (set AROUTE_BIN)"
    token="$("$AROUTE_BIN" keys mint --key-id "$KEY_ID" --ttl-days 0 | awk '/^token: /{print $2}')"
    [ -n "$token" ] || die "mint produced no token"
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
    for f in "$OPENCODE_JSON" "$OMP_MODELS" "$JCODE_ENV"; do
      if [ ! -f "$f" ]; then printf '  %-44s (absent)\n' "$f"; continue; fi
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
    printf 'key id    : %s\n' "$KEY_ID"
    ;;
  *)
    sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
