#!/usr/bin/env bash
# Independent verification of the claims skunk made in its report. Nothing
# here trusts its report: each claim is re-measured from the live binary and
# the live ledger.
set -uo pipefail

BIN=/home/ishanp/Documents/github/my-projects/agentic-utility/proxy/artificial-route/target/release/aroute
CATALOG="$HOME/.codex/aroute-models.json"
PROFILE="$HOME/.codex/aroute.config.toml"
LEDGER="$HOME/.config/ar/usage.sqlite"

fail=0
ok()  { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }

# --- the catalog has 13 routable lanes with distinct priorities 1..13 ------
if [ -f "$CATALOG" ]; then
  ok "the catalog exists at $CATALOG"
else
  bad "the catalog is missing at $CATALOG"
fi

# The catalog carries the 13 aroute lanes plus clones of the bundled Codex
# models, so the *lane* count is what matters, not the raw model count. The
# lanes are the ones the router serves: their priority is 1..13 and their
# slug is one the router knows.
lanes=$(python3 -c "
import json
d=json.load(open('$CATALOG'))
m=d['models']
aroute=[x for x in m if isinstance(x.get('priority'), int) and x['priority'] <= 13]
print(len(aroute))
")
if [ "$lanes" = "13" ]; then
  ok "the catalog carries 13 aroute lanes"
else
  bad "the catalog carries $lanes aroute lanes, not 13"
fi

prios=$(python3 -c "
import json
d=json.load(open('$CATALOG'))
m=d['models']
aroute=sorted([x for x in m if isinstance(x.get('priority'), int) and x['priority'] <= 13], key=lambda x: x['priority'])
print(','.join(str(x['priority']) for x in aroute))
")
if [ "$prios" = "1,2,3,4,5,6,7,8,9,10,11,12,13" ]; then
  ok "the lanes' priorities are 1..13, one per lane"
else
  bad "the lanes' priorities are $prios"
fi

# --- the large-stack lane's window and the hosted-tool flag -----------------
ctx=$(python3 -c "
import json
d=json.load(open('$CATALOG'))
for x in d['models']:
    if x['slug']=='large-stack':
        print(x.get('context_window'))
")
if [ "$ctx" = "1048576" ]; then
  ok "large-stack's context window is 1048576"
else
  bad "large-stack's context window is $ctx"
fi

sw=$(python3 -c "
import json
d=json.load(open('$CATALOG'))
print(sum(1 for x in d['models'] if x.get('supports_search_tool')))
")
if [ "$sw" = "0" ]; then
  ok "no lane claims the hosted search tool"
else
  bad "$sw lanes claim the hosted search tool"
fi

# --- the profile's compact limit is 85% of the raw window ------------------
if [ -f "$PROFILE" ]; then
  ok "the profile exists at $PROFILE"
else
  bad "the profile is missing at $PROFILE"
fi
limit=$(grep -oE 'model_auto_compact_token_limit[[:space:]]*=[[:space:]]*[0-9]+' "$PROFILE" | grep -oE '[0-9]+')
if [ "$limit" = "891289" ]; then
  ok "the profile's compact limit is 891289 (85% of 1048576)"
else
  bad "the profile's compact limit is $limit"
fi
if grep -qE '^web_search[[:space:]]*=[[:space:]]*"disabled"' "$PROFILE"; then
  ok "the profile disables the hosted search tool"
else
  bad "the profile does not disable the hosted search tool"
fi

# --- the base config selects aroute and disables hosted search -------------
BC="$HOME/.codex/config.toml"
if grep -q '^model_provider = "aroute"' "$BC"; then
  ok "the base config selects the aroute provider"
else
  bad "the base config does not select the aroute provider"
fi

# --- the router saw the traffic -------------------------------------------
if [ -f "$LEDGER" ]; then
  rows=$(sqlite3 "$LEDGER" "select count(*) from usage where combo='large-stack';" 2>/dev/null || echo "?")
  if [ "$rows" != "0" ] && [ "$rows" != "?" ]; then
    ok "the ledger has $rows large-stack rows"
  else
    bad "the ledger has no large-stack rows"
  fi
else
  bad "the ledger is missing at $LEDGER"
fi

echo
if [ "$fail" = 0 ]; then echo "CODEX_STATE_CHECK: PASS"; else echo "CODEX_STATE_CHECK: FAIL"; fi
exit $fail
