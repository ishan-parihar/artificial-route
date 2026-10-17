# 19 — /ui upgrade plan: close the gap to the omniroute frontend

Status: **Phases A+B+C+D shipped** (docs-before-code; E pending). Companion to docs/16 §5 (Skin plan) and the
shipped `ar-cli::webui` module. This doc states what the native /ui is still missing
versus the vendored omni frontend, and the staged roadmap that closes it.

### Phase D status (2026-10-17, shipped)
`GET /ui/api/models` now serves the registry browser: all compiled-in providers
with wire format, model ids, per-model prices and whether this config already
routes to the id (`configured`) — 276 providers / 1559 models, read-only,
~112 KB. No favorites: the pane's free-text filter box replaces the pick.
`POST /ui/api/limits` replaces the old `/ui/api/limits/rpm` and now edits the
whole ceiling row (`rpm`, `usd_micros`, `tokens`, `refuse_unpriced`) with
explicit merge semantics: an omitted arm keeps its current value, an explicit
`null` clears it, an all-clear row collapses to "no ceiling"; `rpm: 0` and
non-numeric arms are refused. Verified end-to-end on a scratch config (set →
omit-keeps → null-clear → all-clear removes the row). No store-level enable
arm: a minted key is enabled by construction and a ceiling row is the whole
enforcement surface (402 semantics unchanged). Frontend: keys pane renders all
four arms with per-arm inputs plus the spend column; providers pane gained a
collapsible catalog browser with models+prices and a "use" button that
back-fills the registry form. Gates: workspace clippy `-D warnings` clean,
ar-cli 179 + ar-config 40 tests green; probe 0 FAIL on both panes (advisory
tap-target/text-size warns as before). Side-finding: `H()` ignored `dataset`
objects — Phase C's Edit/Remove buttons silently carried no data attributes;
`H` now maps `data-*` and the buttons work.

### Phase C status (2026-10-17, shipped)
kebab protocol, base-url gate, registry-catalog shadow gate, endpoint relative-path check;
env-var write lands before the config splice so a failed half-write cannot point at a
missing `$AR_KEY_*`; edit keeps the binding's existing variable) and `POST
/ui/api/provider-remove` (refuses while any combo reference survives, then drops the row
+ its orphaned `keys:` entries in one splice; `ar.env` lines never touched). `load_cfg`
shared parse helpers now back every writer (`combos`/`provider-key`/`rpm` refactored)
and state() marks custom rows with `custom/base_url/protocol/endpoint` for the page.
Also fixed the Donate arm's missing `Ok(())`. Frontend: providers pane now renders a
custom-provider form (id/protocol/base_url/endpoint/key; empty key on edit keeps the
binding variable) plus per-row Edit/Remove buttons, verified end-to-end against a
scratch config: add → edit (base moved, `key_ref` kept) → remove (row + orphaned yaml
`keys:` entry pruned, `ar.env` untouched). Probe: 0 FAIL / 3 renders (advisory warns:
tap-target on the 27px table buttons). Gates: workspace clippy `-D warnings` clean,
ar-cli 178 + ar-config 40/75 tests green.

### Phase B status (2026-10-17, shipped)
`Ledger::analytics()` in ar-tokens/ledger.rs (3 SQL bucket queries anchored to the newest
row's day, parametrized so GROUP BY never receives a constant; 3 new tests incl. the
bucket-collapse regression) → `GET /ui/api/usage` shape `{analytics:{days,models,providers},
totals,rows}` (closed ledger ⇒ empty analytics, same shape). Usage pane renders: Spend-by-day
flex-div bar chart (max-normalized, tooltips = date·$·req·tok, no deps), Top models / Top
providers rank tables, existing ledger rows table unchanged. Verified: seeded scratch ledger, probe **0 FAIL ×3 widths**,
clippy `-D warnings` clean, ar-cli+ar-tokens tests green (174+77, 3 new). LedgerRow still
lacks a rich created_at; buckets come straight from SQL `created_at` — phase C untouched.

### Phase A status (2026-10-17, shipped)
Shipped: Sora-headings + Inter-body woff2 (latin subsets, ~73KB embedded), indigo+amber
reskin, sidebar shell with FLIP rail motion, mobile collapse ≤880px (footer row, nowrap nav
scroll, min-height 44 nav targets), `H(tag, attrs…)` hardening (explicit `null` attrs no
longer throws), badge + version text ≥12px, faint/contrast tokens raised to a11y, primary
CTA style on Mint. Verified: probe **0 FAIL / 58 warn ×5 views ×3 widths** (`probe-A4`),
`cargo clippy --all-targets --all-features --locked -- -D warnings` clean, `cargo test -p ar-cli`
40/40. Then ÷ per-view fixes during Phase B if anything regresses.

## 1. Where /ui stands (shipped 2026-10-17)

Single-route axum group in `crates/ar-cli/src/webui.rs`, two `include_str!` assets:
one HTML page, one CSS+JS. Verified: probe 0-FAIL ×3 widths, live round-trips on a
config copy (keys byte-exact incl. `enc:v1:`, combo save 400-guards unknown providers,
rate-limit splice, unarmed mint gate). Visual: near-black + amber, full-mono,
failover rail signature. Both functional and visual are **thin**: one page, four panes
(state / combos / providers / client keys), read-mostly.

## 2. Feature gap vs the omniroute frontend

The vendored UI (compiled omni build, 105 API namespaces) is a full standalone
product. /ui covers ~8% of its surface. Ranked gaps:

| gap (omni has, /ui lacks) | impact | priority |
|---|---|---|
| Usage analytics: charts (spend/day, per-model, per-provider), date ranges, request log with filters | the #1 thing people open a gateway dashboard for | **P0** |
| Config editing beyond two fields: provider add/update/remove (base URLs, auth, rpm, pricing), combo create/delete + strategy/max_tokens/compression editing (not just reorder) | anything else still needs the CLI | **P0** |
| Model registry browser: 700+ models, provider mapping, favorites, capability/grant view | zero model visibility in UI | **P1** |
| API keys: quota, budget, cost-rate, enabled/disabled, model scoping | mint+delete only | **P1** |
| Interactive playground: chat with a model/combo from the UI — streams through the real gateway | none | **P1** |
| Logs / drift / failure views (request rows, errors, latency) | can't debug from the UI | **P2** |
| Cached-tokens panel, RAG/evals panel (omni-only features) | not in aroute's domain | **drop** |

## 3. Backend availability (what exists vs must be built)

| UI need | backend status |
|---|---|
| Usage: spend/day, per-model, per-provider, request log | `ar-tokens` ledger EXISTS (`CostReport`, per-request rows); needs query endpoints (range param), no new storage |
| Provider CRUD | yaml write path EXISTS for keys; add/remove `providers:` rows + `custom_providers` fields = new splice helpers on the same byte-exact writer |
| Combo create/delete/edit | `ProviderSecret` splice exists; combo editing needs the same splice for `combos:` (strategy/etc. deserialize fine) |
| Model registry browser | read-only: `ar-registry` global already exposed to webui (line-count fix pending: webui.rs dup group); needs one `GET /ui/api/models` |
| API key quotas/budget | omni stored these in sqlite; aroute has no per-key budget engine — options: extend `api_keys` table OR record in yaml `client_keys` (decision needed) |
| Playground | needs SSE-proxy endpoint hitting the real gateway loopback with the selected combo; streaming must pass through untampered |
| Logs/drift | gateway already logs per-request rows into ledger; failure view = filtered ledger query, same backend as usage |

## 4. Design upgrade (website-design pipeline, phased)

- **Direction shift**: current page is "terminal strip" — technically clean but flat and
  low-information-density. Target: electric-indigo/dark utility, Sora for headings +
  Inter for body (mono remains for all numeric/key/rail data), amber kept as the single
  action accent. Tokens-over-Tailwind (plain `:root` custom properties, as today).
- **Layout**: sidebar shell (16 rem, icons + labels) replacing center pills; content
  routed by hash (`#/state`, `#/providers`, ...). First screen = comma #18 dashboard:
  stat band, usage chart, routing table with the failover rail inline.
- **Signature moment**: keep the failover rail but give it motion — pills slide on
  reorder with 120 ms eased transitions, active target pulses on live traffic.
- **Component levels**: L0 tokens → L1 custom properties; L2 buttons/pills/inputs/tables;
  L3 cards/panes/toolbar/modals; L4 dashboard/screens. CSS stays in ONE file split by
  banner comments (embedded asset constraint; no build step).

## 5. Roadmap (each phase ships behind the full verify loop)

| phase | content | gate |
|---|---|---|
| **A. Reskin + shell** | sidebar layout, Sora/Inter tokens, chart-capable CSS, rail polish, probe re-run | probe 0-FAIL ×3 widths, contrast 0-fail, CRAFT ≥ 16/20 |
| **B. Usage analytics (P0)** | ledger query endpoints (day/model/provider/range), chart render (inline SVG, no deps), request log w/ filters | savings/parses numeric checks + probe |
| **C. Config CRUD (P0)** | provider add/update/remove, combo create/delete/strategy fields; byte-exact splices + restart flag wired | live round-trips on config copy incl. negative guards |
| **D. Registry + keys (P1)** | `GET /ui/api/models` browser w/ favorites; key quota/enable fields | read-only views, splice tests |
| **E. Playground (P1)** | SSE chat proxy through real gateway, combo picker, token counters | local e2e streaming test |

Non-goals kept: no Next.js, no build step, single-binary embed, no per-user auth
beyond the existing gate; omni-only panels (RAG/evals/cached-tokens) stay dropped.
