# Dashboard recovery + runtime performance + red-team report — 2026-10-23

Status: **measured + fixed.** The dashboard was down after the system restart. Three
defects were found and fixed; it runs now and is verified on every surface a
browser touches. Includes the load profile the operator asked for, the
config-fragmentation measurement, and a red-team report on redundancy,
over-engineering, entropy, fragmentation and implementation gaps.

Companion to the surface audit: [`2026-10-22-dashboard-surface-audit.md`](2026-10-22-dashboard-surface-audit.md).
Ledger rows **F14–F16**, register rows **DASH-08…DASH-11** are new here.

---

## 0. Why it was down — three defects, not one

| # | Defect | Evidence | Fix |
|---|---|---|---|
| 1 | The preview unit was **transient** (`systemd-run`), so the reboot deleted it | `systemctl --user list-units` after the boot: no `ar-dash-preview`, and the persistent `aroute-dashboard.service` was **disabled** with a stale `[Install]` comment explaining the default | `systemctl --user enable --now aroute-dashboard.service` |
| 2 | The unit's binary was an **Oct-19 copy** with none of the audit's fixes | `stat`: `~/.local/bin/aroute` 2026-10-19 vs `target/release/aroute` 2026-10-23; `strings … \| grep -c NEXT_PUBLIC_LIVE_WS_PUBLIC_URL` → **0 vs 1** | rebuilt, backup at `~/.local/bin/aroute.bak-*`, atomic swap (temp + `mv`) |
| 3 | The unit resolved a **second, stale dist** — root-built, no `/dash`, no shim patch | the verb's resolution chain (`dashboard.rs:100-121`) picks `$AR_DASHBOARD_DIR` → a dist beside the binary → **`~/.config/ar/dashboard`** → repo. `readlink /proc/<pid>/cwd` → `~/.config/ar/dashboard`, mtime **2026-10-07**, `basePath` absent, no `healthz` route; vs the audited repo dist 2026-10-22 | stale dist parked as `~/.config/ar/dashboard.stale-20261007-*`; the patched dist installed atomically (`.new` + `server.js` assert + `mv`) |

**Then a fourth, live bug appeared once the patched dist booted:** the dashboard
started answering `/dash/healthz` with **HTTP 200 while** the event loop was stalled
for 40–55 s** — the journal's own words: *"HTTP 200 is not healthy; busy != ready."*
Warm healthz went 10.3 s → 0.05 s → **timeout** → 22.7 s. That is §1 below.

**A fifth, latent, gap the reboot exposed:** `NEXT_PUBLIC_LIVE_WS_PUBLIC_URL` — it was not in the launch-env passthrough list, so the
variable I measured as *the* LiveWS fix (`docs/41` §2, control row 15) never reached
the child when aroute launches the dashboard itself. Fixed in
`crates/ar-cli/src/dashboard.rs` (two tests, `cargo test -p ar-cli dashboard` green:
25 passed). **This was a real implementation gap between the audit and the shipped
launcher, not a dist defect.**

---

## 1. Performance / load profile — what it costs now

Measured live from `/proc` and the unit journal, 2026-10-23, restored dist with
**88 provider connections** and 7 API keys in the canonical store.

### Boot (the expensive part)

| Metric | Value | Note |
|---|---|---|
| Boot stall count | **44** event-loop stalls, magnitudes **21.1–55.8 s** | all logged with `inflight=0` — **synchronous CPU work, not awaited I/O** |
| Peak RSS (VmHWM) | **1 466 428 kB (1.43 GB)** | the 1024 MB V8 cap is exceeded by **RSS**, not the heap |
| Peak RSS wall time | ~8 min of pegged CPU before it settled | `ps`: 87–95% CPU, single core |
| First-request `/dash/healthz` | 200 in **59.6 s** | Next route compilation on top of the restore |
| First-request `/dash/` | 308 in 0.018 s | redirect to the canonical `/dash/dashboard`, which serves 200 in 90 ms |
| Crashes / restarts this session | **0 / 0** | no OOM, no fatal error; `NRestarts=0` |

The stalls are the **boot restore** — `[ConnectionRecovery] proactively restored 14
connections`, then 88 connections × model discovery (one line reported `✓ 388 models`,
others 84/38/14), credential health checks over OAuth accounts, and model sync against
providers that answer 502/504. `inflight=0` on every stall line is the proof it is
CPU-bound parsing/serialisation, not the network.

### Steady state (after the restore completed)

| Metric | Value |
|---|---|
| RSS (VmRSS) | **106 MB** (from 1.0 GB; the earlier sample's 253 MB was mid-settle) |
| Swap (VmSwap) | **410 MB** and **flat over 60 s** (t+15/30/45/60 s all identical) — cold pages from the boot peak, not an active leak |
| Instantaneous CPU | **0% over a 5 s window**, state `Sl` (sleeping) — idle is genuinely idle |
| `/dash/healthz` warm | **3–8 ms** (5 samples: 7.7, 3.2, 3.2, 4.0 ms) |
| `/dash/dashboard` | **200 in 90 ms** |
| Anonymous `POST /dash/v1/chat/completions` | **401** in 43 ms (gateway closed) |
| Stalls in 3 min after settle | **0** |
| Browser end-to-end | banner **absent**, LiveWS **CONNECTED**, client `/api/health/ping` **200** |

**Attribution warning, so this is not misread:** the host's load average during this pass
was **11.41 / 14.20 / 12.62** — but that belongs to three `python3` processes
(206–212% CPU each, ~7.9 GB RSS each), not to aroute. For contrast on the same box:
the **proxy** runs at **1% instantaneous CPU / 88 MB RSS**, comfortably inside the
21.7 MB idle budget's order of magnitude (peak 148 MB under load is its own VmHWM).
The dashboard is the expensive half, and the proxy is not the problem. |

### Verdict, honestly

The dashboard is **healthy but has a violent first 10 minutes after boot**. Every
surface is correct once the restore finishes — 3 ms healthz, working WS, working SSE
— but during restore a browser sees multi-second stalls and occasional 30 s timeouts.
The upstream watchdog (`OMNIROUTE_EVENT_LOOP_STALL_THRESHOLD_MS`, default 5000,
`src/lib/healthzLag.ts`) correctly names the state: *"busy != ready."*

The available throttle knobs (untested — **judgement, flagged as such**):
`HEALTHCHECK_STAGGER_MS`, `HEALTHCHECK_BATCH_SIZE`, `HEALTHCHECK_INTERVAL_MS`,
`HEALTHCHECK_STARTUP_DEFERRED_ENABLED`, `MODEL_SYNC_INTERVAL_HOURS`. Deferring the
first health-check/model-sync pass and widening the stagger should trade a slower
"fully warm" time for an immediately usable UI. That is a candidate change, not a
measurement.

---

## 2. Config fragmentation: web-UI vs Rust binary — measured

Three stores, one sanctioned bridge, and one direction.

| Store | Written by | Contains |
|---|---|---|
| `~/.config/ar/storage.sqlite` (13.6 MB, 140 tables) | the **web dashboard** (the config surface): 88 `provider_connections`, 7 `api_keys`, combos, limits | OAuth/native credentials as `enc:v1:` ciphertext |
| `~/.config/ar/config.yaml` (26.8 KB) | `aroute sync` (derived from the DB) **and hand edits** | `combos:`, `custom_providers:`, `providers:`, `keys:` |
| `~/.config/ar/ar.env` (23 KB) | `aroute sync` (provider keys) **and hand edits** | **337 `AR_KEY_*` lines** the dashboard never sees |

`aroute sync --help` states the model outright: the dashboard's SQLite is the
configuration surface, and the verb rewrites `combos:`/`custom_providers:` and
materialises provider keys. **One-way, DB → YAML/env.** Measured with
`--no-restart` on backed-up files (`/tmp/*.pre-sync`):

- **0 API keys materialised, 0 referenced vars ensured** — against **337 `AR_KEY_*` lines**
  sitting in `ar.env`. The two auth models are not merely non-interoperable, they are
  **disjoint in practice**: the dashboard's 88 connections are OAuth/native, so the
  proxy's static-key world is invisible to it and vice versa.
- **170 lines of hand-edited YAML pruned, 31 added.** The removed lines were ad-hoc
  `custom_providers` (`or-decisions`, `exp-systemone`, …) and legacy combos pointing at
  models that no longer exist downstream; the additions are the same combos re-derived
  in the DB's canonical shape. Correct per the design — but see §3.
- No key loss (337 before and after), no service restart.

**How it is "managed", concretely:** the dashboard is the source of truth; `sync` is
a re-derivation, not a merge; hand-entered YAML outside the dashboard is **transient**
whether anyone intended that or not.

---

## 3. Red-team report

### 3.1 Implementation / feature gaps (highest severity first)

| # | Gap | Evidence | Impact |
|---|---|---|---|
| **R1** | **The call-log artifact worker was never traced into the dist** | journal: `[callLogs] Call-log artifact worker failed: Cannot find module '/home/ishanp/.config/ar/dashboard/src/lib/usage/callLogArtifactWorker.js'` — every 30 s. The file exists only as `.ts` in the sibling OmniRoute checkout (`src/lib/usage/callLogArtifactWorker.ts`), and the candidate resolver (`chunks/src_0ln7259._.js`) probes `callLogArtifactWorker.js` next to the chunk, then 3 source-relative paths, then `.ts` + tsx — none of which exist in a standalone | **call-log detail artifacts are never persisted** on any installed dist. The queue is capped (128) and the warn is throttled to 1/30 s, so it degrades silently rather than failing loudly |
| **R2** | **`sync` has no dry-run** | `aroute sync --help`: `--no-restart` prints the restart command but the files are still rewritten | 170 lines were pruned on my exploratory run. An operator exploring the bridge silently rewrites their routing config. A `--dry-run` is the obvious missing row |
| **R3** | **`NEXT_PUBLIC_LIVE_WS_PUBLIC_URL` was dropped by our own launcher** | absent from `dashboard.rs`'s passthrough list before this pass; `strings` on the shipped binary had 0 occurrences | the one variable that revives LiveWS (docs/41 control 15) **did nothing** when launched by the verb. Fixed + 2 regression tests |
| **R4** | **The dist-resolution chain silently prefers a stale copy** | `dashboard.rs:100-121` guesses `$AR_DASHBOARD_DIR` → dist beside the binary → `~/.config/ar/dashboard` → repo, first hit wins. `~/.config/ar/dashboard` was 15 days stale | the audit was verified against the repo dist while the unit served a different one. No staleness check, no sentinel comparison, no warning |
| **R5** | **Provider icon catalog is basePath-blind** | client chunks carry `image:"/providers/codex.svg"` in a data object; the browser requests `/providers/*` at the ROOT while the assets exist at `/dash/providers/*` (measured: dash 200, root 404). `installBasePathFetch` patches only `globalThis.fetch`/`EventSource`, not `<img src>` | cosmetic: icons render broken on a `/dash` mount. `favicon.svg`/`icon-512.png` are **absent from the build entirely** (404 at both spellings) |

### 3.2 Redundancy and over-engineering

- **Three copies of the same Node runtime footprint.** The repo holds a 1.6 GiB
  `dashboard/dist`, the unit now runs `~/.config/ar/dashboard` (another 1.6 GiB, of
  which 306 MB is `node_modules`), and the parked stale dist adds a third. The
  vendored dist is the single largest artifact in the repo and it is `gitignore`d —
  so the only authoritative copy is one of the *unknowable* ones. The delivery
  recipe in `docs/41` §4 (a node_modules-free tarball) exists but is unexecuted.
- **`webUI` vs dashboard overlap, and the stores prove it.** `docs/16` §6-B's
  convergence answer is unchanged by today's findings, but the config measurement
  adds evidence: 337 `AR_KEY_*` lines + 20+ config.YAML settings on one side, 88
  OAuth connections + 7 dashboard keys on the other, and a **lossy one-way bridge**
  between them that materialises zero of the dashboard's credentials (deliberately —
  the ciphertext refuses to cross).
- **Env plumbing is tiny, invisible, and load-bearing.** One name determines whether
  the live feed works at all (R3), the wrapper silently rewrites `HOSTNAME` (ledger
  F12), and `REQUIRE_API_KEY`'s name differs from the `OMNIROUTE_*` convention around
  it. Three separate ways to spell "loopback" were needed for one bind.

### 3.3 Entropy / things that have already rotted

- The stale dist is the strongest example: a system produced by an old install, still
  self-consistent, silently outranked by nothing — the resolution chain just preferred
  it. Nothing detects this until the 404s.
- 170 lines of dead `config.yaml` combos referencing non-existent models, kept alive
  only by nobody running `sync`.
- `aroute.bak-*` and `dashboard.stale-*` are new entropy I created deliberately
  (reversibility), and should be parked or deleted once the operator is satisfied.

---

## 4. What the operator should do next (ranked)

1. **Test it now** — `http://127.0.0.1:20149/dash/dashboard` (it serves 200 in 90 ms
   once boot settles; ignore the first-request stall, or wait ~10 min after a boot).
   Give it ~10 minutes of quiet before judging latency.
2. **Decide the boot-stall trade** — try `HEALTHCHECK_STARTUP_DEFERRED_ENABLED=1` and
   a wider `HEALTHCHECK_STAGGER_MS`; this is a judgement call flagged as untested.
3. **File R1 upstream** — the untraced `callLogArtifactWorker` is a vendoring gap
   (the release/standalone build drops `src/`); either trace it into the dist or drop
   the feature.
4. **Add `--dry-run` to `sync`** (R2) and a staleness check to the dist resolution
   (R4).
5. **If the dashboard is published**, the cosmetic 404s (R5) want the upstream icon
   fix; they do not block a local or published `/dash`.

**Gates for this pass, all re-run:** `cargo build --release` clean;
`cargo clippy --release -p ar-cli --all-targets --locked -- -D warnings`
clean; `cargo test --release -p ar-cli dashboard` 25 passed; the browser
probe green after the fix. Genome lint/close and the commit follow in the
same pass.
