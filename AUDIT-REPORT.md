# Audit Report: artificial-route vs OmniRoute vs agentgateway

**Scope:** compression settings, API keys, OAuth/token-refresh, provider/model
management. Read-only survey of the live OmniRoute (`v16.3.1`, this machine),
the agentgateway checkout, and `ar` at `v0.1.1`.
**Verdict:** `ar` is not production-grade yet. 3 critical + 5 high gaps below.
No secrets were read or moved during this audit.
**Update:** F-CRIT-1 (OAuth dispatch), the circuit half of F-HIGH-4 and F-MED-2
have landed; each entry below says what is done and what is not.

## Live inventory (OmniRoute, this machine — counts only)

- 3 combos, all `compressionMode: lite`; 1 compression combo ("Standard
  Savings"), 0 assignments. Candidate pools: free-stack 7 providers for
  2 targets (pool ≠ targets — the pool is the fallback bench).
- OAuth sessions active with stored refresh: cline ×2, codex ×2, grok-cli ×1.
  **Anomaly:** kilocode ×2 OAuth with *no* stored refresh token, kimi-coding
  expired. How kilocode authenticates is unexplained — red-team item R1.
- API-key connections: 49 across 19 groups, incl. 2 custom
  `openai-compatible-*` nodes plus `clinepass`, `yolo-auto` (custom nodes
  outside the 276 registry). 6 client API keys.

## Findings

### F-CRIT-1: OAuth dispatch — executors landed, browser login did not
`ar` used to catalogue `oauth` authType but not execute it ("P0 is API-key only",
OAuth paths DROP). The codex/cline/grok-cli sessions — the live system's
most valuable credentials — were unusable through `ar`. The scoring layer
already models OAuth availability; the executor could not consume it.
**Now:** `ar-exec/src/oauth.rs` carries the lifecycle for `codex`, `cline`,
`claude`, `gemini-cli` and `cursor` — token injection, proactive refresh on
expiry, one rotation retry on 401, and a per-connection single-flight so a
concurrent burst cannot trip `refresh_token_reused`. `doctor` fails loudly on
any `oauth` provider that has no executor instead of listing it as known.
**Still open:** *browser-session* login (the `*-web` providers). AGENTS.md
forbids inventing provider wire formats, so no refresh endpoint is hardcoded —
`token_url` is operator-supplied and a session without one reports itself as
unrenewable rather than guessing. `grok-cli` and `kilocode` are deliberately
executor-less: `grok-cli` because its endpoint is not yet transcribed, `kilocode`
because of R1.

### F-CRIT-2: No local credential store
Provider keys resolve from env vars only. There is nowhere to port the 49
live credentials: no local db, no file-backed secret, no AEAD envelope
(`ar-keys` has the crypto — AES-GCM-capable hash/secret modules — but no
provider-credential table, and the ledger never persists to disk).
**Fix:** gitignored local sqlite (`*.db` already ignored), AEAD-encrypted at
rest, `$VAR` as fallback/override. Mirror OmniRoute's `enc:v1` envelope
discipline, not its static-salt derivation. Never commit; never log values.

### F-CRIT-3: Custom providers unroutable — FIXED
Live uses custom nodes (`openai-compatible-chat-<uuid>`, `clinepass`,
`yolo-auto`) created without code changes via `provider_nodes`. `ar`
compiles the registry in — a new endpoint means a rebuild.
**Fix:** `provider_nodes` equivalent: file-declared custom providers
(`openai-compatible` / `anthropic-compatible` + base URL + headers),
validated by `doctor`, no recompile. Highest capability-per-line item here.
**Shipped:** `custom_providers:` in `config.yaml`
(`id` / `protocol` / `base_url` / `key_ref` / optional `headers`), merged into
the catalog by `ar_registry::Registry::merge` at load. A custom id resolves
through `split_target_known` like any catalog id, so `ar serve`, `ar doctor`,
`ar models`, `ar providers` and `ar combo` all see it. `key_ref` is a name under
`keys:`, never a value. An id colliding with a compiled-in entry is refused
loudly — `doctor` reports it as a `fail` row and `serve` refuses to start —
because shadowing a real catalog entry would make the catalog stop describing
the world. `headers` land between `Content-Type` and the auth layer in
`ar-exec`, so a gateway that wants `X-Api-Key` needs no second auth class.
Residual: only the OpenAI wire dispatches, so an `anthropic-compatible` node is
accepted and named but refused at dispatch, exactly like the compiled-in
`anthropic`.

### F-HIGH-1: Per-combo compression not wired
All live combos run `compressionMode: lite`, but `ar-config` has no
compression field and the server calls `plan_resolution(&[], …)` — only the
`x-ar-compression` request header engages engines.
**Fix:** `compression:` per combo (engine + intensity), header overrides
file, file overrides off. Then mirror the live `lite` setting.

### F-HIGH-2: No candidate-pool bench — **FIXED**
free-stack lists 2 targets but 7 candidate providers. `ar` had no pool
concept — targets were the whole universe.
**Fix, as built:** optional `pool:` per combo, appended after the target chain
and excluded from `pick`, so pool feeds failover without touching strategy
scoring. Spec in `docs/04-subsystems.md` §route.

### F-HIGH-3: No model discovery/sync — PARTIAL (staleness warning landed)
Registry is snapshot + `ar import`. OmniRoute syncs upstream catalogs with
capability/intelligence overlays; agentgateway refreshes its catalog over
`POST /api/costs/refresh-base` with file-watch hot reload.
**Fix:** scheduled `ar import` + `doctor` staleness warning (registry age vs
newest live model). Don't build a sync daemon before the store exists.
**Done:** `ar-registry` stamps the build (`build.rs` → `BUILT_AT`), and the
`registry` row of `ar doctor` reports the snapshot's age, going `warn` past
`SNAPSHOT_TTL_DAYS` (7) and naming the `ar import` fix plus the count of
configured targets the build cannot route — each named by its existing
`target/<id>` `fail` row. `warn`, not `fail`, so a week-old build does not train
operators to ignore the exit code.
**Still open:** the *scheduled* `ar import` (a cron entry, not a daemon) and any
live-catalog comparison — `doctor` stays offline by design, so "newest known
live model" means the config's own targets, not models.dev.

### F-HIGH-4: Quota is client-side only — circuits landed, quota store did not
`ar` tracks budgets per client key; OmniRoute tracks per *connection* quota
state, snapshots, pools, 2-bucket sharing counters, persisted refresh/warmup
circuits. Multi-account rotating-token setups will hit
`refresh_token_reused` under concurrency without the mutex + rotation map.
**Now landed (the half that needs OAuth):** every session is an
`ar_exec::oauth::Connection<Connected>` holding a `tokio::sync::Mutex` for its own
refresh plus a process-wide rotation cache keyed by the SHA-256 of the retired
token — never the token itself. The test `refreshes_once_when_n_concurrent_grants_find_the_token_expired`
fires eight concurrent grants at one expired session and asserts one refresh.
**Still open:** the quota *store*. `ar_route::QuotaWindow` is a value a config
supplies; nothing snapshots, rolls over or persists it per connection.

### F-HIGH-5: MCP tools orphaned — CLOSED
8 tools exist as a library no binary wires up. OmniRoute exposes 84+ tools;
agentgateway acts as an MCP OAuth server (DCR + refresh_token).
**Fix:** `ar-mcp` behind the `mcp` feature on `ar`, or cut the crate until
it's wired. Dead code that advertises capability is a trust bug.
**Done (option a):** `ar mcp` behind the same default-off `mcp` feature, serving
the essential-8 + `ar_tool_search` over stdio through `rmcp/transport-io`. Every
call goes through `guard()`, so the scope check and the audit row cannot be
skipped; `ar mcp --list` prints the catalog as TOON with no config; `AR_MCP_SCOPE`
is the grant and is default-deny for whatever it does not name. The seven
`expect(dead_code)` suppressions on the tool bodies are gone, which is the
mechanical proof the crate is no longer orphaned.
**Out of scope, unchanged:** 84-tool parity, and any MCP OAuth server —
agentgateway has that and this change wires only what already exists.

### F-MED-1: Engine catalog discipline (don't copy the mess)
OmniRoute's engine ids disagree across 4 lists (12 catalog / 14 registered /
11 known / 10 prioritized); `omniglyph` is silently dropped from pipelines;
`ionizer` and `read-lifecycle` are registered but unreachable;
`codex-responses` + `omniglyph` modes are accepted by the API but ignored for
combo overrides. **Keep `ar`'s single list; derive the rest.** Add the
intensity axis (rtk minimal/standard/aggressive, caveman lite/full/ultra)
before adding engines — it's a multiplier on the 3 we have.

### F-MED-2: Terminal states — one list, generated
OmniRoute's terminal `test_status` sets diverge across 4 sites
(broadest 4, narrowest 2) with no DB constraint. `ar` must define one
terminal set with a CHECK constraint from day one, plus the carve-outs
(Cursor `expired` is retryable; Claude refresh tokens survive transient
`invalid_grant`).
**Now:** `ar_exec::oauth::TERMINAL_REFRESH_STATUS` is the only copy. The
classifier reads it, `ar doctor` reports its size and the carve-outs from it, and
`terminal_check_constraint()` *generates* the store's CHECK clause from it so the
fourth copy cannot drift. Both carve-outs are honoured by
`OAuthKind::carve_out`, which runs **before** the list — the only way to become
terminal is to be named, and the classifier's fallthrough is transient, because
reading a transient as terminal bricks a working account.
**Not yet:** the store column itself. `ar-keys`' credential store holds the
`keys:` map and nothing else; the CHECK has no table to sit in until F-CRIT-2
grows an oauth table.

### F-MED-3: Counter under-count — **FIXED**
`ar_upstream_attempts_total` incremented once per request, not per attempt. It
now adds the loop's own `tried` count. That needed one thing the counter could
not see before: `AttemptOutcome::Retry` reported `attempts() == 0`, so a fully
throttled chain — the case where every attempt happened — would have added
nothing. `Retry` carries `tried` now. Name and exposition unchanged.

## Red-team items (anomalies, not yet gaps)

- **R1:** kilocode OAuth ×2 with no stored refresh token yet `is_active`.
  Mechanism unknown — trace before porting kilocode. **Still open.** `ar doctor`
  now fails it loudly (`oauth/kilocode`: "no executor for it") and
  `ProviderConfig::is_dispatchable` keeps it out of every candidate list, so it
  costs no attempt slot and no rotation.
- **R2:** kimi-coding OAuth expired with no refresh. Expected terminal;
  confirm `ar` reports equivalent visibility, not a bare 502.
  **Now visible:** a terminal session answers `401` with
  `{"error":{"type":"oauth_terminal", "provider": …, "refresh_status": …,
  "reason": …}}` and is quarantined, so the *next* request fails with the same
  body and no network round trip. `doctor` also names it up front: an unarmed
  session is a `fail`, not a `skip`, because it will serve traffic and then die.
- **R3:** `STORAGE_ENCRYPTION_KEY` unset on this machine means OmniRoute
  tokens sit plaintext (`enc:v1` passthrough). Ops issue, not an `ar` bug —
  but anyone sharing that DB with a port must know.
- **R4:** Probe-origin dispatches skip proactive refresh (burning a rotating
  token on a health check). Any `ar` health path must preserve this.
  **Preserved structurally:** `ar_exec::oauth::Origin` is a required argument to
  the only function that can refresh, and `Origin::Probe` returns the cached
  token without refreshing *or* writing the rotation pool. There is no probe
  dispatch today (`/healthz` is a static body), which is exactly why the axis
  living at the refresh site is the guarantee.

## What `ar` already leads on (hold these)

Memory (2.2 vs ~994 MiB same combos), 10 MB static binary, TOON/AXI CLI,
doctor/serve single grammar, 20 strategies vs agentgateway's 3, per-key
backoff with `Retry-After` precedence, fail-loud unknown flags/strategies.

## Correction to prior briefs

agentgateway **does** compress (gzip/deflate/brotli/zstd, reactive
decode/re-encode, no config knob, no `Accept-Encoding` negotiation) and
**does** rich auth (9 upstream strategies incl. RFC 8693/7523 exchange,
OIDC browser login, MCP OAuth server). Earlier "0/passthrough" cells were
wrong and are corrected in the README table.
