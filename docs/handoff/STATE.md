# State

Current snapshot only (no history: that is the worklog). The first `## ` block is
what `genome wake` shows.

## State — (date)

Date: 2026-10-19. Branch `main` @ `e8192ff` (the routing work below). The proxy runs
**only** on the VPS (`racknerd`), public at
`https://aroute.ishanparihar.com/v1` behind a cloudflared tunnel; the gate is
armed and every harness holds a non-expiring token.

**This iteration made the ROUTER production-grade.** The request: fix the route
implementation gaps and issues, and make the routing production-grade like OmniRoute's
but tailored to this system. Outcome: **code changed** — eleven fixes in
`crates/ar-route`, `crates/ar-server` and `crates/ar-exec`, each with a regression test
that was run against the unfixed code and failed there. The rationale and the evidence
are in [`docs/29-routing-quality.md`](../29-routing-quality.md); the state of each item
is in [`docs/28-gap-ledger.md`](../28-gap-ledger.md) §D1b–D1h.

The audit's core finding: **of twenty-two strategy names, the reachable routing surface
was roughly seven distinct behaviours.** The comparators were correct; the *signals*
they rank on were never populated. `quota`, `in_flight`, `context_window`,
`reliability` and `latency_inv` were `0`/`None` on every production request, so
`headroom`, `reset-window`, `reset-aware`, `quota-weighted`, `quota-share-fair`,
`expiry-first`, `context-optimized` and `cache-optimized` all degenerated to
`priority` — a comparator that maximises a signal nobody sets is `priority` in
disguise. Two names were exact duplicates (`fill-first` ≡ `priority`, `random` ≡
`round-robin`), and one (`pipeline`) was wired to nothing.

What changed, in the order it matters:

1. **The session now reaches the strategy** (`ROUTE-02`). `ar-server` called
   `pick(strategy, None, …)` — a hardcoded `None` — so `lkgp`, `context-relay` and
   `cache-optimized` were unreachable on the request path, while their unit tests
   passed. The LKG pin was written on every success and **never read**.
2. **The quota signal exists** (`ROUTE-01`). The credential store has carried a
   `quota_snapshots` table since it was ported, with a reader; nothing ever asked. Now
   the config path reads the newest reading and puts it on the target, so six strategies
   have a signal for the first time.
3. **The load signal exists** (`ROUTE-01`). The server observes each outcome and
   latency; the attempt loop tracks a live in-flight count and releases it on every
   exit. `p2c` was reading three constant inputs, so every candidate scored identically.
4. **A client disconnect is charged to nobody** (`ROUTE-04`). `ar-exec` had a typed
   `ExecError::Aborted`; the bridge flattened it into a string, so the loop cooled the
   key, locked the model and ticked the breaker for a cancelled request.
5. **A stale pin is cleared** (`ROUTE-03`). `LkgpPins::clear` existed with no caller,
   so a failed provider stayed pinned for the whole 30-minute TTL.
6. **`pipeline` is a staged dispatch** (`ROUTE-05`). The server had no
   `Strategy::Pipeline` branch, so a pipeline combo's stages became *fallbacks*. Wiring
   it surfaced a second bug: the stage answer read only the Anthropic shape, so a
   pipeline against an OpenAI provider could never have run two stages.
7. **`auto/*` is routable** (`ROUTE-01`/`GAP-01`). The two parsers disagreed about the
   name and the server used the router's answer, so a combo naming a strategy this
   build ships answered **501**.
8. **Every body read is bounded** (`ROUTE-07`): the success, error, failure, judge and
   pipeline-stage bodies are capped **at the read**, not only at the slice; and the 2 KB
   scan cap no longer silently scans the whole body when the cut lands mid-character.
9. **`random` is a shuffle** (`ROUTE-06`), as the reference's is. `fill-first` ≡
   `priority` is *faithful* to the reference and stays.

**Still open** (`ROUTE-08`): `cached_prefix_tokens` is always 0, so `cache-optimized`
still degrades to `context-relay`. The signal needs the cache layer to publish the
prefix length it already computes — a change to `ar-cache`, not to the router.

**Not yet deployed.** The routing work is committed on `main` but the VPS still runs
`v0.1.17`; it ships with the next release (register `OPS-01` — 14 commits were already
unreleased before this, so the release carries both). The live smoke that proved the
fixes ran against a **scratch instance** over the operator's own config and keys, not
against the deployed proxy.

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
| the work items, and their state | `TODO.md` (32 open: `UI-00`…`UI-08`, `SEC-01`…`SEC-04`, `DEC-01`…`DEC-04`, `OPS-01`…`OPS-06`, `GAP-01`…`GAP-09`) |
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

<!-- genome:zone status sha=70415ace5f46 -->
| status | items |
|---|---|
| open | 31 |
| in-progress | 1 |
| done | 8 |

- `UI-00` [open] **U0 — close the unauthenticated `/ui` write path** (AR-2 ingress 404 + AR-3 write handlers refuse o
- `UI-01` [open] **U1 — gate `/ui` on the HTTP master** so the panel can be published deliberately (AR-1: bearer gate
- `UI-02` [open] **Fix the accent contrast failure** — `--amber #e0900f` as text on `--panel #ffffff` is 2.6:1; `.sta
- `UI-03` [open] **U2 — honest states**: four data states per view, errors that persist until dismissed, a write that
- `UI-04` [open] **U3 — the operator's missing views**: resilience (breaker/cooldown), provider health/expiry, per-co
- `UI-05` [open] **U4 — a settings pane over the yaml** (server, limits.default, timeouts, discovery, auth mode)
- `UI-06` [open] **U5 — craft**: h1 to 28–32px, table buttons to 44px, footer paths get a title + copy button, a revi
- `UI-07` [open] **Fix the doctor gate check** — it reports `gate,ok,armed` from the row's *name* while the row is un
- `UI-08` [open] **Rule the canonical credential store on the VPS** and delete the other
- `SEC-01` [open] **LIVE EXPOSURE — the public `/ui` mints working tokens.** `POST /ui/api/keys/mint` from the interne
<!-- genome:end status -->
