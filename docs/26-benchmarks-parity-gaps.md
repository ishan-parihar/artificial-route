# Benchmarks, parity and known gaps

Measured numbers, the feature comparison against OmniRoute and agentgateway, and the open gaps. Moved here from the README to keep it in its length band; the figures are dated and the same machine as in the text.

## Benchmarks

Measured on this machine (24-core x86_64, `/proc/PID/status` VmRSS, release build). The `aroute` figure below is the installed systemd service running the full 276-provider / 1559-combo catalog, re-measured 2026-10-04 and flat over 10s; the OmniRoute figure is from its running process on the same box.

| State | RSS |
|---|---|
| idle, sample config (2 providers) | **11.1 – 11.4 MiB** (budget <35 MB) |
| idle, full 276-provider / 1559-combo catalog | **28.3 MiB** (budget <50 MB) |
| OmniRoute v16.3.1, same box, same purpose | **1003 MiB** |
| first dispatched completion | +2.0 MiB one-time warmup (TLS, pool, first-translate) |
| each further dispatch | ~+0.07 MiB marginal |

The OmniRoute number is its full Next.js desktop/PWA server, not just its proxy
path — but that *is* the point: to serve these combos you run either ~1 GB of
Node or one static binary. Both figures are the same measurement, same machine,
same moment, rather than numbers from two different benches. Traffic figures
cover the dispatch path against a localhost upstream (no SSE frames relayed);
the SSE relay itself is proven by `tests/e2e.rs` against a mock upstream.

Re-measured 2026-10-08 against the current development tree: `aroute serve`
alone is ~33.8 MiB RSS; spawning `aroute dashboard` adds ~4.4 MiB of
supervision plus the Next.js child at ~513 MiB RSS, ~548 MiB together under
both. The local omniroute backend is ~1.26 GiB RSS in the same session.

### Streaming under load

Measured 2026-10-09 against the released v0.1.8 musl binary on loopback,
driving free-tier upstreams through the live combos (an `omp` headless agent
for the end-to-end runs, raw SSE probes for the numbers). Every stream below
relayed through `aroute serve` as pure SSE — no batching, no buffering:

| probe | combo | input | first byte | stream length | SSE frames | outcome |
|---|---|---|---|---|---|---|
| short ask | free-stack | 80 B | **1.2 s** | 128 s | 1,211 | budget cut — the model spent its 4k budget on reasoning |
| long generation | browser-stack-2 | 200 B | **1.8 s** | 265 s | 2,307 | clean `stop`; 14k content + 16k reasoning chars |
| 234 KB codebase in | free-stack | 234 KB | **3.0 s** | 8 s | 90 | 234 KB body forwarded intact, upstream prefill fast |
| 234 KB in + long out | browser-stack-2 | 234 KB | **5.3 s** | 341 s | 2,478 | 5.7-minute stream held, budget cut at 8k |
| 3 concurrent streams | free-stack ×3 | 200 B | **0.67–1.9 s** | 167–233 s | ~4,000 | all three completed; zero 429s, zero failovers |

Across the whole battery: zero dropped or corrupt frames, zero guard
refusals or false alarms (234 KB of raw Rust source — file paths, URLs,
secret-shaped test fixtures — forwarded untouched), and `aroute` RSS moved
**28.9 → 33.7 MiB** over 35 minutes of continuous multi-stream load — inside
the idle budget. TTFB is upstream generation start, not proxy work: the relay
adds no measurable first-byte latency. Two honest caveats from the same
session: `glm-5.3` is a heavy reasoning model whose thinking tokens consume
the `max_tokens` budget before visible content, so size the budget
accordingly; and `paper-stack` cannot dispatch at all — see the zenmux bullet
under [Known gaps](#known-gaps).

## Parity

| Surface | Artificial Route | OmniRoute | agentgateway |
|---|---|---|---|
| providers | **276** (identical set) | 276 | 8 + 13 presets |
| strategies | **21** + 8-variant auto | 20 + auto | 3 (+LB modes) |
| dialects | 6 in, 18 named-missing | 24 cells | 8 formats |
| compression | 3 engines + intensity dial, per-combo **and** per-request | 12 (+stubs), per-combo | 4, reactive, no knob |
| upstream auth | apikey + OAuth dispatch (codex, cline, claude, gemini-cli, cursor) | apikey + 24-entry OAuth | 9 strategies, exchange-only |
| key storage | encrypted local sqlite + `$VAR` fallback | encrypted DB | env/file/`ate-secret://` |
| policy | `limits:` block — rpm bucket (`429`) + cumulative spend caps (`402`) per key, `refuse_unpriced`; bearer gate opt-in | per-connection quota, daily/weekly windows | local rate limit, CEL conditions |
| model refresh | snapshot + import | sync + overlays | catalog + refresh API |
| routes | 14 (`chat`, `messages`, `responses`, `api/chat`, `/v1/completions` legacy alias, `embeddings`, `audio/transcriptions`, `audio/translations`, `images/generations`, `ocr`, `models`, `healthz`, `metrics`) | full gateway + UI | 22 data-plane |

## Known gaps

Stated plainly, because the alternative is a reader discovering them. The full
explanation is [AUDIT-REPORT.md](AUDIT-REPORT.md).

- **`bedrock` and `gitlab-duo` route, but are not verified live.** `bedrock` uses a
  Bedrock API key and the runtime endpoint of `base_url` (default `us-east-1`; set
  another region in `config.yaml`). `gitlab-duo` uses a pasted token against the
  monolith `code_suggestions` endpoint. Neither has been called against its
  provider. Their gaps are listed in docs/24 E2.
- **The 12-hour soak is not claimed.** The harness (`scripts/soak.sh`) exists and
  has run four times — longest partial 51 samples, +996 KB, zero failures — but
  no 12-hour run has completed against a released binary. P6's accept line is
  therefore open.
- **`grok-cli` dispatches with the core of its wire, not all of it** — the
  Responses body repairs and `x-grok-*` identity are ported; foreign reasoning
  stripping, tool-schema normalization, custom and namespace tools, and the
  per-user headers are not (see docs/24 E3).
- **`kilocode` has neither mechanism declared**, so its `oauth/` row is a `fail`
  whose fix names the YAML for either one. Its device flow and anonymous dispatch
  are both ported; the editor header now reaches the wire from the catalog row
  (docs/24 E4). The reference has no kilocode-specific executor, so there is no
  further wire to port.
- **Multi-provider `auto/*` pools fall back to config order.** Single-provider pools score identically; bare `auto` maps to `least-used` since 0.1.12, and the auto scoring engages for literal `auto/*` names.
- **Client dialects are re-framed for every wire that has a lift.** `ar-server/src/reframe.rs`
  re-spells the OpenAI, Anthropic, Responses (`grok-cli`), Gemini, Bedrock and GitLab
  answers into the dialect the client spoke. The Anthropic, Responses and Gemini streams are
  lifted to chat-completion chunks first (`ar-translate/src/anthropic_upstream.rs`,
  `responses_upstream.rs`, `gemini_upstream.rs`); a streamed Gemini call dispatches
  `:streamGenerateContent?alt=sse`. Kiro and Clova streams still relay their own framing:
  no reference for either is on disk, so no lift is written for them. Custom
  `anthropic-compatible` nodes are accepted and named by `aroute doctor`, then refused at
  dispatch. `zenmux-free` is on the Anthropic wire (docs/24 E5), but its auth has not been
  verified against a live key. The Responses and Gemini lifts are covered by unit and
  endpoint tests against canned upstream frames; no live `grok-cli` or Gemini key has been
  run through them. A Responses client on a Responses upstream is relayed raw by design:
  that pair is native, and re-synthesising its events would drop fields (pinned by
  `responses_stream_should_relay_a_responses_upstream_unchanged`).
- **The MCP control plane is stdio-only** and ships behind `--features mcp`.
- **The registry snapshot lags OmniRoute's model rotation.** `aroute doctor` reports
  its age and warns past 7 days; `aroute import --from omniroute` regenerates it.

## Artificial Route vs OmniRoute v16.3.1

Measured 2026-10-01, `aroute` 0.1.1 against a live OmniRoute v16.3.1, both on loopback. Every cell is an observation from that one session.

| Surface | Artificial Route 0.1.1 | OmniRoute v16.3.1 |
|---|---|---|
| shared routes | `POST /v1/chat/completions`, `/v1/messages`, `/v1/responses`, `GET /v1/models`, `GET /healthz` | the same five |
| `GET /healthz` | `ok` 200 in **0.4 ms** | `ok` 200 in **4.4 ms** |
| routes beyond those five | `GET /metrics` (Prometheus), `POST /api/chat` (Ollama), `POST /v1/completions` (legacy alias), the media family: `POST /v1/embeddings` (typed reply re-render), `/v1/audio/transcriptions` + `/v1/audio/translations` (multipart verbatim, `?model=` routing), `/v1/images/generations`, `/v1/ocr` | **~723 more**: dashboard, rerank, search, files, batches, WebSocket, tokenized aliases, A2A |
| unknown model | 400, names the routable combos | 400, suggests a `provider/` prefix |
| malformed JSON | 400 | 400 |
| unknown path | 404, JSON envelope carrying the path and the routable list | 404, JSON envelope carrying the path |
| `/v1/*` auth | opt-in bearer gate — armed by `aroute keys arm-gate`, 30-day signed tokens; open until armed, see the warning below | API key required, 401 JSON envelope without one |
| RSS | **~21.7MB** (2026-10-03 re-measure, `/proc/PID/status` VmRSS, with `narenas:2,dirty_decay_ms:1000,muzzy_decay_ms:1000` compiled in after the closeout traced the ~120MB untuned figure to jemalloc's 4-arenas-per-CPU default) | **~823MB** (**~38x**) |
| cold boot | ~1s | not measured |
| CLI verbs | 13: `serve`, `models`, `providers`, `combo`, `doctor`, `run`, `configure`, `auth`, `keys`, `limits`, `import`, `sync`, `dashboard`, plus `mcp` behind `--features mcp` | ~78 |
| MCP tools | 12-tool catalog + `tool_search`, behind the default-off `mcp` feature | 110 tools + A2A |
| terminal-status list | 6 rows + 1 carve-out-only, generated into the store's CHECK | taxonomy it was reconciled against |
| test gate | 44 suites, 2021 passed, 0 failed; clippy `-D warnings` clean | not measured |

No upstream chat call was made in either column, so the shared-route row is a
surface match, not verified call parity. The classifiers agree except for one
owned divergence, first-sighting timing on `(401, token_revoked)`: we retire on
the first sighting, OmniRoute on the third (fix record:
[docs/audit-notes.md](docs/audit-notes.md)). The footprint row is the trade:
those ~723 extra routes, 110 MCP tools and dashboard are real capability and
cost ~823MB, while `aroute` covers the routing core in ~14.6MB. The ~823MB figure
is a different OmniRoute process state than the ~994 MiB above.

> [!WARNING]
> `aroute serve` arms its bearer gate only when `aroute keys arm-gate` has stored a
> master key — `aroute doctor` reports the gate's state, and `aroute keys mint`
> issues further client tokens. Until it is armed the listener is open: keep it on
> loopback or front it with an authenticating proxy. `$AR_MASTER_KEY` arms the
> credential store, not HTTP auth.

Full depth, OAuth mechanics, refresh triggers, quota tables, engine-catalog divergences and every gap with its fix: [AUDIT-REPORT.md](AUDIT-REPORT.md).

Known gaps, stated once: every one of them — the OAuth executor inventory,
`grok-cli` and `kilocode` dispatch wires, the `auto` pool fallback, the
OpenAI-wire-only dispatch surface (including Anthropic-wire registry
providers like `zenmux-free`), the stdio-only MCP plane, and the registry
snapshot's lag — is listed as a bullet under [Known gaps](#known-gaps),
with the full explanations in [AUDIT-REPORT.md](AUDIT-REPORT.md) and the
OAuth login flow and its `$VAR`-must-be-exported rule under
[Configuration](#configuration).
