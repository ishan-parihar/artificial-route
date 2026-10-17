# 16 — Dashboard config unity: the split, the fix plan, the skin plan

Status: **plan** (the §4 rebrand closeout is implemented and live). Companion to
docs/14 (the compiled-dashboard approach) — that doc explains why the
dashboard is a vendored artifact; this one explains what that decision costs,
and what closes the gap.

## 1. Why the config split exists

Two stores, one operator:

| | the proxy (the product) | the dashboard (the vendored UI) |
|---|---|---|
| process | `aroute serve` (Rust) | `aroute dashboard` → Node `server.js` |
| store | `~/.config/ar/config.yaml` + `ar.env` (`$AR_KEY_*`) | `~/.config/ar/dashboard-data/storage.sqlite` (integrated DB) |
| schema | yaml sections: `providers:`, `combos:`, `keys:`, `limits:` | tables: `combos`, `provider_connections`, `provider_nodes`, `api_keys`, `registered_keys`, quota pools, groups … |
| gateway | `:20128` — where real traffic goes | `:20149` — a **full second gateway** from the parent (`/v1/models` answers 200) |

The chain of decisions that produced this:

1. **docs/14 vendored the compiled dashboard** instead of forking ~1,700
   source files. Correct call — but the parent's build is a *standalone
   product*: its config surface is its own SQLite, and its Node server is
   itself an LLM gateway. Both came along with the copy.
2. **The Rust proxy is file-based** (`AGENTS.md`: "CLI + file YAML only").
   Credentials are `$VAR` refs in yaml; the env file carries values. No DB.
3. **`aroute sync` is the bridge** (`crates/ar-cli/src/commands.rs:1748`):
   one-way (dashboard DB → config.yaml), manual, section-splicing. It carries
   exactly four things: the `combos:` section, the `custom_providers:` section
   (from `provider_nodes`), provider connection keys → `ar.env` (`AR_KEY_*`
   numbered by connection priority), and `limits.keys.<name>.rpm` (the one
   1:1 column, docs/15).
4. **`serve` never hot-reloads.** `ConfigHandle::watch` exists
   (`ar-config/src/lib.rs:1516`) but nothing attaches it; routing state is
   boot-built (`ar-cli/src/serve.rs:143`). So even synced edits land only
   after the restart `sync` performs.

## 2. What actually drifts

- **Edits in the dashboard are invisible to the proxy until someone runs
  `aroute sync`.** Nothing reminds them; there is no watcher and no timer.
- **Hand edits to `config.yaml` `combos:`/`custom_providers:` are clobbered**
  by the next sync — sync owns those sections byte-for-byte. Correct design,
  but undocumented contract: hand edits belong in the dashboard (or in
  sections sync does not own).
- **The dashboard's usage/analytics pages show its own DB** — request history
  for traffic that mostly never happened (real traffic goes through :20128
  and lands in `~/.config/ar/usage.sqlite`). Empty charts that look broken.
- **The second gateway on :20149** accepts chat completions with
  dashboard-minted keys, bypassing aroute's limits/ledger entirely. Playwright
  of the config UI, full proxy underneath.
- **Model settings, client `api_keys`, quota pools, groups, agent tasks** —
  dashboard-only state that no part of sync maps.

## 3. Fix plan

### P1 — make the bridge run itself (zero new engine code)

`sync` is already idempotent, cheap (one read-only sqlite walk + yaml splice +
string compare), and no-ops cleanly ("nothing changed"). The missing piece is
scheduling:

1. Ship `aroute-sync.service` (oneshot: `aroute sync`) +
   `aroute-sync.timer` (~every 2 min, randomized). The timer runs as the user;
   sync restarts `aroute.service` itself only when something actually changed
   — the restart is the debounce. Both unit files mirror
   `aroute.update.timer`'s shape; `install.sh --with-dashboard` writes them
   disabled-by-default like the dashboard unit.
2. Document the ownership contract in README's Dashboard section:
   *sync owns `combos:`, `custom_providers:`, numbered `keys:` rows, and
   dashboard-carried rpm; edit those in the dashboard; everything else stays
   hand-owned in yaml.*

Acceptance: edit a combo in the dashboard → within ~2 min `config.yaml`
contains it and `aroute.service` restarted once; no churn when nothing
changed (`systemctl list-timers` shows the run, journal shows "nothing
changed").

### P2 — kill the restart (hot reload)

Attach `ConfigHandle::watch` in `serve` and rebuild the boot-built routing
tables on reload (the watcher re-parses; the server must swap `state.config`
and derived tables atomically). Then sync passes `--no-restart` by default and
edits land in seconds. Medium work in `ar-server` (routing tables are spread
across `models.rs`/`policy.rs`); do after P1 proves the loop.

### P3 — complete the mapping (close the remaining sync gaps)

- **Client keys**: dashboard `api_keys` → config.yaml `keys:` entries so a
  key minted in the UI authenticates against :20128 (today it only works
  against the dashboard's own gateway). Mark the section as sync-owned like
  the others.
- **Endpoint overrides**: `provider_nodes.base_url` is a full endpoint in
  upstream's shape (the zenmux doubly-wrong-URL failure, docs/07 + AROUTE_BUGS);
  carry it into the v0.1.14 endpoint-override field instead of letting the
  executor append `/chat/completions`.
- **Usage honesty**: one-line note in the UI's usage page (compiled-chunk
  patch, same pipeline as the skin) that the charts reflect dashboard-side
  traffic only, and `aroute usage` is the proxy's ledger. Full unification
  (feeding aroute's sqlite into the dashboard) is not worth it until P2's
  reload work makes the UI's own gateway optional.

### P4 — decide the second gateway (default: label, don't neuter)

Neutering :20149 breaks the UI's own playground and connection tests. Keep it,
bound to loopback (already enforced by `aroute dashboard`), and document that
agent-facing endpoints point at :20128. Revisit only if someone actually
routes through it by accident.

## 4. Rebrand closeout — "omni" wording sweep (implemented)

The docs/14 pipeline rebrands capital `OmniRoute` with guards that keep
identifiers/URLs/headers intact. A sweep of the rebranded chunks (the 1,313
files carrying the rebrand) found the user-visible leftovers and the new
`dashboard/patch-artificial.sh` closes them:

| Leftover | Count | Action |
|---|---|---|
| `sk_omniroute` sentinel (locale copy + CLI-config writers/detectors) | 661 | → `sk_artificial`, globally and consistently — writer, detector and copy live in the same dist, so one string patch keeps them coherent. Existing minted keys keep working (matched by value). |
| `sk-your-omniroute` placeholder copy | 29 | → `sk-your-artificial` |
| bare-repo GitHub refs `diegosouzapw/OmniRoute` in docs pages | ~40 | → `ishan-parihar/artificial-route` |
| deep links (issues/PRs/releases/archive) | ~60 | **left** — they reference upstream history; retargeting would 404 |
| `X-OmniRoute-*` headers, `OmniRoute/1.0` UA, `OMNIROUTE_*` env names, `@omniroute/*`, i18n key *names* (`defaultOmnirouteKey`), `~/.omniroute` paths, sw.js cache names, localStorage keys | — | **retained on purpose** — same rationale as the docs/14 table: wire/identifier contracts; renaming breaks runtime for cosmetic gain. The header table in the UI's built-in docs stays truthful because the Node gateway really emits those headers, and the `~/.omniroute` docs strings are truthful because `browser-login-profiles` reads `$HOME` directly (it does not honor `DATA_DIR`, unlike the storage path). |

The script is idempotent, parse-gated (`node --check` baseline/after over
exactly the files it touches, restore+anchor on failure), boots the dist on a
free port and asserts manifest + brand + sentinel absence. `rebrand-dist.sh`
invokes it as a step, so the next `update-dist.sh` run inherits the closeout.

## 5. Skin plan — making the UI look like Artificial Route

The compiled CSS is Tailwind v4: theme colors compile to `--color-*` custom
properties in `:root`/`.dark` (`--color-accent: #6366f1`,
`--color-accent-hover: #8b5cf6`, `--color-accent-light: #a855f7`, surfaces
`--color-bg: #0b0e14` / `#f9f9fb`). Utilities reference the variables, so
redefining the variables recolors everything — no JS, no parse gate.

Mechanism (new `dashboard/apply-skin.cjs`, another pipeline step):

1. **Append an override layer** to each compiled CSS chunk: a
   `/* ar-skin: v1 */` marker comment, then `:root`/`.dark` blocks redefining
   the brand tokens. End-of-file wins the cascade at equal specificity; the
   marker keeps reruns idempotent (replace-in-place instead of append).
2. **Palette** — pick one direction, commit, and hold it everywhere
   (manifest, login, favicon): a warm signal palette distinct from the
   parent's indigo/violet — e.g. amber-500 `#f99c00` accent on the existing
   near-black `#0b0e14` (the amber scale already ships in the CSS, so the
   override is two variable redefinitions, not a new ramp).
3. **Assets**: swap `public/favicon.ico` + logo SVGs, patch
   `manifest.webmanifest.body` `theme_color`/`background_color` (the manifest
   patch seam already exists in `rebrand-dist.sh` step 2).
4. **Verify**: boot smoke + a screenshot of login and the combos page (the
   scrub pipeline's precedent), since a palette override that misses a
   hardcoded hex looks worse than no skin.

Out of scope, deliberately: repainting per-page layouts or rebuilding UI
components — that is a source fork with different economics (docs/14 §why).

## 6. Full integration — scoping (2026-10-17)

"Route the web dashboard to the Rust binary as the backend" is two very
different projects:

**A. Keep the compiled React UI, reimplement its API in Rust — rejected.**
That API is 726 `route.js` handlers across 105 namespaces (a2a, evals, chaos,
cloud, cli-tools, memory-FTS, quota pools…), plus 149 page routes; the compiled
client hardcodes their payload contracts, which exist nowhere but minified JS
and change with every upstream pull. It is a reimplementation of the parent
product's whole feature set — the parts aroute deliberately dropped — against
an undocumented, moving target: months of work that never converges, and
worse than the source fork §1 rejected (a fork at least has readable source).

**B. Aroute-native thin dashboard — the real integration.** Serve a small
UI from the existing axum router and delete the vendored stack entirely. The
backend primitives already exist: `ar-config` (parse/watch/splice), `ar-keys`
(mint/arm-gate), `ar-tokens` (usage ledger), `ar-cli` already mutates every
section (`configure`, `combo`, `keys mint`, `limits set`, `auth login` with
headless-capable browser OAuth). The work is a thin HTTP skin — roughly 10–15
endpoints (list/edit providers, combos, client keys, usage, doctor) plus one
static page; no npm, no build step, loopback-bound like the current dashboard.
Estimate: 2–5 focused days for an MVP, vs months for A. Deletes afterwards:
`dashboard/` (1.6 GiB), the rebrand pipeline (rebrand-dist, patch-artificial,
scrub-sponsors, peer-stamp-launcher), the `sync` verb, the second gateway on
:20149, the ~700 MB Node child, the `enc:v1` problem, and the update-dist
treadmill. Migration is already built: `aroute import --combos storage.sqlite`
plus the sync env materialisation, run once, then archive the DB. This also
restores docs/00's stated vision ("No Next.js dashboard, no PWA, no i18n").
Lost: the parent's UI polish (drag-drop ordering, in-browser OAuth — the CLI
auth flow covers it — usage charts, catalog browsing).

Until B lands, P1 + pointing the dashboard's own playground at
`http://127.0.0.1:20128/v1` as a provider row makes aroute the only gateway
in practice, with zero new code.

## 7. Deferred / not planned

- Feeding aroute's usage ledger into the dashboard charts (needs the UI to
  read a second store — compiled-UI surgery).
- Making the dashboard UI itself write config.yaml (inverts docs/14's whole
  premise; P1–P3 achieve the same operator outcome for a fraction of the
  surface).
