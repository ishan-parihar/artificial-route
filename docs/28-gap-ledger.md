# 28 — The gap ledger: every OPEN bug, decision and implementation gap, in one place

Status: **consolidated 2026-10-19.** Owner: this doc, for the *state* of every open item.
Companions: `TODO.md` (the register — the *work items*, which point here for detail),
[`docs/29-routing-quality.md`](29-routing-quality.md) (the **routing audit** and the
eleven fixes, with the live smoke evidence),
`docs/27-dashboard-contrast-and-upgrade-plan.md` (the `/ui` audit and contrast),
`docs/24-remaining-gaps-outline.md` (the **execution record** of the 2026-10-18 pass),
`docs/21-remaining-gaps.md` (the register that pass read), `docs/07-blockers.md` (provider
blockers), `docs/26-benchmarks-parity-gaps.md` (parity).

**Why this doc exists.** The open work was spread over five documents and the register, with
no row carrying a status that was re-verified against the tree. A reader asking "what is
actually still open?" had to reconcile `docs/21` (2026-10-17), `docs/24` (2026-10-18),
`docs/07`, `docs/26` and `TODO.md` by hand, and several of those rows had gone stale.
This doc is the **one answer to that question**: every row below was re-verified against
the working tree on 2026-10-19, and the row says how.

**Ownership rule (rule 1, one owner per question).** This doc owns *the state of an open
item*. `TODO.md` owns *the work item* — a register row per item, updated in place, with a
verdict appended. The two never restate each other: the register row says what to do and
points here; this doc says what is true and cites the evidence. A doc that used to own a
question (`docs/21`, `docs/24`) now owns the *history* of that pass, and points here.

**Status vocabulary** (the register's): `open`, `in-progress`, `done`, `rejected`,
`blocked`. A row that says `blocked` names **what** blocks it and **who** can unblock it.

---

## A. Security and access control (the ones that cost money)

| # | item | status | what is true now | unblock |
|---|---|---|---|---|
| **A1** | **The public `/ui` write endpoints are unauthenticated** | **open — LIVE EXPOSURE** | The `/ui` router has no credential check of its own; its only layer is `loopback_only`, a DNS-rebinding defence (`crates/ar-cli/src/webui.rs:145-146`). The `/v1` gate (`routes::authorize`, `crates/ar-server/src/routes.rs:2193`) is not applied to `/ui`. From the internet, `POST /ui/api/keys/mint` returns **200 with a signed `execute:completions` token**, and that token passes `/v1/models`, `/metrics` and a **real `/v1/chat/completions`** on the operator's provider keys (measured 2026-10-19 20:2x). `POST /ui/api/combos` **rewrote the prod config** when probed. The class is recorded as regression **RG-001**. | The cloudflared ingress rule that 404s `/ui` is **staged and validated** on the VPS (`~/.cloudflared/hermes-config.yml`, `path: ^/ui(/.*)?$`); it needs `sudo systemctl restart cloudflared` (operator). Then the code fix: gate `/ui` on the HTTP master (`UI-01` in the register). |
| **A2** | **Every token minted since 2026-10-19 20:07:58 is valid** | **open** | The exposure window opened when a parallel session re-armed the gate (the store file's mtime is 20:07:58; the row is now a readable `enc:v2` envelope). The audit's own probe tokens (`audit-probe`, `audit-verify`, `audit-verify-2`, `blast-radius-test`, `timing-probe-1..3`, `verify-home-store`, `verify-cfg-store`, `verify-cfg-store-2`) all carry `execute:completions` and are valid until the master changes. | Rotate the master and re-push to every consumer (`scripts/gate-token.sh push`). **This invalidates every harness token, so it is the operator's step.** |
| **A3** | **`aroute doctor` reports the gate as armed from the row's *name*, not from whether it decrypts** | open | Verified in the code and on the box: `crates/ar-cli/src/commands.rs:1086-1088` says so in its own comment. A store that is present but unreadable still lists as `gate,ok,armed`. The VPS's boot log carried `ar: gate row present but unreadable — serving ungated` on **nine consecutive boots** (18:38 → 19:50) while `doctor` said armed. A store that is present and unreadable must make the check **fail**. | Code, in `commands.rs`. Register row `UI-07`. |
| **A4** | **Two credential stores on the VPS, desynced** | open (partly resolved) | `AR_CRED_STORE` is **unset** in `~/.config/ar/ar.env`, and the unit has no `WorkingDirectory`, so `store_path` (`commands.rs:113-124`) resolves `credentials.db` **beside the config** for `serve` and **`./credentials.db`** for a shell whose CWD is `$HOME`. Two files, same `http-gate` row, different envelopes. **As of 20:2x both decrypt with the current master** (the parallel re-arm fixed the config-relative store) and `doctor` on the VPS reports `store,ok` and `gate,ok,armed`. The *trap* remains: one file is authoritative and the other is a stale copy that nothing points at. | The operator's ruling on which is canonical (ask **Q8**), then delete the other and set `AR_CRED_STORE` explicitly. |
| **A5** | **`loopback_only` refuses exactly what a browser sends from a public URL** | open | The rule refuses a request whose `Host` **or** `Origin` names a non-loopback authority (`crates/ar-server/src/app.rs:680-701`). The tunnel rewrites `Host` to loopback but forwards `Origin`, so a browser's cross-origin `fetch()` and its CSS-driven font requests are refused while a script's silence is admitted. Measured: the public `/ui/fonts/*.woff2` → **403** (both faces fail; `probe.mjs` 6 FAIL vs 0 FAIL on loopback); a browser `POST /ui/api/*` → **403**; `curl` → **200**. | Resolved for the *public* path by A1's ingress block. The general fix is A1's code change plus a decision on whether `loopback_only` should ignore `Origin` when `Host` is loopback. |
| **A6** | **The MCP HTTP transport has no bearer gate** | open | `aroute mcp --http <ADDR>` serves StreamableHTTP at `/mcp` behind the `mcp` feature, loopback only: `ar_mcp::serve_http` refuses a routable listener and rmcp's default `Host` allow-list is loopback. **The loopback bind and the scope guard are the only controls** — there is no credential check. | Same shape as A1, and the same law (RG-001): a route that changes state needs a credential. The MCP surface is stdio by default, so this is lower exposure. |

---

## B. Decisions the operator owns (blocking code)

| # | item | status | what is true now | unblock |
|---|---|---|---|---|
| **B1** | **Should `/ui` be reachable from the internet at all?** | open | The blast radius of a mistake differs by two orders of magnitude between "SSH-forward only" (needs no code) and "publish it, gated" (needs a release and a token-in-the-browser flow). | Operator ruling, ask **Q7**. Register rows `UI-00`, `UI-01`. |
| **B2** | **The start cap, the model deadline and the failover status** | blocked | `STREAM_START_TIMEOUT_CAP` is 110s (`crates/ar-exec/src/lib.rs:155`); a second 110s start still runs past the 120s model deadline, so the two-stall case is not fixed. The model-aware JSON 504 already wins over the bare 504 (`c3bf93a`, `c7a841a`). The failover status is pinned by contract tests. | Operator ruling (docs/24 D1–D3). |
| **B3** | **May a routable bind be allowed while the bearer gate is armed?** | open | No shipped path reads `public`; `ServerConfig::from_env` is the only reader and it is test-only. The build stays loopback-only, and the three refusal texts now say what the build does. The in-code seam `TODO(#p1-config-public)` is text-corrected and waits on this. | Operator ruling (docs/24 G2). |
| **B4** | **`Limit.model` scopes *when* a row applies, not *what it counts*** | open | Unchanged; the choice is recorded in docs/23 G3 and the `--model` help text now says so. | Operator ruling (docs/24 C5). |
| **B5** | **Should the panel be able to restart the proxy after a write?** | open | Today the panel shows the restart command. `ConfigHandle::watch` exists in `ar-config` but is not attached (docs/16), so a restart needs that wiring or a systemd call. Neither is needed for correctness; both would make the panel much more useful. | Operator ruling, ask **Q9**. |

---

## C. Release, soak and operations

| # | item | status | what is true now | unblock |
|---|---|---|---|---|
| **C1** | **Commits after `v0.1.17` are unreleased** | open | Verified 2026-10-19: `Cargo.toml` is `0.1.17`, `v0.1.17` is the newest tag, and **14 commits** are unreleased on `main` (`bc622ef` … `196fb94`). The VPS runs the released `0.1.17` asset. | The operator's go — a version bump, an annotated tag, `release.yml`, then the VPS install. **Never scp a locally built glibc binary** (the 2026-10-19 incident). |
| **C2** | **The `failures` table only exists from the release that ships it** | open | `CREATE TABLE IF NOT EXISTS failures` (`crates/ar-tokens/src/ledger.rs:49`); older rows are not backfilled, so the failed-only view starts empty. | Ships with C1. Say it in the release notes. |
| **C3** | **No 12-hour soak against a released binary** | blocked (on C1) | `scripts/soak.sh` exists and has run four times — longest partial 51 samples, +996 KB, zero failures — but no 12-hour run has completed against a **released** binary. Two earlier attempts ended before acceptance (one operator-killed at 54 min, one host-wide SIGBUS at 1h52m), corrected in `5d7d8e8`. | Launch after C1, detached, killed by PID, CSV outside `/tmp`. |
| **C4** | **The registry snapshot lags OmniRoute's rotation** | open | `crates/ar-registry/src/providerMeta.json` is dated **2026-10-02** (17 days old at the time of writing), and `SNAPSHOT_TTL_DAYS` is 7. `doctor` warns past the TTL. `aroute import --from omniroute` regenerates it. A second scheduled sweep was **rejected** (docs/21 T8), so the cadence is an operations choice. | An operations decision: a weekly `aroute import`, or accept the warning. |
| **C5** | **`docs-check` does not gate** | open | `.git/hooks/pre-commit` runs `docs-check --staged --mode=warn` and does not gate. **Re-verified 2026-10-19: the orphan-page count is now 0** — the D003 orphans the 2026-10-18 pass counted (14) are gone, and the README rubric, the outward boundary, the per-package README contract and the genome boundary all pass. The only open question is whether to make it blocking. | Operator decision (docs/24 B6). |
| **C6** | **The cloudflared tunnel reload needs root** | open | The unit is a **root** system unit (`/etc/systemd/system/cloudflared.service`) reading a **user-owned** config. Editing the config needs no privilege; reloading it does: `kill -HUP` → `Operation not permitted`; `PUT http://127.0.0.1:20241/config` → 200 and a **no-op** (the runtime rules are unchanged). The only way is `sudo systemctl restart cloudflared`. Ingress rules are checkable without root: `cloudflared tunnel --config <file> ingress validate` and `... ingress rule <url>`. | The operator's one command. A1 depends on it. |

---

## D. Code gaps in the proxy (not the panel)

| # | item | status | what is true now | unblock |
|---|---|---|---|---|
| **D1** | **Multi-provider `auto/*` pools fall back to config order** | **fixed 2026-10-19** | The `auto/*` **strategy** name resolved to `Deferred` → a 501, because `ar-config` parsed it as `Auto` and `ar-route` as `Deferred` and the server used the router's answer. `ar-route` now carries `Strategy::Auto(name)`, the eight variants resolve, and the server routes them through the virtual factory over the combo's **own** targets. The `auto/*` **model** name already worked. **Live:** a `strategy: auto/cheap` combo answers 200 (`x-ar-decision: strategy=auto/cheap`). | — |
| **D1b** | **Six of twenty-two strategies had no signal to rank on** | **fixed 2026-10-19** | `quota`, `in_flight`, `context_window`, `reliability` and `latency_inv` were never populated in production, so `headroom`, `reset-window`, `reset-aware`, `quota-weighted`, `quota-share-fair`, `expiry-first`, `context-optimized` and `cache-optimized` all degenerated to `priority`. The quota now comes from the store's `quota_snapshots` table, the context window from the registry, and the load table is written from the request's own outcome and latency. `cached_prefix_tokens` is still 0. | See `docs/29-routing-quality.md` §4 for what is still open. |
| **D1c** | **`lkgp`, `context-relay` and `cache-optimized` were unreachable on the request path** | **fixed 2026-10-19** | `ar-server`'s `order()` called `pick(strategy, None, …)` — a hardcoded `None` — so the session never reached the strategy. The LKG pin was written on every success and **never read**. **Live:** with one session key over a live and a dead target, the attempts went 2 → 1 → 2 (a different session), which is the pin being honoured and session-scoped. | — |
| **D1d** | **`strategy: pipeline` was wired to nothing** | **fixed 2026-10-19** | The server had no `Strategy::Pipeline` branch, so a pipeline combo went through the ordinary attempt loop where its stages became *fallbacks*. It is now a staged dispatch, and the stage answer reads the OpenAI shape as well as the Anthropic one. | — |
| **D1e** | **`random` was `round-robin` wearing a different name** | **fixed 2026-10-19** | Both used `fetch_add % len`. `random` is now a Fisher-Yates shuffle, as the reference's is. `fill-first` ≡ `priority` is **faithful** to the reference and is left alone. | — |
| **D1f** | **A client disconnect was charged to the provider** | **fixed 2026-10-19** | `ar-exec` had a typed `ExecError::Aborted`; the bridge flattened it into a string, so the loop cooled the key, locked the model and ticked the breaker for a cancelled request. | — |
| **D1g** | **A stale LKG pin was never cleared** | **fixed 2026-10-19** | `LkgpPins::clear` existed with no caller, so a pinned provider that failed stayed pinned for the whole 30-minute TTL. | — |
| **D1h** | **Unbounded body reads** | **fixed 2026-10-19** | The success body, the error body, the failure body, the judge body and the pipeline stage body are all capped **at the read**, not only at the slice. The 2 KB scan cap no longer silently scans the whole body when the cut lands mid-character. | — |
| **D2** | **Repeat detection of an empty/degenerate 200 is not implemented** | open | `docs/22-stream-and-failover-repair.md:73` says so. The streamed arm is the live gap: the first-byte gate sees bytes, not content, and a stream cannot fail over after it commits. The non-streamed arm is closed (one classified-empty failover per request, docs/22 D5). | It waits on a corpus that does not exist yet: the 2026-10-18 probe of `free-stack` returned **0 of 4** answers, all four 504 `upstream_timeout` after 150.0–150.1s. The open failure is the **stall** (B2), not the empty 200. |
| **D3** | **Inbound Responses `function_call` items, and the Ollama / cursor / clova / kiro renderers** | partly built | Tool declarations and `tool_choice` now cross every dialect that has them, and tool history crosses (an Anthropic `tool_use` or Gemini `functionCall` reads as a canonical `ToolCall`). **Landed 2026-10-19 (`22c6413`):** an Anthropic block that names no modality — `thinking`, `tool_addition`, `server_tool_use`, `container_upload`, `web_search_tool_result` — is now **dropped** instead of rejected with `unsupported content part type: not a routed modality` (a 400 for the whole request). It is dropped rather than carried because a carried part is re-spelled verbatim for the upstream wire and an OpenAI-shaped upstream answers it with its own 400, so carrying moved the failure downstream and failed over the whole target chain. Verified live: a Claude Code session whose history carried 965 thinking blocks now compacts and answers; the nine Anthropic block types all answer 200. The Responses `function_call` item and the Ollama / cursor / clova / kiro renderers remain open. | Code, in `crates/ar-translate/src/translate.rs`. |
| **D4** | **`grok-cli` dispatch: foreign reasoning stripping, tool-schema normalization, custom and namespace tools, per-user headers** | open (partly built) | The Responses wire, the Grok Build identity headers and the body repair are ported. The four items above are not, and live dispatch is unproven until a login exists. | The reference must be on disk first. |
| **D5** | **`kilocode` has no mechanism declared** | open | Its `oauth/` row is a `fail` whose fix names the YAML for either mechanism. The device flow and anonymous dispatch are ported and the editor header now reaches the wire from the catalog row. The reference has no kilocode-specific executor, so there is no further wire to port. | An operator's YAML choice. |
| **D6** | **`zenmux-free` auth is unverified on the Anthropic wire** | open | `zenmux-free` renders and posts the Messages body with the `anthropic-version: 2023-06-01` header, and auth is `Authorization: Bearer` (not `x-api-key`, because the Claude OAuth path shares the wire). **Not verified: no key to test with** — zenmux has inference locked for this account (403 `access_denied api_key_source: payg`). | A live key. Ask **Q1**. |
| **D7** | **`bedrock` and `gitlab-duo` route but are not verified live** | open | Both are built with unit tests and an axum mock over HTTP. `bedrock` authenticates with a Bedrock API key as `Authorization: Bearer` (no SigV4) against `POST {base}/model/{id}/converse[-stream]`, and decodes the `application/vnd.amazon.eventstream` frame stream. `gitlab-duo` posts to the `code_suggestions` monolith with a pasted token. Neither has been called against its provider. | A Bedrock key and region, and a GitLab Duo token. Asks **Q2** and **Q3**. |
| **D8** | **A custom `anthropic-compatible` node is accepted and named by `doctor`, then refused at dispatch** | open | The name-then-refuse shape is deliberate (the node is real, the wire is not). | A decision on whether to implement the wire or keep the refusal. |
| **D9** | **The `poll_state` column for the device flow** | open | The in-code seam `TODO(#F-MED-2-device)` at `crates/ar-keys/src/store.rs:423`: `poll_state TEXT NULL` is not there. | Code, in `ar-keys`. |

---

## E. Probes that need a live provider (not code)

| # | item | status | what is true now | unblock |
|---|---|---|---|---|
| **E1** | **The live `cline` stream has not been re-probed since docs/22** | open | Needs a live request through the released build. | A released binary (C1) and a live cline key. |
| **E2** | **The cline `{success,data}` envelope unwrap** | open | The 2026-10-18 probe of the API-key cline row found plain OpenAI SSE (44 frames, usage, `[DONE]`) and **no envelope**. | Waits for an **OAuth** cline session that streams the envelope. Not reachable with the API-key rows. |
| **E3** | **`kimi-k3` returns empty content about half the time** | open | Recorded as B2 in `docs/07-blockers.md` with the isolation steps. | A live probe with the current stack. |
| **E4** | **The `glm-5.3` heavy-reasoner trait** | open, **not a bug** | `max_tokens < ~1500` → empty content. Recorded in `AROUTE_BUGS.md` and in `docs/07`. | A known trait, not a defect. **Do not re-open it as one.** |

---

## F. The panel (`/ui`) — the full set

Detail in `docs/27-dashboard-contrast-and-upgrade-plan.md`; the plan in
`docs/plans/2026-10-19-ui-dashboard-upgrade.md`; the audit in
`docs/audit/2026-10-19-ui-dashboard-contrast.md`. Register rows `UI-00` … `UI-08`.

| # | item | status | what is true now |
|---|---|---|---|
| **F1** | **Close the unauthenticated write path** (`UI-00`) | open, **live exposure** | A1 above. The ingress block is staged; the code fix (`AR-3`: the write handlers must refuse when the gate master is unreadable) is not written. |
| **F2** | **Gate the panel on the HTTP master** (`UI-01`) | open | Not written. Needs a bearer gate on the `/ui` router group, a paste-a-token field, `sessionStorage`, `Referrer-Policy: no-referrer`, and a `?token=` first-paint fallback. |
| **F3** | **The accent contrast failure** (`UI-02`) | open | `--amber #e0900f` as text on `--panel #ffffff` is **2.6:1** (body text needs 4.5:1). `.star.on` (`crates/ar-cli/src/webui/page/app.css:674`) and `.minted summary` (`app.css:584`) are the two users, so **a starred model is less legible than an unstarred one**. The dark band already has an `--amber-ink` (`#8a5a00`, 5.9:1 on white). `contrast.py` does not catch it — it pairs tokens, not token-on-ground. |
| **F4** | **Honest states** (`UI-03`) | open | The vocabulary exists (`stateRow(cols, text, cls)` with `.loading` / `.empty` / `.failed`, `app.js:763-765`) but the populated and loading states are not distinct, and an error auto-hides after **8 seconds** (`crates/ar-cli/src/webui/page/app.js:46`) — a failed write can vanish before it is read. A write also reports `{"ok":true}` without re-reading the file. |
| **F5** | **The operator's missing views** (`UI-04`) | open | No resilience view (breaker and cooldown state per connection — the thing the failover rail implies), no provider health or expiry, no per-combo metrics, no budgets, no quota windows. |
| **F6** | **A settings pane over the yaml** (`UI-05`) | open | `server`, `limits.default`, the timeouts, model discovery and the auth mode are CLI-only edits today, and every CLI-only edit is a typo waiting to happen. Byte-exact splices, and a wrong value refused naming the field. |
| **F7** | **Craft** (`UI-06`) | open | CRAFT **12/20**, no zero (the bar is 16). C1 scale **0** (`scale=1.6x`: the h1 is 21px over a 13.5px body — the `type-scale` warn every probe run has carried since phase A), C3 composition **1**, C7 rhythm **1**. The table buttons are 27px against a 44px recommendation (WCAG 2.5.8's 24px passes). The light band is the **default** rendering and had never been screenshotted; both bands are now captured. |
| **F8** | **A "no-op" write still rewrites the file** | open | `POST /ui/api/combos` is a whole-section replace and the YAML renderer emits block-style lists where the hand-written file used flow-style, so posting the config's own combo list changed 39 lines and the `identical` short-circuit (`webui.rs:604`) did not fire. Semantically identical, not byte-identical. **Any write-path test must compare semantics, not bytes** — a test that saves "the same thing" will dirty the config. |

---

## G. Not gaps — recorded so they are not re-litigated

Moved to `docs/kb/CLOSED.md` (append-only) or kept in the doc that owns them. The ones that
keep coming back:

- `/metrics` label cardinality cap; `x-ar-model` carrying the client's spelling;
  trace-channel lossiness and redb's exclusive lock; mid-stream failover after content
  flowed; pipeline/fusion semantics.
- The breaker tripping under repeated probing (`503 every provider in the chain is
  cooling down`) is the system working.
- `~/bin/ar` shadowing `/usr/bin/ar` — resolved; the binary is `aroute`.
- Discovery's memory cost (34 MiB config-only → 120 MiB with 9 715 models) is documented
  in the unit, not a defect.
- The `auto/*` scoring question is D1, not a re-litigation.

---

## H. How to work this list

1. **Read this doc before starting anything.** It is the state of every open item.
2. **Work the register**, not this doc: `TODO.md` has a row per work item, and
   `genome status <id> <state> --evidence <path>` is the supervised transition.
3. **When you close something,** change its status here **and** update the register row
   **and** append the verdict to `docs/kb/CLOSED.md` if it was tried and rejected. A row
   marked done here with a live register row is a lie; a register row closed with no entry
   here is invisible.
4. **Re-verify before you act.** Every status in this doc was measured on 2026-10-19; a
   parallel session was working in this repo at the same time, so re-check the state you
   are about to act on. A row that says `open` may have been closed in the last hour.
5. **A ruling the operator owns is not a task.** B1–B5 are in `docs/kb/ASKS.md` as Q7–Q9
   (and the older asks Q1–Q3 for the live provider keys). Record the answer with
   `genome q rule <id> "<ruling>" --applied <owner path>` and put it in the doc that owns
   the question.
