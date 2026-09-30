# Audit Report: artificial-route vs OmniRoute vs agentgateway

**Scope:** compression settings, API keys, OAuth/token-refresh, provider/model
management. Read-only survey of the live OmniRoute (`v16.3.1`, this machine),
the agentgateway checkout, and `ar` at `v0.1.1`.
**Verdict:** `ar` is not production-grade yet. 3 critical + 5 high gaps below.
No secrets were read or moved during this audit.

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

### F-CRIT-1: No OAuth dispatch in `ar`
`ar` catalogues `oauth` authType but cannot execute it ("P0 is API-key only",
OAuth paths DROP). The codex/cline/grok-cli sessions — the live system's
most valuable credentials — are unusable through `ar`. The scoring layer
already models OAuth availability; the executor can't consume it.
**Fix:** per-provider OAuth executors (codex, cline, claude, gemini-cli at
minimum) + refresh plumbing. Until then `doctor` must fail loudly on
`oauth` targets instead of listing them as known.

### F-CRIT-2: No local credential store
Provider keys resolve from env vars only. There is nowhere to port the 49
live credentials: no local db, no file-backed secret, no AEAD envelope
(`ar-keys` has the crypto — AES-GCM-capable hash/secret modules — but no
provider-credential table, and the ledger never persists to disk).
**Fix:** gitignored local sqlite (`*.db` already ignored), AEAD-encrypted at
rest, `$VAR` as fallback/override. Mirror OmniRoute's `enc:v1` envelope
discipline, not its static-salt derivation. Never commit; never log values.

### F-CRIT-3: Custom providers unroutable
Live uses custom nodes (`openai-compatible-chat-<uuid>`, `clinepass`,
`yolo-auto`) created without code changes via `provider_nodes`. `ar`
compiles the registry in — a new endpoint means a rebuild.
**Fix:** `provider_nodes` equivalent: file-declared custom providers
(`openai-compatible` / `anthropic-compatible` + base URL + headers),
validated by `doctor`, no recompile. Highest capability-per-line item here.

### F-HIGH-1: Per-combo compression not wired
All live combos run `compressionMode: lite`, but `ar-config` has no
compression field and the server calls `plan_resolution(&[], …)` — only the
`x-ar-compression` request header engages engines.
**Fix:** `compression:` per combo (engine + intensity), header overrides
file, file overrides off. Then mirror the live `lite` setting.

### F-HIGH-2: No candidate-pool bench
free-stack lists 2 targets but 7 candidate providers. `ar` has no pool
concept — targets are the whole universe.
**Fix:** optional `pool:` per combo; targets route first, pool feeds
failover/fusion when targets exhaust. Small; high fidelity value.

### F-HIGH-3: No model discovery/sync
Registry is snapshot + `ar import`. OmniRoute syncs upstream catalogs with
capability/intelligence overlays; agentgateway refreshes its catalog over
`POST /api/costs/refresh-base` with file-watch hot reload.
**Fix:** scheduled `ar import` + `doctor` staleness warning (registry age vs
newest live model). Don't build a sync daemon before the store exists.

### F-HIGH-4: Quota is client-side only
`ar` tracks budgets per client key; OmniRoute tracks per *connection* quota
state, snapshots, pools, 2-bucket sharing counters, persisted refresh/warmup
circuits. Multi-account rotating-token setups will hit
`refresh_token_reused` under concurrency without the mutex + rotation map.
**Fix (with F-CRIT-1):** per-connection mutex, rotation cache keyed by token
hash (never the raw token), unrecoverable-vs-transient classification where
transients are *excluded* — getting that list backwards bricks accounts.

### F-HIGH-5: MCP tools orphaned
8 tools exist as a library no binary wires up. OmniRoute exposes 84+ tools;
agentgateway acts as an MCP OAuth server (DCR + refresh_token).
**Fix:** `ar-mcp` behind the `mcp` feature on `ar`, or cut the crate until
it's wired. Dead code that advertises capability is a trust bug.

### F-MED-1: Engine catalog discipline (don't copy the mess)
OmniRoute's engine ids disagree across 4 lists (12 catalog / 14 registered /
11 known / 10 prioritized); `omniglyph` is silently dropped from pipelines;
`ionizer` and `read-lifecycle` are registered but unreachable;
`codex-responses` + `omniglyph` modes are accepted by the API but ignored for
combo overrides. **Keep `ar`'s single list; derive the rest.** Add the
intensity axis (rtk minimal/standard/aggressive, caveman lite/full/ultra)
before adding engines — it's a multiplier on the 3 we have.

### F-MED-2: Terminal states need one list
OmniRoute's terminal `test_status` sets diverge across 4 sites
(broadest 4, narrowest 2) with no DB constraint. `ar` must define one
terminal set with a CHECK constraint from day one, plus the carve-outs
(Cursor `expired` is retryable; Claude refresh tokens survive transient
`invalid_grant`).

### F-MED-3: Counter under-count (known)
`ar_upstream_attempts_total` increments once per request, not per attempt.
Fix when touching metrics; cosmetic until then.

## Red-team items (anomalies, not yet gaps)

- **R1:** kilocode OAuth ×2 with no stored refresh token yet `is_active`.
  Mechanism unknown — trace before porting kilocode.
- **R2:** kimi-coding OAuth expired with no refresh. Expected terminal;
  confirm `ar` reports equivalent visibility, not a bare 502.
- **R3:** `STORAGE_ENCRYPTION_KEY` unset on this machine means OmniRoute
  tokens sit plaintext (`enc:v1` passthrough). Ops issue, not an `ar` bug —
  but anyone sharing that DB with a port must know.
- **R4:** Probe-origin dispatches skip proactive refresh (burning a rotating
  token on a health check). Any `ar` health path must preserve this.

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
