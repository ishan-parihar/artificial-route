# Audit Report: artificial-route vs OmniRoute vs agentgateway

**Scope:** compression settings, API keys, OAuth/token-refresh, provider/model
management. Read-only survey of the live OmniRoute (`v16.3.1`, this machine),
the agentgateway checkout, and `ar` at `v0.1.1`.
**Verdict:** `ar` is not production-grade yet. Of the 8 critical/high gaps this
audit opened, 6 are FIXED, 1 is PARTIAL (model discovery) and 1 (per-connection
quota) has landed its circuits but not its store; the list below says which is
which per row.
No secrets were read or moved during this audit.
**Update:** F-CRIT-1 (OAuth dispatch), F-CRIT-2 (credential store), F-CRIT-3
(custom providers), F-HIGH-1 (per-combo compression), F-HIGH-2 (candidate pool),
F-MED-2 and the circuit half of F-HIGH-4 have landed; each entry below says what
is done and what is not. The strategy count in the summary below was 19 and is
now 21 — `expiry-first` and the fusion judge's own dispatch landed after that
line was written; `Strategy::all()` (strategy.rs:424) is the arbiter.

## Live inventory (OmniRoute, this machine — counts only)

- 3 combos, all `compressionMode: lite`; 1 compression combo ("Standard
  Savings"), 0 assignments. Candidate pools: free-stack 7 providers for
  2 targets (pool ≠ targets — the pool is the fallback bench).
- OAuth sessions active with stored refresh: cline ×2, codex ×2, grok-cli ×1.
  **Mechanism (was an anomaly — red-team R1, now resolved):** kilocode ×2 OAuth
  rows with *no* stored refresh token were not a mystery. The provider publishes
  `oauth.initiateUrl` / `oauth.pollUrlBase` and **no** authorization and **no**
  refresh endpoint: it authenticates by RFC 8628 device flow, so a live session
  legitimately holds a bearer with no refresh half behind it. See R1 below.
  kimi-coding expired with no refresh, which is expected-terminal (R2).
- API-key connections: 49 across 19 groups, incl. 2 custom
  `openai-compatible-*` nodes plus `clinepass`, `yolo-auto` (custom nodes
  outside the 276 registry). 6 client API keys.

## Findings

### F-CRIT-1: OAuth dispatch — executors and browser login landed
`ar` used to catalogue `oauth` authType but not execute it ("P0 is API-key only",
OAuth paths DROP). The codex/cline/grok-cli sessions — the live system's
most valuable credentials — were unusable through `ar`. The scoring layer
already models OAuth availability; the executor could not consume it.
**Now:** `ar-exec/src/oauth.rs` carries the lifecycle for `codex`, `cline`,
`claude`, `gemini-cli` and `cursor` — token injection, proactive refresh on
expiry, one rotation retry on 401, and a per-connection single-flight so a
concurrent burst cannot trip `refresh_token_reused`. `doctor` fails loudly on
any `oauth` provider that has no executor instead of listing it as known.
**Browser login also landed:** `ar auth login --provider <id>` prints the PKCE authorize URL (openable on any device), catches the redirect on a single-use `127.0.0.1` listener or reads one pasted redirect line for remote logins, exchanges the code, and persists both rows; `ar auth status|logout` and four MCP tools (`ar_auth_login_url|complete|status|logout`) drive the same flow. AGENTS.md
forbids inventing provider wire formats, so no refresh endpoint is hardcoded —
`token_url` is operator-supplied and a session without one reports itself as
unrenewable rather than guessing.

**The two rows that were open, closed separately:**

*`grok-cli` — closed as an OAuth kind, wire still deferred.* `OAuthKind::GrokCli`
is a variant (`as_str` → `grok-cli`, alias `gc`), so the provider is no longer
"catalogued but unexecutable" — `doctor` builds a session for it and it resolves
through the same `resolve_oauth` path as the other five. The variant is
**authentication-only**: the registry row is `authType: oauth` /
`authHeader: bearer`, so a bearer goes in and a refresh brings a new one back.
Nothing about the *dispatch* wire is transcribed — the Responses body, the
`x-grok-*` client headers and the model defaults are the dispatch layer's, not the
token layer's. Its refresh verdict is the one place the shared list needed
extending: the reference grok executor carries a terminal set of
`{invalid_grant, invalid_client}`, and only the second differs from
`TERMINAL_REFRESH_STATUS`, so it landed as a `carve_out` arm plus a
carve-out-only row in `CARVE_OUT_TERMINAL_STATUS` rather than as a second copy of
the shared list. That shared list is now **6 rows** — `(400, invalid_grant)`,
`(400, unauthorized_client)`, `(400, no_refresh_token)`, `(401, token_revoked)`,
`(403, account_disabled)`, `(410, token_revoked)` — so the terminal surface is those
6 rows plus that 1 carve-out-only row, and the row-for-row cross-check now reports
**zero** we-terminal/they-transient divergences except one explicitly owned
first-sighting timing divergence on `(401, token_revoked)`: both sides retire that
account, but `grok-cli.ts:211-216` spends two round trips first. A public client id
is a config value, not a secret — RFC 6749
§2.3.1 puts it in every authorization request in the clear — and none is compiled
in.

*`kilocode` — no `OAuthKind`, but its two mechanisms no longer need one.* R1
resolved: the provider is an RFC 8628 **device-flow** provider, not a
refresh-token one, and it also serves a free tier that answers an unauthenticated
caller. Neither mechanism is an OAuth *executor*:
`initiate_device` / `poll_device` are the RFC 8628 §3.2/§3.4 pair with both
endpoints operator-supplied and only §3.5's `access_denied` / `expired_token`
terminal (`authorization_pending` / `slow_down` are "keep asking", handled before
the classifier), and `anonymous: true` + `anonymous_editor:` dispatches on the free
tier with no credential row at all, because there is no account to hold one. So
`OAuthKind::parse("kilocode")` is still `None` — its *dispatch* wire is not
transcribed — and an `oauth/kilocode` row with neither mechanism declared is still a
`fail`. What changed is the fix that row carries: it names the YAML for either
mechanism instead of only refusing, because "no executor" is no longer the same as
"no way to authenticate". The store's `SessionKind` has a `device` and an
`anonymous` arm for the rows those two produce.

### F-CRIT-2: No local credential store — FIXED
Provider keys resolve from env vars only. There is nowhere to port the 49
live credentials: no local db, no file-backed secret, no AEAD envelope
(`ar-keys` has the crypto — AES-GCM-capable hash/secret modules — but no
provider-credential table, and the ledger never persists to disk).
**Fix:** gitignored local sqlite (`*.db` already ignored), AEAD-encrypted at
rest, `$VAR` as fallback/override. Mirror OmniRoute's `enc:v1` envelope
discipline, not its static-salt derivation. Never commit; never log values.
**Shipped:** `crates/ar-keys/src/store.rs` is that table — `CredentialStore` over
`rusqlite`, `open_with_env_key` / `open_with_material` (a `$VAR` master key,
because `config.yaml` has no config entry for it), every value sealed before it
touches disk, and the refresh row written compare-and-swap so a sibling writer
that rotated first is not clobbered. `ar serve` opens it through
`commands::credential_store(cli)` (`serve.rs:59`) and the resolution path prefers
the store over `$VAR` (`config.rs:1337`), so `ar auth login` writes rows the
server then reads. The usage ledger sits beside the same file (`serve.rs:87`).
Tests: `crates/ar-keys/tests/{store,keys,admit}.rs`.

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

### F-HIGH-1: Per-combo compression not wired — FIXED
All live combos run `compressionMode: lite`, but `ar-config` has no
compression field and the server calls `plan_resolution(&[], …)` — only the
`x-ar-compression` request header engages engines.
**Fix:** `compression:` per combo (engine + intensity), header overrides
file, file overrides off. Then mirror the live `lite` setting.
**Shipped:** `Combo.compression: Option<Compression>` (`ar-config/src/lib.rs:436`,
carried through `Wire` at :467/:491), `RouteCombo.compression` with a
`with_compression` builder (`ar-server/src/config.rs:296`/`:332`), and the load
copy at `:628`. `Step` is what the server already planned with, so the combo
value and the `x-ar-compression` header are one dial rather than two paths.
Mirror coverage: the config's own mirror template carries three `lite` combos and
`ar-config/src/lib.rs:1682` asserts it.

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
12 tools exist as a library no binary wires up. OmniRoute exposes 84+ tools;
agentgateway acts as an MCP OAuth server (DCR + refresh_token).
**Fix:** `ar-mcp` behind the `mcp` feature on `ar`, or cut the crate until
it's wired. Dead code that advertises capability is a trust bug.
**Done (option a):** `ar mcp` behind the same default-off `mcp` feature, serving
the essential-12 (`ar-mcp/src/lib.rs:147`) + `ar_tool_search` over stdio through
`rmcp/transport-io`; the catalog is 12 + `tool_search` = 13, pinned by
`ar-cli/src/mcp.rs:428`. Every
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
`invalid_grant`; grok-cli's `invalid_client` is terminal). **Row-for-row
cross-check against the reference taxonomy: `docs/audit-notes.md`** — **0**
we-terminal/they-transient rows except one explicitly owned first-sighting timing
divergence on `(401, token_revoked)` (both sides retire; the reference spends two
round trips first), 3 the other way, and 15 agreements. The 4 former Class A rows
and both Class C detection gaps are closed: A1/A2/A4 demoted to transient, C1 closed
by verbatim phrase aliases, C2 closed by demotion. Per-finding verdicts and the three
accepted divergences are in `docs/audit-notes.md` § Fix record.
**Now:** `ar_exec::oauth::TERMINAL_REFRESH_STATUS` is the only copy. The
classifier reads it, `ar doctor` reports its size and the carve-outs from it, and
`terminal_check_constraint()` *generates* the store's CHECK clause from it so the
fourth copy cannot drift. All three carve-outs are honoured by
`OAuthKind::carve_out`, which runs **before** the list — the only way to become
terminal is to be named, and the classifier's fallthrough is transient, because
reading a transient as terminal bricks a working account.

**Carve-out table (as built).** Two point away from retiring; the third came from
the grok-cli row of F-CRIT-1 and is the only one that moves *toward* terminal.
Cursor's `token_expired` arm is pruned as dead — it existed only to un-retire a row
that is now demoted — so it is the one carve-out reason that went away.

| kind | reason | verdict | why |
|---|---|---|---|
| `cursor` | `expired` | transient (`cursor-expired-is-retryable`) | means "this token is old", not "this account is dead" — the refresh path still works |
| `claude` | `invalid_grant` | transient (`claude-invalid-grant-survives`) | an IdP hiccup answers `invalid_grant` for a token that is still good |
| `grok-cli` | `invalid_client` | **terminal** (`unrecoverable`, status 401) | a client id the issuer does not recognise cannot become valid by refreshing again; retrying spends the whole attempt budget on a refresh that can never succeed |

That third row is why there are two lists. `CARVE_OUT_TERMINAL_STATUS` holds
`(401, "invalid_client")` and feeds `reason_in` and the generated CHECK, while
`classify_refresh`'s membership test still reads only `TERMINAL_REFRESH_STATUS` —
so a carve-out-only reason is terminal *only* where a `carve_out` arm names it, and
one shared list stays the only copy of the shared rows. Precedence is unchanged:
the shared list is scanned first, so a body naming both a shared row and a
carve-out-only row keeps the shared verdict. Both halves are pinned by
`keeps_a_grok_cli_invalid_client_terminal`, including that `codex` on the same body
stays transient.

**Closed (B1'):** the `doctor` cell now spells a carve-out-only reason.
`ar-cli`'s `terminal_reason` resolves against the **union** of
`TERMINAL_REFRESH_STATUS` and `CARVE_OUT_TERMINAL_STATUS`, so the one reason the
generated CHECK admits and the classifier can return is also the one an
operator-facing row can name. Finding B1' in `docs/audit-notes.md`; fixed.
**Also now:** the table it sits in. `ar-keys`' `oauth_sessions`
(`provider` PK, `kind`, `access_key`, `terminal_status`, `terminal_reason`) is
`credentials`-adjacent and holds placement plus status only — a `keys:` *name*,
never a secret — and it carries the generated clause verbatim, so a transient
`(status, reason)` cannot be recorded as a retirement. `kind` is
CHECK-limited to all three session kinds from day one (`refresh` / `device` /
`anonymous`), so the device and anonymous streams need no migration; an anonymous
row may not name a credential (`KeyError::AnonymousCredential` from the API, the
CHECK behind it).
**Not yet:** the device-flow poll state. `ar_exec::oauth` keeps RFC 8628 §3.5's four
poll codes in a private `DeviceReply` enum, so `oauth_sessions` has no vocabulary to
constrain a `poll_state` column against; adding one means making that enum public (or
giving it a generator, as `terminal_check_constraint` does), not transcribing the codes.

### F-MED-3: Counter under-count — **FIXED**
`ar_upstream_attempts_total` incremented once per request, not per attempt. It
now adds the loop's own `tried` count. That needed one thing the counter could
not see before: `AttemptOutcome::Retry` reported `attempts() == 0`, so a fully
throttled chain — the case where every attempt happened — would have added
nothing. `Retry` carries `tried` now. Name and exposition unchanged.

## Red-team items (anomalies, or mechanisms once explained)

- **R1: RESOLVED — anomaly → mechanism.** The observation was kilocode OAuth ×2
  with no stored refresh token yet `is_active`. The mechanism is **RFC 8628 device
  flow**: the provider publishes `oauth.initiateUrl` / `oauth.pollUrlBase` and no
  authorization and no refresh endpoint, so the client asks for a short user code,
  a human approves it on another device, and the client polls. The refresh-token
  column is empty *because there is no refresh grant to hold* — the row was never
  broken, and there was no second mechanism to find. Nothing about kilocode's
  *dispatch* wire is transcribed, so the accounting is unchanged where it matters:
  `OAuthKind::parse("kilocode")` is still `None` (pinned by
  `refuses_a_provider_this_build_has_no_executor_for`), `ar doctor` still fails it
  loudly, and `ProviderConfig::is_dispatchable` keeps it out of every candidate
  list. What changed is the *reason*: the mechanism is understood and ported where
  it belongs — `initiate_device` / `poll_device` in `ar-exec`, the two device
  endpoints as operator-supplied config, and `SessionKind::Device` in the store's
  CHECK-constrained `kind` column. A device session legitimately has no refresh
  half, and the store's `SessionKind::Refresh` arm documents the same asymmetry
  from the credential side.
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
doctor/serve single grammar, 21 strategies vs agentgateway's 3, per-key
backoff with `Retry-After` precedence, fail-loud unknown flags/strategies.

## Correction to prior briefs

agentgateway **does** compress (gzip/deflate/brotli/zstd, reactive
decode/re-encode, no config knob, no `Accept-Encoding` negotiation) and
**does** rich auth (9 upstream strategies incl. RFC 8693/7523 exchange,
OIDC browser login, MCP OAuth server). Earlier "0/passthrough" cells were
wrong and are corrected in the README table.

## Parity closeout — waves A–O (2026-10-03)

Closed per `docs/07-parity-closeout.md`, one commit per wave, all gates green
before each. Where a wave's plan said "build" and the tree already had the
mechanism, the commit records the verification and adds the missing pin
instead of rebuilding.

- **A** — `x-omniroute-compression` honored on the live path (the resolver
  existed test-only), CORS admits the spelling. Echo stays `x-ar-compression`.
- **B** — `x-ar-model` / `x-ar-provider` / `x-ar-version` stamped at
  `decorate` on both arms; the trace id was already middleware-stamped.
- **C** — media surface: `ArExec::post_media` on the routing contract,
  Dispatch-shaped exec with per-provider model rewrite, four handlers on their
  own 25MB sub-router, `/v1/completions` legacy alias, typed envelopes.
- **D** — real usage accounting: key id threaded out of the gate (anonymous
  sentinel), the non-stream arm buffers and records against the ledger `serve`
  opens beside the config; headers match ledger math by construction. Streams
  recorded zero at closeout (the documented split); the follow-up commit the
  same day closes that half — a capped first+tail frame collector records a
  stream's usage when the upstream ends, headers stay zero because they are
  sent before the first byte.
- **E** — `x-ar-savings-tokens` counted, not estimated, on the rewrite branch.
- **F1** — cache-control headers: `no-cache` (both sides), `cache-no-store`
  (write only), `cache-key` (namespaces the digest, both sides),
  `cache-ttl` (reference's seconds/ms heuristic, clamped to the policy
  ceiling, #14484's fix mirrored). Both spellings honored.
- **G** — budget clamp stays eval-only; accepted divergence, pinned by an
  unclamped-forwarding test (row d).
- **H1** — quota marking + preflight cutoff verified present and pinned
  across requests: the terminal-code gate, the timestamped
  `QuotaExhausted` lock and the loop's preflight skip all predate the wave.
- **J1** — upstream error identity: `Failover` carries the last upstream's
  body and window; the envelope's code is the provider's own when it is one of
  six pass-through identifiers, else it collapses to the router default — the
  same projection-vs-allowlist behaviour as the reference's ~310-entry list.
- **I1** — stream resilience pinned: client-hangup aborts the upstream read
  by construction (guard on the stream itself), truncation is described
  in-band per dialect.

Divergences are rows (a)–(f) in `docs/audit-notes.md`. Deferred with owners:
per-provider quota fetchers, stream resume, identifier growth, compression
depth, the auth lifecycle.
