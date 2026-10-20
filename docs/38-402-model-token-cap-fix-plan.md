# 38 — Fix plan: the 402 `model_token_cap` on `minimax-m3.1-flash`

**Status: Phase 1 APPLIED AND VERIFIED, 2026-10-20 19:30 IST. The 402 is gone.**
The root cause was identified during this investigation. The operational fix (mask the guard
timer, remove the block, restart) landed on the VPS at **19:30:15 / 19:30:35**, verified live
through the public endpoint.

**On attribution — stated plainly, and it matters here.** The fix was **not** run by
this session. It was applied by a session working in the
`/home/ishanp/Documents/github/my-projects/paperclip-zeroclaw-adapter` repo, and that repo's
history records it: commit **`99f89e5`** (19:19:13) *installed* `aroute-limits-guard.py` +
`aroute-limits-guard.timer` as **deliberate fault injection** ("Fault-injection proof on the
live host: the block was dropped by hand, the guard reported 'restoring', and the read view
came back with tokens=6000000000"), and commit **`1bebd7d`** (19:44:51) **disarmed it** after
the operator's ruling (that repo's `docs/kb/ASKS.md` **Q1, ruled 2026-10-20**: *"the deployed
system must hold NO spend budgets or limits… a ceiling that refuses the models a run actually
needs is worse than no ceiling"*). The live actions — units masked, the script replaced by a
no-op stub, `aroute limits clear`, restart — are all recorded there with their measurements.

**What is missing from that record, and what this document adds:** the *plain-language* account
of why a **402** reached the user's model request at all, the code path that produced it
(`routes.rs:2842-2950` → `policy.rs:128-134`), and the two engine gaps that made the armed
block unusually destructive. Neither repo holds all three. **The lesson, once: a config can be
re-armed by something OUTSIDE the application, so the code's write paths are not the whole
attack surface, and a config that silently reverts is a timer or a script, not a mystery.**
**Measured:** 2026-10-20 18:54–19:49 IST (VPS `racknerd`, hostname `racknerd-f59f9fc`).
**Symptom (reproduced live, before the fix):**

```
POST https://aroute.ishanparihar.com/v1/chat/completions
  model: minimax-m3.1-flash   auth: OMNIROUTE_API_KEY
  -> HTTP 402
     {"error":{"type":"billing_error","code":"payment_required",
               "reason":"model_token_cap",
               "message":"every model this key may use has reached its own
                          token ceiling: kilocode/minimax/minimax-m3.1-flash:free"}}
  response headers:
     x-ar-decision: strategy=none
     x-ar-deny-reason: model_token_cap
```

The proxy is **not broken** and there is **no billing problem**. A token ceiling you never
asked for is armed in the deployed config, that model's real spend crossed it, and the
request is refused. The user's reading is correct: *"there is no such requirement to use
this model."*

---

## 0. The one-line cause

`~/.config/ar/config.yaml` on the VPS carries a `limits:` block at **line 932** that caps
`racknerd-harness` at 6 B tokens/24 h with four per-model ceilings, one of which is
`kilocode/minimax/minimax-m3.1-flash:free` at **5 000 000 000**. That model's measured
24 h spend crossed it, so `spent >= cap` is true and the target is dropped from the chain.
The `minimax-m3.1-flash` combo has **exactly one** target, so with that target gone the
chain is empty and the request is refused outright.

**Two arms, and this distinction matters — it was wrong in the first draft of this
document, and a second validation pass corrected it.** The `limits:` block arms a
**key-wide** ceiling *and* four per-model ceilings, and the two produce different 402s:

| Arm | Code | Message | 402 rows in the ledger | Window |
|---|---|---|---|---|
| key-wide (`Limit.tokens`) | `token_cap` | *"this key has spent N tokens, which reaches its M-token ceiling"* | **478** | 2026-10-19 20:43:06 → 2026-10-20 13:43:45 |
| per-target (`Limit.model_tokens`) | `model_token_cap` | *"every model this key may use has reached its own token ceiling: …"* | **9** | 2026-10-20 15:43:04 → 19:30:02 |

**Every one of the 9 per-target 402s is `minimax-m3.1-flash`, and `large-stack` has ZERO
of them.** So the refusal the operator saw, and the refusal this document is about, is the
**per-target** arm firing on a **single-target combo** — not a chain being emptied. The
`large-stack` 402s (170 of them) all carry the **key-wide** wording, the **5 B** ceiling, and
stop at **13:43:45**, which is *before* the per-target block existed (the 15:42 write) and
*before* the guard (19:19). They belong to the earlier incident, not this one.

The per-target figures, measured on the VPS `~/.config/ar/usage.sqlite`:

```sql
-- measured on the VPS, ~/.config/ar/usage.sqlite
SELECT provider||'/'||model, SUM(total_tokens) FROM usage
WHERE key_id='racknerd-harness' AND created_at > now-86400;
kilocode/minimax/minimax-m3.1-flash:free | 5542205019   <-- cap 5000000000  >=  REFUSED
nvidia/z-ai/glm-5.3                       |  395632699   <-- cap  500000000  ok
nvidia/stepfun-ai/step-5-preview           |   94086021   <-- cap  200000000  ok
kilocode/reflection/beam-free              |    7203483   <-- cap  200000000  ok
kilocode/nvidia/nemotron-3.5-super-vl:free |      23519   (no ceiling)       ok
```

**Only `minimax` was over its own per-target ceiling.** The other three capped targets were
well inside theirs. The chain emptied for a different reason: `minimax-m3.1-flash` is a
**one-target** combo (`targets: [kilocode/minimax/minimax-m3.1-flash:free]`), so dropping it
left nothing to fail over to. `large-stack` — which has four targets and three of them
under budget — was **not** refused by this arm at all.

The key-wide arm was over too (**6 039 651 311** against the 6 B key cap), and it runs
**first** in the request path (`routes.rs:806`, before the per-target filter at `:849`), so
on the current numbers it would have refused first had the key ceiling been the binding one.

The decision surface that fires is `Strategy::none` (`x-ar-decision: strategy=none`), which
is the same state this proxy returns for a *non-200 upstream* — so the 402 looks like a
provider failure to every harness. Only a `grep` of the VPS config tells the truth.

---

## 1. How it got armed (the part the previous plan could not explain)

`docs/35-cap-surface-removal-plan.md` enumerated **five** code paths that can write a
`limits:` row and concluded that closing them would close the problem. It also recorded that
the live block's re-appearance was *"not one of the five code paths"* and that the writer
was unknown. **This investigation identifies the writer.**

### 1.1 `aroute-limits-guard.py` — a sixth, out-of-tree writer

```
/home/nerd/.local/bin/aroute-limits-guard.py          written 2026-10-20 19:18
/home/nerd/.config/systemd/user/aroute-limits-guard.service
/home/nerd/.config/systemd/user/aroute-limits-guard.timer   OnUnitActiveSec=5min
```

The script's own docstring states the intent (quoted, not paraphrased):

> the `limits:` block was dropped from `~/.config/ar/config.yaml` twice in one day …
> Both times the effect was the same: the `racknerd-harness` key became **uncapped**,
> spent 5.24e9 tokens against a 5e9 ceiling, and every run in the org failed with
> "The selected model provider rejected the request."
> This **restores** the block if it is missing or has drifted.

**Why it existed — and this is the part that surprises nobody more than it should.** It was
installed at **19:19:13** by a session in the `paperclip-zeroclaw-adapter` repo (commit
`99f89e5`, message *"fix: a guard for the gateway's token limits, not a one-off edit"*) as
**deliberate fault injection**: the same commit says *"Fault-injection proof on the live host:
the block was dropped by hand, the guard reported 'limits missing or drifted — restoring', and
the read view came back with tokens=6000000000 over a 24h window."* In other words, a session
set out to prove the block could be re-armed — and then left it armed.

That made it the **opposite of the ruling of record**, which is explicit and later than the
injection: `paperclip-zeroclaw-adapter` `docs/kb/ASKS.md` **Q1, ruled 2026-10-20** — *the
deployed system must hold **NO** spend budgets or limits; a ceiling that refuses the models a
run actually needs is worse than no ceiling, and token burn is controlled by making prompts
smaller, not by refusing work.* The guard enforces a fixed set of ceilings (`tokens: 6e9`,
`spend_window: 24h`, four `model_tokens` rows) and **re-applied them every 5 minutes regardless
of any operator decision**, so `aroute limits clear` could not stick: the next tick rewrote the
block back. The operator ruling landed at ~19:44 and the guard was disarmed at **19:44:51**.

**Measured, twice, before this plan was written:**

```
19:18:36 python3: limits ok
19:23:36 python3: limits missing or drifted — restoring
19:23:36 python3: restored
```

Between those two ticks the 402 log rows land:
`19:18:17` and `19:18:18` — `402 model_token_cap` in the `failures` table.

### 1.2 It destroys data on every restore (a second defect, same file)

The guard re-serializes the whole file with `yaml.safe_dump(doc, f, sort_keys=False)`:

- **Every YAML comment is destroyed.** The VPS config now has **0** comment lines; the dev
  box's same-generation copy has 1. The `# Generated by 'aroute import'` header is already
  gone. Comments in that file carry operator intent (which key is which, which combo is
  primary). This is silent data loss on a 1094-line production config.
- **A backup file accumulates per restore** (`.bak-limits-guard-<ts>`), so the 18-file
  `config.yaml*` backup zoo grows on every tick.

**Verified benign:** the `steps:` blocks survived (checked directly at `config.yaml:946-960`),
and the combo structure is intact — `safe_dump` is not destroying the tree, only the
comments. That narrows the damage, and it is the reason the config still *works* at all,
which is exactly why the corruption went unnoticed.

This is the same *whole-file rewrite* failure class already recorded in `docs/36` §1.3
(`aroute import` deletes an `oauth:` block) and the same one `docs/35` §3 flagged for the
CLI's `splice_section`.

### 1.3 Why it beats every removal

`docs/35` assumed all writers were in the tree. This one is not — it is a root-owned-by-`nerd`
systemd timer with no entry in the repo, no entry in `AR-*` automation, and **no mention in
any doc** (`grep -rn "limits-guard" --include=*.md` returns nothing). Nothing in the tree can
disable it, and nothing in the tree will ever detect it.

---

## 2. The engine gaps that make this block unusually destructive

These are pre-existing and recorded, and they shape how the incident reads — but the first
draft of this document claimed they produced a *"total inability to use any of the four main
models"*, and **a second validation pass found that overstated**. Measured: the per-target
arm refused **only** `minimax-m3.1-flash` (9 rows). `large-stack` — which has four targets,
three of them under budget — was never refused by it. All verified against the running
`aroute 0.1.20` binary and the VPS's own ledger.

### 2.1 `model_tokens` applies to **every** model the key requests (not just that model)

`Policy::target_cap` (`crates/ar-server/src/policy.rs:128-134`) reads
`limits.keys[key].model_tokens[provider/model]` with **no request-time model scoping**.
`Limits::for_request` — the scope-filtering accessor (`crates/ar-config/src/lib.rs:1172-1178`)
— has **no production caller**; the policy layer uses unscoped accessors (`for_key` at
`policy.rs:85`, `row_for` at `:104`/`:130`/`:142`/`:151`). Recorded in `docs/35` §2.2 and
Ask **Q16** Gap 1, still open.

### 2.2 A chain whose every target is capped is refused with no fallback

`filter_by_target_budget` (`routes.rs:2842-2870`) drops each target whose own ceiling is
spent; when nothing survives, `budget_filtered_candidates` returns **402** rather than
letting the request reach an uncapped sibling.

**This gap did not fire in the incident, and the first draft of this document said it did.**
Measured: `large-stack` has **zero** per-target 402s. Its three sibling targets were all
under their own ceilings (GLM 395 M of 500 M, step-5 94 M of 200 M, beam-free 7 M of
200 M), so the chain would have failed over to one of them. What actually refused
`minimax-m3.1-flash` is that the combo has **one** target and it was over its ceiling — the
"no fallback" case, but for a target set of size one.

The gap is still real and still worth fixing: a combo whose every target is individually
capped **is** refused outright, and the refusal is terminal rather than a fail-over to an
uncapped sibling elsewhere in the config. The correct statement is narrower than the one
this document first made.

### 2.3 The refusal is indistinguishable from a provider outage

The 402 carries `strategy=none` and no combo resolution. A harness sees the same shape as a
504 upstream timeout, which dominates the `failures` table (multiple 504s in the minutes
around the incident). This is why "no such requirement to use this model" is the right
operator intuition — nothing in the response says *your VPS has a budget running out*.

### 2.4 The media routes are never policed (uneven enforcement)

`crates/ar-server/src/media.rs` (`json_route_inner`, `:227-276`) calls `authorize`,
`require_json` and lane `admit` but **no** `rate_wait`, `key_cap` or `target_cap`. Family
registered at `app.rs:640-653` (`/v1/embeddings`, `/v1/audio/*`, `/v1/images/generations`,
`/v1/ocr`). Recorded in `docs/35` §2.2 and Ask **Q16** Gap 2. Consequence: the same armed
config caps chat but not media, so the ceiling is not even a reliable statement of spend.

---

## 3. What is affected (blast radius, measured)

| Item | State |
|---|---|
| `racknerd-harness` key | the only key with a `limits:` row; its key-wide ceiling **and** its four per-model ceilings were all live |
| `minimax-m3.1-flash` (combo alias) | **402** — the **per-target** arm, `model_token_cap`, 9 rows 15:43:04 → 19:30:02. A one-target combo whose only target was over its ceiling |
| `large-stack` combo | **200** under the per-target block — 3 of its 4 targets were under budget and it would have failed over. Its 170 recorded 402s are the **key-wide** arm (`token_cap`, 5 B ceiling) and all stop at **13:43:45**, before the per-target block existed |
| `small-stack`, `glm-5.3` | **200** — unaffected |
| key-wide arm | **over** — 6 039 651 311 against the 6 B key cap. It runs **first** (`routes.rs:806`), so on the current numbers it is the arm that would refuse |
| `/v1/*` media family | uncapped by design (§2.4) |
| `aroute` binary on VPS | `0.1.20` — **same as repo (`git tag v0.1.20`). No version skew. The repo is not the cause.** |
| repo `config.yaml`, dev-box `~/.config/ar/config.yaml` | **no `limits:` block** — clean, the code is fine |

The engine behaved exactly as designed against exactly the config it was given. **Nothing
in the Rust tree needs fixing for this incident.** The fix is operational.

---

## 4. Fix plan

### Phase 1 — Stop the 402 (immediate, reversible)

**APPLIED AND VERIFIED, 2026-10-20 19:30 IST** by a concurrent host-side session (see the
attribution note at the top). The sequence ran in exactly this order, and each step's outcome
was measured.

| # | Action | Result (measured) |
|---|---|---|
| 1 | **Disarm the guard timer** | both `~/.config/systemd/user/aroute-limits-guard.timer` and its `.service` are now **symlinks to `/dev/null`** (written 19:34) — that is what `systemctl --user mask` does, and a masked unit **cannot run even if invoked directly**, so it also blocks a manual `systemctl --user start aroute-limits-guard.service`. Stronger than `disable` |
| 2 | **Neutralise the script** | `~/.local/bin/aroute-limits-guard.py` replaced by a **609-byte no-op stub** (19:44) whose docstring reads *"DISARMED 2026-10-20 by the operator ruling… This stub never writes"*; the **original guard preserved verbatim** as `aroute-limits-guard.py.disabled` (19:24) and in the evidence bundle |
| 3 | **Remove the block** | config rewritten at **19:30:15**; `grep -n "^limits:"` → **no match**. The removal was **surgical**: a diff against the pre-write copy is exactly **11 lines** (`1085,1094d1084` — the 10-line `limits:` block, nothing else). Pre-write content preserved as `config.yaml.1` |
| 4 | **Restart** | `aroute.service` stopped 19:30:23, **started 19:30:35**; boot log `aroute 0.1.20 listening on http://127.0.0.1:20128 (6 provider(s), 21 combo(s))` |
| 5 | **Verify with the recorded probe** | `POST /v1/chat/completions minimax-m3.1-flash` → **HTTP 200**, a real completion (`"content":"ok"`), with `x-ar-decision: strategy=priority;outcome=ok;provider=kilocode;model=minimax/minimax-m3.1-flash:free;attempts=1` and **no** `x-ar-deny-reason` header. `aroute limits` now reports `count: 1 of 1 total; default row: rpm=- usd_micros=- tokens=- refuse_unpriced=false model=- model_tokens=- spend_window=-` — **no spend arms anywhere** |
| 6 | **Verify the guard does not return** | at **19:49:28** — 19 minutes later, past three+ 5-minute tick windows — `grep -c "^limits:"` → **0**, sha `0cf8fffb…` unchanged from the 19:30:15 write, and `journalctl -u aroute-limits-guard --since 19:30` → **No entries**. The newest 402 of any kind is **19:30:02**, 13 seconds before the final clear, with **none since**. **The removal holds.** |

**Implementation note.** The units are **masked** (symlinks to `/dev/null`), which is
stronger than `disable`: a masked unit cannot run even when invoked directly. The script
itself was also replaced by a no-op stub, so **neither the timer nor a manual invocation can
write the config** — belt and braces, because a masked unit still protects against a timer
but not against someone re-creating the unit file.

> **One correction to the plan's own instruction.** The table below says "disarm the
> timer first", and that is still the right instruction. But the live sequence ran it the
> other way — the block was removed at **19:30:15** and the units masked at **19:34** — and
> the removal *still* held, because the guard happened not to fire in that 3½-minute gap.
> That was **luck, not procedure**: the removal landed 13 s after a tick, and a tick comes
> every 5 minutes, so a removal at any other point in the window would have been undone
> within five minutes. **Disarm the timer first** — the live evidence does not weaken the
> instruction, it just shows the guard's cadence left one gap.
>
> **Preserved, not deleted.** The original guard is kept verbatim as
> `~/.local/bin/aroute-limits-guard.py.disabled` and in
> `~/.config/ar/evidence-20261020-limits-restore/guard/`, and the pre-change configs are
> preserved there too (the as-found file, both bracketing backups, and the staged
> `/tmp/artest*` copies with a `forensics.txt`). That is the right final state: the intent
> behind the guard (making 24 h spend visible) is legitimate, and Phase 2 replaces the
> *enforcement* with a *report*.

### Phase 2 — Make the situation durable

The guard timer is **masked**, so the 402 cannot return from it. These are the steps that keep
it from ever happening again.

| # | Action | Rationale |
|---|---|---|
| 6 | **Leave `aroute-limits-guard.py` masked, not deleted** | Its intent (the 24 h spend being visible) is legitimate. Masked-but-present means the operator can re-arm deliberately with `systemctl --user unmask aroute-limits-guard.timer` |
| 7 | **Replace it with a report-only check** | a script (or a cron) that reads `usage.sqlite` and warns when a target's 24 h spend exceeds a threshold. Writes **no config**. Pre-empts the confusion that spawned the guard, without re-arming ceilings |
| 8 | **Rename the guard's units** | `aroute-limits-guard` is dangerously close in name to the systemd drop-in `aroute.service.d/limits.conf`, which is **cgroup `MemoryMax=`/`MemoryHigh=` only** and has nothing to do with spend. That collision is how the file went unnoticed for a day |

### Phase 3 — The code half: a silent ceiling is the real defect

These are the `docs/35` CAP rows, now **re-scoped**: the incident's live writer was a
sixth path that CAP-01–05 did not cover, so the bookkeeping is what's wrong, not the list.
None of them is needed to keep the 402 away — Phase 1 did that. They are what stops the next
one from being impossible to diagnose.

| Row | Change | File | Priority |
|---|---|---|---|
| **CAP-06** (new) | **An out-of-tree writer is undetectable.** Add `aroute doctor`-level reporting: "this config carries N spend ceilings", and a startup `tracing::warn!` naming every armed ceiling with its key and window. A silent ceiling is the actual defect here | `ar-cli/src/doctor*`, `ar-server/src/app.rs` boot path | **P1** |
| CAP-01 | Apply `Limits::for_request` through the policy layer so a `model:`-scoped row cannot govern unrequested models | `ar-server/src/policy.rs:85,104,130,142,151` | P2 |
| CAP-02 | Bring media routes under the same arms | `ar-server/src/media.rs:227-276` | P2 |
| CAP-03 | Make the 402 self-describing: name the window, the spend and the ceiling, and say it is config-side | `ar-server/src/routes.rs:2876-2882` | P2 |
| CAP-04 | Emit `strategy=capped_by_budget` (not `none`) when this path refuses | `routes.rs` decision stamping | P3 |
| CAP-05 | Delete the panel's mint-`rpm` writer (the one implicit writer that is in-tree) | `webui.rs / app.js` | P3 |

### Phase 4 — Registry bookkeeping — **DONE**

- `docs/28-gap-ledger.md`: row **D14** added for the sixth, out-of-tree write path.
- `TODO.md`: row **CAP-06** added.
- `docs/kb/ASKS.md` Q15: still open — it should be answered convergently with §1 here (the
  live writer is not in the tree, so "which of the five survive" was never the whole question).
- `docs/handoff/STATE.md`: the "open and dangerous" block now carries the identified writer.

---

## 5. Answer to "why is there no such requirement to use this model?"

Directly: **there is none, and the request is not being billed.** `402` +
`code: payment_required` is this proxy's spelling for "a spend ceiling in `~/.config/ar/config.yaml`
says stop", not for an upstream billing error. The `minimax-m3.1-flash` model costs nothing
and is not the problem; it is simply the first chain target the ceiling filter found over
budget, so it is the one that gets named in the message. The requirement existed only in the
VPS config, was never an operator decision, and was being re-applied every five minutes by
`aroute-limits-guard.timer` — which is why removing it by hand kept appearing to fail.

---

## 6. Rollback and safety

- Phase 1 is fully reversible: restore the `limits:` block and `systemctl --user unmask
  aroute-limits-guard.timer`, then restart. The pre-change file is preserved as
  `config.yaml.1` (19:30:15) and the two `.bak-limits-guard-*` backups.
- No upstream provider key or credential is touched. No `usage.sqlite` row is touched
  (the `usage` table is append-only by trigger).
- The harness key is unaffected — it remains usable on `small-stack` / `glm-5.3` throughout.
