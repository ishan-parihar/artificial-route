# Artificial Route

**One OpenAI-compatible endpoint over 276 providers — in 10 MB of static binary idling at ~11 MiB RSS.**

[![release](https://img.shields.io/github/v/release/ishan-parihar/artificial-route)](https://github.com/ishan-parihar/artificial-route/releases) [![license](https://img.shields.io/badge/License-Apache--2.0-blue)](LICENSE) [![musl](https://img.shields.io/badge/binary-static--musl-lightgrey)](https://github.com/ishan-parihar/artificial-route/releases) ![tests](https://img.shields.io/badge/tests-800%2B_passing-green)

A minimal-RAM Rust port of the [OmniRoute](https://github.com/ishan-parihar/OmniRoute) gateway core: same combos, strategies and dialects, none of the desktop. File YAML in, SSE out — no control plane, no UI, no runtime deps.

## Features

- **20 routing strategies + `auto/*`** — priority, least-used, quota-weighted/fair, cost-optimized, reset-aware, fusion, pipeline… plus a 6-variant auto factory scored over 16 factors, with `simulate`/`explain` in the library
- **276 providers, 1559 models** compiled in — same provider set as OmniRoute's registry, verified byte-identical
- **6 wire dialects** — OpenAI, Anthropic, OpenAI-Responses, Gemini, Ollama inbound, OpenAI render; 18 further pairs named loudly instead of half-ported
- **3 compression engines** (lite, rtk, caveman) with honor-and-echo semantics, per-combo in `config.yaml` and per-request via header, on an intensity dial (`rtk` minimal/standard/aggressive, `caveman` lite/full/ultra) rather than a dozen half-wired engine ids
- **OAuth dispatch** — codex, cline, claude, gemini-cli and cursor: token injection, refresh on expiry, one rotation retry on 401, per-connection single-flight so a concurrent burst cannot trip `refresh_token_reused`
- **One terminal-status list** — 9 `(status, reason)` rows, generated into the store's CHECK clause; Cursor's `expired` and Claude's transient `invalid_grant` are carve-outs, and a transient can never retire an account
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
| routes | 7 (`chat`, `messages`, `responses`, `api/chat`, `models`, `healthz`, `metrics`) | full gateway + UI | 22 data-plane |

Full depth — OAuth mechanics, refresh triggers, quota tables, engine-catalog
divergences, and every gap with its fix — lives in [AUDIT-REPORT.md](AUDIT-REPORT.md).

Known gaps, stated plainly: OAuth **browser-session** login (the `*-web` providers) is not implemented, and no refresh endpoint is hardcoded — `token_url` is operator-supplied, because an auth endpoint guessed from a provider id would be an invented wire format; `grok-cli` and `kilocode` have no executor and fail `ar doctor` loudly rather than routing; the local credential store holds API keys only, so a `config.yaml` must still *declare* each OAuth key name (a `$VAR` there has to be exported, empty is fine) and the store has no table for the terminal-status CHECK; custom providers are file-declared (`custom_providers:`), so a new endpoint needs no rebuild — but only the OpenAI wire dispatches, and an `id` that collides with a compiled-in one is refused; multi-provider `auto` pools fall back to config order (single-provider pools score identically); the MCP control plane is stdio-only (StreamableHTTP deferred) and ships behind `--features mcp`; the registry snapshot lags OmniRoute's model rotation — `ar doctor` reports the snapshot's age, warns past 7 days, names each stale model and the `ar import` fix.

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

Keys can also live encrypted at rest in a gitignored `credentials.db` beside the config
(`$AR_CRED_STORE` moves it; `$AR_MASTER_KEY` holds the 32-byte master). Resolution order
is store → `$VAR`, so an install with no store behaves exactly as before. `ar doctor`
reports the `store` row and which source each key resolved from.

Config defaults to `./config.yaml` (see the repo's for the shape: `keys` → `providers` → `combos`). Mirror your OmniRoute combos 1:1 with `config.omni-mirror.yaml` as the template — same ids, strategies, target order. Point any OpenAI client at `http://127.0.0.1:20128/v1` with `model` set to a combo id.

## Configuration

```yaml
server: { host: 127.0.0.1, port: 20128 }
keys: { openai: $OPENAI_API_KEY }          # env refs, never values
providers: [{ id: openai, key: openai }]   # id must be in the registry
combos:                                    # clients request the combo id as `model`
  - { id: cheap, strategy: cost-optimized, targets: [openai/gpt-5.4-nano] }
```

An OAuth session adds one block. The **access** token is the provider's existing
`keys:` entry; the **refresh** token is a second credential row; the block itself
carries no secret, so a committed `config.yaml` cannot leak one through it.

```yaml
providers: [{ id: codex, key: codex }]
keys:      { codex: $CODEX_ACCESS, codex_refresh: $CODEX_REFRESH }
oauth:
  - provider: codex
    refresh_key: codex_refresh
    token_url: https://auth.openai.com/oauth/token   # required; never guessed
    client_id: <your public OAuth client id>
```

`token_url` is mandatory and deliberately not hardcoded: an auth endpoint
inferred from a provider id would be an invented wire format. Without it the
session still dispatches, and `ar doctor` says its first 401 will be terminal.

An endpoint the registry does not carry is declared in the same file, with no
rebuild — `id` resolves exactly like a catalog provider's:

```yaml
custom_providers:
  - { id: my-gateway, protocol: openai-compatible,
      base_url: https://gateway.internal.example/v1, key_ref: my_gateway }
```

`protocol` is `openai-compatible` or `anthropic-compatible`; only the OpenAI wire
dispatches today, and `headers:` adds outbound headers for a gateway that wants
something other than a bearer token. `ar doctor` checks the base URL shape, the
credential binding, and refuses an `id` that collides with a compiled-in one.

> [!TIP]
> `config.omni-mirror.yaml` mirrors three live OmniRoute combos 1:1 — same
> ids, strategies, target order. Copy it as your starting point.

> [!WARNING]
> `ar doctor` and `ar serve` share one target grammar: a config one accepts,
> the other accepts. If `serve` refuses a file, `doctor` says why.

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
