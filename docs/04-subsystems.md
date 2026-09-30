# 04 — Subsystems (hardened specs)

## keys (`ar-keys`)

OmniRoute `enc:v1:iv:ct:tag` AES-GCM, `scrypt(secret, STATIC_SALT)`, IV 16B, fail-open on encrypt.
Fix: `enc:v2:` argon2id + 12B nonce + AAD=`provider|key-id|v`, per-install salt in `key.meta`,
`Zeroizing` heap, `keyring` desktop / `age` file VPS, fail-closed both ways, v1 read-migrate.
Tokens JWT 15m + refresh 30d + jti, scopes `models:read llm:write admin:*`, device-id, `revoke<jti>` polled 60s.
Process-spawn routes loopback-only enforced server-side. Accept: no-env write refuses; rotation re-encrypts; stolen token expires.

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
P1 essential-8 only (full 110-tool catalog in `06-axi-mcp.md`):
`get_health, list_combos, switch_combo, check_quota, route_request, cost_report, list_models_catalog, explain_route`.
Bodies are `pub(crate)`; `guard()` is the sole public entry, so no host can reach a tool without the scope check and the audit row. `Tool::ALL` stays public for listing — listing is not calling.
`get_health` reads `ar-keys::Admission` lane state (in-flight/queued/capacity per lane); the caller-supplied `KeyPressure` path is `Health::from_snapshot` and tags itself `source: Snapshot`. Breakers + cache counters stay host-supplied — no P0 equivalent exists.
Scopes `read:*|write:*|execute:completions` enforced per tool, default deny when `ENFORCE_SCOPES=true`. `ar_route_request` keeps `execute:*` even though its body is a pure pick: the pick is the only observable difference between a routed and an unrouted completion, and it is what a caller would address a provider with.
Audit `blake3(prompt)+truncate(200)+tool|duration|key-id` in `redb`, extended to `tool|duration|key-id|input_hash|outcome|out_len|truncated|output` — a scope denial writes a `denied` row before the `Err` returns, and `truncated:true`+`out_len` mark a cut output so it cannot be read as the whole one. `open()` resumes the sequence from the stored max so a restart appends instead of overwriting.
`tool_search` one-line signatures from day one, as a function over `Tool::ALL` (wire names + one-liners) rather than a ninth `Tool` variant, so the catalog count hosts report stays 8. CCR/oneproxy/web_search/skills deferred — each needs RAM budget to re-enter.
Accept: default build has zero `rmcp` in tree (`cargo tree -e no-dev | grep rmcp` empty); `+mcp` idle +<25MB.
