#!/usr/bin/env bash
# Reproduces Codex's plugin-content fingerprint over the omo bundle, and
# shows which entries make it fail.
#
# Codex's `plugin_content_hash` walks the plugin root with `symlink_metadata`
# and rejects anything that is neither a regular file nor a directory. A
# symlink is neither, so the walk fails and the hook is skipped.
#
# This script does the same walk, so the failure is measured rather than
# assumed, and it names every entry that would stop it.
set -uo pipefail

# The plugin's own version is not pinned here: the bundle is replaced on
# every upgrade, and a check that names a version stops being a check.
DEFAULT_ROOT="$HOME/.codex/plugins/cache/sisyphuslabs/omo"
if [ "$#" -ge 1 ]; then
  ROOT="$1"
elif [ -d "$DEFAULT_ROOT" ]; then
  ROOT=$(find "$DEFAULT_ROOT" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -1)
else
  ROOT="$DEFAULT_ROOT"
fi

fail=0
ok()  { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }

if [ ! -d "$ROOT" ]; then
  bad "the plugin root is missing: $ROOT"
  exit 1
fi

# The same walk Codex does: symlink_metadata, and anything that is not a
# directory or a regular file stops it.
python3 - "$ROOT" <<'PY'
import os, sys

root = sys.argv[1]
pending = [root]
seen = set()
offenders = []
broken = []
count = 0

while pending:
    path = pending.pop()
    if path in seen:
        continue
    seen.add(path)
    try:
        st = os.lstat(path)
    except OSError as err:
        offenders.append((path, f"unreadable: {err}"))
        continue
    import stat as statmod
    if statmod.S_ISDIR(st.st_mode):
        try:
            entries = sorted(os.listdir(path))
        except OSError as err:
            offenders.append((path, f"unreadable dir: {err}"))
            continue
        for name in entries:
            pending.append(os.path.join(path, name))
    elif statmod.S_ISREG(st.st_mode):
        count += 1
    else:
        kind = "symlink" if statmod.S_ISLNK(st.st_mode) else "other"
        target = ""
        dangling = False
        if kind == "symlink":
            target = os.readlink(path)
            dangling = not os.path.exists(path)
        offenders.append((path, f"{kind}{' (DANGLING)' if dangling else ''} -> {target}"))

print(f"regular files walked: {count}")
print(f"entries Codex rejects: {len(offenders)}")
for path, why in sorted(offenders):
    print(f"  REJECT {path}  [{why}]")
PY

# The fingerprint's own failure, expressed as Codex would.
if python3 - "$ROOT" <<'PY'
import os, stat as statmod, sys
root = sys.argv[1]
pending = [root]
seen = set()
while pending:
    path = pending.pop()
    if path in seen:
        continue
    seen.add(path)
    try:
        st = os.lstat(path)
    except OSError:
        sys.exit(1)
    if statmod.S_ISDIR(st.st_mode):
        for name in sorted(os.listdir(path)):
            pending.append(os.path.join(path, name))
    elif statmod.S_ISREG(st.st_mode):
        continue
    else:
        print(f"unsupported plugin entry type: {path}")
        sys.exit(1)
sys.exit(0)
PY
then
  ok "Codex's fingerprint walk completes over the bundle"
else
  bad "Codex's fingerprint walk fails over the bundle, so every hook in it is skipped"
fi

echo
if [ "$fail" = 0 ]; then echo "PLUGIN_FINGERPRINT_CHECK: PASS"; else echo "PLUGIN_FINGERPRINT_CHECK: FAIL"; fi
exit $fail
