# TODO — the register

One row per work item; rows update in place, verdicts append — never delete.
`genome status <id> <state> --evidence <path>` is the supervised transition.

**The state of every open item lives in one place: `docs/28-gap-ledger.md`.** This file owns
the *work items* (what to do, and in what order); the ledger owns *what is true* and cites
the evidence. A row here that says `open` points at a ledger row that says the same thing
and how it was measured — the two never restate each other. **When you close an item,
change it in both.** The ledger's A–F sections are keyed to these ids.

| id | status | item / verdict | notes |
|---|---|---|---|
| UI-00 | open | **U0 — close the unauthenticated `/ui` write path** (AR-2 ingress 404 + AR-3 write handlers refuse on an unreadable store + the credential-store fix) | blocking; the public write endpoints are reachable with no credential today. `docs/27` §2, `docs/plans/2026-10-19-ui-dashboard-upgrade.md` U0. AR-2 needs a root `systemctl restart cloudflared` (operator's step) |
| UI-01 | open | **U1 — gate `/ui` on the HTTP master** so the panel can be published deliberately (AR-1: bearer gate + paste-a-token field + `?token=` first paint) | needs a release; depends on UI-00 landing first |
| UI-02 | open | **Fix the accent contrast failure** — `--amber #e0900f` as text on `--panel #ffffff` is 2.6:1; `.star.on` and `.minted summary` are the two users | add an `--amber-ink` for the light band (`#8a5a00` = 5.9:1 on white). `docs/27` §4.4, audit F6. Cheap; should not wait for the craft pass |
| UI-03 | open | **U2 — honest states**: four data states per view, errors that persist until dismissed, a write that re-reads and compares | `docs/27` §6 |
| UI-04 | open | **U3 — the operator's missing views**: resilience (breaker/cooldown), provider health/expiry, per-combo metrics, budgets + quota windows | `docs/27` §6 |
| UI-05 | open | **U4 — a settings pane over the yaml** (server, limits.default, timeouts, discovery, auth mode) | byte-exact splices; a wrong value refused naming the field |
| UI-06 | open | **U5 — craft**: h1 to 28–32px, table buttons to 44px, footer paths get a title + copy button, a reviewed light band | CRAFT is 12/20 today; the bar is 16/20 with no zero. `docs/27` §4.3 |
| UI-07 | open | **Fix the doctor gate check** — it reports `gate,ok,armed` from the row's *name* while the row is undecryptable; a store that is present but unreadable must make the check FAIL | `crates/ar-cli/src/commands.rs:1086-1088` says so in its own comment. Audit F3 |
| UI-08 | open | **Rule the canonical credential store on the VPS** and delete the other | `~/.config/ar/credentials.db` (what `serve` opens) vs `~/credentials.db` (what a shell in `$HOME` opens) — desynced today. `docs/27` §3, §7.2 |

### Security and access control (ledger §A)

| id | status | item / verdict | notes |
|---|---|---|---|
| SEC-01 | open | **LIVE EXPOSURE — the public `/ui` mints working tokens.** `POST /ui/api/keys/mint` from the internet returns a signed `execute:completions` token that passes `/v1/models`, `/metrics` and a real `/v1/chat/completions` on the operator's provider keys | the `/ui` router has no credential check of its own, only `loopback_only` (a rebinding check). `POST /ui/api/combos` **rewrote the prod config** when probed. The class is regression **RG-001**. Ledger A1, audit F1/F12 |
| SEC-02 | open | **Every token minted since 2026-10-19 20:07:58 is valid** — the exposure window opened when a parallel session re-armed the gate | rotate the master and re-push to every consumer (`scripts/gate-token.sh push`). Invalidates every harness token, so it is the operator's step. Ledger A2 |
| SEC-03 | open | **The MCP HTTP transport (`aroute mcp --http`) has no bearer gate** — the loopback bind and the scope guard are the only controls | same shape and same law as SEC-01 (RG-001). Lower exposure: stdio is the default. Ledger A6 |
| SEC-04 | open | **`loopback_only` refuses exactly what a browser sends from a public URL** (the tunnel forwards `Origin` while rewriting `Host`) | resolved for the public path by SEC-01's ingress block; the general fix is SEC-01's code change. Ledger A5 |

### Decisions the operator owns (ledger §B — the asks are Q7, Q8, Q9)

| id | status | item / verdict | notes |
|---|---|---|---|
| DEC-01 | open | **Rule the start cap, the model deadline and the failover status** | `STREAM_START_TIMEOUT_CAP` is 110s; a second 110s start still runs past the 120s model deadline, so the two-stall case is unfixed. The failover status is pinned by contract tests. Ledger B2, docs/24 D1–D3 |
| DEC-02 | open | **Rule whether a routable bind may be allowed while the bearer gate is armed** | no shipped path reads `public`; the build stays loopback-only. The in-code seam `TODO(#p1-config-public)` waits on this. Ledger B3, docs/24 G2 |
| DEC-03 | open | **Rule what `Limit.model` scopes** — *when* a row applies, not *what it counts* | recorded in docs/23 G3; the `--model` help text says so. Ledger B4, docs/24 C5 |
| DEC-04 | open | **Rule whether the panel may restart the proxy after a write** | `ConfigHandle::watch` exists in `ar-config` but is not attached (docs/16). Not needed for correctness; much more useful with it. Ask Q9. Ledger B5 |

### Release, soak and operations (ledger §C)

| id | status | item / verdict | notes |
|---|---|---|---|
| OPS-01 | open | **14 commits after `v0.1.17` are unreleased** (`bc622ef` … `196fb94`) | version bump, annotated tag, `release.yml`, then the VPS install. **Never scp a locally built glibc binary** — the 2026-10-19 incident. Ledger C1 |
| OPS-02 | open | **The `failures` table only exists from the release that ships it** — older rows are not backfilled, so the failed-only view starts empty | ships with OPS-01; say it in the release notes. Ledger C2 |
| OPS-03 | open | **No 12-hour soak against a released binary** | `scripts/soak.sh` exists and has run four times (longest partial 51 samples, zero failures). Launch after OPS-01, detached, killed by PID, CSV outside `/tmp`. Ledger C3 |
| OPS-04 | open | **The registry snapshot is 17 days old** (`providerMeta.json` 2026-10-02; `SNAPSHOT_TTL_DAYS` is 7) so `doctor` warns | `aroute import --from omniroute` regenerates it. A scheduled sweep was rejected (docs/21 T8), so the cadence is an operations choice. Ledger C4 |
| OPS-05 | open | **Decide whether `docs-check` gates** | re-verified 2026-10-19: the orphan-page count is now **0** (it was 14 on 2026-10-18) and the README rubric, outward boundary, per-package README contract and genome boundary all pass. Only the gating decision is open. Ledger C5 |
| OPS-06 | open | **The cloudflared ingress change needs a root restart to take effect** | the unit is root's, the config is the user's. `kill -HUP` → not permitted; `PUT` on the tunnel's config API → 200 and a no-op. `sudo systemctl restart cloudflared` is the operator's step. SEC-01 depends on it. Ledger C6 |

### Code gaps in the proxy (ledger §D — not the panel)

| id | status | item / verdict | notes |
|---|---|---|---|
| GAP-01 | done | **Multi-provider `auto/*` pools fell back to config order** → [2026-10-19] `docs/29-routing-quality.md` | the `auto/*` *strategy* name resolved to `Deferred` → a 501, because the two parsers disagreed and the server used the router's answer. `Strategy::Auto(name)` now carries the eight variants, and the server routes them through the virtual factory over the combo's own targets. Ledger D1, `docs/29-routing-quality.md` F5 |
| GAP-02 | open | **Repeat detection of an empty/degenerate 200 is not implemented** (the streamed arm) | the non-streamed arm is closed. The streamed arm waits on a corpus that does not exist: 0 of 4 probes answered, all 504 after 150s. The live failure is the stall (DEC-01), not the empty 200. Ledger D2 |
| GAP-03 | in-progress | **`grok-cli` dispatch: foreign reasoning stripping, tool-schema normalization, custom and namespace tools, per-user headers** | the wire, the identity headers and the body repair are ported; live dispatch is unproven until a login exists. Ledger D4 |
| GAP-04 | open | **`kilocode` has no mechanism declared** — its `oauth/` row is a `fail` | the device flow and anonymous dispatch are ported and the editor header reaches the wire. An operator's YAML choice. Ledger D5 |
| GAP-05 | open | **`zenmux-free` auth is unverified on the Anthropic wire** | inference is locked for this account (403 `access_denied api_key_source: payg`); needs a live key. Ask Q1. Ledger D6 |
| GAP-06 | open | **`bedrock` and `gitlab-duo` route but are not verified live** | built with unit tests and an axum mock, never called against the provider. Needs a Bedrock key + region (Q2) and a GitLab Duo token (Q3). Ledger D7 |
| GAP-07 | open | **A custom `anthropic-compatible` node is named by `doctor` and then refused at dispatch** | the node is real, the wire is not. Decide: implement the wire, or keep the refusal. Ledger D8 |
| GAP-08 | open | **The `poll_state` column for the device flow** | the in-code seam `TODO(#F-MED-2-device)` at `crates/ar-keys/src/store.rs:423`. Ledger D9 |
| GAP-09 | open | **A parallel session is editing `crates/ar-translate/src/translate.rs` (dirty, uncommitted)** | do not start a second pass on that file while it is dirty. Re-verify GAP-09/D3 after it lands. Ledger D3 |
| ROUTE-01 | done |  **Six of twenty-two strategies had no signal to rank on** — `quota`, `in_flight`, `context_window`, `reliability`, `latency_inv` were never populated in production, so `headroom`, `reset-window`, `reset-aware`, `quota-weighted`, `quota-share-fair`, `expiry-first`, `context-optimized` and `cache-optimized` all degenerated to `priority` → [2026-10-19] `docs/29-routing-quality.md` | the quota comes from the store's `quota_snapshots` table, the context window from the registry, the load table from the request's own outcome and latency. `cached_prefix_tokens` is still 0. Ledger D1b, `docs/29` F6–F8|
| ROUTE-02 | done |  **`lkgp`, `context-relay` and `cache-optimized` were unreachable on the request path** — the session never reached the strategy (`pick(strategy, None, …)`) → [2026-10-19] `docs/29-routing-quality.md` | the session and the model now reach `pick_for_model`. The LKG pin was written on every success and never read. Ledger D1c, `docs/29` F1|
| ROUTE-03 | done |  **A stale LKG pin was never cleared** — `LkgpPins::clear` had no caller, so a failed provider stayed pinned for the whole 30-minute TTL → [2026-10-19] `docs/29-routing-quality.md` | the pin is cleared when the pinned target fails, scoped to that provider. Ledger D1g, `docs/29` F2|
| ROUTE-04 | done |  **A client disconnect was charged to the provider** — the typed `ExecError::Aborted` was flattened into a string, so the loop cooled the key, locked the model and ticked the breaker → [2026-10-19] `docs/29-routing-quality.md` | the abort is a typed variant and is charged to nobody. Ledger D1f, `docs/29` F4|
| ROUTE-05 | done |  **`strategy: pipeline` was wired to nothing** — the stages became fallbacks, so the second stage answered the client's question instead of the first stage's → [2026-10-19] `docs/29-routing-quality.md` | it is now a staged dispatch, and the stage answer reads the OpenAI shape as well as the Anthropic one. Ledger D1d, `docs/29` F10|
| ROUTE-06 | done |  **`random` was `round-robin` wearing a different name** — both used `fetch_add % len` → [2026-10-19] `docs/29-routing-quality.md` | `random` is now a Fisher-Yates shuffle, as the reference's is. `fill-first` ≡ `priority` is faithful to the reference and stays. Ledger D1e, `docs/29` F9|
| ROUTE-07 | done |  **Unbounded body reads** — the success, error, failure, judge and pipeline-stage bodies were all read whole before the cap was applied, and the 2 KB scan cap silently scanned the whole body when the cut landed mid-character → [2026-10-19] `docs/29-routing-quality.md` | every cap is now a **read** bound, and the scan cut walks back to a character boundary. Ledger D1h, `docs/29` F3/F11|
| ROUTE-08 | open | **`cached_prefix_tokens` is still always 0**, so `cache-optimized` still degrades to `context-relay` and then to `priority` | the signal needs the cache layer to publish the prefix length it computes; that is a change to `ar-cache`, not to the router. `docs/29` §4 |
