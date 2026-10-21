# 40 — The project closing outline: what remains, in what order, and what "done" means

Status: **written 2026-10-21.** This doc owns the **order and the shape of the
remaining work** across the whole project. It owns no state: the state of every
open item is in [`docs/28-gap-ledger.md`](28-gap-ledger.md), and the work items
are in [`TODO.md`](../TODO.md). Where this doc and those two disagree, **they
are wrong, not this doc** — re-measure and fix the owner.

Companions: [`docs/28-gap-ledger.md`](28-gap-ledger.md) (the state of every open
item — the master register),
[`TODO.md`](../TODO.md) (the register — one row per work item),
[`docs/kb/ASKS.md`](kb/ASKS.md) (the operator's open questions — the things a
fresh agent may NOT decide alone),
[`docs/31-remaining-gaps-execution-outline.md`](31-remaining-gaps-execution-outline.md)
(the earlier pass over the same work — the ordering constraints in its
"ordering constraints that are not obvious" section are the reason §2 below is
ordered the way it is),
[`docs/35-cap-surface-removal-plan.md`](35-cap-surface-removal-plan.md),
[`docs/36-cli-agentic-workflow-plan.md`](36-cli-agentic-workflow-plan.md),
[`docs/37-dashboard-deployability-plan.md`](37-dashboard-deployability-plan.md),
[`docs/38-402-model-token-cap-fix-plan.md`](38-402-model-token-cap-fix-plan.md),
[`docs/39-organism-instruction-audit.md`](39-organism-instruction-audit.md)
(the five plans of record),
[`docs/RUNBOOK.md`](RUNBOOK.md) (exact commands and environment traps),
[`docs/handoff/STATE.md`](handoff/STATE.md) (the current snapshot),
[`docs/29-routing-quality.md`](29-routing-quality.md) (the routing audit and
its eleven fixes).

---

## Contents

- [0. What "done" means here](#0-what-done-means-here)
- [1. The state in one table](#1-the-state-in-one-table)
- [2. The closure sequence](#2-the-closure-sequence)
- [3. The operator's decisions](#3-the-operators-decisions)
- [4. The end-state of each workstream](#4-the-end-state-of-each-workstream)
- [5. What is NOT a gap](#5-what-is-not-a-gap)
- [6. The work that is already closed](#6-the-work-that-is-already-closed)
- [7. The starting procedure](#7-the-starting-procedure)
- [8. What this document does not own](#8-what-this-document-does-not-own)

## 0. What "done" means here

The project is **a proxy with a panel, a CLI, a compiled dashboard, a release
pipeline and a production deployment**. "Done" is therefore **per-surface**, and
each surface has a different end-state. A single global end-state would be
unmeasurable; the checkable conditions below are the ones the ledger's own open
rows imply.

### 0.1 The proxy (the data plane)

| # | Condition | Check |
|---|---|---|
| P1 | Every request the client sends is either served or refused **for a reason the client can read**; no request is refused with a reason indistinguishable from a provider outage. | The 402 arm names the models, the spend and the ceiling (ledger **D11** / **D14** `CAP-03`); a budget refusal stamps `strategy=capped_by_budget`, not `strategy=none` (`CAP-04`). |
| P2 | A ceiling that is scoped to a model governs **that model only**; a row scoped to `model: jev` does not cap a request for `glm-5.3`. | `Limits::for_request` has a production caller and the policy layer uses it (ledger **D11** `CAP-05`, `docs/35` §2.2). |
| P3 | **Every** route that can spend is under the same arms — the media family included (`/v1/embeddings`, `/v1/audio/transcriptions`, `/v1/audio/translations`, `/v1/images/generations`, `/v1/ocr`). | The media family calls `rate_wait`/`key_cap`/`target_cap` (ledger **D11** `CAP-05`). |
| P4 | No surface can put a `limits:` row into the deployed config **without a deliberate operator action**. | `grep -n '^limits:' ~/.config/ar/config.yaml` on the VPS → absent, and absent after every writer path has been exercised. The six writers: ledger **D11** (`CAP-01`, `CAP-03`, `CAP-05`), **D12** (`CAP-02`, `CAP-04`), **D14** (`CAP-06`). |
| P5 | An armed ceiling is **never silent** — `doctor` and the boot path name every armed ceiling, its key and its window. | `aroute doctor` carries a row that reports the armed ceilings (ledger **D14** `CAP-06`, `docs/38` Phase 3). |
| P6 | The strategy that ranked a request is the strategy that ran it; no strategy silently degenerates to `priority`. | `docs/29-routing-quality.md` — the one signal still missing is `cached_prefix_tokens` (register **ROUTE-08**). |
| P7 | A stall is bounded by a **ruled** deadline, and the second-stall case is fixed. | Ledger **B2** (ruling) and **D2** (the empty-200 arm), which waits on that ruling. |

### 0.2 The panel (`/ui`)

| # | Condition | Check |
|---|---|---|
| PNL-1 | The panel is reachable only by a **credential**: armed → the bearer decides alone; unarmed → the `Origin` decides alone; a broken gate → writes refuse. | Ledger **A1**/**F1**/**F2**/**A8** — already true for the deployed panel; the open part is ledger **A5**'s general form. |
| PNL-2 | A credential can be **withdrawn** without rotating everything. | `aroute keys revoke --token-file <path>` kills the whole session on both surfaces (ledger **A7**) — shipped. |
| PNL-3 | The panel can be reached **without the tunnel's 404 rule** if and only if the operator has ruled that it should be public. | Ledger **B1** (ruling) then ledger **C6** (the tunnel's root restart). |
| PNL-4 | The panel's own surface is the panel the operator uses — the limits **editors** are gone, the read-only Budgets view stays. | Ledger **D12** (`CAP-02`, `CAP-04`); plan `docs/35` §5 Step 2. |
| PNL-5 | The config the panel shows is the config the proxy runs, and a write that changes nothing does not dirty the file. | Ledger **F8** — shipped (`docs/31` Phase 3.3). |

### 0.3 The CLI (the agent surface)

| # | Condition | Check |
|---|---|---|
| CL-1 | An agent can reach **every config section** the panel reaches, without hand-editing YAML, and without a browser. | Ledger **D13** `CLI-08` (`oauth:`, `server:`, a targeted `providers:` add); plan `docs/36` §4 Step 8. |
| CL-2 | No CLI write path can **silently delete** a config section. | Ledger **D13** `CLI-01` — the hyphen-section data loss in `is_section_header`; plan `docs/36` §4 Step 1. |
| CL-3 | The CLI can answer "what did we spend", and the answer is the **same renderer** the MCP uses. | Ledger **D13** `CLI-02`; plan `docs/36` §4 Step 2. |
| CL-4 | Every write command says the truth about **when the change takes effect**, and the help text no longer claims a live reload. | Ledger **D13** `CLI-03`; plan `docs/36` §4 Step 3. |
| CL-5 | An OAuth login can be **resumed across a process boundary** — a human-in-the-loop handoff with a durable handle. | Ledger **D13** `CLI-04`; plan `docs/36` §4 Step 4. |
| CL-6 | The `keys` subtree can **inspect** the gate, and `arm-gate` cannot silently overwrite a live master. | Ledger **D13** `CLI-05`; plan `docs/36` §4 Step 5. |
| CL-7 | The shipped binary carries a **machine-readable** surface, and the unbounded list commands are bounded. | Ledger **D13** `CLI-06` (the release-feature decision) and `CLI-07` (filters/pagination); plan `docs/36` §4 Steps 6–7. |
| CL-8 | A credential check is evidence about **the surface that made the request**. | Ledger **F2.4** — already closed; the rule stays. |

### 0.4 The compiled dashboard (the second surface)

| # | Condition | Check |
|---|---|---|
| DSH-1 | There is **one** operator surface, not two. | Ledger **F9** (register **DASH-01**): converge on the Rust panel, or keep maintaining the dashboard; plan `docs/37` §8. |
| DSH-2 | If the dashboard is to be published, its artifact has a **sanctioned delivery path** — a release-attached, checksummed linux-x64 tarball the installer verifies. | Ledger **F9** (register **DASH-02**); plan `docs/37` §4. |
| DSH-3 | The dashboard's gateway tier is **not anonymous** — `REQUIRE_API_KEY=true` is in effect, and every one of the twelve controls in the plan's control set is green. | Ledger **F10** (register **DASH-04**); plan `docs/37` §6 (its table numbers the controls 1–12 — they are the plan's own, not ledger ids). |
| DSH-4 | The tunnel's forwarding behaviour is **measured**, not assumed. | Ledger **F10** (register **DASH-03**); plan `docs/37` §7 O1. |

### 0.5 Release, operations and the deployment

| # | Condition | Check |
|---|---|---|
| REL-1 | `main` is **released** — the deployed binary is the newest tag's asset, and nothing on `main` is unreleased. | `git rev-list --count v<N>..HEAD` = 0, and the VPS's `aroute --version` matches the newest tag (ledger **C1**, register **OPS-01**). |
| REL-2 | The release **soak** has run against a released binary. | Ledger **C3** (register **OPS-03**): a completed 12-hour `scripts/soak.sh` run. |
| REL-3 | The credential store on the VPS is **unambiguous** — one canonical file, `AR_CRED_STORE` set explicitly, the other deleted. | Ledger **A4** (register **UI-08**), operator ruling ask **Q8**. |
| REL-4 | Every token minted during the exposure window is **dead**. | Ledger **A2** (register **SEC-02**, ask **Q10**): the master rotated and re-pushed. |
| REL-5 | The registry snapshot is inside its TTL, or the staleness is an accepted decision. | Ledger **C4** (register **OPS-04**). |
| REL-6 | The doc gate and the tunnel's ingress are **settled** — `docs-check` gating is ruled, and the tunnel's rules are live. | Ledger **C5** (register **OPS-05**, ask **Q14**); **C6** is **closed** — the tunnel's rules are live (register **OPS-06**, done). |
| REL-7 | The instruction corpus can **stop** a repeat of the out-of-tree guard, in a workspace that is not a pool. | Ledger **D15** (register `CAP-07`): laws L1–L6 in `docs/39` §7. |
| REL-8 | A **fresh agent** can orient in one call and find the owner of any question. | `genome wake` → `genome ask "<question>"` → `genome close`; §7 below. |

### 0.6 The release rule — stated once, relied on everywhere

**Every shippable step is one release**, and the release path is the same every
time (`docs/35` §0, `docs/36` §4 preamble, project memory #226):

> commit the change → bump the workspace version in `Cargo.toml` and refresh
> `Cargo.lock` → a `chore(release): vN` commit → an **annotated** tag `vN` →
> push `origin main` and the tag → `.github/workflows/release.yml` builds and
> publishes the static **musl** asset → install on the VPS over `ssh racknerd`.

**Never `scp` a locally built binary to the VPS.** The host's glibc is **2.36**;
a local release build links 2.39 and dies with `GLIBC_2.39 not found`. The
2026-10-19 incident came from doing exactly that (ledger **C1**, memory #113,
`docs/RUNBOOK.md`).

---

## 1. The state in one table

**How the counts were measured.** The ledger was parsed row by row: every line
of `docs/28-gap-ledger.md` beginning `| **<id>** |` is one row, and its
**status cell** (the third cell) was read. A row counts as **open** if that
status cell begins with `open` or `blocked`; every other status (`fixed`,
`closed`) counts as closed. The register was parsed the same way from
`TODO.md`: one row per `| <id> | <status> | …` line, where a status of `open` or
`in-progress` counts as open.

**Ledger** — 59 rows total, **31 open** (open *or* blocked), 28 closed:

| Section | Open/blocked | Closed | Total | Owner document |
|---|---|---|---|---|
| **A** — security and access control | 3 | 5 | 8 | `docs/28` §A; plan `docs/31` Phases 0–3 |
| **B** — decisions the operator owns | 5 | 0 | 5 | `docs/28` §B; asks `Q7`–`Q12`, `Q14` |
| **C** — release, soak, operations | 5 | 1 | 6 | `docs/28` §C; plan `docs/31` Phases 1, 5 |
| **D** — code gaps in the proxy | 12 | 10 | 22 | `docs/28` §D; plans `docs/35`, `docs/36`, `docs/38` |
| **E** — live probes | 4 | 0 | 4 | `docs/28` §E; plan `docs/31` Phase 5 |
| **F** — the panel | 2 | 8 | 10 | `docs/28` §F; plan `docs/30` |
| **F2** — context window / harness surfaces | 0 | 4 | 4 | `docs/28` §F2 (all closed) |
| **Total** | **31** | **28** | **59** | |

The 31 open ledger rows, by id: **A2, A4, A5** (A); **B1, B2, B3, B4, B5** (B);
**C1, C2, C3, C4, C5** (C); **D2, D4, D5, D6, D7, D8, D9, D11, D12, D13,
D14, D15** (D); **E1, E2, E3, E4** (E); **F9, F10** (F).

**E4 is counted as open by that rule** (its status cell begins with `open`), but
it is a **known trait, not a defect** — see §5. The count of *work* is therefore
**30**, not 31.

**Register (`TODO.md`)** — 68 rows total, **40 open** (39 `open` + 1
`in-progress`), 28 done:

| Register section | Open | Done | Total | Ledger rows it carries | Operator-gated? |
|---|---|---|---|---|---|
| Panel (unnamed section, `UI-00`…`UI-10`) | 1 | 10 | 11 | F1–F8, A3 | no |
| Security and access control (`SEC-*`) | 2 | 4 | 6 | A2, A5, A6, A7, A8 | **yes** — SEC-02 |
| Decisions (`DEC-01`…`DEC-04`) | 4 | 0 | 4 | B2, B3, B4, B5 | **yes** — all four |
| Release / soak / ops (`OPS-*`) | 5 | 1 | 6 | C1–C6 | **yes** — OPS-01 |
| Proxy code gaps (`GAP-*`, `ROUTE-*`, `CTX-*`, `LIMIT-*`) | 9 | 13 | 22 | D2–D10, D1b–D1h | no (except the provider-key rows) |
| Spend-cap write surfaces (`CAP-*`) | 7 | 0 | 7 | D11, D12, D14, D15 | **yes** — CAP-03, CAP-04, CAP-05 |
| CLI as an agent surface (`CLI-*`) | 8 | 0 | 8 | D13 | **yes** — CLI-06 (the plan's decision `docs/36` §5 DEC-2) |
| Compiled dashboard (`DASH-*`) | 4 | 0 | 4 | F9, F10 | **yes** — all four |
| **Total** | **40** | **28** | **68** | | |

**How the two registers differ — read this before counting.** The ledger and the
register are **not** 1:1, and the difference is structural, not an error:

* The ledger's **A** section has 8 rows; the register's `SEC-*` has 6. **A1** is
  carried inside `SEC-01`'s notes (which cite **A1** and **F1** together), and
  **A3** is carried by the register's `UI-07` (the doctor gate check).
* The ledger's **B** section has 5 rows; the register's `DEC-*` has **4**.
  **B1 has no register row.** Its ask is **Q7** and its content is carried by
  `DASH-01`, `DASH-04` and `SEC-04`'s notes.
* The ledger's **E** section (E1–E4) has **no register rows at all** — the
  probes are scheduled in `docs/31` Phase 5, not in `TODO.md`.
* The ledger's **F2** subsection (F2.1–F2.4) is carried by the register's
  `CTX-01`…`CTX-03` (F2.4 is folded into `CTX-03`).
* The register's `ROUTE-08` and `GAP-09` have **no ledger row of their own**:
  `ROUTE-08` is the `cached_prefix_tokens` signal named in `docs/29` §4, and
  `GAP-09` is a re-verification note on ledger **D3**, not new work.
* The register's id **`GAP-04` was used twice** — once for ledger **D3** and once
  for ledger **D5**. **Fixed 2026-10-21**: the **D3** row was renamed
  **`GAP-10`**, and **`GAP-04` belongs to D5**; see §6.3.

---

## 2. The closure sequence

Seven phases, in dependency order. Each names what it closes, what it depends
on, what it produces, how it is verified, and its release. **Every phase's
release is the §0.6 path** — that rule is not restated per phase.

**These are this document's phases, not `docs/31`'s.** `docs/31` (the earlier
pass) numbered its own phases 0–6 and most of its work has since shipped; this
sequence is re-derived from the ledger's state on 2026-10-21. The
correspondence: `docs/31` Phase 0 → §2 Phase 0; `docs/31` Phase 1 → §2 Phase 1;
`docs/31` Phases 2–3 (all shipped) → §6.1; `docs/31` Phase 4 → §2 Phase 4;
`docs/31` Phase 5 → §2 Phase 6; `docs/31` Phase 6 (the unscheduled rows) →
§3 and §5. `docs/31` has **no** phase for the cap surfaces or the CLI plan —
those are §2 Phases 2 and 3, and they are the largest open block.

### Phase 0 — Close the exposure and settle the store (operator; minutes; no release)

**Closes:** ledger **A2** (register `SEC-02`, ask **Q10**) and **A4** (register
`UI-08`, ask **Q8**). **The operational half of `C6` is already closed** (register
`OPS-06`, done): the tunnel's `^/ui(/.*)?$` → 404 rule is live.

**Depends on:** nothing. This is first because the exposure is live and because
**A2 is the only row whose window closes on its own** — the panel's mint
defaults to `--ttl-days 30`, so every token minted in the window is good for 30
days and only a rotation kills them.

**Produces:** a rotated master, re-pushed to every consumer; one canonical
credential store with `AR_CRED_STORE` set; the tunnel's `^/ui(/.*)?$ → 404` rule
live.

**How it is verified:** the public `/ui` answers **404**; an anonymous
`POST http://127.0.0.1:20128/ui/api/keys/mint` answers **401
`credential_rejected`**; a token minted **before** the rotation answers **401**;
`doctor` on the VPS reports `store,ok` and `gate,ok,armed`; the pre-rotation
tokens answer **401** at the public endpoint.

**The order is not a task list, it is an order** (from `docs/31`'s "ordering
constraints that are not obvious", item 1): **restart the tunnel first, then
rotate the master.** Restarting closes the **door**; rotating kills the **keys
already handed out**. Rotating first leaves a window in which a fresh 30-day
token can still be minted, and the rotation then kills it too, having rotated
for nothing.

**Release:** none. This is operations on the VPS, and the sequence is
`docs/RUNBOOK.md`'s — "The gate has one master, three names" — plus the
`AR_CRED_STORE` edit, which belongs in the **same** `ar.env` edit as the
rotation so the store is unambiguous the moment the new master lands.

### Phase 1 — Release what is already written (agent, then operator)

**Closes:** ledger **C1** (register `OPS-01`) and, with it, **C2** (register
`OPS-02`, a release note, not a task).

**Depends on:** Phase 0 (the exposure is closed before a new binary is
published).

**Produces:** a new tag whose musl asset is on the release, installed on the
VPS from that asset.

**How it is verified:** `git rev-list --count v<N>..HEAD` = 0;
`origin/main` is at the same commit as `HEAD`; the VPS's `aroute --version`
reports the new version; `file -b ~/.local/bin/aroute` says **static-pie
linked**; the panel is still armed and `/v1` still answers.

**Release:** §0.6. **Never scp a local build** (memory #113, ledger **C1**).

**Measured at the time of writing:** `git rev-list --count v0.1.20..HEAD` = **19**
commits, of which **0** are not yet pushed (`origin/main` is at `15f027c`, the
same commit as `HEAD` `15f027c`). The range runs `81b3ce3` … `15f027c`: the
docs, the audits, a behaviour-neutral formatter pass and the five plans — **the
blast radius is documentation, not the panel**. The ledger's **C1** row and
register `OPS-01` carry the same pair; re-measure before releasing.

### Phase 2 — Remove the cap write surfaces (agent; one release per step)

**Closes:** ledger **D11** (register `CAP-01`, `CAP-03`, `CAP-05`) and **D12**
(register `CAP-02`, `CAP-04`); and the code half of **D14** (register `CAP-06`).

**Depends on:** Phase 1 is not a hard dependency, but the plan's steps are
ordered by blast radius and each step is its own release.

**Produces:** the CLI as the **only** way a ceiling can be armed; a `doctor` row
and a boot warning that name every armed ceiling.

**How it is verified:** `grep -rn '/ui/api/limits' crates/ scripts/ docs/
dashboard/` returns nothing (the panel's writer is gone); `aroute sync` no longer
creates a `limits:` block and the inverted test from `docs/35` §8 passes; the
**read-only** Budgets pane still renders; `aroute doctor` names every armed
ceiling; the boot log names every armed ceiling with its key and window.

**Release:** §0.6, one release per step in `docs/35` §5: Step 1 (delete the
`sync` rpm carry) → Step 2 (remove the panel's Limits pane, the default-ceilings
form and the mint `rpm` field) → Step 3 (documentation and state alignment) →
Step 4 (remove `aroute limits set`) **only if** the operator rules it, per
`docs/35` §7 decision 1 → Step 5 (disarm the engine) **only if** the operator
rules it, per §7 decision 3. The **code half of D14** — `doctor` and the boot
path naming every armed ceiling (`CAP-06`) — is a separate release, per
`docs/38` Phase 3.

**Two of the five steps are the operator's ruling, not the agent's** — see §3.

### Phase 3 — Make the CLI an agent-facing surface (agent; one release per step)

**Closes:** ledger **D13** (register `CLI-01`…`CLI-08`).

**Depends on:** nothing in Phases 0–2, except that **CLI-01 ships first**
because every other write step depends on it — a wider `is_section_header`
predicate is the precondition for splicing safely (`docs/36` §4 Step 1).

**Produces:** the eight shippable steps of `docs/36` §4, in order — Step 1
(hyphen-section data loss) → Step 2 (`cost-report` verb) → Step 3 (the
"not live until restart" contract and the false help text) → Step 4 (resumable
OAuth login) → Step 5 (`keys` inspect surface; `arm-gate` refuses to overwrite
without `--force`) → Step 6 (machine-readable output mode and a command
catalog) → Step 7 (filters and pagination) → Step 8 (the three unreachable
config sections).

**How it is verified:** each step's own verification, per `docs/36` §4 — e.g.
Step 1: a config with `api-keys:` **after** the spliced section comes back
byte-identical; Step 2: the verb renders `CostReport::toon()`, the **same**
renderer the MCP uses; Step 6: `aroute commands --json` works in the **shipped**
build; Step 8: `oauth:`, `server:` and a targeted `providers:` add are all
reachable from the CLI without a browser.

**Release:** §0.6, one release per step.

**One step carries the release-artifact decision** — Step 6 (register `CLI-06`,
`docs/36` §5 DEC-2): ship `aroute commands --json` (the plan's recommendation),
or enable the `mcp` feature in the release build, which changes the published
artifact and re-opens the RAM gate in `docs/06:60`. **The agent may not decide
this** (§3).

### Phase 4 — Rule and close the panel's and the proxy's remaining defects (operator, then agent)

**Closes:** ledger **A5** (register `SEC-04`), **B1** (no register row; ask
**Q7**), **B5** (register `DEC-04`, ask **Q9**), **B4** (register `DEC-03`),
**B3** (register `DEC-02**, ask **Q12**), **B2** (register `DEC-01**, ask
**Q11**), **D2** (register `GAP-02**).

**Depends on:** **B1** gates **A5**'s general form — if `/ui` is never public,
the general `loopback_only` change is moot and only the panel's copy matters
(`docs/31`, ordering constraint 4). **B2** gates **D2**: the empty-200 arm
waits on a corpus that does not exist, because the live failure is the **stall**
(2026-10-18: 0 of 4 probes answered, all 504 `upstream_timeout` after 150 s).

**Produces:** the rulings, recorded with
`genome q rule <id> "<ruling>" --applied <owner path>` and carried into the
doc that owns the question; then the code each ruling unblocks.

**How it is verified:** each ruling appears in `docs/28` (the ledger owns the
state) and in `docs/kb/ASKS.md` with its `applied` path filled in; the
affected ledger rows move.

**Release:** the code that follows each ruling ships under §0.6.

### Phase 5 — Decide the two-panels question and the dashboard's publishability (operator ruling first; agent after)

**Closes:** ledger **F9** (register `DASH-01`, `DASH-02`) and **F10** (register
`DASH-03**, `DASH-04**).

**Depends on:** **Q18** (which surface reaches the `aroute` hostname) is the
top question. It is answered by `docs/37`'s own position — **converge on the
Rust panel**; the compiled dashboard stays a local SSH-forwarded tool
(`docs/37` §8.4 item 3). The measurement that gates any *publishing* is **O1**
(register `DASH-03`): does the tunnel forward an `x-forwarded-*` header? If the
`via-proxy` marker is never set, every public request is classified `loopback`,
which opens the LOCAL_ONLY tier — a fail-open **no ingress rule can see**.

**Produces:** a position on two panels; a release-attached, checksummed
linux-x64 tarball for the 1.6 GiB dist (`keytar` rebuilt for the target) if the
dashboard is to be delivered at all; the plan's twelve-row control set
(`docs/37` §6) green before any ingress rule exists.

**How it is verified:** the plan's per-control and per-artifact verification
(`docs/37` §9); for each control in the §6 table, the command beside it returns
the stated value.

**Release:** §0.6 for the delivery path; the **dashboard's own unit** is not
the aroute release and is installed separately (`install_dashboard()` and
`install_dashboard_unit()`, `install.sh:437-537`).

### Phase 6 — The probes, the soak, and the laws (agent, after Phase 1; one is the operator's)

**Closes:** ledger **C3** (register `OPS-03**), **C4** (register `OPS-04**),
**C5** (register `OPS-05**, ask **Q14**), **E1**, **E2**, **E3** (no register
rows), and **D15** (register `CAP-07**).

**Depends on:** the probes need a **released** binary — that is Phase 1's
output (`docs/31` Phase 5's own rule: the ledger's rule is that they wait on
**C1**).

**Produces:** a completed 12-hour soak against a released binary
(`scripts/soak.sh`, detached, killed by PID, CSV outside `/tmp`); a registry
snapshot inside its TTL, or a recorded decision to accept the warning; a
recorded `docs-check` gating decision; the six laws L1–L6 in the organism's
instruction corpus.

**How it is verified:** the soak's CSV shows the run length and zero failures;
`doctor` no longer warns past the TTL; each probe's **result** is recorded in
the ledger with the evidence, not just a status word (`docs/31`'s gate).

**Release:** the laws L1–L6 land in the **organism** repository
(`~/organism/organism/`), not here — this project only owns the finding
(ledger **D15**, `docs/39` §7).

**Two of these are not the agent's to close:** **C4** is an operations cadence
decision, and **C5** is a policy decision (ask **Q14**).

---

## 3. The operator's decisions

Everything in this section is a ruling the **operator** owns. A fresh agent may
**prepare** each one — gather the evidence, lay out the options, recommend one
— and must not **make** it. Record the answer with
`genome q rule <id> "<ruling>" --applied <owner path>` and put it in the doc
that owns the question (`docs/28` §H item 5).

### 3.1 The rulings that BLOCK a phase

| Ask | Ledger / register | The question | Blocks | Why it cannot be deferred |
|---|---|---|---|---|
| **Q10** | **A2** / `SEC-02` | Rotate the gate master — the tokens minted in the exposure window are still valid. | **Phase 0**, and therefore everything | Only a rotation kills them; the panel's mint defaults to 30 days. `aroute keys revoke` now exists (ledger **A7**) and is the *narrow* lever, but the window's tokens are the operator's to withdraw. |
| **Q8** | **A4** / `UI-08` | Which credential store is canonical on the VPS? | **Phase 0** | Two files, two envelopes, one authority. It belongs in the same `ar.env` edit as the rotation. |
| **Q15** | **D11**, **D12** / `CAP-01`…`CAP-05` | Which of the cap-writing paths survive? | **Phase 2**, steps 2, 4, 5 | `docs/35` §7 decision 1 (does `aroute limits set` survive), decision 2 (does the Budgets pane keep displaying ceilings), decision 3 (do the engine arms go). The plan **recommends**: keep the CLI verb, keep the read-only pane, keep the engine arms. |
| **Q7** | **B1** (no register row) | Should `/ui` be reachable from the internet at all? | **Phase 4**, and the general form of **A5** | Two orders of magnitude apart in blast radius: SSH-forward only (no code) vs published-and-gated (a release plus a token-in-the-browser flow). |
| **Q18** | **F9**, **F10** / `DASH-01`…`DASH-04` | Which surface reaches the `aroute` hostname? | **Phase 5** | Publishing the dashboard would publish `POST /api/v1/chat/completions` **anonymously** over the operator's provider keys — outside aroute's gate, its ledger and its `limits:`. |
| **Q11** | **B2** / `DEC-01` | Rule the start cap, the model deadline and the failover status. | **Phase 4**, and **D2** behind it | `STREAM_START_TIMEOUT_CAP` is 110 s and the model deadline is 120 s, so the two-stall case is unfixed. |
| **Q12** | **B3** / `DEC-02` | May a routable bind be allowed while the bearer gate is armed? | **Phase 4** | The in-code seam `TODO(#p1-config-public)` waits on it. |
| **Q17** | **D13** / `CLI-01`…`CLI-08` | Should the CLI reach everything the panel can, and what is the shipped machine-readable surface? | **Phase 3** | `docs/36` §5 DEC-1 (the CLI's scope) and §5 DEC-2 (the release-artifact decision) — the one item in that plan that changes what is published. The plan recommends: yes for config sections, and ship `aroute commands --json` rather than enabling the `mcp` feature. |
| **Q16** | **D11** / `CAP-05` | Are the two engine gaps intended behaviour or defects? | **Phase 2**, the engine half | The `model:` scope is never applied at request time, and the media routes consult no policy. Neither caused the 402; each is a real defect in its own right. |

### 3.2 The rulings that CAN be deferred

| Ask | Ledger / register | The question | Why deferring is safe |
|---|---|---|---|
| **Q9** | **B5** / `DEC-04` | Should the panel be able to restart the proxy after a write? | Not needed for correctness. The panel shows the restart command; `ConfigHandle::watch` exists in `ar-config` but is not attached (`docs/16`). |
| **Q14** | **C5** / `OPS-05` | Does `docs-check` gate the pre-commit hook? | It runs in **warn** mode and the orphan-page count is **0**; the README rubric, the outward boundary, the per-package README contract and the genome boundary all pass. A policy choice, not a gap in the tree. |
| **Q1** | **D6** / `GAP-05` | A live `zenmux-free` key. | Not code: the wire is built; inference is locked for this account (403 `access_denied api_key_source: payg`). |
| **Q2** | **D7** / `GAP-06` | A Bedrock API key and region. | Not code: the wire is built and unit-tested; it has never been called against the provider. |
| **Q3** | **D7** / `GAP-06` | A GitLab Duo token. | As above. |

### 3.3 Rulings already answered in practice

| Ask | Ledger / register | Where the answer already lives |
|---|---|---|
| **Q4** | `docs/22` D10 | **Ruled**: keep the per-combo 150 s for `free-stack`; the global 120 s stays. |
| **Q5** | `docs/24` E2 | **Ruled**: keep the GitLab Duo tool refusal; the port waits for the reference on disk. |
| **Q6** | `docs/kb/WORKLOG.md` | **Ruled**: the push was approved. |
| — | **B4** / `DEC-03` | `Limit.model` scopes **when** a row applies, not **what it counts** — recorded in `docs/23` G3, and the `--model` help text says so. The ledger row is open only because it has no way to record "settled by precedent". |
| — | **Q13** | **Answered by the code**: the MCP HTTP transport now carries the same `execute:completions` bearer `/v1` accepts (ledger **A6**, shipped). The remaining sub-question — whether the control plane should be reachable off-box — is the same question as **Q7** one layer down. |

---

## 4. The end-state of each workstream

One subsection per workstream, with the concrete check that closes it.

### 4.1 The proxy (data plane)

**Closes when:** **P1**–**P7** of §0.1 hold.

* The check for **P1/P4/P5**: no `limits:` block in the deployed config, `grep -n
  '^limits:' ~/.config/ar/config.yaml` → no match, and `aroute doctor` names any
  ceiling that *is* present.
* The check for **P2**: a `model:`-scoped row does not govern an unrequested
  model — the policy layer calls the scope-filtering accessor.
* The check for **P3**: a media route and a chat route are refused by the same
  arms.
* The check for **P6**: the `cache-optimized` strategy sees a non-zero
  `cached_prefix_tokens` (register **ROUTE-08**, `docs/29` §4) — **or** the
  operator accepts that `cache-optimized` stays an alias for `priority`, which
  must then be recorded as a decision, not left as a silent gap.
* The check for **P7**: a two-stall request is bounded by the ruled deadline and
  the ruling is in `docs/28` **B2** with the ask **Q11** closed.

### 4.2 The panel (`/ui`)

**Closes when:** **PNL-1**…**PNL-5** of §0.2 hold.

* The check for **PNL-1**: three states, three behaviours — no gate row → reads
  and writes allowed; a decrypting row → a token is required; a row that will
  not decrypt → writes refuse, reads degrade. Arming the gate **while the proxy
  runs** turns it on (ledger **A8**, shipped).
* The check for **PNL-3**: the public `/ui` answers **404** *or* answers **401**
  without a token, depending on the **B1** ruling — and the ledger says which.
* The check for **PNL-4**: `grep -rn '/ui/api/limits' crates/` returns nothing;
  the Budgets pane still renders.

### 4.3 The CLI (agent surface)

**Closes when:** **CL-1**…**CL-8** of §0.3 hold, and the plan's eight steps
in `docs/36` §4 are each marked shipped in the ledger **D13** row and in their
register rows.

* The check: an agent given only the CLI and no browser can set `server:`, add
  an `oauth:` block and a `providers:` row, read the spend, log in to a provider
  across two process boundaries, and list the command catalog as JSON.

### 4.4 The compiled dashboard

**Closes when:** **DSH-1**…**DSH-4** of §0.4 hold, and one of the two
positions below is taken and recorded:

* **Converge** (the plan's position, `docs/37` §8.4): the Rust panel is the one
  surface, `dashboard/` and the rebrand pipeline and `sync` and the second
  gateway are retired, and the migration is **one import + one sync** with the
  dashboard's DB archived. The check: the aroute hostname serves a browser
  surface, and there is no second gateway and no 1.6 GiB artifact in the
  release.
* **Keep two**: the dashboard keeps a **sanctioned delivery path** (register
  `DASH-02`: a release-attached, checksummed linux-x64 tarball the installer
  verifies) and its gateway tier is **not anonymous** (register `DASH-04`:
  `REQUIRE_API_KEY=true` in effect, and all twelve controls green). The check:
  `curl -X POST http://127.0.0.1:20149/api/v1/chat/completions` → **401**, not
  200.

**Either way**, register **DASH-03** (O1) is measured and recorded: the
`x-forwarded-*` behaviour of the tunnel is a **fact**, not an assumption.

### 4.5 Release, operations, deployment

**Closes when:** **REL-1**…**REL-8** of §0.5 hold.

* The check for **REL-1**: `git rev-list --count v<N>..HEAD` = 0, and the
  deployed binary's `--version` equals the newest tag.
* The check for **REL-3/REL-4**: one credential store; the pre-rotation tokens
  answer **401** at the public endpoint.
* The check for **REL-7**: laws L1–L6 are in the organism corpus, and L1
  ("never autonomously operate an existing system") is a **platform-wide §0**,
  not a pool rule.

### 4.6 The instructions of this project (the organism's laws)

**Closes when:** ledger **D15** (register `CAP-07`) moves, with the six laws
recorded. This workstream lands in a **different repository**; this project
owns the finding and the specification, not the merge.

---

## 5. What is NOT a gap

From the ledger's section **G** (`docs/28` §G) and the register's own notes.
A fresh agent that re-litigates any of these has wasted a turn. **Do not
re-open them.** **Twenty-two items**, all still true.

| Item | Id / where | The ruling |
|---|---|---|
| `/metrics` label cardinality | `docs/28` §G | Deliberate cap. No model labels on `/metrics`. |
| `x-ar-model` carries the client's spelling | `docs/28` §G, memory #90 | By design. The serving target is stamped elsewhere. |
| Trace-channel lossiness; redb's exclusive lock | `docs/28` §G | Documented, not a defect. |
| Mid-stream failover after content flowed | `docs/28` §G | Not attempted by design. |
| Pipeline / fusion semantics | `docs/28` §G | Settled. |
| The breaker tripping under repeated probing (`503 every provider in the chain is cooling down`) | `docs/28` §G | **The system working.** |
| `~/bin/ar` shadowing `/usr/bin/ar` | `docs/28` §G | Resolved; the binary is `aroute`. |
| Discovery's memory cost (34 MiB config-only → 120 MiB with 9 715 models) | `docs/28` §G | Documented in the unit, not a defect. |
| The `auto/*` scoring question | `docs/28` §G | It is **D1**, not a re-litigation. |
| `glm-5.3`'s heavy-reasoner trait: `max_tokens < ~1500` → empty content | ledger **E4** | **A known trait, not a defect.** The ledger says explicitly: *do not re-open it as one*. |
| `Limit.model` scopes *when* a row applies, not *what it counts* | ledger **B4** / register `DEC-03` | Settled by precedent (`docs/23` G3). See §3.3. |
| The `model:` scope gap contributed to the 402 | ledger **D14** / `docs/38` §2.1 | **It did not.** Neither version of the live config carried a `model:` scope. The gap is real in a *different* scenario. |
| `fill-first` ≡ `priority` | ledger **D1e** | Faithful to the reference; left alone on purpose. |
| The MCP stdio transport carries no bearer | ledger **A6** | The pipe is the boundary. |
| A revoked token's own expiry | ledger **A7** | Only the access half is revoked; the refresh half expires on its own. |
| Per-provider `expired` view in the panel | `docs/31` "not in this outline" | The registry has no expiry field; the Health view reports what exists and does not invent an expiry. |
| Charts in the panel | `docs/31` | Out of scope by the website-design skill; no charting dependency to extend. |
| A React/Next port of the panel | `docs/31` | Vanilla JS over one embedded CSS and one embedded JS file, with no build step, because the panel must work on an offline VPS. |
| A per-**combo** budget | `docs/32` (via `docs/kb/WORKLOG.md`) | `usage.combo` records what the client asked for, but two combos may share a target — a combo budget is a second name for the same money. |
| A per-target USD arm | `docs/32` | Every `cost_micros` on the VPS is 0. |
| A "reset the spend" button | `docs/32` | The ledger is the audit trail; the rolling window solves the need. |
| A scheduled registry sweep | `docs/21` T8 (rejected) | **Rejected.** The cadence is an operations choice, not a build. **This is not the stale snapshot** — that is a separate, open item: ledger **C4** (§0.5 **REL-5**, §2 Phase 6). |

---

## 6. The work that is already closed

**Do not redo any of this.** Each row is closed with the evidence the ledger
names. The commit ids are from this repository; the measurement is the
ledger's own.

### 6.1 Closed with a code change

| What | Ledger | Register | Commit | The measurement |
|---|---|---|---|---|
| The routing audit's eleven fixes (the `auto/*` strategies, the missing ranking signals, the LKG pin never read or never cleared, `pipeline` wired to nothing, `random` as round-robin, a client disconnect charged to the provider, unbounded body reads) | **D1**, **D1b**–**D1h** | `GAP-01`, `ROUTE-01`…`ROUTE-07` | `b858a7f` | `docs/29-routing-quality.md` — the live smoke evidence. `auto/cheap` answers 200 with `x-ar-decision: strategy=auto/cheap`. |
| The inbound Responses `function_call` item, and the Ollama / cursor / clova / kiro renderers | **D3** | `UI-10` | `1859dde` | The item is a canonical `ToolCall`; the four renderers now emit declarations and calls. Before it, a tool-calling conversation was a **400 for the whole request**. |
| The MCP HTTP transport's bearer gate | **A6** | `SEC-03` | `b0fa948` | An unauthenticated request → **401** with `WWW-Authenticate: Bearer`, and the body is **not** an MCP response. Five tests. |
| Per-token revocation | **A7** | `SEC-05` | `f829478`, `e414e0b` | `aroute keys revoke --token-file <path>`; the **whole session** dies, on **both** surfaces. Lookup **p95 12.3 µs** against a 1 ms budget. |
| The panel's gate resolved per request, not at boot | **A8** | `SEC-06` | `ec259c6`, `8cd134f` | Arming the gate while the proxy runs turns it on; breaking the master makes it `Broken`. The master re-derives only when the gate row's **ciphertext** changes. Six tests. |
| A "no-op" write that rewrote the config file | **F8** | `UI-09` | `8cd134f` | The write path compares **semantics** on the **unexpanded** documents. Four tests. |
| The panel's own gate, four honest views, a real craft pass | **F1**, **F2**, **F3**, **F4**, **F5**, **F6**, **F7** | `UI-00`…`UI-06` | `d24e854` | The panel carries its own three-state gate; CRAFT **18/20** (bar 16); all 22 token pairs clean. |
| `doctor` reporting the gate from the row's *name* | **A3** | `UI-07` | `d24e854` | The row now **decrypts**: no row → `skip`, decrypts → `ok`, will not decrypt → **`fail`** ("the server is serving UNGATED"). |
| The per-target token ceiling, the per-target failover, the rolling spend window | **D10** | `LIMIT-01` | `8405006`, `ddb6e58`, `b105b9b` | Deployed as **v0.1.20**. The key-wide `SUM` was **0.030–0.056 s**; the grouped query that replaces it is **0.017 s warm** on the VPS's 32 988-row ledger. An over-budget target is **dropped from the chain** and the request is served by a sibling. |
| The context-window floor and the two drifted harness model lists | **F2.1**–**F2.4** | `CTX-01`…`CTX-03` | — | Every affected combo now carries a `context_length:` in the config (the operator-owned place — the registry is **generated**). `~/.local/bin/harness-models-sync` renders the harness lists from the live gateway. |
| The `/ui` exposure, closed on two layers | **A1**, **F1** | `SEC-01`, `UI-00` | `d24e854` (deployed as v0.1.20) | The VPS runs **`aroute 0.1.20`**; an anonymous `POST http://127.0.0.1:20128/ui/api/keys/mint` → **401 `credential_rejected`**; `GET https://aroute.ishanparihar.com/ui/` → **404**. |

### 6.2 Closed by an operational fix

| What | Ledger | Evidence |
|---|---|---|
| The 402 `model_token_cap` on `minimax-m3.1-flash` | **D14** (operational half), **D10** | The guard timer is **masked**; the script is a no-op stub; the removal was an **11-line** diff; the last 402 of that shape is **19:30:02**, 13 s before the final clear, and none since. `docs/38` Phase 1, step by step. |
| The `limits:` block removed from the VPS config | `docs/34` §1 | The operator's ruling (2026-10-20): the deployed system must hold **no** spend budgets. The pre-change file is kept at `~/.config/ar/config.yaml.bak-20261020-limits`. |
| The tunnel's `/ui → 404` ingress rule | **C6**'s operational half, **A1** | The rule is in `~/.cloudflared/hermes-config.yml`, `ingress validate` says **OK**, and the public `/ui` answers **404**. |

### 6.3 Closed — and two things to report

**Two register/ledger mismatches, both structural, and one of them a defect:**

1. **The register's id `GAP-04` was used twice** — once for ledger **D3** (the
   Responses `function_call` item and the four renderers, which is **closed** as
   `UI-10`) and once for ledger **D5** (`kilocode` has no mechanism declared,
   still open). **Fixed 2026-10-21**: the **D3** row was renamed **`GAP-10`** and
   **`GAP-04` belongs to D5**, so the two ids are distinct and closing one
   cannot close the other. The defect was in `TODO.md`, not in the ledger.
2. **Ledger rows with no register row of their own**: **B1** (ask **Q7**),
   **E1**, **E2**, **E3**, **E4** (no register rows at all — the probes live in
   `docs/31` Phase 5), and **A1**'s separate identity. Conversely, register
   rows with no ledger row: **ROUTE-08** (the `cached_prefix_tokens` signal,
   owned by `docs/29` §4) and **GAP-09** (a re-verification note on **D3**, not
   new work). None of these hides work — but a fresh agent counting either file
   alone will get a different number, which is why §1 states both.

---

## 7. The starting procedure

### 7.1 The first three commands

The project's entry contract is `AGENTS.md` §3:

```sh
./scripts/genome wake                      # state, resume point, open register rows, open asks, health
./scripts/genome ask "<question>"          # the owning sections, as path:lines
./scripts/genome close                      # end the iteration: regenerate the genome zones
```

`genome wake` prints the state block, the open register rows, the open operator
asks, the last worklog entries and the health line. `genome ask` returns the
**owning sections with their line ranges** — read only those ranges, not the
whole file. `genome close` ends the iteration and regenerates the genome zones.

**Then, and only then, the two registers:** `docs/28-gap-ledger.md` for the
*state* of an open item, `TODO.md` for the *work item*. Ledger §H item 3 is the
rule: **a row marked done in one and live in the other is a lie** — close them
together, and append the verdict to `docs/kb/CLOSED.md` if it was tried and
rejected.

**Re-verify before you act** (ledger §H item 4): every status in the ledger was
measured on **2026-10-19**, except the rows touched on **2026-10-20** — **A1**,
**A2**, **D10**, **D11**, **D12**, **D13**, **F1**, **F9**, **F10** — and a
parallel session has been working in this repo. A row that says `open` may have
been closed in the last hour.

### 7.2 Where each question is answered

One `genome ask` should be enough. The table gives the question, the
**phrasing that routes well** (measured), and the owning document.

| Your question | Ask it as | The owner |
|---|---|---|
| What is still open? | `genome ask "what is still open"` | `docs/28-gap-ledger.md` §"Why this doc exists" and §H; `TODO.md`'s header |
| What is the state of one item? | `genome ask "<the item's words>"` | `docs/28-gap-ledger.md` — the row that carries the id |
| How do I ship a release? | `genome ask "how do I release a version"` | `docs/35` §0 (the release path, stated once); `docs/RUNBOOK.md`; `docs/36` §4 preamble |
| How do I rotate the gate master? | `genome ask "how do I rotate the gate master"` | `docs/RUNBOOK.md` — "The gate has one master, three names" |
| How do I close an item? | `genome ask "how do I close an item in the ledger"` | `docs/28` §H (the four-step rule) |
| Which cap write paths survive? | `genome ask "which cap write paths survive"` | `docs/35` §7 (the five decisions) and §5 (the sequence) |
| Should the CLI reach everything the panel can? | `genome ask "should the CLI reach everything the panel can"` | `docs/36` §5 DEC-1…DEC-4 and §4 (the eight steps) |
| Which surface reaches the `aroute` hostname? | `genome ask "which surface reaches the aroute hostname"` | `docs/37` §5 (the options), §6 (the control set), §7 (O1), §8 (the position) |
| What happened in the 402 incident? | `genome ask "the 402 model_token_cap"` | `docs/38` §0–§4; `docs/34` §1–§4; ledger **D10**, **D14** |
| What should have stopped the guard? | `genome ask "the organism instruction audit laws"` | `docs/39` §7 (L1–L6) |
| What is a known trait, not a bug? | `genome ask "what is not a gap"` | `docs/28` §G; `docs/kb/CLOSED.md` |
| What did the last session do? | `genome wake` (the LAST WORKLOG block) | `docs/kb/WORKLOG.md` |
| What is deployed on the VPS? | `genome wake` (the STATE block) | `docs/handoff/STATE.md` |

### 7.3 The loop

1. `genome wake` → read the STATE block and the open asks.
2. `genome ask "<question>"` → read **only** the returned ranges.
3. Pick the **first** phase in §2 whose dependencies are met and whose rulings
   are in hand. If a phase needs a ruling (§3.1), **prepare** it and stop — do
   not decide it.
4. Do the work. One shippable step = one release (§0.6). Never scp a build.
5. Close the item in **both** registers, with the **evidence** — the command and
   its output, not a status word (`docs/31`'s gate).
6. `genome log "<entry>"` for the worklog; `genome close` to end the iteration.

---

## 8. What this document does not own

* The **state** of any open item — `docs/28-gap-ledger.md`.
* The **work item** and its verdict — `TODO.md`.
* The **plans** — `docs/35`, `docs/36`, `docs/37`, `docs/38`, `docs/39`. This
  doc points at them; it does not restate them, because a second document that
  restates a first is how the two drift.
* The **current snapshot** — `docs/handoff/STATE.md`.
* The **ruling** on anything in §3 — `docs/kb/ASKS.md` and the doc that owns
  the question.
* The **code** — no Rust file is owned by this document.
