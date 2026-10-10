#!/bin/bash
# omniroute-to-agentgateway.sh — port the OmniRoute stacks to the VPS agentgateway.
#
# SOURCE OF TRUTH: the RUNNING OmniRoute instance (systemd user service, :20129), whose state
# lives in its sqlite database — NOT the aroute config, NOT the stale source checkout.
#   ${OMNIROUTE_DATA_DIR:-~/.local/share/omniroute-data}/storage.sqlite
#     - combos                  — every combo (free-stack, small-stack, paper-stack, ...) with its
#                                 ordered model targets and strategy
#     - provider_connections    — the ACTIVE API-KEY POOL per provider (omniroute rotates these;
#                                 the port fans out one agentgateway entry per key, so the gateway
#                                 inherits the pool as failover breadth). OAuth connections are
#                                 skipped with a warning (the gateway cannot refresh their tokens).
#   BASE table below — upstream baseUrls, verified against the live service responses
#                       (zenmux-free from the omniroute dist itself).
#
# TARGET: <ssh-target>:~/.config/agentgateway/config.yaml
#   llm.models        — one entry per (provider, model, api key), name = provider__model__<sha8(key)>
#   llm.virtualModels — one per combo; targets ordered by combo model order, then key priority.
#   config:, llm.port and llm.policies preserved verbatim. Restarted via `systemctl --user`.
#
# Usage:
#   scripts/omniroute-to-agentgateway.sh [ssh-target] [--dry-run]
#   Default target: racknerd. --dry-run prints the plan and ships nothing.
#
# Safety: backs up the remote config; auto-restores if the gateway rejects the new config.
# Deterministic names → idempotent reruns.

set -euo pipefail
TARGET="racknerd"; DRY=0
for a in "$@"; do case "$a" in --dry-run) DRY=1 ;; *) TARGET="$a" ;; esac; done

DB="${OMNIROUTE_DATA_DIR:-$HOME/.local/share/omniroute-data}/storage.sqlite"
[ -f "$DB" ] || { echo "no omniroute storage at $DB (is the omniroute service running?)" >&2; exit 2; }

WORK="$(mktemp -d)"; chmod 700 "$WORK"; trap 'rm -rf "$WORK"' EXIT
CUR="$WORK/current.yaml"; NEW="$WORK/new.yaml"
scp -q "$TARGET:.config/agentgateway/config.yaml" "$CUR" || { echo "cannot fetch remote config" >&2; exit 2; }

python3 - "$DB" "$CUR" "$NEW" <<'PY' 1>&2
import sys, re, hashlib, sqlite3, yaml
db_path, cur_path, new_path = sys.argv[1], sys.argv[2], sys.argv[3]
db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)

BASE = {
    "nvidia":       "https://integrate.api.nvidia.com/v1",
    "cline":        "https://api.cline.bot/api/v1",
    "kilocode":     "https://api.kilo.ai/api/openrouter/v1",
    "opencode-zen": "https://opencode.ai/zen/v1",
    "openrouter":   "https://openrouter.ai/api/v1",
    "zenmux-free":  "https://zenmux.ai/api/v1",
    "gemini":       "https://generativelanguage.googleapis.com/v1beta/openai/",
}
HEALTH = {
    "unhealthyExpression": "response.code == 401 || response.code == 403 || response.code == 429 || response.code >= 500 || response.code == 404",
    "eviction": {"duration": "30s", "consecutiveFailures": 3},
}
CACHE = {"cacheSystem": True, "cacheMessages": True, "cacheTools": True,
         "minTokens": 1024, "cacheMessageOffset": 0}
def slug(s):
    return re.sub(r"[^a-zA-Z0-9]+", "_", s).strip("_")

# active api-key pool per provider, in omniroute's own priority order.
# api_keys are stored as "enc:v1:<iv>:<ct>:<tag>" envelopes: aes-256-gcm with
# key = scrypt(STORAGE_ENCRYPTION_KEY, "omniroute-field-encryption-v1"). The secret
# lives in the omniroute service .env (same lookup order as omniroute itself).
import hashlib as _hl, os as _os
_secret = None
for _p in (_os.path.join(_os.path.dirname(db_path), ".env"), _os.path.expanduser("~/.hermes/.env")):
    if _os.path.exists(_p):
        for _line in open(_p):
            if _line.startswith("STORAGE_ENCRYPTION_KEY="):
                _secret = _line.split("=", 1)[1].strip().strip('"').strip("'"); break
    if _secret: break
if not _secret:
    print("ABORT: STORAGE_ENCRYPTION_KEY not found (omniroute service .env)"); sys.exit(4)
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
_dk = _hl.scrypt(_secret.encode(), salt=b"omniroute-field-encryption-v1", n=16384, r=8, p=1, dklen=32)
_decrypted = 0
def _dec(env_blob):
    global _decrypted
    if not env_blob.startswith("enc:v1:"): return None
    iv, ct, tag = env_blob[len("enc:v1:"):].split(":")
    _decrypted += 1
    try:
        return AESGCM(_dk).decrypt(bytes.fromhex(iv), bytes.fromhex(ct) + bytes.fromhex(tag), None).decode()
    except Exception:
        return None
pool = {}
oauth_skipped = set()
for provider, auth, key, prio in db.execute("""
        SELECT provider, auth_type, api_key, priority FROM provider_connections
        WHERE is_active = 1 ORDER BY provider, priority"""):
    if auth == "apikey" and key:
        plain = _dec(key)
        if plain:
            pool.setdefault(provider, []).append(plain)
    elif auth == "oauth":
        oauth_skipped.add(provider)
if _decrypted == 0:
    print("ABORT: no envelopes decrypted — wrong secret?"); sys.exit(4)

models, vms, skipped, by_name = [], [], [], {}
for cid, name, so in db.execute("SELECT id, name, sort_order FROM combos ORDER BY sort_order"):
    data = __import__("json").loads(
        db.execute("SELECT data FROM combos WHERE id=?", (cid,)).fetchone()[0])
    names = []
    for m in data.get("models", []):
        target = m["model"]; provider = m.get("providerId") or target.split("/", 1)[0]
        model = target.split("/", 1)[1] if "/" in target else target
        model = re.sub(r":free$", "", model)
        keys = pool.get(provider, [])
        if not keys:
            if provider in oauth_skipped:
                skipped.append(f"{name}: {provider}/{model} — oauth connection only (needs live token refresh, omniroute-only)")
            else:
                skipped.append(f"{name}: {provider}/{model} — no active api-key connection")
            continue
        base = BASE.get(provider)
        if not base:
            skipped.append(f"{name}: {provider}/{model} — no known baseUrl (skipped)"); continue
        for key in keys:
            entry = f"{slug(provider)}__{slug(model)}__{hashlib.sha256(key.encode()).hexdigest()[:8]}"
            if entry not in by_name:
                by_name[entry] = {
                    "name": entry, "visibility": "public", "provider": "openAI",
                    "params": {"model": model, "apiKey": key, "baseUrl": base},
                    "health": HEALTH, "promptCaching": CACHE,
                }
                models.append(by_name[entry])
            names.append(entry)
    if not names:
        print(f"  ! combo {name}: nothing portable — omitted"); continue
    vms.append({"name": name,
                "routing": {"failover": {"targets": [{"model": n, "priority": i} for i, n in enumerate(names)]}}})

if not models:
    print("ABORT: nothing portable found"); sys.exit(3)
cur = yaml.safe_load(open(cur_path))
old_n = len(cur["llm"].get("models", []))
cur["llm"]["models"] = models
cur["llm"]["virtualModels"] = vms
yaml.safe_dump(cur, open(new_path, "w"), sort_keys=False)
print(f"models: {old_n} old -> {len(models)} new ({len(pool)} providers with key pools); virtual models: {len(vms)}")
for p, ks in sorted(pool.items()):
    print(f"  pool {p}: {len(ks)} key(s)")
for v in vms:
    t = v["routing"]["failover"]["targets"]
    print(f"  vm {v['name']}: {len(t)} targets")
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
for M in free-stack small-stack paper-stack; do
  grep -q "name: $M" ~/.config/agentgateway/config.yaml || continue
  R=$(curl -s -m 120 http://127.0.0.1:20129/v1/chat/completions \
      -H 'Content-Type: application/json' -H "Authorization: Bearer $KEY" \
      -d "{\"model\":\"$M\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}],\"max_tokens\":5}")
  if echo "$R" | grep -q '"choices"'; then echo "$M: OK"; else echo "$M: FAIL — $(echo "$R" | head -c 160)"; fi
done
REMOTE
