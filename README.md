# Artificial Route

**One OpenAI-compatible endpoint over 276 providers — in 10 MB of static binary idling at ~11 MiB RSS.**

[![release](https://img.shields.io/github/v/release/ishan-parihar/artificial-route)](https://github.com/ishan-parihar/artificial-route/releases) [![license](https://img.shields.io/badge/License-Apache--2.0-blue)](LICENSE) [![musl](https://img.shields.io/badge/binary-static--musl-lightgrey)](https://github.com/ishan-parihar/artificial-route/releases) ![tests](https://img.shields.io/badge/tests-800%2B_passing-green)

A minimal-RAM Rust port of the [OmniRoute](https://github.com/ishan-parihar/OmniRoute) gateway core: same combos, strategies and dialects, none of the desktop. File YAML in, SSE out — no control plane, no UI, no runtime deps.

## Features

- **20 routing strategies + `auto/*`** — priority, least-used, quota-weighted/fair, cost-optimized, reset-aware, fusion, pipeline… plus a 6-variant auto factory scored over 16 factors, with `simulate`/`explain` in the library
- **276 providers, 1559 models** compiled in — same provider set as OmniRoute's registry, verified byte-identical
- **6 wire dialects** — OpenAI, Anthropic, OpenAI-Responses, Gemini, Ollama inbound, OpenAI render; 18 further pairs named loudly instead of half-ported
- **3 compression engines** (lite, rtk, caveman) with honor-and-echo semantics
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
| routing strategies | **20** + 6-variant auto factory | 20 + auto | 3 |
| wire dialects | 6 in, 18 named-missing | 24 cells | passthrough |
| compression | 3 engines | 12+ engines | 0 |
| routes | 7 (`chat`, `messages`, `responses`, `api/chat`, `models`, `healthz`, `metrics`) | full gateway + UI | 22 data-plane |

Known gaps, stated plainly: user-defined custom providers need a registry import; multi-provider `auto` pools fall back to config order (single-provider pools score identically); the 8 MCP tools exist as a library crate not yet wired to the binary; the registry snapshot lags OmniRoute's model rotation — `ar doctor` names each stale model and the `ar import` fix.

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh | sh
```

Static musl binary, checksum-verified, no runtime deps. Lands in `~/.local/bin/ar` (`--dir` to change, `--version` to pin).

## Cold start

```sh
export OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-...
ar doctor     # 9 checks, never prints secret values
ar serve      # loopback :20128
curl -s localhost:20128/healthz && curl -s localhost:20128/v1/models
```

One-shot without the server: `ar run -m cheap -p 'hello'`.

Config defaults to `./config.yaml` (see the repo's for the shape: `keys` → `providers` → `combos`). Mirror your OmniRoute combos 1:1 with `config.omni-mirror.yaml` as the template — same ids, strategies, target order. Point any OpenAI client at `http://127.0.0.1:20128/v1` with `model` set to a combo id.

## Configuration

```yaml
server: { host: 127.0.0.1, port: 20128 }
keys: { openai: $OPENAI_API_KEY }          # env refs, never values
providers: [{ id: openai, key: openai }]   # id must be in the registry
combos:                                    # clients request the combo id as `model`
  - { id: cheap, strategy: cost-optimized, targets: [openai/gpt-5.4-nano] }
```

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
