# Artificial Route

**One OpenAI-compatible endpoint over 276 providers — a 16 MB static binary idling at ~120 MiB RSS, dominated by allocator arenas rather than request state (see the RAM re-measure in the head-to-head).**

[![release](https://img.shields.io/github/v/release/ishan-parihar/artificial-route)](https://github.com/ishan-parihar/artificial-route/releases) [![license](https://img.shields.io/badge/License-Apache--2.0-blue)](LICENSE) [![musl](https://img.shields.io/badge/binary-static--musl-lightgrey)](https://github.com/ishan-parihar/artificial-route/releases) ![tests](https://img.shields.io/badge/tests-800%2B_passing-green)

A minimal-RAM Rust port of the [OmniRoute](https://github.com/ishan-parihar/OmniRoute) gateway core: same combos, strategies and dialects, none of the desktop. File YAML in, SSE out — no control plane, no UI, no runtime deps.

## Features

- **20 routing strategies + `auto/*`** — priority, least-used, quota-weighted/fair, cost-optimized, reset-aware, fusion, pipeline… plus a 6-variant auto factory scored over 16 factors, with `simulate`/`explain` in the library
- **276 providers, 1559 models** compiled in — same provider set as OmniRoute's registry, verified byte-identical
- **6 wire dialects** — OpenAI, Anthropic, OpenAI-Responses, Gemini, Ollama inbound, OpenAI render; 18 further pairs named loudly instead of half-ported
- **3 compression engines** (lite, rtk, caveman) with honor-and-echo semantics, per-combo in `config.yaml` and per-request via header, on an intensity dial (`rtk` minimal/standard/aggressive, `caveman` lite/full/ultra) rather than a dozen half-wired engine ids
- **OAuth dispatch** — codex, cline, claude, gemini-cli, cursor and grok-cli: token injection, refresh on expiry, one rotation retry on 401, per-connection single-flight so a concurrent burst cannot trip `refresh_token_reused`. Two login mechanisms, both with every endpoint operator-supplied: PKCE browser redirect, and RFC 8628 **device flow** for providers that publish no redirect endpoint
- **One terminal-status list** — 6 `(status, reason)` rows plus 1 carve-out-only row, generated into the store's CHECK clause; Cursor's `expired` and Claude's transient `invalid_grant` are carve-outs toward retry, grok-cli's `invalid_client` is a carve-out toward terminal, and a transient can never retire an account — the one remaining intentional divergence from OmniRoute's taxonomy is first-sighting timing on `(401, token_revoked)`: we retire it on the first sighting, OmniRoute on the third attempt
- **Quota-aware failover** — per-key backoff, `Retry-After` wins, throttle anywhere outranks later transport failure
- **Agent-native CLI** — TOON output, `--fields` narrowing, stdout data / stderr diagnostics, exit 0/1/2, fail-loud flags
- **Hot-reload config** — `config.yaml` watched live; secrets stay in `$VAR`, never in the file

## Benchmarks

Measured on this machine (24-core x86_64, `/proc/PID/status` VmRSS, release build):

| State | RSS |
|---|---|
| idle, sample config, 10 boots | **11.1 – 11.4 MiB** (budget <35 MB) |
| idle, full 276-provider catalog | **11.7 – 11.9 MiB** (budget <50 MB) |
| first dispatched completion | **+2.0 MiB** one-time warmup (TLS, pool, first-translate) |
| each further dispatch | **~+0.07 MiB** marginal |
| same 3 live combos as OmniRoute | **2.2 MiB** vs OmniRoute's ~994 MiB process |

The OmniRoute figure is its full Next.js desktop/PWA server, not just its proxy path — but that *is* the point: to serve these three combos you run either ~1 GB of Node or one 10 MB static binary. Traffic figures cover the dispatch path against a localhost upstream (no SSE frames relayed); the SSE relay itself is proven by `tests/e2e.rs` against a mock upstream.

## Parity

| Surface | Artificial Route | OmniRoute | agentgateway |
|---|---|---|---|
| providers | **276** (identical set) | 276 | 8 + 13 presets |
| strategies | **20** + 6-variant auto | 20 + auto | 3 (+LB modes) |
| dialects | 6 in, 18 named-missing | 24 cells | 8 formats |
| compression | 3 engines + intensity dial, per-combo **and** per-request | 12 (+stubs), per-combo | 4, reactive, no knob |
| upstream auth | apikey + OAuth dispatch (codex, cline, claude, gemini-cli, cursor) | apikey + 24-entry OAuth | 9 strategies, exchange-only |
| key storage | encrypted local sqlite + `$VAR` fallback | encrypted DB | env/file/`ate-secret://` |
| model refresh | snapshot + import | sync + overlays | catalog + refresh API |
| routes | 13 (`chat`, `messages`, `responses`, `api/chat`, `/v1/completions` legacy alias, `embeddings`, `audio/transcriptions`, `images/generations`, `ocr`, `models`, `healthz`, `metrics`) | full gateway + UI | 22 data-plane |

## Artificial Route vs OmniRoute v16.3.1

Measured 2026-10-01, `ar` 0.1.1 against a live OmniRoute v16.3.1, both on loopback. Every cell is an observation from that one session.

| Surface | Artificial Route 0.1.1 | OmniRoute v16.3.1 |
|---|---|---|
| shared routes | `POST /v1/chat/completions`, `/v1/messages`, `/v1/responses`, `GET /v1/models`, `GET /healthz` | the same five |
| `GET /healthz` | `ok` 200 in **0.4 ms** | `ok` 200 in **4.4 ms** |
| routes beyond those five | `GET /metrics` (Prometheus), `POST /api/chat` (Ollama), `POST /v1/completions` (legacy alias), the media family: `POST /v1/embeddings` (typed reply re-render), `/v1/audio/transcriptions` (multipart verbatim, `?model=` routing), `/v1/images/generations`, `/v1/ocr` | **~723 more**: dashboard, rerank, search, files, batches, WebSocket, tokenized aliases, A2A |
| unknown model | 400, names the routable combos | 400, suggests a `provider/` prefix |
| malformed JSON | 400 | 400 |
| unknown path | 404, JSON envelope carrying the path and the routable list | 404, JSON envelope carrying the path |
| `/v1/*` auth | none, see below | API key required, 401 JSON envelope without one |
| RSS | **~120MB** (re-measured 2026-10-03, `/proc/PID/status` VmRSS: ~110MB of it is anonymous allocator/runtime pages, ~10MB file-backed; the 2026-10-01 figure of ~14.6MB no longer reproduces on the binary the d24ea9a parity program produces) | **~823MB** (**~7x**) |
| cold boot | ~1s | not measured |
| CLI verbs | 10: `serve`, `models`, `providers`, `combo`, `doctor`, `run`, `configure`, `auth`, `import`, plus `mcp` behind `--features mcp` | ~78 |
| MCP tools | 12-tool catalog + `tool_search`, behind the default-off `mcp` feature | 110 tools + A2A |
| terminal-status list | 6 rows + 1 carve-out-only, generated into the store's CHECK | taxonomy it was reconciled against |
| test gate | 42 suites, 1282 passed, 0 failed; clippy `-D warnings` clean | not measured |

No upstream chat call was made in either column, so the shared-route row is a
surface match, not verified call parity. The classifiers agree except for one
owned divergence, first-sighting timing on `(401, token_revoked)`: we retire on
the first sighting, OmniRoute on the third (fix record:
[docs/audit-notes.md](docs/audit-notes.md)). The footprint row is the trade:
those ~723 extra routes, 110 MCP tools and dashboard are real capability and
cost ~823MB, while `ar` covers the routing core in ~14.6MB. The ~823MB figure
is a different OmniRoute process state than the ~994 MiB above.

> [!WARNING]
> `ar serve` listens unauthenticated. `ar-server` has a bearer gate, but it reads
> `Components.master_key` and no shipped command sets that field, so the
> listener is open whatever `AR_MASTER_KEY` holds (that arms the credential
> store, not HTTP auth). Keep it on loopback or front it with an authenticating proxy.

Full depth, OAuth mechanics, refresh triggers, quota tables, engine-catalog divergences and every gap with its fix: [AUDIT-REPORT.md](AUDIT-REPORT.md).

Known gaps, stated plainly: OAuth login works via `ar auth login` — a PKCE browser redirect (loopback catch locally, or carry the URL to any device and paste the redirect back) or, for a session that declares `device_initiate_url`/`device_poll_url`, an RFC 8628 device code you approve on another device — plus four MCP tools, and no refresh/authorize/device endpoint is hardcoded, because an auth endpoint guessed from a provider id would be an invented wire format; `grok-cli` has an executor but only for authentication, its Responses body and `x-grok-*` headers are not transcribed, so it builds a session and still does not dispatch; `kilocode` is an RFC 8628 device-flow provider that also serves a free tier, and both mechanisms are ported — a device code, and `anonymous: true` dispatch that needs no credential row at all — but neither is an OAuth *executor*, because its dispatch wire is not transcribed, so with neither mechanism declared its `oauth/` row is still a `fail` whose fix names the YAML for either one; the full explanation is [AUDIT-REPORT.md](AUDIT-REPORT.md) R1; the local credential store holds API keys plus a non-secret `oauth_sessions` table (which credential name a provider's session lives behind, and whether a refresh retired it — never a token), so the generated terminal-status CHECK has a home, but a `config.yaml` must still *declare* each OAuth key name (a `$VAR` there has to be exported, empty is fine); custom providers are file-declared (`custom_providers:`), so a new endpoint needs no rebuild — but only the OpenAI wire dispatches, and an `id` that collides with a compiled-in one is refused; multi-provider `auto` pools fall back to config order (single-provider pools score identically); the MCP control plane is stdio-only (StreamableHTTP deferred) and ships behind `--features mcp`; the registry snapshot lags OmniRoute's model rotation — `ar doctor` reports the snapshot's age, warns past 7 days, names each stale model and the `ar import` fix; an unknown path returns a bare 404 with no body, where OmniRoute returns a JSON envelope carrying the path, so a client that parses every error as JSON gets nothing to parse on a typo'd route; `ar serve` listens unauthenticated, because the `ar-server` bearer gate is driven by a `Components.master_key` that no shipped command sets (`AR_MASTER_KEY` arms the credential store, not HTTP auth), so loopback or a reverse proxy is the only thing between a caller and the provider keys; and the non-routing surface is deliberately narrow, with no dashboard, embeddings, media, audio, rerank, search, OCR, files, batches, WebSocket or A2A, which is where OmniRoute's other ~723 routes and 110 MCP tools live; and only `x-ar-compression` is read, so a client sending OmniRoute's `x-omniroute-compression` header, or its standard/aggressive/ultra plan modes, is silently ignored.

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh | sh
```

Static musl binary, checksum-verified, no runtime deps. Lands in `~/.local/bin/ar` (`--dir` to change, `--version` to pin).

## Cold start

```sh
export OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-...
ar doctor     # per-check report, never prints secret values
ar serve      # loopback :20128
curl -s localhost:20128/healthz && curl -s localhost:20128/v1/models
```

One-shot without the server: `ar run -m cheap -p 'hello'`.

Keys can also live encrypted at rest in a gitignored `credentials.db` beside the config (`$AR_CRED_STORE` moves it; `$AR_MASTER_KEY` holds the 32-byte master). Resolution order is store → `$VAR`, so an install with no store behaves as before, and `ar doctor` reports the `store` row and which source each key resolved from.

Config defaults to `./config.yaml` (see the repo's for the shape: `keys` → `providers` → `combos`). Mirror your OmniRoute combos 1:1 with `config.omni-mirror.yaml` as the template — same ids, strategies, target order. Point any OpenAI client at `http://127.0.0.1:20128/v1` with `model` set to a combo id.

## Configuration

```yaml
server: { host: 127.0.0.1, port: 20128 }
keys: { openai: $OPENAI_API_KEY }          # env refs, never values
providers: [{ id: openai, key: openai }]   # id must be in the registry
combos:                                    # clients request the combo id as `model`
  - { id: cheap, strategy: cost-optimized, targets: [openai/gpt-5.4-nano] }
```

An OAuth session adds one block. The **access** token is the provider's existing `keys:` entry, the **refresh** token is a second credential row, and the block carries no secret, so a committed `config.yaml` cannot leak one through it.

```yaml
providers: [{ id: codex, key: codex }]
keys:      { codex: $CODEX_ACCESS, codex_refresh: $CODEX_REFRESH }
oauth:
  - provider: codex
    refresh_key: codex_refresh
    token_url: https://auth.openai.com/oauth/token   # required; never guessed
    client_id: <your public OAuth client id>
```

`token_url` is mandatory and deliberately not hardcoded: an auth endpoint inferred from a provider id would be an invented wire format. Without it the session still dispatches, and `ar doctor` says its first 401 will be terminal.

An endpoint the registry lacks is declared in the same file, no rebuild, and `id` resolves exactly like a catalog provider's:

```yaml
custom_providers:
  - { id: my-gateway, protocol: openai-compatible,
      base_url: https://gateway.internal.example/v1, key_ref: my_gateway }
```

`protocol` is `openai-compatible` or `anthropic-compatible`; only the OpenAI wire dispatches today, and `headers:` adds outbound headers for a gateway that wants something other than a bearer token. `ar doctor` checks the base URL shape, the credential binding, and refuses an `id` that collides with a compiled-in one.

> [!WARNING]
> `ar doctor` and `ar serve` share one target grammar: a config one accepts, the other accepts. If `serve` refuses a file, `doctor` says why.

## Documentation

`docs/00-overview.md` → `01..06` are the sources of truth (budgets, subsystems, roadmap, AXI/MCP). `AGENTS.md` holds the enforced Rust disciplines. Detail lives there; this file stays a funnel.

## Contributing

Read `AGENTS.md`, then:

```sh
cargo test --release
cargo clippy --all-targets --all-features --locked -- -D warnings
```

Both green, minimal diffs, no `unwrap` outside tests. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache-2.0](LICENSE) — © 2026 Ishan Parihar.
