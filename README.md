# Artificial Route

**One OpenAI-compatible endpoint over 276 providers — a static binary that idles at ~28 MiB serving the full catalog.**

[![release](https://img.shields.io/github/v/release/ishan-parihar/artificial-route)](https://github.com/ishan-parihar/artificial-route/releases) [![license](https://img.shields.io/badge/License-Apache--2.0-blue)](LICENSE) [![musl](https://img.shields.io/badge/binary-static--musl-lightgrey)](https://github.com/ishan-parihar/artificial-route/releases) ![tests](https://img.shields.io/badge/tests-1910_passing-brightgreen)

A minimal-RAM Rust port of the [OmniRoute](https://github.com/ishan-parihar/OmniRoute) gateway core: same combos, strategies and dialects, none of the desktop. File YAML in, SSE out — no control plane, no UI, no runtime dependencies.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh | sh
```

That one command installs the newest release, writes a working `~/.config/ar/config.yaml`, writes a `600` credential env file, and installs and starts a **systemd unit** so the proxy comes up on boot.

It does **not** add the weekly update timer when piped. The timer runs a saved copy of the installer, and a piped `curl | sh` has no file to copy — `$0` is `sh`. For unattended updates, save it first:

```sh
curl -fsSL -o install.sh https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh
sh install.sh
```

The default install is **lite**: the rust binary only. The web dashboard — ~2 GiB of compiled Next.js built locally by `dashboard/rebrand-dist.sh`, too large to ship as a release asset — installs from that local tree with `--with-dashboard`; every flag is in the table under [Installation](#installation).

Then:

```sh
export OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-...
aroute doctor     # per-check report, never prints secret values
aroute serve      # loopback :20128
curl -s localhost:20128/healthz && curl -s localhost:20128/v1/models
```

Full flag table, service details and `--uninstall` are further down under
[Installation](#installation).

## Features

- **21 routing strategies + `auto/*`** — priority, least-used, quota-weighted/fair, cost-optimized, expiry-first, reset-aware, fusion, pipeline… plus an 8-variant auto factory scored over 16 factors, with `simulate`/`explain` in the library
- **276 providers, 1559 models** compiled in — the same provider set as OmniRoute's registry, regenerated from it by `aroute import --from omniroute`
- **6 wire dialects** — OpenAI, Anthropic, OpenAI-Responses, Gemini, Ollama inbound, OpenAI render; 18 further pairs named loudly instead of half-ported
- **3 compression engines** (lite, rtk, caveman) with honor-and-echo semantics, per-combo in `config.yaml` and per-request via header, on an intensity dial (`rtk` minimal/standard/aggressive, `caveman` lite/full/ultra) rather than a dozen half-wired engine ids
- **OAuth dispatch** — codex, cline, claude, gemini-cli, cursor and grok-cli: token injection, refresh on expiry, one rotation retry on 401, per-connection single-flight so a concurrent burst cannot trip `refresh_token_reused`. Two login mechanisms, both with every endpoint operator-supplied: PKCE browser redirect, and RFC 8628 **device flow** for providers that publish no redirect endpoint
- **One terminal-status list** — 6 `(status, reason)` rows plus 1 carve-out-only row, generated into the store's CHECK clause; Cursor's `expired` and Claude's transient `invalid_grant` are carve-outs toward retry, grok-cli's `invalid_client` is a carve-out toward terminal, and a transient can never retire an account — the one remaining intentional divergence from OmniRoute's taxonomy is first-sighting timing on `(401, token_revoked)`: we retire it on the first sighting, OmniRoute on the third attempt
- **Quota-aware failover** — per-key backoff, `Retry-After` wins, throttle anywhere outranks later transport failure
- **Policy & gate** — a `limits:` block (per-key rpm + cumulative dollar/token ceilings + `refuse_unpriced`) enforced at the edge: `429` with `Retry-After` before any body work, `402` with the denying arm in `x-ar-deny-reason` before dispatch, spend checked against the same usage ledger the response path records into; a bearer gate armed by `aroute keys arm-gate` (master in the encrypted store, 30-day signed client tokens, `keys mint` for more, `aroute doctor` reports armed/unarmed); `aroute limits` views and edits the block, and `aroute sync` carries the dashboard's per-key rpm into it
- **Agent-native CLI** — TOON output, `--fields` narrowing, stdout data / stderr diagnostics, exit 0/1/2, fail-loud flags
- **Hot-reload config** — `config.yaml` watched live; secrets stay in `$VAR`, never in the file

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

- **2 of 276 providers cannot route.** `bedrock` resolves its endpoint per AWS
  region at request time behind SigV4, and `gitlab-duo` builds its URL at
  runtime from environment variables through a function call. Both are named on
  stderr by the importer rather than given an invented base URL. The other 274
  carry a usable one.
- **The 12-hour soak is not claimed.** The harness (`scripts/soak.sh`) exists and
  has run four times — longest partial 51 samples, +996 KB, zero failures — but
  no 12-hour run has completed against a released binary. P6's accept line is
  therefore open.
- **`grok-cli` authenticates but does not dispatch** — its Responses body and
  `x-grok-*` headers are not transcribed.
- **`kilocode` has neither mechanism declared**, so its `oauth/` row is a `fail`
  whose fix names the YAML for either one. Its device flow and anonymous dispatch
  are both ported; the dispatch wire is not.
- **Multi-provider `auto` pools fall back to config order.** Single-provider
  pools score identically; the auto scoring engages for literal `auto/*` names.
- **Custom providers dispatch the OpenAI wire only.** An `anthropic-compatible`
  node is accepted and named by `aroute doctor`, then refused at dispatch.
  The same limit reaches registry providers whose OmniRoute entry points at an
  Anthropic endpoint — `zenmux-free` (base URL `…/api/anthropic/v1/messages`)
  serves the catalog but returns `upstream_unavailable` at dispatch, so any
  combo whose targets are all Anthropic-wire cannot route until the
  anthropic-messages executor lands.
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
| test gate | 44 suites, 1910 passed, 0 failed; clippy `-D warnings` clean | not measured |

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

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh | sh
```

Static musl binary, checksum-verified, no runtime deps. Lands in `~/.local/bin/aroute` and does the whole job in one command: installs the newest release, writes a working `config.yaml` under `~/.config/ar/` (never overwriting an existing one), writes a `600` env file for credentials, installs a **systemd unit** so the proxy starts on boot, and finishes by running `aroute doctor` so the result is proved rather than asserted.

| flag | effect |
| --- | --- |
| `--version <tag>` | pin an exact tag instead of the newest release |
| `--dir <path>` | install somewhere else (default `~/.local/bin`) |
| `--service <scope>` | `system` \| `user` \| `none`; defaults to `system` under root, else `user` |
| `--no-service` | same as `--service none` |
| `--lite` | rust binary only — **the default**; the weekly update timer always runs this mode |
| `--with-dashboard` | also install the web dashboard (see [Dashboard](#dashboard)): ~2 GiB of compiled Next.js with its traced `node_modules`, built locally by `dashboard/rebrand-dist.sh` and installed from that tree — nothing is fetched from npm. Writes an `aroute-dashboard.service` unit, **not started** |
| `--dashboard-src <path>` | the dist to install: the `dashboard/dist` directory or a tarball of it; defaults to `dashboard/dist` beside the script |
| `--check` | installed vs newest, config and dashboard presence, service and dashsvc state — changes nothing |
| `--uninstall` | remove binary, unit, timer and dashboard dist; **keeps** config, credentials and the dashboard's data DB |

Re-running it *is* the update path: it replaces the binary, leaves your config
and credentials untouched, and refreshes the unit. `--check` tells you honestly
whether an update exists.

Unattended updates are a systemd timer (`aroute.update.timer`, weekly, with a first
re-check 15 minutes after boot). It runs a **saved copy of the installer**
rather than a URL, so the audited script is what runs, and an updated binary
restarts the service so the live proxy changes too. The timer never passes a
dashboard flag, so an installed dist is left alone by weekly updates.

The unit is `aroute.service`, `Restart=on-failure`, reading credentials from an
`EnvironmentFile` so secrets stay out of the process table. It does **not**
override the bind address: loopback comes from `server.host` in the config, so
the sample config binds `127.0.0.1` and an operator who edits that line changes
where it listens. A user unit enables `loginctl enable-linger`, without which
"starts on boot" is false for any session that logs out first.

Nothing is claimed that is not checked: a failed start is reported as such, and
the usual cause on a first run is empty credentials in the env file, not a
broken unit.

## Running it

The dashboard is a vendored, rebranded build of the OmniRoute front end: `aroute dashboard` starts the compiled UI as a supervised Node child, resolving the dist from `--path`, `$AR_DASHBOARD_DIR`, beside the binary, or the `--with-dashboard` install location (`~/.config/ar/dashboard`) — no flags needed after an install. `dashboard/rebrand-dist.sh` is the one-step rebuild path (pull upstream → `npm ci` → build → rebrand); refresh an installed dist by re-running the installer with `--with-dashboard`. Detail lives in [docs/14-web-ui-port-plan.md](docs/14-web-ui-port-plan.md).

The install already puts `aroute` on a systemd unit, so on a box with systemd the
proxy is already running — `curl -s localhost:20128/healthz` answers before you
type anything. The commands above are for the no-systemd case, or for running a
second instance in the foreground. One-shot without a server at all:
`aroute run -m cheap -p 'hello'`.

Smoke-tested locally on 2026-10-08: `aroute serve` + `aroute dashboard` serve
`GET /healthz`, `GET /v1/models`, `GET /api/health`, `GET /` and `GET /login` and
return valid OpenAI JSON for `free-stack` and `small-stack` chat completions.

Keys can also live encrypted at rest in a gitignored `credentials.db` beside the config (`$AR_CRED_STORE` moves it; `$AR_MASTER_KEY` holds the 32-byte master). Resolution order is store → `$VAR`, so an install with no store behaves as before, and `aroute doctor` reports the `store` row and which source each key resolved from.

## Configuration

```yaml
server: { host: 127.0.0.1, port: 20128 }
keys: { openai: $OPENAI_API_KEY }          # env refs, never values
providers: [{ id: openai, key: openai }]   # id must be in the registry
combos:                                    # clients request the combo id as `model`
  - { id: cheap, strategy: cost-optimized, targets: [openai/gpt-5.4-nano] }
```

Three keys, and every one is load-bearing: without `combos` no model is
routable and `aroute doctor` says so, because providers are addresses and only a
combo is something a client can ask for.

A provider may name more than one credential, and the extras cycle: `key` is
the first, `keys:` lists the rest in try order, and a throttle (429) or an auth
refusal (401/403) on one moves traffic to the next instead of cooling the
whole provider — a 5xx is the endpoint's fault, not the credential's, and
charges the key nothing. `Retry-After` on a throttle is honored per key, and an
auth refusal doubles the lockout per strike, so a dead key is probed
geometrically less often rather than forgotten.

```yaml
providers: [{ id: nvidia, key: nvidia, keys: [nvidia-2, nvidia-3] }]
```

`aroute sync` builds that list itself: the dashboard's connections beyond each
provider's first become numbered credentials (`nvidia-2` holding
`$AR_KEY_NVIDIA_2`), in the dashboard's own priority order — a provider with
seven keys there is never stuck on the first one here.

To adopt an OmniRoute provider tree wholesale, `aroute import --from omniroute
--path <OmniRoute/open-sse/config/providers> --out-dir <dir>` regenerates the
config and the registry from OmniRoute's own source. It reports every provider
it could not give a base URL rather than inventing one. (`config.omni-mirror.yaml`
in this repo is a hand-written template and is **stale** — it describes an
OmniRoute state that no longer exists on disk.)

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

`token_url` is mandatory and deliberately not hardcoded: an auth endpoint inferred from a provider id would be an invented wire format. Without it the session still dispatches, and `aroute doctor` says its first 401 will be terminal.

An endpoint the registry lacks is declared in the same file, no rebuild, and `id` resolves exactly like a catalog provider's:

```yaml
custom_providers:
  - { id: my-gateway, protocol: openai-compatible,
      base_url: https://gateway.internal.example/v1, key_ref: my_gateway }
```

`protocol` is `openai-compatible` or `anthropic-compatible`; only the OpenAI wire dispatches today, and `headers:` adds outbound headers for a gateway that wants something other than a bearer token. `aroute doctor` checks the base URL shape, the credential binding, and refuses an `id` that collides with a compiled-in one.

> [!WARNING]
> `aroute doctor` and `aroute serve` share one target grammar: a config one accepts, the other accepts. If `serve` refuses a file, `doctor` says why.

### Guardrails

Every request passes the same two stages as OmniRoute's sanitizer: credentials and PII are redacted before the body is logged, cached or forwarded, and prompt-injection families (`override`, `system_leak`, `delimiter_injection`, plus role/jailbreak redactions) match upstream's exact needle sets — probes carry the `(system|initial|hidden|original)` qualifier upstream's #4041 requires, and bare template tokens that occur in ordinary code are never refusal triggers. Enforcement follows upstream's `INPUT_SANITIZER_MODE`: `warn` by default — rule names are logged and the request forwards — and `block` refuses with `400` before dispatch.

### Limits

The `limits:` block is the policy layer, and it is opt-in: a config that never mentions it serves exactly as before. `limits.default` governs the anonymous bucket and every key without a row; `limits.keys.<client-key>` overrides per credential. Each row arms any subset of four ceilings — `rpm` (requests per minute: a token bucket keyed by the authenticated client key, bursting to `rpm` and refilling continuously), `usd_micros` and `tokens` (cumulative spend ceilings, checked against the usage ledger before dispatch), and `refuse_unpriced` (fail closed on a model with no pricing row, so an unpriced model cannot slip through a dollar cap as "free"):

```yaml
limits:
  default: { rpm: 60 }
  keys:
    prod-deploy: { rpm: 600, usd_micros: 5_000_000, tokens: 20_000_000 }
```

Enforcement sits at the two points where refusing costs nothing. Past `rpm` the request is refused `429` with `Retry-After` before any body work — the wait is the bucket's own answer, rounded up one second so a client that waits exactly it is admitted. A projected spend at or past a ceiling is refused `402 Payment Required` with the arm named in `x-ar-deny-reason` (`usd_cap`, `token_cap`, `unpriced`, `unauditable`) and in the error envelope's `reason` — never the amounts, which the operator already knows. The projection prices the resolved model from the same pricing rows the response path bills against: input tokens are counted, output tokens are the request's own `max_tokens`, and the token ceiling counts both because the ledger records both. Cache hits skip the spend arm — a hit serves from memory and is never recorded as spend, so the cap does not deny it either. The spend arms read the usage ledger (`ar serve` opens one by default); without one they stay inert rather than pretending a ceiling they cannot evaluate, and a ledger that errors under the request fails closed. The mechanism is ported from agentgateway's local rate limiter minus its CEL bucket-keying and cache dependency; `crates/ar-limit`'s and `crates/ar-server/src/policy.rs`'s module docs name their source.

`aroute limits` prints the block as one row per key, `set` merges arms into a row (`--key` names a client key; the `default` row without it), and `clear` removes a row or the block. `aroute sync` updates each dashboard key's `rpm` — `api_keys.max_requests_per_minute`, the one dashboard column that maps 1:1 — and leaves every other arm as written: the dashboard's daily/weekly dollar and token windows are a different shape from these cumulative arms and are deliberately not carried.

### Dashboard

`aroute dashboard` serves the rebranded web UI on loopback with its data at `~/.config/ar/dashboard-data` — the one integrated config DB. Providers, API keys, combos and models are configured there, in the browser. `aroute sync` is the bridge onto the routing engine: it rewrites the `combos:` and `custom_providers:` sections of `config.yaml` from that DB, materialises every provider connection's API key into the env file (highest-priority active connection per provider), guarantees every `$AR_KEY_*` reference exists so a sync can never hand `serve` a config it cannot load, and restarts `aroute.service` so the boot-built routing tables pick the changes up — `--no-restart` prints the command instead. What the dashboard saves is what the proxy routes.

The dist is ~1.6 GiB of compiled Next.js with its traced `node_modules` — too large for a release asset — so it is built locally (`dashboard/rebrand-dist.sh`) and installed from that tree: `install.sh --with-dashboard --dashboard-src <dashboard/dist>` lays it down at `~/.config/ar/dashboard`, which `aroute dashboard` resolves with no flags and nothing fetched from npm. The default install is `--lite`: the Rust binary only, and the weekly update timer never touches an installed dashboard.

The dist ships **without the ONNX model-inference stack**: `onnxruntime-node` (288 MB), `onnxruntime-web` (141 MB) and their `@huggingface`/`@atjsh/llmlingua-2` companions are ~450 MB for two features that are off unless an operator opts in — OmniRoute's vector memory is `enabled: false` by default, and LLMLingua compression is opt-in with a fail-open contract, so a missing module degrades those two features rather than breaking the server (verified: boot, combos and completions all serve with the stack stripped). `dashboard/enable-onnx.sh` fetches it into an installed dist when the memory or real-LLMLingua features are actually turned on.

When the dashboard *is* installed it comes up as its own unit, **stopped by default** — `--with-dashboard` writes the unit, starts nothing:

```sh
systemctl --user start aroute-dashboard   # UI up; the Node child holds ~700 MB RSS
systemctl --user stop  aroute-dashboard   # RSS reclaimed, port 20149 freed
systemctl --user enable aroute-dashboard  # optional: bring it up at boot instead
sh install.sh --check                    # reports dashboard + dashsvc state
```

The proxy has never needed the dashboard: `serve` reads config and env only, so on a memory-tight box the UI is a deliberate, reversible choice. An update while the unit is running stops it for the binary swap and restarts it on the new build (`dashsvc: restarted on aroute <version>`), so unattended updates never leave a stale dashboard or a half-swapped binary.

For the initial one-time import, `aroute import --from omniroute --combos <storage.sqlite>` reads real OmniRoute combos: nested `combo-ref` steps expand into the parent chain, per-step prompts, tags, connection allow-lists, weights and quota-exhaustion-only markers are preserved, and `fallbackOnlyOnQuotaExhaustion` steps are copied into the combo's `pool:` — without `--combos` the importer falls back to one combo per `provider/model`. One operational note: OmniRoute encrypts provider keys at rest (`enc:v1:` AES-256-GCM); a dashboard instance without `STORAGE_ENCRYPTION_KEY` cannot open them, and `aroute sync` deliberately skips such rows with a note rather than materialise ciphertext as a credential — decrypt the connections in the dashboard first, then re-sync. Every verified dispatch claim in this file comes from the [streaming battery](#streaming-under-load).

## Documentation

`docs/` 00–07 are the standing sources of truth (budgets, subsystems, roadmap, AXI/MCP); 11–15 are the port plans with their closeouts — 15 is the policy/security layer. `AGENTS.md` holds the enforced Rust disciplines. Detail lives there; this file stays a funnel.

## Contributing

Read `AGENTS.md`, then:

```sh
cargo test --release
cargo clippy --all-targets --all-features --locked -- -D warnings
```

Both green, minimal diffs, no `unwrap` outside tests. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache-2.0](LICENSE) — © 2026 Ishan Parihar.
