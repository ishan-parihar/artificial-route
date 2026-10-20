# 35 — Cap-surface removal plan: closing every path that can put a `limits:` row back

Status: **plan, written 2026-10-20.** No code changed by this document. The
operator ruled on 2026-10-20 that the deployed system must hold **no** spend
budgets or limits; the VPS `config.yaml` already carries no `limits:` block
([`docs/34-spend-cap-audit.md`](34-spend-cap-audit.md) §1). This doc answers
the open question the audit raised ([`docs/34`](34-spend-cap-audit.md) §5):
**which code paths can put a ceiling back, and which of them should be
removed.**
Owner: this doc, for the *removal plan*.
Companions: [`docs/34-spend-cap-audit.md`](34-spend-cap-audit.md) (the audit
that raised the question and why the block was there at all),
[`docs/32-per-model-token-budgets.md`](32-per-model-token-budgets.md) (the
design of the arms, and the failover behaviour worth keeping),
[`docs/15-policy-security-layer.md`](15-policy-security-layer.md) (the policy
layer's decision record — the vocabulary "arms", "cap", "deny reason"),
[`docs/33-vps-limits-config.md`](33-vps-limits-config.md) (the ceilings the VPS
ran, superseded by the audit),
[`docs/28-gap-ledger.md`](28-gap-ledger.md) (the state of every open item),
[`docs/36-cli-agentic-workflow-plan.md`](36-cli-agentic-workflow-plan.md) (the
CLI's agent-facing gaps — the same `limits:` writers seen from the agent's
side), [`docs/37-dashboard-deployability-plan.md`](37-dashboard-deployability-plan.md)
(the dashboard's own spend surface, and the `limits:` block `aroute sync`
would re-create on a host that has one),
`TODO.md` (the register).

---

## 0. The release path, stated once

Every change below ships as a **GitHub release**, never as a copied binary.
The sequence is: commit the change → bump the workspace version in
`Cargo.toml` (`Cargo.toml:7`, currently `0.1.20`) and refresh `Cargo.lock` →
a `chore(release): vN` commit → an annotated tag `vN` → push `origin main`
and the tag → `.github/workflows/release.yml` builds and publishes the static
musl asset → install on the VPS over `ssh racknerd`.

**Never `scp` a locally built binary to the VPS.** The host's glibc is 2.36
and a glibc build fails there; the published asset is the only sanctioned
install path. Every step in §5 and every verification in §8 assumes this and
does not restate it.

## 1. The problem, stated precisely

"Two surfaces can put a cap back" is not a slogan. In code terms it is:
**there are five write paths in the tree that can write a `limits:` row into
`~/.config/ar/config.yaml`, and the deployed config currently has no
`limits:` block.** The engine enforces any row a config carries, so any one
of these five paths re-arms a ceiling on the next restart — silently, because
none of them asks the operator.

The five paths, exhaustively:

| # | path | entry point | what it writes | notes |
|---|---|---|---|---|
| 1 | **the CLI verb** `aroute limits set` | `crates/ar-cli/src/commands.rs:703` (`limits_set`), declared at `crates/ar-cli/src/cli.rs:410` | one row, all seven arms (`--rpm`, `--usd-micros`, `--tokens`, `--refuse-unpriced`, `--model`, `--model-tokens`, `--spend-window`; `crates/ar-cli/src/cli.rs:423-474`) | the only CLI verb that writes a `limits:` row. `--key` omitted ⇒ the `default:` row (`crates/ar-cli/src/commands.rs:767-770`) |
| 2 | **the panel's per-key form** | `POST /ui/api/limits` → `set_limits`, `crates/ar-cli/src/webui.rs:342` (route), `:1975` (handler) | `limits.keys[<key>]` — all seven arms | the browser form at `crates/ar-cli/src/webui/page/app.js:819-839`, submitting to `/ui/api/limits` at `app.js:833-836` |
| 3 | **the panel's default-ceilings form** | `POST /ui/api/settings` → `save_settings`, `crates/ar-cli/src/webui.rs:336` (route), `:1286` (handler); the `limits` branch at `:1341-1349` | `limits.default` — **only four arms** (`rpm`, `usd_micros`, `tokens`, `refuse_unpriced`; `limit_from` at `crates/ar-cli/src/webui.rs:1379-1442` has no `model`, `model_tokens` or `spend_window` branch) | the form is `limits-default-form` at `app.js:1456-1465`, submitted at `app.js:1538-1546` |
| 4 | **the panel's mint form's `rpm` field** | `POST /ui/api/keys/mint` (`crates/ar-cli/src/webui.rs:341`, handler `:1857`) mints the token; the JS then calls `POST /ui/api/limits {key, rpm}` — `app.js:853` reads the field, `app.js:856` issues the call | a per-key row carrying **only** `rpm` | the mint handler itself writes no limits; the *form* does, one request later |
| 5 | **`aroute sync`'s rpm carry** | `crates/ar-cli/src/commands.rs:2126-2151` (`read_rpm` → `limits.keys.entry(name).or_default().rpm = Some(value)` at `:2147-2149`, then `splice_section` at `:2150`) | a `limits:` block — **only the `rpm` arm** — keyed by the dashboard's `api_keys.name` | the reader is `crates/ar-cli/src/import/limits.rs:27-50`; the whole `if !rpm.is_empty()` block at `commands.rs:2128` is the only gate |

Two asymmetries in that table are worth stating plainly, because they shape
the options in §4:

- **The Budgets pane is read-only.** `GET /ui/api/budgets` → `budgets`
  (`crates/ar-cli/src/webui.rs:335`, handler `:1117-1194`) reads `cfg.limits`,
  `Ledger::spend` and the quota snapshots, and writes nothing — no
  `write_file`, no `splice_section` in the handler. The front end
  (`app.js:1314-1347`, `paneBudgets`) renders two tables and issues exactly one
  request, `GET /ui/api/budgets` (`app.js:1356`): no form, no submit handler,
  no POST. **It is a view, not a writer.**
- **The `default:` row is settable only through `/ui/api/settings`.**
  `set_limits` writes `cfg.limits.keys.entry(key)` (`webui.rs:2118`), so a body
  naming `{"key":"default"}` creates a *per-key row literally named
  `default`* — a different row from `limits.default`. There is therefore no
  single endpoint that owns "the default row", and the two rows are edited
  through two different forms with two different arm sets (four arms on the
  default row, seven on a per-key row).

**The blast radius of a panel write** is what makes paths 2–4 different in
kind from path 1 and path 5:

1. The write lands in the file immediately — `splice_section` → `write_file`
   (`webui.rs:2143-2149`; `commands::write_file` is backup rotation + atomic
   write, `crates/ar-cli/src/commands.rs:2596-2599`). The response is
   `{"ok":true,"pending_restart":true}` (`webui.rs:2155`).
2. The panel reflects it at once, because it re-reads the **live file** — the
   state handler calls `load_cfg` (`webui.rs:1450-1469`), which reads and
   re-parses `config.yaml` on every request, and the payload serialises that
   freshly-loaded `cfg.limits` (`webui.rs:767`).
3. **The running proxy is NOT affected.** `Policy` is built once at boot
   (`crates/ar-server/src/app.rs:396-400`) and is never rebuilt;
   `ConfigHandle::watch` exists (`crates/ar-config/src/lib.rs:1787-1834`) but
   nothing attaches it. A restart is required, and a restart is also the only
   way to clear it.
4. The `pending_restart` banner (`webui.rs:761`) is the only signal that the
   file and the running proxy disagree.

So the dangerous window is the pre-restart one: the file says the key is
capped, the panel shows the cap, and the proxy serves the key at full rate.

**Why the write paths are worth removing at all**, given the config is
already clean: `docs/34` §3 established that the `limits:` block was never an
operator decision — it was an audit probe's `tokens: 1` fixture, later
inflated, left armed on the live key, and it refused every request on the
box. The ruling that followed was about the *deployed config*. A path that
can silently re-create that block is how the same accident happens again,
without anyone deciding it.

## 2. What the engine actually enforces

`Policy` is constructed once at boot: `Policy::from_limits(&self.config.limits, armed)`
at `crates/ar-server/src/app.rs:396-400`, where `armed = self.ledger.is_some()`
(`app.rs:396`), stored as an `Arc<Policy>` on `AppState` (`app.rs:233`).
`Policy::disabled()` exists (`crates/ar-server/src/policy.rs:70-76`) but has
**no production caller** — the server always calls `from_limits`.

### 2.1 The arms, arm by arm

| arm | read at | enforced at | status + `x-ar-deny-reason` |
|---|---|---|---|
| `rpm` | `policy.rs:84-88` `rate_wait` → `limits.for_key(key_id).rpm`; token bucket from `ar-limit` (`crates/ar-limit/src/lib.rs:44` `Spec::per_minute`, `:86` `check`) | `crates/ar-server/src/routes.rs:590-602` — the 429 arm, after `authorize`, before the body is parsed | **429**, body `code: "rate_limited"`, header `retry-after: <seconds>`; `x-ar-decision: strategy=none` (`routes.rs:3854-3857`). **No `x-ar-deny-reason`** — the 429 arm does not stamp the deny header |
| `usd_micros` | `policy.rs:100-114` `key_cap` → `Cap.usd_micros` | `routes.rs:806-843` → `Ledger::admit_against` (`crates/ar-tokens/src/ledger.rs:889-920`, dollar arm at `:904-912`) | **402**, `error.type: "billing_error"`, `code: "payment_required"`, `reason: "usd_cap"`, header **`x-ar-deny-reason: usd_cap`** (`routes.rs:840`; the header const is `routes.rs:92`) |
| `tokens` | same | `routes.rs:806-843` → `ledger.rs:913-918` | **402**, `reason: "token_cap"`, header **`x-ar-deny-reason: token_cap`**; message `ledger.rs:332-335` |
| `refuse_unpriced` | `policy.rs:105` — alone it arms a `Cap` | `ledger.rs:897-899` — checked **before** the "no arm set ⇒ Allow" early return at `:900-902` | **402**, `reason: "unpriced"`, header **`x-ar-deny-reason: unpriced`**; message `ledger.rs:336-339` |
| `model` (scope) | parsed/validated/rendered/displayed (`crates/ar-config/src/lib.rs:1200-1210`; `commands.rs:737-740`; `webui.rs:2096-2117`; `app.js:835`) | **nowhere** — see §2.2 | **inert** |
| `model_tokens` (per-target) | `policy.rs:128-134` `target_cap`, `:141-143` `has_target_caps` | `routes.rs:848-859` (`filter_by_target_budget`, `routes.rs:2842-2870`) and `routes.rs:1652`/`:1772` (`budget_filtered_candidates`, `routes.rs:2897-2950`) | a per-target refusal **fails over** to a sibling; only an empty chain is **402** with `reason: "model_token_cap"`, header **`x-ar-deny-reason: model_token_cap`** (`routes.rs:857`) |
| `spend_window` | `policy.rs:150-152` → `ar_config::Limit::spend_window` (`ar-config/src/lib.rs:1292-1295`), `parse_window` (`:1303-1317`) | not a denial itself — it selects **which** spend figure the two spend arms compare against: `routes.rs:772-773` (windowed vs cumulative, key-wide arm), `routes.rs:2848` and `:2910-2911` (per-target arm) | changes the measurement, not the verdict |

Summary: **`ar-server` enforces `rpm`, `usd_micros`, `tokens`, `refuse_unpriced`
and `model_tokens`, and consumes `spend_window`.** `ar-route` enforces none
of them — the router is budget-blind, exactly as `docs/32:270` states.
`ar-tokens` owns the comparison (`admit_against`, `ledger.rs:889-920`) and the
data (`key_caps` table, `ledger.rs:44-48`), but the `key_caps` table is **not
consulted by the request path**: the cap comes from config
(`policy.rs:100-114`) and the call is `admit_against`, not `admit`
(`routes.rs:823-829`). The `key_caps` **table** is read in production only by the
MCP tool `ar_check_quota` (`crates/ar-mcp/src/tools.rs:456-460`); `Ledger::cap`
is also reachable from `Ledger::admit` (`ledger.rs:845-855`), but the request
path calls `admit_against` instead. `Ledger::set_cap`
has **no production caller** outside tests (`ledger.rs:705`).

One detail that matters for any blast-radius estimate: the `rpm` bucket is
**in-memory** and resets on restart, and `rpm` is **not** gated on `armed` — a
config with no ledger still gets a working rate limit.

### 2.2 Two engine gaps — OUT OF SCOPE for this plan

These are separate defects, found by the same recon. They are **not** part of
the cap-surface removal, they are **not** fixed by it, and each deserves its
own issue. They are recorded here so the removal does not get blamed for them
and so they do not get lost.

**Finding 1 — the `model:` scope is never applied at request time.**
`Limits::for_request` (`crates/ar-config/src/lib.rs:1172-1178`, using
`Limit::applies_to` at `:1272-1274`) is the scope-filtering accessor, and it
has **no production caller**: a search across `crates/` finds it only in
`ar-config/src/lib.rs` and its own tests (`:1945`, `:1952`, `:1959`). The
policy layer uses the **unscoped** accessors instead — `for_key`
(`policy.rs:85`), `row_for` (`policy.rs:104`, `:130`, `:142`, `:151`).
**A row scoped to `model: jev` therefore applies to every model that key asks
for.** Why it matters: `docs/32:55-59` describes the scope as functional, and
`docs/15` treats the scope as gating the spend arms — so an operator who
scopes a row believes the ceiling is narrower than it is. It is a
correctness bug in the arm, independent of whether any ceiling should exist.
*Carried forward as an open item — see §9.*

**Finding 2 — the media routes consult no policy at all.**
`crates/ar-server/src/media.rs` (`json_route_inner`, `:227-276`) calls
`authorize`, `require_json` and the lane `admit` — but no `rate_wait`, no
`key_cap`, no `target_cap`. The media family is registered at
`crates/ar-server/src/app.rs:640-653`: `/v1/embeddings`,
`/v1/audio/transcriptions`, `/v1/audio/translations`,
`/v1/images/generations`, `/v1/ocr`. **Every arm is bypassed on those
routes.** Why it matters: it means a `limits:` block, wherever it comes from,
is not a complete statement of what the proxy refuses — so "the config has no
limits block" does not imply "no route is capped" once a block returns. It
also means the arm set was never uniformly enforced, which bears on the
question of whether the arms are trustworthy as a capability.
*Carried forward as an open item — see §9.*

## 3. What stays and what goes

### 3.1 What stays, and why

| thing | where | why it stays |
|---|---|---|
| **the engine arms** — `ar_config::Limit`'s seven fields and `Limits` | `crates/ar-config/src/lib.rs:1127` (`Limits`), `:1185` (`Limit`) | The per-target failover behaviour is **correct and tested**: a target that has spent its own budget is dropped from the chain and the request continues on a sibling with budget left (`routes.rs:2842-2870`). The operator's ruling of 2026-10-20 was about the **deployed config**, not about removing the capability. The **spend** arms are inert without a `limits:` block — they are gated on `armed` (`policy.rs:101-103`) — and `docs/34` §4 says so explicitly: the v0.1.20 *code* is correct and tested; it is the *config* that carries no cap. (`rpm` is the exception: it is not gated on `armed`, so a config with no ledger still gets a working rate limit — §2.1.) Removing the arms is a further release with a far larger blast radius (§4, Option 4). |
| **the `aroute limits` VIEW verb** | `crates/ar-cli/src/commands.rs:621` (`limits_view`) | A read-only inspection of what a config would enforce. It writes nothing. It is how an operator checks the state of a config they inherited — which is exactly the failure mode `docs/34` §3 describes. Removing it removes the ability to confirm the ruling was carried out. |
| **the panel's read-only Budgets pane** | `crates/ar-cli/src/webui.rs:335` (route), `:1117-1194` (handler); `app.js:1314-1347` | It writes nothing (§1). It is the panel's view of the same truth the CLI view gives. |
| **the MCP `ar_check_quota`** | `crates/ar-mcp/src/tools.rs:456-460` | It reads the ledger and decides nothing — the module comment says so in as many words ("Reads the ledger; it decides nothing"). It is the tool a harness asks "what have I spent?" with. It is also the **only** reader of the `key_caps` table, which is otherwise dead weight in the request path. |

The reasoning common to all four: **the ruling was about the deployed
configuration, not about the capability.** Deleting correct, tested
enforcement machinery in response to a config mistake is the wrong trade — it
makes the mistake unrecoverable (a future `limits:` block would have to be
rebuilt from scratch) without making it less likely. The mistakes to fix are
the ones where a *write path* can put a ceiling back **without the operator
deciding to**.

### 3.2 What goes

| thing | where | why it goes |
|---|---|---|
| the panel's **per-key limits form** (path 2) | `crates/ar-cli/src/webui/page/app.js:742-757` (the Limits section), `:776-841` (`renderKeys`), the form at `:819-839`; the route at `crates/ar-cli/src/webui.rs:342` and the handler `:1975-2156` | It is the surface `docs/34` §5 names: "the one surface that can put a ceiling back **without editing a file**". A browser form is not a deliberate operator tool. |
| the panel's **default-ceilings form** (path 3) | `app.js:1451-1465` (the form), `:1497-1502`, `:1537-1546` (the submit handler); `webui.rs:1341-1349` (the `limits` branch of `save_settings`) and `limit_from` at `webui.rs:1379-1442` | Same reason. It is also the only writer of `limits.default`, so removing it removes the `default:` row's only write path — the `default` row then has exactly one writer left, the CLI. |
| the panel's **mint form's `rpm` field** (path 4) | `app.js:731-732` (the input), `:853` (the read), `:856` (the follow-up `POST /ui/api/limits`) | It is a hidden second writer of the same endpoint: the user mints a token and, without knowing it, a `limits:` row appears. |
| **`aroute sync`'s rpm carry** (path 5) | `crates/ar-cli/src/commands.rs:2126-2151`; the reader `crates/ar-cli/src/import/limits.rs:27-50` | The **implicit** writer. It creates a `limits:` block out of a dashboard table nobody thinks of as a budget, with no operator action at all. The dashboard's `max_requests_per_minute` becomes advisory only. |
| **the `limits` key in the panel's state/settings payloads** — *decision pending, see §7* | `webui.rs:767` (state), `:1243-1246` (settings) | The panel needs to *display* ceilings to be useful; it does not need to *carry* them in a form-editable payload. Keeping the display and dropping the editor is the clean cutover. |

**What does not go:** `aroute limits set` (path 1) — see §7, decision 1.

## 4. The options

The recon ranked five, lowest risk first. The ranking is preserved here
verbatim.

### Option 1 — Delete the `sync` rpm carry *(lowest risk)*

- **Change.** Remove the `if !rpm.is_empty()` block at
  `crates/ar-cli/src/commands.rs:2126-2151` (and the `carried` counter at
  `:2127` and its print at `:2187-2189`); delete
  `import::limits::read_rpm` (`crates/ar-cli/src/import/limits.rs:27-50`).
  `import::limits::render` **stays** — it is still used by `limits_set`,
  `limits_clear`, `set_limits` and `save_settings`.
- **Blast radius.** The dashboard→proxy bridge stops carrying
  `api_keys.max_requests_per_minute`; the dashboard's rpm becomes advisory.
  **No live behaviour changes on the VPS today**: there is no dashboard DB
  there (`docs/34` §2 row 5), and `sync` refuses outright at
  `commands.rs:2079-2084` before touching the config.
- **Could break.** The "sync owns dashboard-carried rpm" invariant documented
  at `docs/16-dashboard-unity-plan.md:31-32` and `:70-72`; the test
  `sync_carries_dashboard_rpm_per_arm_and_leaves_the_rest`
  (`crates/ar-cli/src/commands.rs:4089-4184`) must be **deleted or
  inverted**; `docs/19-ui-upgrade-plan.md` and `docs/34:151-154` need
  updating.
- **Verification.** `cargo test -p ar-cli`; grep that `read_rpm` has no
  callers; re-read `docs/16` and amend the invariant.

### Option 2 — Remove the panel's Limits pane and the default-ceilings form

- **Change.** Delete the route `/ui/api/limits`
  (`crates/ar-cli/src/webui.rs:342`) and `set_limits` (`:1975-2156`); delete
  the `limits` branch in `save_settings` (`:1341-1349`) and `limit_from`
  (`:1379-1442`); in the front end delete the Limits section
  (`app.js:742-757`), `renderKeys` (`:776-841`), the default-ceilings form
  (`:1451-1465`, `:1497-1502`, `:1537-1546`) and the mint `rpm` field
  (`:731-732`, `:853-856`); drop `limits` from the state/settings payloads
  (`webui.rs:767`, `:1243-1246`) or keep it read-only. `limit_json`
  (`:1258-1270`) becomes unused if the payloads drop `limits`.
- **Blast radius.** The panel can no longer create or edit any ceiling. The
  CLI becomes the only writer. The Budgets pane can keep *displaying* ceilings
  that a hand-written config carries — it is read-only and harmless.
- **Could break.** The panel's own limits tests
  (`crates/ar-cli/src/webui.rs:3105-3120` and `:3433-3575` — five
  limits-specific tests) and `scripts/ui/panel-check.mjs` /
  `panel-armed-check.mjs` if they exercise the endpoints.
- **Verification.** `cargo test -p ar-cli`; grep `/ui/api/limits` across
  `scripts/`, `docs/`, `dashboard/`.

### Option 3 — Remove the CLI `aroute limits set`

- **Change.** Remove `LimitsCommand::Set` (`crates/ar-cli/src/cli.rs:410`,
  args `:423-474`) and `limits_set` (`crates/ar-cli/src/commands.rs:703-825`).
- **Blast radius.** The operator loses the sanctioned way to arm a ceiling.
  `aroute limits` (view) and `limits clear` become vestigial;
  `import::limits::render` loses one of its callers.
- **Could break.** `docs/15-policy-security-layer.md`,
  `docs/23-ui-round-two-plan.md:145`, `docs/32:269` and
  `docs/33-vps-limits-config.md:105` all describe the verb; roughly 15 CLI
  tests (`crates/ar-cli/src/commands.rs:3753-4086`) die.
- **Verification.** `cargo test -p ar-cli`; re-read every doc that names the
  verb.

### Option 4 — Disarm the engine at the source *(highest guarantee, medium blast radius)*

- **Change (small).** Make `Policy::key_cap` return `None` unconditionally
  (`crates/ar-server/src/policy.rs:100-114`) and `Policy::rate_wait` return
  `None` unconditionally (`policy.rs:84-88`). One small diff, two functions,
  and the 402/429 arms become unreachable regardless of what any config
  carries.
- **Change (large, clean cutover).** Delete `Limit`/`Limits` from `ar-config`
  (`crates/ar-config/src/lib.rs:1127`, `:1185`), the `limits` field from
  `Config` (`ar-config/src/lib.rs:1118`) and `ServerConfig`
  (`crates/ar-server/src/config.rs:452`, `:818`), the whole
  `crates/ar-server/src/policy.rs`, the `ar-limit` crate, `ar_tokens::Cap` /
  `DenyReason` / `key_caps` (`crates/ar-tokens/src/ledger.rs:44-48`,
  `:245-264`, `:269-286`, `:705-740`, `:889-920`), and the panel/CLI writers.
- **Blast radius (small version).** The enforcement disappears; the config
  block may still parse and be displayed, so the panel/CLI can still *write* a
  ceiling that is never enforced — misleading, and the project's "no shims"
  rule argues against it.
- **Blast radius (large version).** Six crates, every doc that mentions
  `limits:`, and `docs/15`, `docs/32`, `docs/33`, `docs/34`.
- **Could break.** The failover-on-per-target-budget behaviour
  (`docs/33-vps-limits-config.md:117` records a live probe where a
  `model_tokens` cap on one model made the request fail over to a sibling) —
  that behaviour is worth keeping, per `docs/34:144-147`.
- **Verification.** `cargo test --workspace`; the `ar-server` policy tests
  (`crates/ar-server/src/policy.rs:216-393`) and the routes tests
  (`crates/ar-server/src/routes.rs:7039-7083`) will need deletion or
  inversion.

### Option 5 — Combination (the recon's recommended sequence)

1. **Option 1** (delete the sync rpm carry) — smallest diff, removes the
   implicit writer.
2. **Option 2** (remove the panel's Limits pane + default ceilings) — removes
   the browser writer, the surface `docs/34:148-150` names.
3. Leave `aroute limits set` and the engine arms alone: the operator keeps a
   deliberate, documented, CLI-only way to arm a ceiling, and the arms stay
   correct and tested (`docs/34:144-147`).
4. If the operator wants **no** ceiling possible at all, follow with
   **Option 4-large** as a separate release.

### RECOMMENDATION: **Option 5**

Option 5 preserves the recon's ranking exactly — it is Option 1 then Option 2
then a deliberate decision to stop — and it is the option whose blast radius
matches the ruling. The reasoning:

1. **It targets exactly the accident that happened.** `docs/34` §3 established
   that the block on the live key was an audit fixture, not a decision. The
   two writers that could repeat that accident without an operator deciding
   are the **browser form** and the **implicit sync carry**. Both are in
   Option 5. The CLI verb is not — it is a person typing a command.
2. **It preserves the capability that is correct.** The per-target failover
   is the one behaviour worth having (`docs/34:144-147`); Option 4 destroys
   it, Option 5 keeps it.
3. **Its steps are independently shippable and independently reversible.**
   Option 1 and Option 2 touch disjoint files and disjoint tests; each is one
   release, and each can be reverted by reverting its tag.
4. **It leaves the operator with a way to arm a ceiling deliberately.**
   `aroute limits set` is a deliberate operator tool — the difference between
   it and a browser form is the difference between a decision and an accident.

**One precondition, stated plainly — and it is now SATISFIED.** Removing the
panel's Limits pane is only meaningful once the panel itself is gated, or the
blast radius of "a browser user sets a cap" drops from "an anonymous browser
user sets a cap" to "a token holder sets a cap".

**Measured on the VPS, 2026-10-20, after the v0.1.20 install** (the earlier
2026-10-19 build was ungated — an anonymous `POST /ui/api/keys/mint` returned
`200` plus a spendable token; that is closed):

| probe | result |
|---|---|
| `aroute --version` on the VPS | `aroute 0.1.20` |
| anonymous `POST http://127.0.0.1:20128/ui/api/keys/mint` | **401** `credential_rejected` |
| `GET https://aroute.ishanparihar.com/ui/` | **404** (the tunnel's `^/ui(/.*)?$` rule is live) |

So the blast radius of a panel write today is "**a token holder** sets a cap",
not "an anonymous browser user sets a cap". The precondition holds, and the
removal of the panel's Limits pane is meaningful as written.

**The plan does not depend on that being true**, and should not be read as
assuming it. The tree's `ui_guard` (`crates/ar-cli/src/webui.rs:402-453`) is
what the deployed v0.1.20 runs, and the measurement above confirms it is
armed — but a later downgrade of the VPS binary, or a re-arm of the gate into
`Unarmed`, would restore the anonymous writer. Removing the writer regardless
is the right posture either way; the measurement is a fact about today's
build, not a precondition the plan leans on. See §9.

## 5. The recommended sequence — discrete shippable steps

Each step is its own release. Each names the change, the verification, and
the rollback. The release path is §0.

### Step 1 — Delete the `sync` rpm carry

- **Change.** Remove the `if !rpm.is_empty()` block at
  `crates/ar-cli/src/commands.rs:2126-2151`, the `carried` counter at
  `:2127` and its print at `:2187-2189`; delete `read_rpm`
  (`crates/ar-cli/src/import/limits.rs:27-50`). Keep `render`
  (`import/limits.rs:58-112`).
- **Verification.** `cargo test -p ar-cli`; a grep that `read_rpm` has no
  callers; the inverted test from §8; amend the invariant in
  `docs/16-dashboard-unity-plan.md:31-32` and `:70-72`; update
  `docs/34:151-154`.
- **Rollback.** Revert the tag and re-release; or re-apply the block from the
  previous tag. No config migration — sync stops writing a section it had
  stopped writing on a machine with no dashboard DB anyway.

### Step 2 — Remove the panel's Limits pane, the default-ceilings form, and the mint `rpm` field

- **Change.** `crates/ar-cli/src/webui.rs`: delete the route at `:342` and
  `set_limits` (`:1975-2156`); delete the `limits` branch of `save_settings`
  (`:1341-1349`) and `limit_from` (`:1379-1442`). `app.js`: delete the Limits
  section (`:742-757`), `renderKeys` (`:776-841`), the default-ceilings form
  (`:1451-1465`, `:1497-1502`, `:1537-1546`), and the mint `rpm` input
  (`:731-732`) with its read (`:853`) and its follow-up call (`:856`). Keep
  the **read-only** Budgets pane (`webui.rs:1117-1194`; `app.js:1314-1347`).
  Decision pending on the payloads — see §7, decision 2.
- **Verification.** `cargo test -p ar-cli`; the added and inverted tests from
  §8; a grep that `/ui/api/limits` appears nowhere in `crates/`, `scripts/`,
  `docs/` or `dashboard/`; `scripts/ui/panel-check.mjs` and
  `panel-armed-check.mjs` still pass if they touch the endpoints; a manual
  check that the Budgets pane still renders (read-only).
- **Rollback.** Revert the tag and re-release. Any `limits:` block a user
  created through the panel in the interim stays in the config — clearing it
  is `aroute limits clear` (`commands.rs:888-909`), which is unaffected.

### Step 3 — Documentation and state alignment

- **Change.** Update `docs/16-dashboard-unity-plan.md` (the sync invariant),
  `docs/19-ui-upgrade-plan.md`, `docs/34:148-154`, `docs/23-ui-round-two-plan.md:145`,
  `docs/33-vps-limits-config.md:105`, and `docs/28-gap-ledger.md`; record the
  ruling in `docs/kb/ASKS.md` if it is not already there. No code change.
- **Verification.** A grep for `limits set` and `/ui/api/limits` across
  `docs/` returns only statements that are true after Step 2.
- **Rollback.** Revert the doc commit. Trivial.

### Step 4 (CONDITIONAL) — Remove the CLI `aroute limits set`

Only if the operator decides (decision 1, §7) that no ceiling may be armable
by any means. Not recommended. If taken: remove `LimitsCommand::Set`
(`crates/ar-cli/src/cli.rs:410`, args `:423-474`) and `limits_set`
(`commands.rs:703-825`); invert the ~15 CLI tests in
`commands.rs:3753-4086`; re-read `docs/15`, `docs/23:145`, `docs/32:269`,
`docs/33:105`. Verification: `cargo test -p ar-cli`. Rollback: revert the tag.

### Step 5 (CONDITIONAL) — Option 4-large, disarm the engine entirely

Only if the operator decides (decision 3, §7) that the capability itself
must go. **Not recommended** — it destroys the per-target failover, which is
correct and tested. If taken: the deletions listed in Option 4; `cargo test
--workspace`; the `ar-server` policy tests (`policy.rs:216-393`) and the
routes tests (`routes.rs:7039-7083`) deleted or inverted; every doc that
mentions `limits:` updated. Rollback: revert the tag — but note that this
step is not cleanly revertible at the *config* level if any operator's
config carries a `limits:` block, because the parser changes with it.

## 6. What the sequence does to the five write paths

| path | after Step 1 | after Step 2 | after Step 4 (optional) |
|---|---|---|---|
| 1. `aroute limits set` (`commands.rs:703`) | unchanged | unchanged | **gone** |
| 2. panel per-key form (`webui.rs:1975`) | unchanged | **gone** | gone |
| 3. panel default-ceilings form (`webui.rs:1341-1349`) | unchanged | **gone** | gone |
| 4. panel mint form's rpm (`app.js:856`) | unchanged | **gone** | gone |
| 5. `aroute sync` rpm carry (`commands.rs:2126-2151`) | **gone** | gone | gone |

After Step 2 the **only** remaining writer is the CLI verb, and it requires a
deliberate operator action. After the optional Step 4 there is no writer at
all — but the engine would still enforce a hand-written block, which is why
Step 5 exists only as an alternative to Step 4, not in addition to it.

## 7. Open decisions for the operator

**Decision 1 — should `aroute limits set` itself survive?**
*Recommendation: **yes, it survives** (i.e. do not take Step 4).*
A CLI verb is a deliberate operator tool; a browser form is not. The verb
requires someone to type a command, on a box they hold a shell on, naming a
ceiling explicitly. That is exactly the shape of action that produced the
good half of `docs/34` §3 (the operator *did* clear the block deliberately,
with `aroute limits clear`) and not the bad half (the audit fixture). The
ruling was "the deployed system must hold no budgets"; a CLI verb does not
put a budget in the deployed system until someone runs it. Keeping the verb
also keeps the capability recoverable if a future runaway model needs one —
which `docs/32` was built for. *Counter-argument, for completeness:* if the
operator's intent is "no ceiling may ever exist in this system", the verb is
the last writer and removing it is the only way to guarantee that. That
intent is stronger than the ruling as recorded.

**Decision 2 — should the Budgets pane keep displaying ceilings for a config
that has no `limits:` block?**
*Recommendation: **yes, keep the pane.***
It writes nothing (`webui.rs:1117-1194`; `app.js:1314-1347`), so it cannot
re-arm anything, and it is how an operator confirms that a config really has
no ceilings — the thing `docs/34` §3 shows nobody did before the incident.
Note the asymmetry the operator should know about: `budget_json`
(`webui.rs:1197-1213`) exposes `usd_cap`, `token_cap`, `rpm` and `model` but
**not** `model_tokens` and **not** `spend_window`, while `limit_json`
(`webui.rs:1258-1270`) exposes both. After Step 2 the Budgets pane is the
*only* panel surface that shows any ceiling, so this gap becomes more
prominent; the clean fix is to make `budget_json` expose the same arms as
`limit_json`. *Recommendation for that fix: yes, include it in Step 2* —
otherwise the pane silently under-reports what a hand-written config
enforces.

**Decision 3 — should the engine arms themselves be removed (Option 4)?**
*Recommendation: **no.***
They are correct and tested, the per-target failover is the one behaviour
worth keeping (`docs/34:144-147`), and the **spend** arms are inert without a
`limits:` block (`rpm` is not — §2.1). Removing them makes a future need expensive to satisfy and gives up
the failover. Take Option 4 only if the operator's intent is "this capability
must not exist in this codebase", which is a stronger statement than the
ruling.

**Decision 4 — should the panel's state/settings payloads keep the `limits`
key after Step 2?**
*Recommendation: **keep it read-only** (`webui.rs:767`, `:1243-1246`).*
The Budgets pane needs the data to render; dropping the key from the payload
would either break the pane or force a second endpoint. The thing to remove
is the *editor*, not the *data*. `limit_json` (`webui.rs:1258-1270`) is
therefore still used.

**Decision 5 — should the `default:` row keep a writer at all after Step 2?**
*Recommendation: **yes — the CLI's.***
`aroute limits set` with `--key` omitted writes the `default:` row
(`commands.rs:767-770`), and `limits clear` removes it
(`commands.rs:888-909`). The asymmetry the panel had (the `default:` row
settable only through `/ui/api/settings`, with four arms, while a per-key row
got seven through `/ui/api/limits`) disappears with the panel forms; the CLI
writes both row kinds through one renderer (`import/limits::render`), which
is the same code path the sync carry used, so the spellings cannot drift.

## 8. The test plan

### 8.1 Tests to ADD per step

**Step 1 (sync rpm carry removed).**
- A test that `aroute sync` against a dashboard DB whose `api_keys` table
  carries `max_requests_per_minute` **leaves the config's `limits:` block
  absent** — i.e. the inverse of
  `sync_carries_dashboard_rpm_per_arm_and_leaves_the_rest`. It must assert
  that no `limits:` section appears in the written file, and that the
  dashboard's rpm value is nowhere in the config.
- A test that sync against a dashboard DB with rpm rows **still carries the
  other sections** it owns (combos, custom providers, provider keys) — the
  removal must not have broken the bridge's actual job.
- If `read_rpm` is deleted rather than neutered, no test is needed for it; if
  it is kept and simply unused, a `#[test] fn read_rpm_is_not_called` is
  pointless and should not be written.

**Step 2 (panel writers removed).**
- A test that `POST /ui/api/limits` is **404** — the route is gone, not
  refused. This is the test that makes the removal observable.
- A test that `POST /ui/api/settings` with a `limits` body **does not create a
  `limits:` section** in the config (the `limits` branch is gone; the rest of
  the settings save still works and still writes the other settings).
- A test that the mint flow leaves **no `limits:` section** behind — the
  mint handler plus the (now-removed) follow-up call must not produce a row.
- A test that `GET /ui/api/budgets` **still returns 200 and still renders**
  its payload after the Limits pane is gone (the read-only path survives).
- A test that the panel's state payload still carries the read-only `limits`
  key (decision 4), if that is the shape Step 2 takes.

**Step 3 (docs) and Steps 4–5 (conditional).** No new behavioural tests;
documentation-only, and the conditional steps invert the tests listed below.

### 8.2 Existing tests to DELETE or INVERT

| test | where | action |
|---|---|---|
| `sync_carries_dashboard_rpm_per_arm_and_leaves_the_rest` | `crates/ar-cli/src/commands.rs:4089-4184` | **invert** — it asserts the exact behaviour Step 1 removes. Either delete it or rewrite it as the §8.1 "leaves no `limits:` block" test. |
| `read_rpm_treats_a_missing_table_as_nothing_to_say` | `crates/ar-cli/src/commands.rs:4185`+ | **delete** if `read_rpm` is deleted with Step 1. |
| `a_broken_gate_should_refuse_a_write_and_admit_a_read` | `crates/ar-cli/src/webui.rs:3103-3125` (its `/ui/api/limits` write at `:3112-3118`) | **rewrite** — the write it exercises no longer exists. Keep the read half (a broken gate still degrades a read to 200). Point the write half at a route that still exists and still writes (e.g. `/ui/api/settings` or `/ui/api/combos`), so the broken-gate refusal is still covered. |
| `limits_should_refuse_a_scope_that_names_no_combo_and_write_nothing` | `crates/ar-cli/src/webui.rs:3433-3455` | **delete** with Step 2. |
| `limits_should_keep_a_scope_when_omitted_and_clear_it_on_null` | `crates/ar-cli/src/webui.rs:3457-3502` | **delete** with Step 2. |
| `limits_should_write_the_per_target_ceilings_and_the_window` | `crates/ar-cli/src/webui.rs:3504-3542` | **delete** with Step 2. |
| `limits_should_refuse_a_zero_or_unreadable_per_target_ceiling` | `crates/ar-cli/src/webui.rs:3544-3561` | **delete** with Step 2. |
| `limits_should_refuse_a_scope_that_is_not_a_string` | `crates/ar-cli/src/webui.rs:3563-3574` | **delete** with Step 2. |
| the `ar-server` policy tests | `crates/ar-server/src/policy.rs:216-393` | **untouched by Steps 1–3.** Only under the conditional Step 5 (Option 4) do they need deletion or inversion. |
| the `ar-server` routes tests | `crates/ar-server/src/routes.rs:7039-7083` (the `token_cap` / `usd_cap` / `unpriced` refusal tests) | **untouched by Steps 1–3.** Only under the conditional Step 5 do they need deletion or inversion. |
| the CLI limits tests | `crates/ar-cli/src/commands.rs:3753-4086` (~15 tests) | **untouched by Steps 1–3.** Only under the conditional Step 4 do they die. |

**The five limits-specific panel tests** the recon names are exactly the five
rows above at `webui.rs:3433-3575`: refuse-an-unknown-scope,
keep-or-clear-the-scope, write-the-per-target-ceilings-and-the-window,
refuse-a-zero-or-unreadable-per-target-ceiling, and refuse-a-non-string-scope.
All five are about the *editor* and go with it. The broken-gate test at
`:3105-3120` is the sixth panel test that mentions `/ui/api/limits` and needs
rewriting rather than deletion, because the broken-gate behaviour it covers
is unrelated to limits.

## 9. Open items carried from the recon

These are the recon's `CLAIMS_TO_VERIFY` items that this plan depends on, or
that the plan's steps touch. Each is an **open item**, not a fact this plan
asserts.

1. **The `model:` scope is unenforced at runtime** (recon claim 1). This plan
   does not depend on it — §2.2 states it as an out-of-scope finding. But it
   must be confirmed as a dropped check rather than a deliberate
   simplification **before** anyone describes the scope as functional in a
   doc. It wants its own issue.
2. **The deployed VPS panel is ungated** (recon claim 2) — **RESOLVED
   2026-10-20.** The recon was reading a stale premise. Measured on the VPS
   after the v0.1.20 install: `aroute --version` → `aroute 0.1.20`; an
   anonymous `POST http://127.0.0.1:20128/ui/api/keys/mint` → **401**
   `credential_rejected`; `GET https://aroute.ishanparihar.com/ui/` → **404**.
   The deployed binary carries the tree's `ui_guard`, and the gate is armed.
   The 2026-10-19 build was the ungated one; that exposure is closed on two
   independent layers (the panel's own gate and the tunnel's ingress rule).
   **What remains true:** the removal of the writer is still the right
   posture, because a later binary downgrade or a re-arm into `Unarmed` would
   restore the anonymous path — and the mint form's `rpm` field is
   anonymous-input-to-a-`limits:`-row in exactly that state.
3. **The cloudflared `Host` rewrite** is what satisfies `webui.rs:407-418`
   on the public URL (recon claim 3). **Measured 2026-10-20:** the public
   `/ui` is **404** at the tunnel, so the rewrite is not currently load-
   bearing for the public path — the rule blocks the path before the origin
   sees it. It would become load-bearing the moment the rule is removed to
   publish the panel. The ingress file is
   `~/.cloudflared/hermes-config.yml`; the `aroute.ishanparihar.com` rule
   sets only `originRequest.httpHostHeader: 127.0.0.1:20128` and injects no
   header of its own. Not a dependency of this plan; recorded so it is not
   mistaken for one.
4. **Media routes bypass the policy entirely** (recon claim 4) — §2.2
   Finding 2. Confirm whether media spend is intended to be uncapped; if not,
   it is a separate bug worth fixing regardless of this decision.
5. **`usd_micros` is nearly inert on the VPS** because most models are free
   (recon claim 5). Confirm the live price table before relying on the dollar
   arm in any blast-radius estimate. Note this does not change §2.1's table —
   it changes how much a dollar ceiling is worth having.
6. **The `rpm` bucket resets on restart** (recon claim 6,
   `crates/ar-limit/src/lib.rs:64`) — confirm whether a restart is an
   acceptable escape hatch for a rate cap.
7. **`Policy::disabled()` has no production caller** (recon claim 7,
   `policy.rs:70-76`). Confirmed by inspection: the server always calls
   `from_limits` (`app.rs:397`). So no plan step may assert "no `limits:`
   block ⇒ `Policy::disabled`" — the correct statement is "no `limits:` block
   ⇒ `key_cap` returns `None`", which is what `docs/34` §2 says.
8. **`limits.keys["default"]` vs `limits.default`** are different rows
   (recon claim 8). `Limits::for_key` (`ar-config/src/lib.rs:1148-1150`) does a
   **single** lookup — `self.keys.get(key_id).unwrap_or(&self.default)` — so
   `keys["default"]` is reached only when the *requested* key id is itself the
   string `"default"`; the fallback is the `default` **field**
   (`ar-config/src/lib.rs:1132`), a different storage. The `--key-id` CLI
   default is the literal `"default"` (`crates/ar-cli/src/cli.rs:362`, `:374`).
   Confirmed distinct. **Do not rename either** as part of this work.
9. **`aroute sync` cannot run on the VPS at all** (recon claim 9) — no
   dashboard DB ⇒ hard error at `commands.rs:2079-2084`. Confirmed by
   inspection. If a dashboard DB is ever installed on the VPS, Option 1
   becomes mandatory rather than merely preferable.
10. **Whether the Budgets pane should keep displaying ceilings** (recon claim
    10) — decided in §7, decision 2.
11. **The release path** (recon claim 11) — stated in §0.

## 10. Acceptance for this plan

- [x] Every write path to a `limits:` row is enumerated with a `path:line`
      (§1).
- [x] The engine's arm-by-arm enforcement table carries the HTTP status and
      the `x-ar-deny-reason` value for each arm (§2.1).
- [x] The two engine gaps — the unenforced `model:` scope and the
      policy-free media routes — are stated as separate, out-of-scope
      findings with their own issues (§2.2, §9).
- [x] The recon's five options appear with its ranking intact, each with
      change / blast radius / what could break / verification (§4).
- [x] There is a clear RECOMMENDATION with reasoning (§4).
- [x] The recommendation is broken into discrete, separately-shippable steps,
      each with change / verification / rollback, and the release path is
      stated once (§0, §5).
- [x] What stays and what goes is stated, with reasons (§3).
- [x] The open decisions are listed with a recommendation for each (§7).
- [x] The test plan names the tests to add and the tests to delete or invert,
      by name and by line (§8).
- [x] No code was changed by this document.
