#!/usr/bin/env python3
"""Keep local LLM lanes in agentic-harness configs synced with aroute's active combos.

Direction: aroute (~/.config/ar/config.yaml `combos`) -> harness configs.
  - adds every aroute combo id missing from each harness's local-gateway provider
  - repairs dead omniroute endpoints (port 20129 -> 20128) so lanes point at the live gateway
  - rewrites omp modelRoles that still prefix `omniroute/` back onto `aroute/` (top-level
    roles only; fallbackChains are left untouched and reported)
  - reports lane ids a harness lists that aroute does not define (e.g. paper-stack);
    it never invents upstream targets for them (define the combo in ar config first)
  - never deletes unless --prune --apply, which removes lane-style ids (no `/` in them)
    that are not aroute combos; direct passthrough ids (`provider/model`) are kept

Default mode is a dry-run report; changes are written only with --apply, and every
modified file gets a `.bak-lanesync` backup first (an older backup is never clobbered).

Harness registry (missing files are skipped with a note):
  opencode  ~/.config/opencode/opencode.json   provider.<id>.models dict
  pi        ~/.pi/agent/models.yml             providers.<id>.models list
  omp       ~/.omp/agent/models.yml + agent/config.yml (modelRoles)
  jcode     ~/.jcode/config.toml               [[providers.<id>.models]] list
  zeroclaw / openclaw / hermes-agent: no local static-model config found -> skipped

Usage:
  scripts/sync-harness-lanes.py                 # dry-run (shows what --apply would do)
  scripts/sync-harness-lanes.py --apply
  scripts/sync-harness-lanes.py --apply --prune
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
from dataclasses import dataclass, field
from pathlib import Path

import yaml

HOME = Path.home()
AR_CONFIG = HOME / ".config/ar/config.yaml"
DEAD_PORT = "20129"  # omniroute was decommissioned 2026-10-16
AR_PORT = "20128"


@dataclass
class Change:
    file: Path
    verb: str  # add | fix | role-fix | prune | plan
    detail: str


@dataclass
class Note:
    file: Path
    detail: str


@dataclass
class Harness:
    name: str
    changes: list = field(default_factory=list)
    notes: list = field(default_factory=list)

    def add(self, path, verb, detail):
        self.changes.append(Change(path, verb, detail))

    def note(self, path, detail):
        self.notes.append(Note(path, detail))


def load_combos() -> dict:
    """Active combo ids -> context_length, from aroute's own config (read-only)."""
    data = yaml.safe_load(AR_CONFIG.read_text())
    out = {}
    for combo in data.get("combos") or []:
        out[combo["id"]] = int(combo.get("context_length") or 0) or 1000000
    return out


def backup(path):
    bak = path.with_suffix(".bak-lanesync")
    if not bak.exists():  # never clobber the first backup
        shutil.copy2(path, bak)


def port_fix(path, text, h):
    """Point gateway URLs still using the dead omniroute port at aroute."""
    pattern = r"(127\.0\.0\.1|localhost):" + DEAD_PORT
    hits = re.findall(pattern, text)
    if not hits:
        return text
    h.add(path, "fix", ":20129 -> :%s on %d URL(s)" % (AR_PORT, len(hits)))
    return re.sub(pattern, r"\1:" + AR_PORT, text)


# ---------------------------------------------------------------- pi / omp models.yml
def yaml_models(path, provider_id, combos, prune, apply, h):
    text = path.read_text()
    lines = text.splitlines()

    b0 = None
    for i, ln in enumerate(lines):
        if ln.rstrip() == "  " + provider_id + ":":
            b0 = i
            break
    if b0 is None:
        h.note(path, "no '%s:' provider block" % provider_id)
        return
    end = len(lines)
    for i in range(b0 + 1, len(lines)):
        if re.match(r"^  \S", lines[i]):
            end = i
            break
    mline = None
    for i in range(b0, end):
        if lines[i].rstrip() == "    models:":
            mline = i
            break
    if mline is None:
        h.note(path, "'%s' block has no static models list" % provider_id)
        return

    ent = re.compile(r"^      - id: (\S+)")
    starts = []
    for i in range(mline + 1, end):
        m = ent.match(lines[i])
        if m:
            starts.append((i, m.group(1)))
    if not starts:
        h.note(path, "models list empty or unusual shape")
        return

    changed = False
    # port fix is scoped to THIS managed provider block: other blocks (e.g. omp's
    # dead `omniroute` marker) keep their 20129 URL so they stay visibly dead
    block_part = "\n".join(lines[b0:end])
    fixed_part = port_fix(path, block_part, h)
    if fixed_part != block_part:
        lines[b0:end] = fixed_part.splitlines()
        changed = True

    def spans(entries):
        out = []
        for k, (i0, lane) in enumerate(entries):
            i1 = entries[k + 1][0] if k + 1 < len(entries) else end
            out.append((i0, i1, lane))
        return out

    changed = False
    if prune:
        for i0, i1, lane in sorted(spans(starts), key=lambda t: -t[0]):
            if "/" in lane or lane in combos:
                continue
            del lines[i0:i1]
            h.add(path, "prune", lane)
            changed = True
        starts = []
        for i in range(mline + 1, end):
            m = ent.match(lines[i])
            if m:
                starts.append((i, m.group(1)))

    present = {lane for (_i, lane) in starts}
    extra = [lane for lane in present if "/" not in lane and lane not in combos]
    if extra:
        h.note(path, "lane(s) aroute does not define: %s - kept (define the combo in ar config, or --prune)" % ", ".join(extra))

    addition = []
    for lane, ctx in combos.items():
        if lane in present:
            continue
        addition.append("      - id: " + lane)
        addition.append("        name: aroute combo " + lane)
        addition.append("        contextWindow: %d" % ctx)
        addition.append("        maxTokens: 32000")
        h.add(path, "add", "%s (context %d)" % (lane, ctx))
    if addition:
        last_start = starts[-1][0]
        anchor = next((i for i in range(last_start + 1, end) if ent.match(lines[i])), end)
        lines[anchor:anchor] = addition
        changed = True

    if not changed:
        h.note(path, "models already synced with aroute combos")
        return
    new = "\n".join(lines) + ("\n" if text.endswith("\n") else "")
    if new == text:
        return
    if apply:
        backup(path)
        path.write_text(new)
    else:
        h.add(path, "plan", "would rewrite the models list")


# ---------------------------------------------------------------- omp config.yml roles
def omp_roles(path, apply, h):
    text = path.read_text()
    new = re.sub(r"^(\s{2}(?:advisor|task|smol|default|slow|web): )omniroute/",
                 r"\1aroute/", text, flags=re.M)
    if new != text:
        n = len(re.findall(r"\baroute/", new, flags=re.M)) - len(re.findall(r"\baroute/", text, flags=re.M))
        if apply:
            backup(path)
            path.write_text(new)
            h.add(path, "role-fix", "%d modelRole(s) omniroute/* -> aroute/*" % n)
        else:
            h.add(path, "plan", "would rewrite %d modelRole(s) omniroute/* -> aroute/*" % n)
    else:
        h.note(path, "top-level modelRoles already aroute-prefixed")
    if re.search(r"^\s{4}.*omniroute/", text, flags=re.M):
        h.note(path, "fallbackChains still name omniroute/* - left as-is (dead-gateway "
                     "passthrough ids; trim manually in config.yml)")


# ---------------------------------------------------------------- jcode config.toml
def jcode(path, combos, prune, apply, h):
    old = path.read_text()
    text = port_fix(path, old, h)
    lines = text.splitlines()
    hdr = re.compile(r"^\[\[providers\.([\w-]+)\.models\]\]$")

    def headers():
        return [i for i, ln in enumerate(lines) if hdr.match(ln)]

    def block(i0):
        hs = headers()
        idx = hs.index(i0)
        bound = hs[idx + 1] if idx + 1 < len(hs) else len(lines)
        stop = next((j for j in range(i0 + 1, bound) if re.match(r"^\[", lines[j])), bound)
        lane = None
        for j in range(i0 + 1, stop):
            m = re.match(r'^id = "(.*)"$', lines[j].strip())
            if m:
                lane = m.group(1)
                break
        return (i0, stop, lane)

    def ranges():
        return [block(i) for i in headers()]

    rg = ranges()
    if not rg:
        h.note(path, "no [[providers.*.models]] blocks")
        return
    pid = hdr.match(lines[rg[0][0]]).group(1)

    changed = False
    if prune:
        for i0, i1, lane in sorted(rg, key=lambda t: -t[0]):
            if lane and "/" not in lane and lane not in combos:
                del lines[i0:i1]
                h.add(path, "prune", lane)
                changed = True
        rg = ranges()

    have = {lane for (_s, _e, lane) in rg}
    missing = [lane for lane in combos if lane not in have]
    extra = [lane for (_s, _e, lane) in rg if lane and "/" not in lane and lane not in combos]
    if extra:
        h.note(path, "lane(s) aroute does not define: %s - kept" % ", ".join(extra))

    if missing:
        addition = []
        for lane in missing:
            addition.append("[[providers.%s.models]]" % pid)
            addition.append('id = "%s"' % lane)
            addition.append("context_window = %d" % combos[lane])
            addition.append("")
            h.add(path, "add", "%s (context %d)" % (lane, combos[lane]))
        stop_last = rg[-1][1]
        lines[stop_last:stop_last] = addition
        changed = True

    if not changed:
        h.note(path, "models already synced with aroute combos")
        return
    new = "\n".join(lines) + ("\n" if old.endswith("\n") else "")
    if new == old:
        return
    if apply:
        backup(path)
        path.write_text(new)
        env = path.parent / ("provider-" + pid + ".env")
        if env.is_file():
            h.note(env, "kept as jcode's provider key source; aroute must accept it "
                        "(test: omni key vs aroute /v1/models)")
    else:
        h.add(path, "plan", "would add %d lane(s): %s" % (len(missing), ", ".join(missing)))


# ---------------------------------------------------------------- opencode.json
def opencode(path, combos, prune, apply, h):
    old = path.read_text()
    data = json.loads(old)
    provs = data.get("provider") or {}
    target = None
    for name, prov in provs.items():
        opts = prov.get("options") or {}
        url = opts.get("baseURL") or ""
        if ":" + DEAD_PORT in url:
            opts["baseURL"] = url.replace(":" + DEAD_PORT, ":" + AR_PORT)
            h.add(path, "fix", "%s baseURL :20129 -> :%s" % (name, AR_PORT))
            old = None  # force rewrite below
        if target is None and AR_PORT in url:
            target = name
    if target is None:
        h.note(path, "no provider pointing at the local gateway")
        return
    models = provs[target].setdefault("models", {})
    added = []
    for lane, ctx in combos.items():
        if lane not in models:
            models[lane] = {"name": "aroute combo " + lane,
                            "limit": {"context": ctx, "output": 32000}}
            added.append(lane)
            h.add(path, "add", "%s (context %d)" % (lane, ctx))
    if prune:
        for lane in [k for k in list(models) if "/" not in k and k not in combos]:
            del models[lane]
            h.add(path, "prune", lane)
    extra = [k for k in models if "/" not in k and k not in combos]
    if extra:
        h.note(path, "lane(s) aroute does not define: %s - kept" % ", ".join(extra))
    new = json.dumps(data, indent=2) + "\n"
    if new == old:
        if not added:
            h.note(path, "models already synced with aroute combos")
        return
    if apply:
        backup(path)
        path.write_text(new)
    else:
        h.add(path, "plan", "would add %d lane(s): %s" % (len(added), ", ".join(added)))


# ---------------------------------------------------------------- driver
def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[1])
    ap.add_argument("--apply", action="store_true", help="write changes (default dry-run)")
    ap.add_argument("--prune", action="store_true",
                    help="with --apply: also drop lane-style ids that are not aroute combos")
    args = ap.parse_args()

    combos = load_combos()
    if not combos:
        print("no active combos found in %s" % AR_CONFIG, file=sys.stderr)
        return 1
    print("aroute active combos: %s\n" % ", ".join(combos))

    harnesses = [Harness("opencode"), Harness("pi"), Harness("omp"), Harness("jcode"),
                 Harness("zeroclaw"), Harness("openclaw"), Harness("hermes-agent")]
    h = {x.name: x for x in harnesses}

    p = HOME / ".config/opencode/opencode.json"
    if p.is_file():
        opencode(p, combos, args.prune, args.apply, h["opencode"])
    else:
        h["opencode"].note(p, "file not found")

    p = HOME / ".pi/agent/models.yml"
    if p.is_file():
        yaml_models(p, "aroute", combos, args.prune, args.apply, h["pi"])
    else:
        h["pi"].note(p, "file not found")

    hs = h["omp"]
    p = HOME / ".omp/agent/models.yml"
    if p.is_file():
        yaml_models(p, "aroute", combos, args.prune, args.apply, hs)
        hs.note(p, "omniroute provider block left untouched (dead gateway marker; "
                   "roles are rewritten below)")
    else:
        hs.note(p, "file not found")
    p = HOME / ".omp/agent/config.yml"
    if p.is_file():
        omp_roles(p, args.apply, hs)
    else:
        hs.note(p, "file not found")

    p = HOME / ".jcode/config.toml"
    if p.is_file():
        jcode(p, combos, args.prune, args.apply, h["jcode"])
    else:
        h["jcode"].note(p, "file not found")

    p = HOME / ".config/zeroclaw"
    if p.is_dir():
        h["zeroclaw"].note(p, "found (ZCode/); its model config is auto-managed (no static "
                              "lane list) - extend this script when a provider file appears")
    else:
        h["zeroclaw"].note(p, "no config dir")
    h["openclaw"].note(HOME / ".openclaw", "no static-model config (skills/plugin data only)")
    h["hermes-agent"].note(HOME / ".hermes/config.yaml", "routes via provider names, not "
                           "lane ids - lane sync not applicable")

    for hx in harnesses:
        if not hx.changes and not hx.notes:
            continue
        print("== %s ==" % hx.name)
        for c in hx.changes:
            print("  [%-8s] %s: %s" % (c.verb, c.file.name, c.detail))
        for n in hx.notes:
            print("  [note   ] %s: %s" % (n.file.name, n.detail))
        print()

    if not args.apply:
        print("dry-run: nothing written. Re-run with --apply to write (.bak-lanesync backups).")
    else:
        print("applied. Harnesses pick up model lists on their next start/reload.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
