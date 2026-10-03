# 07 — Parity closeout plan (waves A–O)

Recorded before any closeout code lands, per AGENTS.md §3 ("update docs when
scope changes, before code"). This file is the commit map and the divergence
ledger's index; `docs/audit-notes.md` carries the divergence rows themselves.

Reference: `../OmniRoute` v16.3.1, read-only. Baseline for every wave: commit
`d24ea9a`, gates green (clippy `-D warnings` 0, `cargo test --release` 0,
`ar doctor` exit 0 on the same config as baseline — its row count is
config-derived, not a constant — and `ar serve` boot to `/healthz`).

## Ground rules

- Write only inside this repo. `../OmniRoute` is never edited.
- One workstream = one commit, each gated green before the commit.
- Secrets stay `$VAR` + dummies. Any body-buffering workstream re-proves the
  RAM budget in `docs/00-overview.md`.
- No ONNX/model-serving in-proxy; strict config validation stays (upstream
  accept-and-ignore is not adopted); nothing silent — every divergence gets an
  `docs/audit-notes.md` row.

## Audit corrections folded into the wave specs

The plan's original wording vs what the tree at `d24ea9a` actually holds:

- **C is not "mount built media routes".** `ar-server/src/routes.rs` has no
  media handler at all, and `ar_route::ArExec` (contract.rs) exposes exactly
  one method, `post_chat`; `post_media` is an inherent method on the concrete
  `ar_exec::ArExec`, unreachable through the `Arc<dyn ArExec>` the server
  holds. C therefore adds a media method to the trait, delegates from
  `HttpExec`, and writes four handlers before any `.route()` line.
- **D has no buffered body to read.** `Upstream.stream` is always a stream;
  the "non-streaming" arm still pipes `Body::from_stream`. D buffers with
  `axum::body::to_bytes` (against `MAX_BODY_BYTES`) before it can parse usage.
  D also needs a key: `AuthGate::verify`/`authorize` return `Result<(), _>`
  and drop the introspected `key_id`, so D first threads it out (anonymous
  sentinel when no gate is configured).
- **E needs the after-count.** Savings today is estimated from the original
  body only; exact counting requires keeping the rewritten string in scope
  before it is moved into `Bytes`.
- **B's request-id is done.** The `trace_id` middleware already stamps
  `x-ar-trace-id` on every response, 404s included. B stamps model, provider,
  version only.
- **J's kind set is nine**, not six (`kind_for`, routes.rs).
- **F's gap is ~25 control headers vs three read** (compression, session,
  accept) plus the credential matrix in `keys.rs`.
- Media routes are **declared P6 scope** (`docs/02`, modality row: "executors
  media family — P6, separate adapters"), not an expansion.

## Wave → commit map

| Wave | Commit | Acceptance |
|---|---|---|
| — | docs (this file + `docs/05` flip) | baseline gates still green (docs-only) |
| C | media surface: trait method, `HttpExec` delegation, 4 handlers (`embeddings`, `transcriptions`, `image_generations`, `ocr`), mounts, `/v1/completions` alias | typed-envelope 404s; doctor unchanged; RAM budget holds |
| G | budget clamp = accept-and-record: pin test + divergence row | over-budget body provably passes dispatch unclamped |
| A | honor `x-omniroute-compression` on the live path + CORS entry | alias ≡ native end-to-end; `off` wins; echo stays `x-ar-compression` |
| B | stamp `x-ar-model` / `x-ar-provider` / `x-ar-version` at `decorate` | both stream and non-stream arms |
| E | exact savings: `count_text` before and after, on the rewrite branch only | header = before − after; estimator stays in `eval` |
| D | key_id threading, non-stream buffering, usage parse → `record_response`; document the stream/non-stream split | non-stream headers match ledger math; stream bytes byte-identical |
| F1 | cache control request-header family (`no-store`, `cache-key`, `cache-ttl` if the cache API supports it), reference alias names honored | per-header probe tests; unknown headers stay ignored |
| H1 | quota: 429-marking + preflight cutoff, local state only — **verified present**: `classify_fault`'s terminal-code gate, the `lock_model(QuotaExhausted)` timestamp mark and the loop's `is_cooling_for` preflight skip all predate this wave; the commit adds the cross-request proof the acceptance names | forced-429 flips a provider to exhausted and the next request skips it (`a_quota_locked_provider_is_skipped_by_the_next_request`) |
| J1 | upstream code + `Retry-After` passthrough, envelope shape unchanged | a 429 from upstream reaches the client with its code and retry window |
| I1 | disconnect-aware abort: client-gone cancels the upstream token; mid-stream drop truncates cleanly | kill-upstream → clean close; client-drop → upstream abort observed |
| M | README parity table, `AUDIT-REPORT.md`, `docs/audit-notes.md`, `docs/05` final state | every landed wave reflected; every divergence rowed |
| N | final full gates + serve boot + RSS + push | all green on origin/main |

Deferred with owners, not dropped: H per-provider quota fetchers (one commit
each, on need), I stream resume (on need), J identifier growth (on demand), K
compression depth (port `relevance`/`readLifecycle` only if asked; ONNX paths
stay documented drops), L auth lifecycle (single master key stands;
loopback-first, revisit if network-exposed).
