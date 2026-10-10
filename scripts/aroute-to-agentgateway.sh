#!/bin/bash
# aroute-to-agentgateway.sh — port the artificial-route model/provider surface to the
# VPS agentgateway: EVERY aroute combo becomes a directly-callable virtual model, plus a
# flat `provider__model` alias per unique target so each provider model is addressable
# on its own. Complements sync-stacks-to-agentgateway.sh (stacks-only provisioning):
# run this one when you want the full aroute catalog on the gateway instead.
#
# Sources of truth (identical to the stacks script):
#   ~/.config/ar/config.yaml       — combos (auto-discovered, all of them), custom_providers
#   ~/.config/ar/ar.env            — credentials (read at runtime, never printed, never copied)
#   ~/.config/ar/providerMeta.json — dialects (cookie-auth providers are not portable; skipped)
#   BASE table below               — standard API baseUrls
# Target:
#   <ssh-target>:~/.config/agentgateway/config.yaml — llm.models + llm.virtualModels replaced;
#   config:, llm.port and llm.policies preserved verbatim. Gateway restarted via `systemctl --user`.
#
# Usage:
#   scripts/sync-models-to-agentgateway.sh [ssh-target] [--dry-run]
#   Default target: racknerd. --dry-run prints the plan and ships nothing.
#
# Notes:
#   - A combo whose targets are all non-portable (e.g. paper-stack → zenmux-free cookie
#     sessions) is omitted with a warning, not fatal.
#   - Deterministic names (sha256 of the api key) → idempotent reruns.

set -euo pipefail
TARGET="racknerd"; DRY=0
for a in "$@"; do case "$a" in --dry-run) DRY=1 ;; *) TARGET="$a" ;; esac; done

AR_DIR="$HOME/.config/ar"
WORK="$(mktemp -d)"; chmod 700 "$WORK"; trap 'rm -rf "$WORK"' EXIT
CUR="$WORK/current.yaml"; NEW="$WORK/new.yaml"

[ -f "$AR_DIR/config.yaml" ] || { echo "no aroute config at $AR_DIR/config.yaml" >&2; exit 2; }
scp -q "$TARGET:.config/agentgateway/config.yaml" "$CUR" || { echo "cannot fetch remote config" >&2; exit 2; }

python3 - "$AR_DIR" "$CUR" "$NEW" <<'PY' 1>&2
import sys, re, hashlib, json, yaml
ar_dir, cur_path, new_path = sys.argv[1], sys.argv[2], sys.argv[3]
cfg = yaml.safe_load(open(f"{ar_dir}/config.yaml"))
env = {}
for line in open(f"{ar_dir}/ar.env"):
    line = line.strip()
    if line and not line.startswith("#") and "=" in line:
        k, v = line.split("=", 1)
        env[k.strip()] = v.strip().strip('"').strip("'")
try:
    meta = json.load(open(f"{ar_dir}/providerMeta.json"))
except OSError:
    meta = {}
keys = cfg.get("keys", {})
def key_for(provider):
    ref = keys.get(provider, "")
    if not ref:
        return None
    return env.get(ref.lstrip("$").strip()) or None
custom = {c["id"]: c for c in (cfg.get("custom_providers") or [])}
BASE = {
    "nvidia":       "https://integrate.api.nvidia.com/v1",
    "cline":        "https://api.cline.bot/api/v1",
    "kilocode":     "https://api.kilo.ai/api/openrouter/v1",
    "opencode-zen": "https://opencode.ai/zen/v1",
    "openrouter":   "https://openrouter.ai/api/v1",
    "gemini":       "https://generativelanguage.googleapis.com/v1beta/openai/",
}
def slug(s):
    return re.sub(r"[^a-zA-Z0-9]+", "_", s).strip("_")
HEALTH = {
    "unhealthyExpression": "response.code == 401 || response.code == 403 || response.code == 429 || response.code >= 500 || response.code == 404",
    "eviction": {"duration": "30s", "consecutiveFailures": 3},
}
CACHE = {"cacheSystem": True, "cacheMessages": True, "cacheTools": True,
         "minTokens": 1024, "cacheMessageOffset": 0}

models, vms, skipped = [], [], []
concrete_by_pair = {}   # (provider, model) -> concrete model name
for c in (cfg.get("combos") or []):
    combo_id = c["id"]
    targets = list(dict.fromkeys((c.get("targets") or []) + (c.get("pool") or [])))
    names = []
    for t in targets:
        provider, _, model = t.partition("/")
        model = re.sub(r":free$", "", model)
        if meta.get(provider, {}).get("authHeader") == "cookie":
            skipped.append(f"{combo_id}: {provider}/{model} — browser-session provider, aroute-only"); continue
        if provider in custom:
            base = custom[provider].get("base_url")
            key = key_for(custom[provider].get("key_ref") or provider)
        else:
            base = BASE.get(provider)
            key = key_for(provider)
        if not key:
            skipped.append(f"{combo_id}: {provider}/{model} — no credential in ar.env"); continue
        if not base:
            skipped.append(f"{combo_id}: {provider}/{model} — no known baseUrl"); continue
        pair = (provider, model)
        if pair not in concrete_by_pair:
            name = f"{slug(provider)}__{slug(model)}__{hashlib.sha256(key.encode()).hexdigest()[:8]}"
            models.append({
                "name": name, "visibility": "public", "provider": "openAI",
                "params": {"model": model, "apiKey": key, "baseUrl": base},
                "health": HEALTH, "promptCaching": CACHE,
            })
            concrete_by_pair[pair] = name
        names.append(concrete_by_pair[pair])
    if not names:
        print(f"  ! combo {combo_id}: no portable targets — omitted")
        continue
    vms.append({"name": combo_id,
                "routing": {"failover": {"targets": [{"model": n, "priority": i} for i, n in enumerate(names)]}}})
for (provider, model), cname in concrete_by_pair.items():
    vms.append({"name": f"{slug(provider)}__{slug(model)}",
                "routing": {"failover": {"targets": [{"model": cname, "priority": 0}]}})

if not models:
    print("ABORT: nothing portable found"); sys.exit(3)
cur = yaml.safe_load(open(cur_path))
old_n = len(cur["llm"].get("models", []))
cur["llm"]["models"] = models
cur["llm"]["virtualModels"] = vms
yaml.safe_dump(cur, open(new_path, "w"), sort_keys=False)
print(f"models: {old_n} old -> {len(models)} new; virtual models: {len(vms)}")
for m in vms:
    t = m["routing"]["failover"]["targets"]
    print(f"  vm: {m['name']} -> {[x['model'] for x in t[:3]]}{'...' if len(t) > 3 else ''}")
for s in skipped:
    print("  ! " + s)
PY

if [ "$DRY" = 1 ]; then echo "(dry-run: nothing shipped)"; exit 0; fi

TS=$(date -u +%Y%m%dT%H%M%SZ)
ssh "$TARGET" "cp ~/.config/agentgateway/config.yaml ~/.config/agentgateway/config.yaml.bak-$TS"
scp -q "$NEW" "$TARGET:.config/agentgateway/config.yaml"
ssh "$TARGET" bash -s "$TS" <<'REMOTE' 1>&2
set -e
export XDG_RUNTIME_DIR=/run/user/$(id -u)
chmod 600 ~/.config/agentgateway/config.yaml
systemctl --user restart agentgateway || true
sleep 4
if ! systemctl --user is-active --quiet agentgateway; then
  cp ~/.config/agentgateway/config.yaml ~/.config/agentgateway/config.yaml.rejected-$1
  cp ~/.config/agentgateway/config.yaml.bak-$1 ~/.config/agentgateway/config.yaml
  systemctl --user restart agentgateway
  echo 'REJECTED: gateway refused the new config; backup restored' >&2
  exit 5
fi
echo 'gateway restarted OK'
RAW=$(grep -m1 -oE 'key: [^ ]+' ~/.config/agentgateway/config.yaml | cut -d' ' -f2)
if [ "${RAW#$}" != "$RAW" ]; then
  KEY=$(sed -n "s/^${RAW#$}=//p" ~/.hermes/.env | head -1)
else
  KEY="$RAW"
fi
[ -n "$KEY" ] || { echo 'verify: cannot resolve client key (check ~/.hermes/.env)' >&2; exit 6; }
echo "model names now: $(curl -s http://127.0.0.1:20129/v1/models -H "Authorization: Bearer $KEY" | grep -o '"id":"[^"]*"' | wc -l)"
for M in small-stack free-stack; do
  R=$(curl -s -m 120 http://127.0.0.1:20129/v1/chat/completions \
      -H 'Content-Type: application/json' -H "Authorization: Bearer $KEY" \
      -d "{\"model\":\"$M\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}],\"max_tokens\":5}")
  if echo "$R" | grep -q '"choices"'; then echo "$M: OK"; else echo "$M: FAIL — $(echo "$R" | head -c 160)"; fi
done
REMOTE
