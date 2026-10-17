# 23 — /ui round two: the gaps A–E left open

Status: **G1–G5 implemented 2026-10-18, uncommitted** (see "Outcome" at the end). Written after docs/19 Phases A–E shipped.
Each phase is self-contained; execute in any order but G1 first (largest surface).
Companion to docs/19 (what shipped) and docs/21 (non-UI gap register — different scope).

## 0. How to verify (unchanged from docs/19)

Per phase: `cargo clippy --all-targets --all-features --locked -- -D warnings` +
`cargo test -p ar-cli -p ar-tokens`, then the visual probe at 1440/1024/390:

```
scratch: /var/tmp/ishanp-agentic/opencode/ar-webui-demo  (config.yaml + ar.env copy)
run:     cd <scratch> && set -a && . ./ar.env 2>/dev/null; set +a; \
         setsid nohup <repo>/target/release/aroute serve --config ./config.yaml --port <free> \
         > serve-probe.log 2>&1 < /dev/null &
probe:   NODE_PATH=/var/tmp/ishanp-agentic/opencode/ar-webui-test/node_modules \
         node ~/.agents/skills/website-design/scripts/probe.mjs http://127.0.0.1:<port>/ui/#/<view> --widths 1440,1024,390
```

## 0.1 Traps learned during A–E (do not rediscover)

- All three assets are `include_str!` — **every** HTML/CSS/JS edit needs an ar-cli rebuild.
- Run the freshly built binary: `target/release/aroute`, NOT `~/.local/bin/aroute`
  (the installed one lags until release+reinstall; serving the stale one looks like
  "my edit didn't land").
- Graceful shutdown holds the port for a while: `pkill -x aroute; sleep 2` may still
  bind-fail; if `bind: Address already in use` appears while `pgrep -x aroute` is
  empty, the old server was draining — wait and relaunch.
- `H(tag, attrs, ...kids)` coerces a raw ARRAY kid to a string. Spread it:
  `H("select", {...}, ...ids.map(...))`. This bit twice (catalog rows, playground options).
- New panes MUST be added to BOTH `VIEWS` and `TITLES` in `page/app.js` — `route()`
  destructures `TITLES[view]`, so a missing title throws and the state-error pane shows
  a misleading "could not read /ui/api/state" message.
- Playwright: `waitForSelector("option")` defaults to visible-state — closed `<select>`
  options never become visible; use `{ state: "attached" }`.
- Both free combos were dead upstream on 2026-10-17 (or-decisions free quota, nvidia
  failover 502). For playground happy-path tests, mock `/v1/chat/completions` via
  `page.route` with a canned SSE body; the real gateway is covered by the error-path test.

## 0.2 File map

| file | role |
|---|---|
| `crates/ar-cli/src/webui.rs` | axum routes + handlers + `Ctx{config_path, env_path, store, gate_armed, ledger}`; `load_cfg` helper; `arm_of()` limits parser (module-level, keep-semantics: omitted field = keep, `0`/false = remove) |
| `crates/ar-cli/src/webui/page/{index.html,app.css,app.js}` | embedded assets; panes = `paneX(root, st)` fns registered in `VIEWS`/`TITLES` |
| `crates/ar-cli/src/import/mod.rs` | yaml renderers (byte-exact splices); combo/provider round-trip tests live here |
| `crates/ar-tokens/src/ledger.rs` | `Ledger::analytics()` (30-day buckets anchored to newest row's day; SQL has `created_at`, `LedgerRow` does not) |
| `crates/ar-config/src/lib.rs` | `Limits{default, keys: BTreeMap<String,Limit>}`, `Limit{rpm, usd_micros, tokens, refuse_unpriced}` (~line 1100–1160) |
| `crates/ar-registry/src/lib.rs` | global model registry (276 rows, prices, `configured` flag) |

Existing endpoints: `GET /ui/api/state`, `/ui/api/usage`, `/ui/api/models`;
`POST /ui/api/keys` (mint), `/ui/api/providers/add|update|remove`, `/ui/api/combos/*`,
`/ui/api/limits/set` (generalized `arm_of` splice with orphan pruning).

## G1 — request log with filters + date ranges (was docs/19 P2; largest)

WHERE: `webui.rs` (new `GET /ui/api/log`), `ar-tokens/ledger.rs` (range/filtered query),
`app.js` usage pane (new log section + range picker).

- Backend: extend `Ledger::analytics()` or add `Ledger::rows_since(day_count)` +
  optional filters (model, provider, status) — SQL only, `report()` untouched.
- `GET /ui/api/log?days=30&model=<id>&provider=<id>&status=error` returns recent rows
  (created_at, model, provider, tokens, usd, status/error text).
- Frontend: a filter bar (range select 7/30/90, model+provider dropdowns fed from
  analytics buckets, error-only checkbox) above a table; failure view = the error-only
  preset. Reuse the existing usage-table styles.
- expect: date ranges change the chart + table; error filter shows only failed rows;
  0 FAIL probe; ledger untouched by writes.

## G2 — playground multi-turn + system prompt

WHERE: `page/app.js` (`panePlayground`/`wirePlayground`, ~line 706) only — no backend.

- Add a system-prompt textarea (default empty; sent as `messages[0]` when non-empty).
- Keep a `PG_MESSAGES` array: on Send push `{role:"user",content}` + accumulate the
  assistant reply from the stream; send the full history each turn. "Clear" button
  resets. Render replies with a `user`/`assistant` prefix line instead of one blob.
- Non-stream path: use `data.choices[0].message.content` and push it too.
- keep: AbortController stop, usage meta, error surfacing (incl. mid-stream
  `json.error` frames — already handled).
- expect: two consecutive turns where the second references the first is answered
  coherently (mock two SSE bodies); Clear resets; 0 FAIL probe.

## G3 — key model scoping (`Limit.model`)

WHERE: `ar-config/src/lib.rs` (`Limit` struct), `ar-server` policy/cap path, `webui.rs`
keys pane payload, `page/app.js` keys pane.

- Add `pub model: Option<Strng or String>` to `Limit` (serde default, skip if none).
- Enforcement: `Policy::cap`/`spend_verdict` currently matches limits by client key
  name; scope the cap check to requests whose serving target's combo/model matches —
  read how `cap()` receives the request (it may need the requested model passed in;
  if that's invasive, scope on the *combo id* the client asked for, which is what
  `x-ar-model` already carries).
- UI: keys pane Limits row gains a "combo scope" text input (empty = all); `arm_of()`
  + round-trip tests extended (omit = keep, empty = remove — same semantics as arms).
- expect: a key with `model: jev` limit is capped when calling jev and untouched for
  other combos; round-trip test; clippy+tests green.

## G4 — registry favorites + capability view

WHERE: browser-local (`localStorage`) + `page/app.js` paneProviders catalog section;
no backend unless grants need exposure.

- Star/pin rows: persisted in `localStorage["ar-favorites"]` (list of model ids);
  favorites section renders first with a toggle. One `H("button", {"data-star": id})`
  + delegation — remember `H` now maps `dataset`→`data-*`.
- Capability/grant view: check what `/ui/api/models` already returns; if grants
  (which keys may use a model) are not in the registry row, SKIP the grant view and
  note it — do not build a new backend for a nice-to-have.
- expect: star survives reload; favorites-first ordering; 0 FAIL probe.

## G5 — playground ↔ mint integration

WHERE: keys pane (mint button) + playground bearer field, both in `page/app.js`.

- On successful mint, offer "use in playground" (store the token in a module var +
  swap to `#/playground` with the bearer field pre-filled). Minted tokens show once —
  the prefill must happen in the same JS turn as the mint response.
- Do NOT persist the token anywhere (sessionStorage included) — it's a live secret.
- expect: mint → click → playground field filled; reload loses it (by design).

## Non-goals (unchanged from docs/19 §2)

Cached-tokens and RAG/evals panels (omni-only, not aroute's domain). Grant-level
model ACLs beyond G3's scope field if the registry doesn't already expose them.

## Outcome (2026-10-18)

Gates: `cargo clippy --all-targets --all-features --locked -- -D warnings` clean; `cargo test --workspace --locked` green;
probe 0 FAIL at 1440/1024/390 on dashboard, usage, keys, providers, playground; 13-check Playwright behavior script
(mocked SSE and mint) green. Release build verified, not installed.

- **G1** — plan said "status/error from the ledger" but `usage` has no such column and is append-only. Decision: a separate
  `failures` table in the same SQLite file (`Ledger::record_failure`, trimmed to 10 000 rows, 300-char text), so spend sums, caps and
  `report()` are untouched. Written from a thin `handle_chat` wrapper in `ar-server/routes.rs` (`FailNote` response extension carries
  the reason); 401/403 are skipped so auth probes cannot fill it. New `GET /ui/api/log?days&model&provider&status=error`,
  `GET /ui/api/usage?days=` (range applies to chart and ranks; no `days` keeps the all-time ranks). Endpoint names in §0.2 were wrong; real
  routes are `combos`, `provider-key`, `provider-custom`, `provider-remove`, `keys/mint`, `limits`.
  Limits: failures only exist from this build onward; only requests that reach `handle_chat` are logged (a mid-stream failure after a 200 is not).
- **G2** — `PG_MESSAGES` history, system prompt, Clear, role-tagged transcript. A failed send leaves history untouched and the prompt in the box.
- **G3** — `Limit.model` + `Limits::for_request`. Deviation: scope selects *when a row applies* (matched on the combo id the client asked
  for); the spend it counts is still the key's cumulative total, because the ledger stores the serving provider's model, not the combo.
  `rpm` stays key-wide (checked before the body is parsed). `/ui/api/limits` takes `model` (omit keeps, null/"" clears, unknown combo refused).
  Keys-pane inputs are now prefilled with the row's values (a blank used to clear every other arm on each Set).
- **G4** — favorites in `localStorage["ar-favorites"]`, star in catalog rows, favorites block first. Grant view skipped: `/ui/api/models` does not expose grants.
- **G5** — "Use in playground" on the minted token; memory-only (`MINTED`, `PG_TOKEN`), dropped on reload and when leaving keys/playground.
- Also fixed: `[hidden]` was overridden by `.error`/flex display rules (global `[hidden]{display:none!important}`).
