# 04 — Subsystems (hardened specs)

## compress (`ar-compress`)

`lite|rtk|caveman`, one list, one owner: `ar_config` resolves a combo's `compression.engine`
through `ar_compress::Engine::from_id` rather than keeping a second table, because the reference's
four catalogs disagree (12/14/11/10) and `omniglyph`/`ionizer` are registered but unreachable.
No more engines before the dial is done.
Intensity is a multiplier on those 3, not a 4th engine: `rtk` `minimal|standard|aggressive`
scales the repeat run length 4/3/2, `caveman` `lite|full|ultra` gates its rule table by rank
(`ultra` = + the noun-abbreviation table, opt-in because it rewrites a noun the model may read
back into code), `lite` has no ladder. `Engine::levels` is the only declaration; the default
level, the parser, and the rung comparison all derive from it.
`Plan.steps: Vec<Step>` — a step is an engine *at* a level, because one plan can carry a
header-selected pipeline and a config-selected step in the same list. Echo names a non-default
level `engine@level` so a default dial is invisible and a real one is not.
Precedence `header > combo > off`. A wrong engine or a level the engine does not offer is a
**load** error naming the fix, never a silent no-op that reads as "this combo is not configured
to compress". `ar doctor` refuses the file; the reference accepts and ignores it.
Accept: `cargo test -p ar-compress` shows the 3 rungs of each ladder differ; `live_path` shows
`combo;engines=rtk@aggressive` on the wire with no client header, and a header outranking it.

## keys (`ar-keys`)

OmniRoute `enc:v1:iv:ct:tag` AES-GCM, `scrypt(secret, STATIC_SALT)`, IV 16B, fail-open on encrypt.
Fix: `enc:v2:` argon2id + 12B nonce + AAD=`provider|key-id|v`, per-install salt in `key.meta`,
`Zeroizing` heap, `keyring` desktop / `age` file VPS, fail-closed both ways, v1 read-migrate.
Tokens JWT 15m + refresh 30d + jti, scopes `models:read llm:write admin:*`, device-id, `revoke<jti>` polled 60s.
Process-spawn routes loopback-only enforced server-side. Accept: no-env write refuses; rotation re-encrypts; stolen token expires.

**Credential store** (audit F-CRIT-2): gitignored `credentials.db` (default beside the config
file, `$AR_CRED_STORE` overrides) holding `enc:v2:` envelopes, one row per `keys:` name. The
`enc:vN:nonce:ct:tag` grammar is OmniRoute's `enc:v1` *shape*; the derivation is ours — argon2id
over a per-install salt persisted in the store's own `store_meta` row, so a store reopens with the
key it was written with. Master key material comes from `$AR_MASTER_KEY` (hex/base64/raw), never a
file in the repo. Resolution order is `store -> keys: ($VAR) -> error`: a store that is present and
will not decrypt is an error, never a silent downgrade to `$VAR`. `ar doctor` adds a `store` row
(`skip` when absent — env-only is a supported install) and names the source per `key/<name>`.
Accept: close-and-reopen reads back; a wrong master refuses; no value reaches stdout, `Debug`, or
`Display`.

**Session store** (audit F-MED-2): `oauth_sessions` beside `credentials` in the same file, opened by
the same `CREATE TABLE IF NOT EXISTS` — no migration. One row per provider: `kind` (CHECK-limited to
`refresh` / `device` / `anonymous`, so the device and anonymous streams add rows without a schema
change), `access_key` (the `keys:` *name* holding the session token, never the token), and
`terminal_status` + `terminal_reason`, whose CHECK is `ar_exec::oauth::terminal_check_constraint()`'s
output verbatim — generated, not transcribed, so the DB cannot hold a status the classifier calls
retryable. An `anonymous` row may not name a credential (`KeyError::AnonymousCredential`, CHECK
behind it): there is no secret to point at. Device poll state is deliberately absent —
`ar_exec::oauth` keeps RFC 8628 §3.5's codes in a private enum, so there is no vocabulary to
constrain against. Accept: an unknown status or kind is refused by the database; a transient pair
cannot be recorded; refresh and device rows round-trip.

## oauth (`ar-exec`, `ar-config`, `ar-keys`)

Two login mechanisms, chosen by what the *config* declares, never by a per-provider
table. A session that sets `device_auth_url` + `device_poll_url` is a device
session; a session that sets `authorization_url` is a browser session; one that
sets neither is dispatch-only and reports itself unrenewable. `Config::validate`
pairs the two device fields and refuses one without the other, because half a
device flow is not a slower device flow — it is a hole the operator discovers as a
login that waits out its whole timeout for a poll that was never going to happen.

Device flow is RFC 8628 §3.2/§3.4 (`initiate_device`, `poll_device`), and it exists
because of red-team R1, not for symmetry: the live provider named there publishes
`oauth.initiateUrl` / `oauth.pollUrlBase` and no authorization and no refresh
endpoint, which is why its sessions are active with no stored refresh token. Three
properties it holds to, same as the browser half: no endpoint is inferred from a
provider id (`Session::with_device_auth_url` / `with_device_poll_url`, an absent one
is a `LoginError`); no refresh-token shaping (§3.4 returns what the provider
returns, and a provider with no refresh grant gets none — the token records that by
having no refresh half, and `Session::can_refresh` reports it); and failures share
the taxonomy, because a poll failure goes through `classify_refresh` too.
`authorization_pending` and `slow_down` are handled *before* the classifier — §3.5
makes them "keep asking", not verdicts — and the only two device-flow terminal
conditions are §3.5's `access_denied` and `expired_token`. Poll interval defaults to
§3.2's 5s, walks up by §3.5's 5s on `slow_down`, and is capped at 30s so a provider
answering `slow_down` repeatedly reports its own rate limit rather than an expiry.
The device code is the flow's only secret and is never printed: it lives in
`DevicePending` behind a `Secret`, split from the printable `DeviceGrant` so the
two cannot be logged together.

Neither mechanism needs an `OAuthKind`: a device session logs in and a free tier
never does, so the provider id is the only thing that would want one, and the
provider named by R1 has no transcribed dispatch wire to attach it to. What that
costs is `ar doctor`'s vocabulary, and it is paid in the fix rather than the
status: an `oauth/<id>` row for a provider with no kind reads *mechanism-aware*, so
a block declaring `anonymous: true` gets an `ok` row (the free tier needs no
credential, so it is **armed by design**, not merely usable) and a block declaring
`device_auth_url` gets the honest "the mechanism is here, the kind is not" — instead
of a bare refusal that reads as "this provider is impossible".

`anonymous: true` is the third session shape, and it is a *dispatch* mode rather
than a login mode: some gateways serve a free model set to an unauthenticated caller
against a constant credential they name themselves. A session declaring it needs
**no** store rows, because there is no account to hold one — which is the point,
since the free tier is the tier an operator reaches for *because* they have no
account. `Config::validate` refuses it alongside `refresh_key`,
`client_secret_key` or `authorization_url`: a session that both refreshes and
dispatches anonymously sends a free-tier request while holding a refresh token it
never uses, and every one of those fields reads as a working config in YAML while
behaving as something else on the wire. `anonymous_editor` is required alongside it
(the gateway rejects the request without that header) and is never printed — a value
in a config file is no licence for a log line, and `ar doctor` names the field, not
its contents.

`SessionKind` in `ar-keys` is the store's `kind` column vocabulary —
`refresh` / `device` / `anonymous` — and it is deliberately *not*
`ar_exec::oauth::OAuthKind`: the two do not cross. A `cursor` session can be any of
the three below, and `ar-exec` has no session-kind vocabulary to resolve against.
It is generated into the table's CHECK from `SessionKind::ALL`, so a fourth kind is
a compile error rather than a silent hole. Two arms carry deliberate asymmetries:
`SessionKind::Refresh` permits a missing `access_key` (R1's two live sessions are
refresh-shaped with no credential in this store), and `SessionKind::Anonymous`
forbids one, enforced at the API (`KeyError::AnonymousCredential`) and by a CHECK
behind it.

The device poll state (`pending` / `authorized` / `expired` / `denied`) is **not** a
column, because `ar-exec` keeps §3.5's four codes in a private `DeviceReply` enum
and a hand-written list is the drift F-MED-2 exists to kill. Until that enum is
public or given a generator like `terminal_check_constraint`, a device row records
its outcome as a terminal status, which is the one thing every device flow
eventually produces.

Accept: a config declaring one device endpoint is a **load** error naming both; a
200 device response with no `user_code` is a named `LoginError`, not a poll loop;
`authorize_url` for a provider whose access token is already stale returns the
cached token without refreshing *or* writing the rotation pool.

## route (`ar-route`, `ar-config`)

A combo's `targets:` is the chain; an optional `pool:` is the **bench** (audit
F-HIGH-2). Targets are the whole universe in the reference's terms — live
`free-stack` lists 2 targets against 7 candidate providers, and the 5 that are
not targets are already wired upstream of a failover. The split here is the same
one, stated once: `pool:` entries are **appended after the target chain**, in
declaration order, and are *not* eligible for `pick`. So a strategy still scores
only targets — pool never changes which provider wins, it only extends what
happens after the winner and its target fallbacks fail. That is what makes it
small: no strategy scoring change, no second routing table, no candidate list to
keep in sync with the targets.

Pool entries resolve through the same `split_target_known` -> catalog ->
credential path a target does, at load: a bench that validated at request time
would fail one provider at a time, at 3am. `ar doctor` prints `pool/<id>` rows
alongside `target/<id>` so an operator can see which half of the 7 is the 2.
`MAX_ATTEMPTS` still caps the whole request, pool included — a pool widens *what
is tried*, never *how long a request may spend being failed over*.

Accept: a combo with `targets: [a, b]` and `pool: [c..g]` tries `a`, `b`, then
`c` when both refuse; `pick` over a pool-only-cheaper bench still returns `a`.

## admission (`ar-admission`)

Lanes `interactive(q100/30s)|heavy(q20/600s)|mgmt(q50/never-queue)`. `Semaphore+timeout`,
per-conn `governor` bucket + RPM rolling lease, honor `Retry-After`, backoff 3s/5s x2+jitter anti-herd.
Headers `X-Artificial-Queue: lane,pos` + `X-Artificial-Decision`. Token-in-URL legacy only, redacted.
Accept: 2x long `/v1/responses` overlap no FATAL; p95 wait exported.

## cache (`ar-cache`)

Exact: `quick_cache` sharded 32MB, key `blake3(tenant|model|canonical-msgs|params)` via `BTreeMap` canonical JSON,
TTL `200:5m 4xx:30s 5xx:no-store`, `Cache-Status`, `tokens_saved`. Persistent `redb` single file.
Fixes OmniRoute 64-bit trunc + unsorted-nested + 2MB thrash + client `No-Cache` overload (require scope to bypass).
`Idempotency-Key` store 24h stops double-spend on retry.

## guard (`ar-guard`)

`promptInjection|piiMasker|credentialMasker` two-stage: full scan non-stream, 4KB-overlap incremental stream.
## obs (`ar-obs`)

`tracing json + crossbeam 128k lossy + dedicated writer`, 7d rotation, `RUST_LOG=warn`.
Headers `Decision|Usage|Cost|Cache|Queue`, cardinality cap `provider|family|decision`.
Ledger SQLite append-only + per-key USD caps, flat-rate `$0` cost but full quota. Audit redacted default, raw 24h TTL `admin` only.
Accept: no raw prompt in metrics; stolen `read` token can't dump traffic.

## mcp (`ar-mcp`, optional `--features mcp`, default off)

Control-plane, not data-plane. Wraps same `ar-route|ar-tokens|ar-keys` fns — no second impl.
P1 a 12-tool catalog only (full 110-tool catalog in `06-axi-mcp.md`):
`get_health, list_combos, switch_combo, check_quota, route_request, cost_report, list_models_catalog, explain_route`.
Bodies are `pub(crate)`; `guard()` is the sole public entry, so no host can reach a tool without the scope check and the audit row. `Tool::ALL` stays public for listing — listing is not calling.
`get_health` reads `ar-keys::Admission` lane state (in-flight/queued/capacity per lane); the caller-supplied `KeyPressure` path is `Health::from_snapshot` and tags itself `source: Snapshot`. Breakers + cache counters stay host-supplied — no P0 equivalent exists.
Scopes `read:*|write:*|execute:*|*:health|*:combos|*:quota|*:usage|*:models|*:completions` enforced per tool. A *grant* is a subset of those ten bits (`AR_MCP_SCOPE=read:*,write:combos` is invalid; `AR_MCP_SCOPE='read:*'` is not), default-deny for whatever is not named; unset means everything, which is the honest reading for a stdio child the operator launched. A tool's *need* is a mask, and `Tool::scope_doc()` renders it in `06`'s per-tool spelling (`read:health+read:usage`) so the catalog and a host config read the same words. `ar_route_request` keeps `execute:*` even though its body is a pure pick: the pick is the only observable difference between a routed and an unrouted completion, and it is what a caller would address a provider with.
Audit `blake3(prompt)+truncate(200)+tool|duration|key-id` in `redb`, extended to `tool|duration|key-id|input_hash|outcome|out_len|truncated|output` — a scope denial writes a `denied` row before the `Err` returns, and `truncated:true`+`out_len` mark a cut output so it cannot be read as the whole one. `open()` resumes the sequence from the stored max so a restart appends instead of overwriting.
`tool_search` one-line signatures from day one, as a function over `Tool::ALL` (wire names + one-liners) rather than a ninth `Tool` variant, so the catalog count hosts report stays 8. CCR/oneproxy/web_search/skills deferred — each needs RAM budget to re-enter.
Wired, not orphaned (F-HIGH-5): `ar mcp` behind the same default-off `mcp` feature, serving over stdio through `rmcp/transport-io`. `Host` is the binary's half — a plain struct of public fields `ar` fills from the config it already resolved (`ar_server::ServerConfig` for the combos and candidates, `ar-keys` for lane state), so this crate still owns no config loader and no second routing. `ar mcp --list` prints the catalog as TOON and needs no config at all.
`ar_tool_search` is registered by the transport (9 registered, 8 in `Tool::ALL`) and needs no scope — it reads no host state — but still writes an audit row, so "every tool leaves one" has no exception.
`ar_route_request` passes **no** `LkgpPins`: a pin is recorded on a dispatched success that a pure pick never has, so `lkgp` honestly falls back to priority rather than advertising stickiness it cannot honour. The ledger is opened per call because `ar_tokens::Ledger` wraps a `rusqlite::Connection`, which is neither `Send` nor `Sync` and would make the whole server unspawnable.
Accept: default build has zero `rmcp` in tree (`cargo tree -e no-dev | grep rmcp` empty); `+mcp` idle +<25MB; an MCP host can `health -> list_combos -> route -> cost` over stdio.
