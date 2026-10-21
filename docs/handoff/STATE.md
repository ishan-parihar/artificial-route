# State

Current snapshot only (no history: that is the worklog). The first `## ` block is
what `genome wake` shows.

## State — 2026-10-21

Date: 2026-10-21. Branch `feat/per-model-token-budgets` @ `15f027c` (local `main` is
`ad233e7`; `origin/main` is `15f027c`). The three plans were committed at `638a284`.
**19 commits after `v0.1.20` are unreleased** — all 19 are already on `origin/main`,
none are unpushed (`git rev-list --count origin/main..HEAD` = 0; measure with
`git rev-list --count`). The proxy runs **only** on the VPS (`racknerd`), public at
`https://aroute.ishanparihar.com/v1` behind a cloudflared tunnel; the gate is armed
and every harness holds a non-expiring token.

**What is deployed.** `aroute 0.1.20` on `racknerd`, installed from the **published
release asset** — never a locally built binary (the host's glibc is 2.36 and a glibc
build fails there). Verified on the box: `aroute --version` → `aroute 0.1.20`;
`file -b ~/.local/bin/aroute` → `ELF 64-bit LSB pie executable, x86-64, … static-pie
linked, stripped`. The unit is `aroute.service` (systemd `--user`), config
`~/.config/ar/config.yaml`, env `ar.env`. `GET https://aroute.ishanparihar.com/ui/`
→ **404**: the tunnel's `^/ui(/.*)?$` → `http_status:404` rule is live, applied by
the cloudflared restart at **13:54:59 IST** (the tunnel is a root system unit and
reads its ingress at startup). The panel is reached over an SSH port-forward —
`ssh -L 20129:127.0.0.1:20128 racknerd`, then `http://127.0.0.1:20129/ui/?token=<jwt>`
with the token from `~/.config/ar/racknerd-gate-token.jwt` on the VPS. **Use a local
port other than 20128**: a local aroute **v0.1.17** still listens on `127.0.0.1:20128`
on the dev box and answers first. The panel's own gate is armed: an anonymous
`POST http://127.0.0.1:20128/ui/api/keys/mint` → **401** `credential_rejected`.

**The 2026-10-21 public outage — measured, and it RECOVERED.** A ~15-minute total
outage: `2026-10-20T23:59:27Z` cloudflared logged `Failed to refresh DNS … i/o
timeout`; `00:29–00:31Z` the VPS **refused every TCP port with RST** (25897, 22, 80,
443, 8080 and a 17-port scan — the host's own sshd was not accepting) while **ICMP
still answered** (rtt ~267 ms), so the guest kernel was up; and every public path
answered **530 / Cloudflare 1033** (v4 and v6 alike). The tunnel was restarted at
**`00:39:13Z`** and re-registered 4 conns — its precheck passed, including "Cloudflare
API is reachable", so the credentials were never invalid — and by `00:45:20Z`
`/healthz` → **200**, `/v1/models` → **401** (the gate), `/ui/` → **404** (the ingress
rule live again). **aroute was never down**: `NRestarts=0` and `loopback=200` in every
watchdog check during the window, including the degraded ones — the watchdog is what
recorded the degradation and the recovery. Cause, ranked: a **provider/network-side
fault** on the VPS (the DNS timeout, then every listener refusing, then an unclean
journald resume with an `oom_reaper` reap on the host's 00:12 reboot) plus the
**cloudflared tunnel losing its connections**; a guest VM stall explains the RST-only
behaviour but not the DNS timeout. RULED OUT: an aroute crash, lost tunnel credentials.
`[UNKNOWN]` the exact reason the guest TCP stack refused every port for ~15 minutes.
**A provider-side network incident report is warranted for 2026-10-20 23:45Z –
2026-10-21 00:40Z** — the cause could not be determined from inside the host (no
sudo, no provider console). Ledger row **C6** carries the full record.

**What was just done — the 402 incident, its cause and its closure.** The 402 was
**aroute's OWN refusal, not a provider's**: the prose is `ar-tokens`'s
`DenyReason::TokenCap` verbatim, the `failures` row carries `provider = '-'` (the
sentinel for "no provider was tried"), and the code is aroute's own
`payment_required`/`token_cap` envelope. **Three defects, all in the ceiling's axis
and its consequences.** (1) **Counted on the wrong axis** — `Ledger::admit_with`
compared the ceiling against `spend(key_id)`, the sum over *every* model the key ever
served, so a `model:`-scoped row still spent against the whole key's history;
`kilocode/minimax/minimax-m3.1-flash:free` held **4 841 787 875 of the key's
5 002 339 805 tokens — 96.8 %** — and GLM, step-5 and beam-free were collateral
damage, all refused at once. (2) **Terminal rather than failing over** — the 402 arm
ran against `plan.chain.first()` only, so a chain whose winner had exhausted its own
budget was refused outright even when a fallback had budget left. (3)
**Unrecoverable** — every arm was cumulative against an append-only ledger, so a key
that had spent 5 B was refused *forever* until someone hand-edited `config.yaml`.

**The three fixes, live in v0.1.20.** `Limit.model_tokens` gives each
`provider/model` its own ceiling measured against **that model's** spend;
`Limit.spend_window: 24h` makes the arms **rolling rates**, so a blown key recovers
without a hand-edit; and the 402 arm now runs **per target** — an over-budget target is
**dropped from the chain**, so the request continues on a **sibling** (only a chain
with nothing left is refused, and the 402 names the models, `reason: model_token_cap`,
header `x-ar-deny-reason: model_token_cap`).

**The measurement (2026-10-20).** The failure log carries **24** 402s in the 30
minutes before the **13:44:34 IST** restart — all of them the old key-wide shape —
and **0** of that old shape after it, the newest 402 of the old shape being
**13:43:45 IST, 49 seconds before the new code took over**. That "0" does **not**
mean "no 402s after": there are **9** post-restart 402s of a **different** shape —
the new per-target `model_token_cap` form ("every model this key may use has
reached its own token ceiling") — at **15:43:04**, **17:52:50**, **18:18:56**,
**18:37:33**, **18:51:48**, **19:18:17**, **19:18:18**, **19:27:34** and
**19:30:02 IST**. Every one of them falls inside a window in which the `limits:`
block was armed again; the **last**, **19:30:02 IST**, is **13 seconds before the
final clear**, and there has been **none since** (re-measured 19:5x IST). The old
arm is gone; the new arm fired **because a `limits:` block was armed again** — the
next paragraph names the writer and says when it took over.
(The figure "18" in
`docs/33` and the ledger is the same window read at 13:49, so it counted 30
minutes ending *after* the restart; the boundary is the same, the window was not
anchored to the restart.) The live failover probe
passed: a target's per-target ceiling set below its current spend served the request
from a **sibling** — `model_tokens["nvidia/z-ai/glm-5.3"]` set to `1` against a spend
already past it was answered `x-ar-decision: …;provider=kilocode;model=minimax/minimax-m3.1-flash:free`
rather than refused (`docs/33` step 8). `large-stack` was **never** refused by the
per-target arm — it holds **zero** of the 9 (`docs/38` §0): it would have failed over.

**Resolved — the `limits:` block is GONE, and the writer that kept putting it back
is identified and disarmed (measured 2026-10-20).** The operator ruled on 2026-10-20
that the deployed system must hold **no** spend budgets or limits; the audit that
established this is `docs/34-spend-cap-audit.md` and the plan for the paths that
could put it back is `docs/35-cap-surface-removal-plan.md` (the audit found the
block was an **audit probe's own `tokens: 1` fixture**, later inflated to 5 B and
6 B and never an operator decision). The block was removed **once** at ~13:06
with the same verb (`aroute limits clear`; that pre-change file is kept at
`~/.config/ar/config.yaml.bak-20261020-limits`, and the block it removed carried
a bare `tokens: 5000000000`). It **came back** by a whole-file write at
**15:42:40** and was measured **armed** at **15:42:40**, **19:07:58**, **19:13:59**
and **19:23:36 IST**, carrying `tokens: 6000000000`, `spend_window: 24h` and four
`model_tokens` ceilings (minimax 5 B, GLM 500 M, step-5 200 M, beam-free 200 M)
and **enforcing 402s**: a live `POST /v1/chat/completions` for
`minimax-m3.1-flash` through the public endpoint answered **402**
`model_token_cap` ("every model this key may use has reached its own token
ceiling: kilocode/minimax/minimax-m3.1-flash:free"), with the 24 h spend at the
ceiling (minimax **5 198 861 995** against 5 B, the key **5 582 699 399** against
6 B). **It is now removed, and the writer that kept putting it back is
disarmed.** The final clear was the same product verb — `aroute limits clear
--config ~/.config/ar/config.yaml` → `limits: block removed`, exit 0 — and **no
YAML text edit was ever made**; the diff after the removal was **11 lines**
(`932,941d931` — the 10-line `limits:` block deleted, nothing else changed). The writer was a **5-minute systemd `--user`
timer** `aroute-limits-guard.timer` (enabled) running
`~/.local/bin/aroute-limits-guard.py`, which did a whole-file `yaml.safe_load` →
inject `limits.keys.racknerd-harness` → `yaml.safe_dump` → `os.replace`; that
whole-file rewrite is exactly why the block always reappeared with **unquoted**
`model_tokens` and the arm order `tokens, spend_window, model_tokens`. It was
disabled with `systemctl --user disable --now aroute-limits-guard.timer`, then
**both units masked** (symlinks to `/dev/null` — the mask fails while the unit
file exists, so the files were deleted first), the VPS script replaced by a
**no-op stub** (the original preserved verbatim as
`aroute-limits-guard.py.disabled`), and the **source repo disarmed** so a later
`scp` cannot reintroduce it. The forensic record is preserved on the VPS at
`~/.config/ar/evidence-20261020-limits-restore/` — the as-found config, the
backups, the guard's original script and units, the guard's unit journal, the
SSH sessions, and the staged `/tmp/artest*` copies. **The model that was being
refused now works**: one request through the public endpoint → **HTTP/2 200**,
`x-ar-decision: strategy=priority;outcome=ok;provider=kilocode;model=minimax/minimax-m3.1-flash:free;attempts=1`,
no `x-ar-deny-reason`, and a real completion in the body. The **newest 402 of
any kind is 19:30:02 IST**, 13 seconds before the final clear, and there has
been **none since**; 749 successful usage rows in the last hour, 75 049 855
tokens, all successful. The config's mtime is **2026-10-20 19:30:15 IST** and
was **unchanged through 19:49:28** — 19 minutes later, past three-plus of the
guard's former 5-minute tick windows — so the fix holds. `aroute limits` now reports `count: 1 of 1 total; default row:
rpm=- usd_micros=- tokens=- refuse_unpriced=false model=- model_tokens=-
spend_window=-` — **no spend arms anywhere**.

**How it got back, measured — the writer is named, and it is still NOT one of the
five code paths.** The 15:42:40 file was a **whole-file PyYAML re-serialization**,
not an aroute splice: reconstructing it with `yaml.safe_dump` over the 15:38 file
plus a `limits:` block inserted before `combos:` reproduces the live file
**byte-for-byte** (both `sha256 2b00c7d5…`), and a whole-file diff of the 15:38
file against the live one runs **1 525 lines across 5 hunks** (`diff -U0`) — the
signature of a re-serialization, not of a surgical edit. Three fingerprints
prove it was not aroute: aroute's own `import::limits::render`
(`crates/ar-cli/src/import/limits.rs:58-112`) emits `model_tokens` targets
**double-quoted** and in the order `tokens, model_tokens, spend_window` — the live
file has them **unquoted** and ordered `tokens, spend_window, model_tokens`;
aroute's `render_combos_section` (`import/mod.rs:449-503`) emits `  - id:`
(indented) while the live file has `- id:` at column 0; and the
`# Generated by 'aroute import'` header is gone. The **content** came from **the
dev box's `~/.config/ar/config.yaml`** — the VPS's 15:38 file is **byte-identical**
to it (`sha256 a407596c…`) — but the **write was staged on the VPS, not fetched
to it**: `/tmp/artest/config.yaml` and `/tmp/artest2/config.yaml` (mtimes
**15:39:49** and **15:40:09 IST**) hold a `yaml.safe_dump` re-serialization of
that file with a trailing `limits:` block. **Two writers, and they must not be
confused.** (1) The **15:42:40** write was that hand-run PyYAML rewrite — staged
on the VPS, not fetched to it — and it pre-dates the timer. (2) The
**`aroute-limits-guard.timer`** unit then ran `~/.local/bin/aroute-limits-guard.py`
every five minutes; its own unit journal records `limits missing or drifted —
restoring` at **19:23:36 IST**, and it is the writer that kept re-arming the
block after it was installed at **19:19:13** (commit `99f89e5` in
`/home/ishanp/Documents/github/my-projects/paperclip-zeroclaw-adapter`) as a
deliberate fault-injection attempt to prove the block would be restored — and it
succeeded. The three earlier re-arms (**15:42:40**, **19:07:58**, **19:13:59**)
all pre-date that install, so the timer cannot be their writer; they are the
same whole-file rewrite run by hand, and the `/tmp/artest*` copies are the
surviving staged artifacts of it. The fourth (**19:23:36**) is the timer's own
journal entry, and it post-dates the install. **The lesson, once: a config can be
re-armed by something OUTSIDE the application, so the code's write paths are not
the whole attack surface, and a config that silently reverts is a timer or a
script, not a mystery.** The five `docs/35` write paths are still real work, but
they are not what happened here — and closing them would not have prevented it.
The backups that bracket the write are `config.yaml.bak-20261020-restore-limits`
(15:38, no `limits:`) and the 15:42:23 pre-write copy of `config.yaml.1` (no
`limits:`), preserved together with the `/tmp/artest*` staged copies and the
as-found file in `~/.config/ar/evidence-20261020-limits-restore/`.

**Also open and dangerous.**
- **The CLI has a latent DATA-LOSS bug** — `is_section_header`
  (`crates/ar-cli/src/commands.rs:2279-2285`) accepts only lowercase letters, digits
  and `_`, so `splice_section`/`remove_section` **silently DELETE** a top-level YAML
  section whose name contains a hyphen (or an uppercase letter): the skip that finds
  the *end* of the section never terminates, so the section and everything under it are
  swallowed. Reproduced 2026-10-20: a config whose `api-keys:` section follows a
  `limits:` section loses it entirely after `aroute limits clear`. **LATENT, not
  live** — no hyphenated top-level section exists in the repo's `config.yaml` (its
  top-level keys are `server`, `keys`, `providers`, `combos` — `config.yaml:15`, `:21`,
  `:28`, `:95`) nor in the VPS's, and `ar_config::Config` does **not**
  `deny_unknown_fields`, so one CAN be added. It rides **every** splicing write path
  (CLI `commands.rs:808`, `:903`, `:905`, `:911`, `:2098`, `:2100`, `:2150`, `:2418`,
  `:2419`; the panel's ten splice sites in `webui.rs`). **The fix is ONE predicate**
  and it is the **first thing the executor should do** — it is the only finding that
  destroys data silently, and every later write path inherits it.
- **The `model:` scope is NOT applied at request time** — `Limits::for_request`
  (`crates/ar-config/src/lib.rs:1172-1178`) has no production caller; the policy layer
  uses the unscoped accessors (`policy.rs:85` `for_key`; `:104`, `:130`, `:142`, `:151`
  `row_for`). A row scoped to `model: jev` therefore applies to **every** model that
  key asks for. Recorded in `docs/35` §2.2 — a separate issue from the panel/limits
  work, and **it did not contribute to the 402**: neither version of the live config
  carried a `model:` scope at all, so there was no scope to mis-apply (`docs/38` §2.1).
- **The compiled web dashboard ships its GATEWAY tier OPEN** — `REQUIRE_API_KEY` has
  `defaultValue:"false"` in the compiled bundle, and the gateway policy falls through
  to `allow({kind:"anonymous",id:"local"})` when the flag is off, so an anonymous
  completion is admitted on `/api/v1/*` and the rewritten families. Publishing it
  would publish an **unledgered spend surface** over the operator's provider keys —
  outside aroute's gate, outside its ledger, outside its `limits:`. Its *management*
  tier is gated. **This is the finding that decides the dashboard plan.**
- **`aroute mcp` does not exist in the release binary** — the `mcp` cargo feature is
  default-off (`crates/ar-cli/Cargo.toml:23-25`) and `.github/workflows/release.yml`
  builds with no `--features`, so the shipped artifact has **no machine-readable
  surface at all**. Verified on the deployed binary: `error: unrecognized subcommand
  'mcp'`. CI's test job likewise passes no `--features`, so the MCP surface is
  untested in CI too.
- **A silent ceiling is still invisible** — `aroute doctor`'s `findings()` never reads
  `cfg.limits` (no `limits|model_tokens|spend_window` reference in
  `crates/ar-cli/src/commands.rs:1250-1490`), so no row anywhere in `doctor` names an
  armed ceiling, its key or its window. That is the code half of the out-of-tree
  writer, and it is why the block was invisible for a day (`CAP-06`).
- **The platform's own instruction corpus has no law that reaches an unregistered
  workspace** — measured on the VPS, the two strongest laws that would each have
  stopped the guard are pool-scoped, and the repo it was installed from is in no
  registry entry, so `arch route` found no owner; its enforcement tool cannot even
  refuse (it prints "No owning system pool found" and **exits 0**). Six laws (L1–L6)
  are specified in `docs/39` §7 (`CAP-07`, ledger **D15**).

**What to read next.** Start with **`docs/40-project-closing-outline.md`** — the
closing outline: what "done" means per surface, the measured state of both registers,
the closure sequence in dependency order, the operator's decisions and which phase
each one blocks, what is NOT a gap, and the starting procedure. It owns the *order and
the shape*; the ledger and the register below own the state, and the plans own the
detail. Then the three plans committed at `638a284` are the executor's
inputs, in this order: **`docs/35-cap-surface-removal-plan.md`** — closing every path
that can put a `limits:` row back (**five** write paths, not two; the live block on
the VPS is the reason it is urgent, not theoretical); **`docs/36-cli-agentic-workflow-plan.md`**
— the CLI as an agent-facing surface (eleven findings, four blocking, an eight-step
plan of record); **`docs/37-dashboard-deployability-plan.md`** — the compiled
dashboard's deployability and auth surface (three hard blockers and a twelve-row
security control set). Two newer documents own the questions the plans do not:
**`docs/38-402-model-token-cap-fix-plan.md`** — why the `402 model_token_cap` came
back at all, the two arms it hides behind (key-wide `token_cap` vs per-target
`model_token_cap`), and the out-of-tree guard that wrote it (Phase 1 applied and
verified; Phases 2–3 open); **`docs/39-organism-instruction-audit.md`** — whether
any instruction in the platform's own corpus *compelled* an agent to install the
guard, and the six laws (L1–L6) that would have stopped it. Then the ledger:
**`docs/28-gap-ledger.md`** owns the *state* of every open item (the 402 arms are
**D14**, the instruction corpus **D15**, the cap write paths **D11**/**D12**, the
CLI surface **D13**, the dashboard **F9**/**F10**), **`TODO.md`** the register of the
*work items* (rows `CAP-01`–`CAP-07`, `CLI-01`–`CLI-08`, `DASH-01`–`DASH-04` —
`CAP-06` is the out-of-tree writer, `CAP-07` the six laws), and the operator's open
decisions are `docs/kb/ASKS.md` **Q15**–**Q18**. For the forensic record of the
`limits:` block and its writer, read **`~/.config/ar/evidence-20261020-limits-restore/`**
on the VPS — it holds the as-found config, the backups, the guard's original script
and units (`aroute-limits-guard.timer` / `.service`) and the unit journal that
recorded `limits missing or drifted — restoring`. For the **2026-10-21 public
outage** and its recovery, the record is the ledger's row **C6** (and the paragraph
above) — the watchdog log holds the transition timestamps.

---

## Earlier state — 2026-10-19 (the `/ui` panel audit)

Branch `main` @ 8784b3f. The proxy runs **only** on the VPS (`racknerd`),
public at `https://aroute.ishanparihar.com/v1` behind a cloudflared tunnel; the gate is
armed and every harness holds a non-expiring token.

**That iteration audited the `/ui` control panel** (the request: contrast the OmniRoute
dashboard with ours, and land the plans/bugs/audit in the knowledge base so the upgrade
can start). Outcome: an audit, a contrast, and a seven-phase plan. **No code changed.**

Three findings, all verified against the live VPS on 2026-10-19:

1. **The public `/ui` write endpoints are unauthenticated** (register `UI-00`, audit F1).
   The panel has no credential check of its own; the only layer it carries is
   `loopback_only`, a DNS-rebinding check. From the internet, `POST /ui/api/combos`
   **rewrote the production config** (proved, then restored byte-exact; prod verified
   healthy, no residue). With the gate row readable, an anonymous caller could mint a
   client token — the operator's provider keys, spendable.
2. **`loopback_only` refuses exactly what a browser sends** (F2). The tunnel rewrites
   `Host` to loopback but forwards `Origin`, so the public page loses both fonts and
   every write button, while `curl` (no `Origin`) is admitted. The check is a rebinding
   defence being used as an access control it never was.
3. **Two desynced credential stores on the VPS** (F3). `aroute serve` opens
   `~/.config/ar/credentials.db` (unreadable → "serving ungated" in the log, nine boots
   running); a shell in `$HOME` opens `~/credentials.db` (readable, and its tokens are
   accepted by the public endpoint). `aroute doctor` reports `gate,ok,armed` because it
   reads the row's *name*, not whether it decrypts.

**The fix that needs no code: the panel works completely over an SSH port-forward**
(`ssh -L 20199:127.0.0.1:20128 racknerd`) — verified in a real browser: both font faces
load, a write returns 200, the computed font is the real one. **The fix that needs a
release: bearer-gate the panel** so it can be published deliberately.

Also found and recorded: an accent-contrast failure in the light band
(`--amber` as text on white is 2.6:1, so a *starred* model is less legible than an
unstarred one — `UI-02`), the CRAFT score **12/20** with no zero (the bar is 16; the
failover rail is the signature and scores 2), and the fact that the light band is the
default rendering and had never been screenshotted.

**Next: U0** (`docs/plans/2026-10-19-ui-dashboard-upgrade.md`) — close the write path
before anything else. `AR-2` (ingress → 404) needs a root `systemctl restart cloudflared`
(operator's step); `AR-3` (the write handlers refuse on an unreadable store) is a code
change; the store fix needs ruling **Q8**.

### LIVE RIGHT NOW (2026-10-19, 20:2x): the public `/ui` mints working tokens

**One privileged command closes it: `sudo systemctl restart cloudflared`.**

The cloudflared ingress rule that 404s `/ui` is **staged, validated and installed on the
VPS** (`~/.cloudflared/hermes-config.yml`, `path: ^/ui(/.*)?$` → `http_status:404`;
`ingress validate` OK; `ingress rule` matches `/ui/api/state` → 404 and `/v1` →
the proxy). It is **not live** because the running cloudflared is a root system unit.
`kill -HUP` → `Operation not permitted`; `PUT` on the tunnel's own config API → 200 and a
**no-op** (the runtime rules were re-read and unchanged; tunnel health re-verified after).

Until that restart, from the internet and with no credential:
`POST /ui/api/keys/mint` → **200 with a signed `execute:completions` token**, and that
token passes `GET /v1/models` (200), `GET /metrics` (200) and `POST /v1/chat/completions`
(**200, a real completion on the operator's provider keys**). The refusal that used to
cover this was an accident (an unreadable store row), and it ended at **20:07:58** when a
parallel session re-armed the gate.

**After the restart: rotate the gate master and re-push the token to every consumer**
(`scripts/gate-token.sh push`) — every token minted since 20:07:58 is valid, including
the audit's own probe tokens. Tracked as **Q10**.

### Where the knowledge for the upgrade lives

| question | owner |
|---|---|
| **what is actually still open, across the whole project** | **`docs/28-gap-ledger.md`** — one row per open item, each re-verified against the tree on 2026-10-19 |
| the work items, and their state | `TODO.md` (40 open: `UI-08`, `SEC-02`, `SEC-04`, `DEC-01`…`DEC-04`, `OPS-01`…`OPS-05`, `GAP-02`…`GAP-10`, `ROUTE-08`, `CAP-01`…`CAP-07`, `CLI-01`…`CLI-08`, `DASH-01`…`DASH-04`) |
| what the audit found, and the evidence | `docs/audit/2026-10-19-ui-dashboard-contrast.md` (+ `docs/audit/2026-10-19-ui-dashboard/`, probe JSON + 13 screenshots) |
| the contrast with OmniRoute, the CRAFT score, the design baseline, the reasoning | `docs/27-dashboard-contrast-and-upgrade-plan.md` |
| the executable phases and their gates | `docs/plans/2026-10-19-ui-dashboard-upgrade.md` |
| the operator's decisions | `docs/kb/ASKS.md` (Q7 publish or not, Q8 which store, Q9 restart-from-the-panel, Q10 the urgent tunnel restart) |
| what was tried and rejected | `docs/kb/CLOSED.md` |
| what shipped before this | `docs/19-ui-upgrade-plan.md` (A–E), `docs/23-ui-round-two-plan.md` (G1–G5) |

**The ledger is the one answer to "what is still open?".** It was consolidated on
2026-10-19 from `docs/21`, `docs/24`, `docs/07`, `docs/26` and the register, because the
open work had been spread over five documents with no row re-verified against the tree —
several of them had gone stale (A1 said "v0.1.16 is the newest tag" when `v0.1.17` was
tagged; B6 said "14 orphan pages" when the count is now 0). The old documents keep the
**history** of their pass and point at the ledger; the ledger owns the **state**.

<!-- genome:zone status sha=ebe61ec9a80b -->
| status | items |
|---|---|
| open | 39 |
| in-progress | 1 |
| done | 28 |

- `UI-08` [open] **Rule the canonical credential store on the VPS** and delete the other
- `SEC-02` [open] **Every token minted since 2026-10-19 20:07:58 is valid** — the exposure window opened when a parall
- `SEC-04` [open] **`loopback_only` refuses exactly what a browser sends from a public URL** (the tunnel forwards `Ori
- `DEC-01` [open] **Rule the start cap, the model deadline and the failover status**
- `DEC-02` [open] **Rule whether a routable bind may be allowed while the bearer gate is armed**
- `DEC-03` [open] **Rule what `Limit.model` scopes** — *when* a row applies, not *what it counts*
- `DEC-04` [open] **Rule whether the panel may restart the proxy after a write**
- `OPS-01` [open] **19 commits after `v0.1.20` are unreleased** (`81b3ce3` … `15f027c`; **0** not yet pushed — `origin
- `OPS-02` [open] **The `failures` table only exists from the release that ships it** — older rows are not backfilled,
- `OPS-03` [open] **No 12-hour soak against a released binary**
<!-- genome:end status -->
