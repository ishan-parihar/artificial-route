# Artificial Route

[![release](https://img.shields.io/github/v/release/ishan-parihar/artificial-route)](https://github.com/ishan-parihar/artificial-route/releases) [![license](https://img.shields.io/badge/License-Apache--2.0-blue)](LICENSE) [![musl](https://img.shields.io/badge/binary-static--musl-lightgrey)](https://github.com/ishan-parihar/artificial-route/releases) ![tests](https://img.shields.io/badge/tests-2298_passing-brightgreen)

**One OpenAI-compatible endpoint over 276 providers — a static binary that idles at ~28 MiB serving the full catalog.** A minimal-RAM Rust port of the OmniRoute gateway core: same combos, strategies and dialects, none of the desktop. File YAML in, SSE out — no runtime dependencies. A small built-in control page at `/ui` edits the same YAML; the full OmniRoute dashboard is an optional extra.

Every document in `docs/` is listed with the question it answers in [docs/PAGE-INDEX.md](docs/PAGE-INDEX.md).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh | bash
```

That one command installs the newest release, writes a working `~/.config/ar/config.yaml`, writes a `600` credential env file, and installs and starts a **systemd unit** so the proxy comes up on boot.

The command reads `install.sh` from `main`, so the installer is always the newest one. Every binary it downloads is checked against that release's `SHA256SUMS`, and the release carries a build-provenance attestation you can verify with `gh attestation verify aroute-x86_64-unknown-linux-musl --repo ishan-parihar/artificial-route`. Pass `--verify-attestation` to have the installer run that check and refuse to install if it fails. To pin the installer as well, use the tag URL printed in that release's notes instead of `main`.

It does **not** add the weekly update timer when piped. The timer runs a saved copy of the installer, and a piped `curl | bash` has no file to copy — `$0` is `bash`. For unattended updates, save it first:

```sh
curl -fsSL -o install.sh https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh
bash install.sh
```

The default install is **lite**: the rust binary only. The web dashboard — ~2 GiB of compiled Next.js built locally by `dashboard/rebrand-dist.sh`, too large to ship as a release asset — installs from that local tree with `--with-dashboard`; every flag is in the table under [Installation](#installation).

Then:

```sh
export OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-...
aroute doctor     # per-check report, never prints secret values
aroute serve      # loopback :20128
curl -s localhost:20128/healthz && curl -s localhost:20128/v1/models
```

Open **http://127.0.0.1:20128/ui/** for the control page: routing combos, providers and client keys, the live resilience and provider-health state, per-key budgets, usage and a failure log, settings, and a playground. The installer prints this address (from your configured host and port) when it finishes.

Full flag table, service details and `--uninstall` are further down under [Installation](#installation).

## Features

- **21 routing strategies + `auto/*`** — priority, least-used, quota-weighted/fair, cost-optimized, expiry-first, reset-aware, fusion, pipeline… with `simulate`/`explain` in the library. A combo's `strategy:` reaches the pick since 0.1.12 — it was silently shadowed by the global default before. Bare `auto` maps to `least-used`; the 8-variant auto factory engages for literal `auto/*` names - **276 providers, 1559 models** compiled in — the same provider set as OmniRoute's registry, regenerated from it by `aroute import --from omniroute` - **6 wire dialects** — OpenAI, Anthropic, OpenAI-Responses, Gemini, Ollama inbound, OpenAI render; 18 further pairs named loudly instead of half-ported - **3 compression engines** (lite, rtk, caveman) with honor-and-echo semantics, per-combo in `config.yaml` and per-request via header, on an intensity dial (`rtk` minimal/standard/aggressive, `caveman` lite/full/ultra) rather than a dozen half-wired engine ids
- **OAuth dispatch** — codex, cline, claude, gemini-cli, cursor and grok-cli: token injection, refresh on expiry, one rotation retry on 401, per-connection single-flight so a concurrent burst cannot trip `refresh_token_reused`. Two login mechanisms, both with every endpoint operator-supplied: PKCE browser redirect, and RFC 8628 **device flow** for providers that publish no redirect endpoint - **One terminal-status list** — 6 `(status, reason)` rows plus 1 carve-out-only row, generated into the store's CHECK clause; Cursor's `expired` and Claude's transient `invalid_grant` are carve-outs toward retry, grok-cli's `invalid_client` is a carve-out toward terminal, and a transient can never retire an account — the one remaining intentional divergence from OmniRoute's taxonomy is first-sighting timing on `(401, token_revoked)`: we retire it on the first sighting, OmniRoute on the third attempt - **Quota-aware failover** — per-key backoff, `Retry-After` wins, throttle anywhere outranks later transport failure. When every target in the chain is cooling before dispatch, the answer is a `429` with the soonest recovery window in `Retry-After` — never a bare 503 a client would read as "gateway broken" and retry into the same cooling chain. Since 0.1.13 the verdict also reads the body: a non-streaming 2xx that carries an `error` object fails over with its code instead of reaching the client as a 200, a fully auth-locked key set is refused without paying the round trip, and the Cline gateway's `{"success":…,"data":…}` reply skin is stripped (its usage is counted) rather than relayed - **Policy & gate** — a `limits:` block (per-key rpm + cumulative dollar/token ceilings + `refuse_unpriced`) enforced at the edge: `429` with `Retry-After` before any body work, `402` with the denying arm in `x-ar-deny-reason` before dispatch, spend checked against the same usage ledger the response path records into; **per-model ceilings** (`model_tokens:`) measured against that model's own spend, so a model that burns its budget fails over to a sibling instead of taking the key down with it, and an optional rolling `spend_window` makes the ceilings rates rather than one-way totals; a bearer gate armed by `aroute keys arm-gate` (master in the encrypted store, 30-day signed client tokens, `keys mint` for more, `aroute doctor` reports armed/unarmed); `aroute limits` views and edits the block, and `aroute sync` carries the dashboard's per-key rpm into it
- **Built-in control page** — `/ui` on the proxy's own port: edit combos, providers and per-key limits, mint client keys, read the usage and failure log by date range, and try a combo in a playground. Vanilla JS embedded in the binary, no build step, no extra process ([details](#control-page-ui)) - **Agent-native CLI** — TOON output, `--fields` narrowing, stdout data / stderr diagnostics, exit 0/1/2, fail-loud flags - **Hot-reload config** — `config.yaml` watched live; secrets stay in `$VAR`, never in the file

## Benchmarks, parity and open gaps


## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh | bash
```

Static musl binary, checksum-verified, no runtime deps. Lands in `~/.local/bin/aroute` and does the whole job in one command: installs the newest release, writes a working `config.yaml` under `~/.config/ar/` (never overwriting an existing one), writes a `600` env file for credentials, installs a **systemd unit** so the proxy starts on boot, and finishes by running `aroute doctor` so the result is proved rather than asserted.

The written config carries two routable combos out of the box: `default` (an `openai/gpt-5.4` → `anthropic/claude-sonnet-5` least-used chain that you re-point at your own keys) and `glm-5.3` (`nvidia/z-ai/glm-5.3`, a standalone single-target combo — one combo per bare id is the house pattern that keeps harness model pickers stable as stacks change). With the `AR_MODEL_DISCOVERY=1` env line (set it in the env file), `/v1/models` reports each model's real context window — provider-native catalogs first, [models.dev](https://models.dev) second, a 128K floor last.

| flag | effect |
| --- | --- |
| `--version <tag>` | pin an exact tag instead of the newest release |
| `--dir <path>` | install somewhere else (default `~/.local/bin`) |
| `--service <scope>` | `system` \ |
| `--no-service` | same as `--service none` |
| `--lite` | rust binary only — **the default**; the weekly update timer always runs this mode |
| `--with-dashboard` | also install the web dashboard (see [Dashboard](#dashboard)): ~2 GiB of compiled Next.js with its traced `node_modules`, built locally by `dashboard/rebrand-dist.sh` and installed from that tree — nothing is fetched from npm. Writes an `aroute-dashboard.service` unit, **not started** |
| `--dashboard-src <path>` | the dist to install: the `dashboard/dist` directory or a tarball of it; defaults to `dashboard/dist` beside the script |
| `--no-cta` | skip the closing star/donate note (`SKIP_CTA=1`) |
| `--all-harnesses` / `--no-harness` | register the MCP server (`aroute mcp`) in every detected AI harness without asking / skip it (`ALL_HARNESS=1` / `SKIP_HARNESS=1`). Claude Code and Codex are registered and verified; OpenCode gets its JSON printed. Non-interactive runs skip it unless `--all-harnesses` is given |
| `--verify-attestation` | also run `gh attestation verify` on the downloaded binary against this repo, and stop before installing if it fails. Opt-in: needs the [GitHub CLI](https://cli.github.com), and releases published before attestations existed have none to check |
| `--force-root` | allow running as root (default: refuse) |
| `--check` | installed vs newest, config and dashboard presence, service and dashsvc state — changes nothing |
| `--uninstall` | remove binary, unit, timer and dashboard dist; **keeps** config, credentials and the dashboard's data DB |

The installer prints the portable `.mcp.json` entry for aroute's MCP server instead of writing it anywhere; paste it into a project root to use the tools there.

Re-running it *is* the update path: it replaces the binary, leaves your config and credentials untouched, and refreshes the unit. `--check` tells you honestly whether an update exists.

Unattended updates are a systemd timer (`aroute.update.timer`, weekly, with a first re-check 15 minutes after boot). It runs a **saved copy of the installer** rather than a URL, so the audited script is what runs, and an updated binary restarts the service so the live proxy changes too. The timer never passes a
dashboard flag, so an installed dist is left alone by weekly updates.

The unit is `aroute.service`, `Restart=on-failure`, reading credentials from an `EnvironmentFile` so secrets stay out of the process table. It does **not** override the bind address: loopback comes from `server.host` in the config, so the sample config binds `127.0.0.1` and an operator who edits that line changes
where it listens. A user unit enables `loginctl enable-linger`, without which "starts on boot" is false for any session that logs out first.

Nothing is claimed that is not checked: a failed start is reported as such, and the usual cause on a first run is empty credentials in the env file, not a broken unit.

## Running it

The dashboard is a vendored, rebranded build of the [OmniRoute](https://github.com/ishan-parihar/OmniRoute) front end: `aroute dashboard` starts the compiled UI as a supervised Node child, resolving the dist from `--path`, `$AR_DASHBOARD_DIR`, beside the binary, or the `--with-dashboard` install location (`~/.config/ar/dashboard`) — no flags needed after an install. `dashboard/rebrand-dist.sh` is the one-step rebuild path (pull upstream → `npm ci` → build → rebrand); refresh an installed dist by re-running the installer with `--with-dashboard`. Detail lives in [docs/14-web-ui-port-plan.md](docs/14-web-ui-port-plan.md).

The install already puts `aroute` on a systemd unit, so on a box with systemd the proxy is already running — `curl -s localhost:20128/healthz` answers before you type anything. The commands above are for the no-systemd case, or for running a second instance in the foreground. One-shot without a server at all:
`aroute run -m cheap -p 'hello'`.

Smoke-tested locally on 2026-10-08: `aroute serve` + `aroute dashboard` serve `GET /healthz`, `GET /v1/models`, `GET /api/health`, `GET /` and `GET /login` and return valid OpenAI JSON for `free-stack` and `small-stack` chat completions.

Keys can also live encrypted at rest in a gitignored `credentials.db` beside the config (`$AR_CRED_STORE` moves it; `$AR_MASTER_KEY` holds the 32-byte master). Resolution order is store → `$VAR`, so an install with no store behaves as before, and `aroute doctor` reports the `store` row and which source each key resolved from.

## Configuration

```yaml
server: { host: 127.0.0.1, port: 20128 }
keys: { openai: $OPENAI_API_KEY }          # env refs, never values
providers: [{ id: openai, key: openai }]   # id must be in the registry
combos:                                    # clients request the combo id as `model`
  - { id: cheap, strategy: cost-optimized, targets: [openai/gpt-5.4-nano] }
```

Three keys, and every one is load-bearing: without `combos` no model is routable and `aroute doctor` says so, because providers are addresses and only a combo is something a client can ask for.

A provider may name more than one credential, and the extras **rotate**: `key` serves first, `keys:` lists the rest, and a rotating cursor walks them one per dispatch so load spreads across every account instead of burning the first until it throttles — the dynamic round robin OmniRoute promises. A throttle (429) or an auth refusal (401/403) on one skips it and the rotation continues with the next; when every credential is cooling, the soonest to recover serves without advancing the cursor. A 5xx is the endpoint's fault, not the credential's, and charges the key nothing. `Retry-After` on a throttle is honored per key, and an auth refusal doubles the lockout per strike, so a dead key is probed geometrically less often rather than forgotten.

```yaml
providers: [{ id: nvidia, key: nvidia, keys: [nvidia-2, nvidia-3] }]
```

`aroute sync` builds that list itself: the dashboard's connections beyond each provider's first become numbered credentials (`nvidia-2` holding `$AR_KEY_NVIDIA_2`), in the dashboard's own priority order — a provider with seven keys there is never stuck on the first one here.

The router's cooldowns follow the same scope. A provider with more than one key is marked multi-credential at boot, and its credential-scoped faults — a throttle, an auth refusal, quota spent on one key, one banned account — charge the executor's per-key state and nothing at the provider scope, so a fresh
key's very next request routes as if nothing happened. Endpoint-scoped faults (5xx, transport) keep every charge: a dead socket is not one credential's opinion. A single-credential provider keeps the strict P0 semantics unchanged — with one key there is nothing to fall to, so its 429 *is* the provider's —
and the fallback chain no longer collapses same-provider targets: a combo pairing two models on one multi-key provider keeps both, so a 429 on the first fails over to the second inside the same request.

To adopt an OmniRoute provider tree wholesale, `aroute import --from omniroute --path <OmniRoute/open-sse/config/providers> --out-dir <dir>` regenerates the config and the registry from OmniRoute's own source. It reports every provider it could not give a base URL rather than inventing one. (`config.omni-mirror.yaml`
in this repo is a hand-written template and is **stale** — it describes an OmniRoute state that no longer exists on disk.) An OAuth session adds one block. The **access** token is the provider's existing `keys:` entry, the **refresh** token is a second credential row, and the block carries no secret, so a committed `config.yaml` cannot leak one through it.

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

An optional `endpoint:` overrides where dispatches POST — an absolute path joined onto `base_url`, or an absolute URL. Without it every non-Gemini wire goes to the wire's own path (`/chat/completions`); with it the body still rides the wire's renderer, only the URL changes. That is how a non-chat endpoint — OpenRouter's Decisions router (`respan/span-01-lite:free`, `inception/mercury-decide:free`) or Experiential's decisions gateways (`api.experientiallabs.ai/v1/systemone`, `/v1/decisions`) — is served without a second wire: the client posts the endpoint's own JSON to `/v1/chat/completions` and the relay carries it verbatim, `model` rewritten to the target's spelling, reply untouched, `usage` counted (`input_tokens`/`output_tokens` spellings included):

```yaml
custom_providers:
  - { id: or-decisions, protocol: openai-compatible,
      base_url: https://openrouter.ai, endpoint: /api/alpha/decisions,
      key_ref: openrouter, headers: { HTTP-Referer: https://your.site, X-Title: aroute } }
  - { id: exp-systemone, protocol: openai-compatible,
      base_url: https://api.experientiallabs.ai, endpoint: /v1/systemone,
      key_ref: experiential }

combos:
  - { id: decide, strategy: priority,
      targets: [or-decisions/respan/span-01-lite:free, or-decisions/inception/mercury-decide:free] }
```

> [!WARNING] > `aroute doctor` and `aroute serve` share one target grammar: a config one accepts, the other accepts. If `serve` refuses a file, `doctor` says why.

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

A row may also carry `model:` — a combo id (or `provider/model`) — to apply only to requests for that model; the key is unrestricted for any other, falling through to `limits.default`. The scope gates *when* the row applies, not what it counts: the ledger records the serving provider's model rather than the combo asked for, so the spend arms still total the key's spend, and `rpm` stays key-wide because it is checked before the body is read.

```yaml
limits:
  keys:
    ci-runner: { tokens: 2000000, model: cheap }   # capped only when it asks for `cheap`
```

**A per-model ceiling** is the other half of that, and it is the arm that
scopes *what* a ceiling counts. `model_tokens:` names `provider/model` targets
and their own ceilings, measured against **that model's** spend rather than
the key's whole history. A model that has spent its own budget is dropped
from the chain and the request fails over to a sibling that has budget left —
so one model burning a budget is a failover, not an outage for every other
model the key can reach. Only a chain with nothing left is refused, and the
`402` names the models (`x-ar-deny-reason: model_token_cap`).

```yaml
limits:
  keys:
    prod-deploy:
      tokens: 5000000000                          # the key's whole ceiling
      model_tokens:                               # each model's own ceiling
        "kilocode/minimax/minimax-m3.1-flash:free": 1000000000
        "nvidia/z-ai/glm-5.3": 500000000
```

The ceilings are **cumulative** by default, which against an append-only ledger
is a one-way door: a key that has spent its budget is refused until the config
is edited, because the spend can never be un-spent. `spend_window: 24h` makes
them **rates** instead — the spend inside the last 24 hours counts and the
rest falls out of the window, so the key is inside its budget again once the
old spend ages out. It covers both spend arms, so the two can never disagree
about which spend is in force. The window is rolling, not a calendar bucket,
and nothing runs on a timer: the ceiling is evaluated per request.

`aroute limits` prints the block as one row per key, `set` merges arms into a row (`--key` names a client key; the `default` row without it), and `clear` removes a row or the block. `aroute limits set --model-tokens provider/model=N` arms a per-model ceiling and `--spend-window 24h` sets the window; a target no combo routes to is refused at write time, so a typo cannot arm a ceiling that never applies. `aroute sync` updates each dashboard key's `rpm` — `api_keys.max_requests_per_minute`, the one dashboard column that maps 1:1 — and leaves every other arm as written: the dashboard's daily/weekly dollar and token windows are a different shape from these cumulative arms and are deliberately not carried.

### Control page (/ui)

`aroute serve` mounts a control page at `/ui` **on the proxy's own listener** — the same port and the same loopback bind as `/v1`, nothing extra to run. It is a thin editor over `config.yaml`, `ar.env` and the usage ledger, so what it saves is what the proxy loads at its next start (a banner says when a restart is pending).

| pane | what it does |
| --- | --- |
| Dashboard | providers, combos and keys at a glance, total spend |
| Combos | add, reorder, edit and delete routing chains |
| Providers | paste a key (goes to `ar.env`, never shown again), add a registry or custom provider, browse the 276-provider catalog and star favorites (kept in the browser) |
| Resilience | what is out of rotation right now and for how long: the per-provider breaker, the key cooldowns and the `provider:model` lockouts, read from the router's own table |
| Health | whether each credential is present, readable and bound to a live session. A credential that will not decrypt reads as **broken**, which is a different fact from a missing one |
| Budgets | what each key has spent against its `limits:` ceiling, and what each provider's own allowance says |
| Client keys | mint a token (shown once; "Use in playground" hands it over in memory only) and set per-key `rpm`, dollar, token and `model` ceilings |
| Usage | spend by day plus top models, providers and **combos** over a 7/30/90-day range, and a request log filterable by model, provider and failed-only |
| Settings | the `server:` and `limits.default:` blocks of the YAML. The environment-backed values (model deadlines, discovery, auth mode) are shown with the variable that carries them, and are not editable there |
| Playground | multi-turn chat with an optional system prompt through the real `/v1/chat/completions`, streaming |

The request log shows answered requests from the usage ledger and failed ones from a separate `failures` table in the same SQLite file (last 10 000 kept). Failures are recorded from the build that added the table onward, authentication refusals (401/403) are not logged, and a stream that fails after it has already begun is not counted as a failure. The ledger's append-only usage rows are never touched, so spend sums and caps are unaffected.

The panel carries **its own gate**, separate from the one `/v1` uses, because it can mint the tokens that authorize `/v1` and rewrite the config that decides where `/v1` sends traffic. On a box whose gate is armed, every `/ui/api/*` call needs a bearer token; the page asks for one and keeps it in `sessionStorage` for the tab. A `?token=` on the first URL is accepted once and then stripped from the address bar. On a box with **no** gate, the panel is open on its loopback bind — that is the supported default — and refuses a cross-site request, so a page in another tab cannot spend through it. If the credential store's gate row cannot be decrypted, the panel still reads but **refuses every write**, because it cannot prove which credentials it is managing.

The page's behavior has browser checks, `scripts/ui/behave.mjs` (the playground, mint, favorites and log filters) and `scripts/ui/panel-check.mjs` (every pane, the gate, and the settings save). Playwright, run by hand against a demo `serve`; not in CI.

Endpoints behind it, if you want to script them: `GET /ui/api/state|usage|log|models|resilience|health|budgets`, `GET`+`POST /ui/api/settings`, `POST /ui/api/combos|provider-key|provider-custom|provider-remove|keys/mint|limits`. Plans and outcomes: [docs/19](docs/19-ui-upgrade-plan.md), [docs/23](docs/23-ui-round-two-plan.md), [docs/30](docs/30-ui-production-grade.md).

### Dashboard

`aroute dashboard` serves the rebranded web UI on loopback with its data at `~/.config/ar/dashboard-data` — the one integrated config DB. Providers, API keys, combos and models are configured there, in the browser. `aroute sync` is the bridge onto the routing engine: it rewrites the `combos:` and `custom_providers:` sections of `config.yaml` from that DB, materialises every provider connection's API key into the env file (highest-priority active connection per provider), guarantees every `$AR_KEY_*` reference exists so a sync can never hand `serve` a config it cannot load, and restarts `aroute.service` so the boot-built routing tables pick the changes up — `--no-restart` prints the command instead. What the dashboard saves is what the proxy routes.

The dist is ~1.6 GiB of compiled Next.js with its traced `node_modules` — too large for a release asset — so it is built locally (`dashboard/rebrand-dist.sh`) and installed from that tree: `install.sh --with-dashboard --dashboard-src <dashboard/dist>` lays it down at `~/.config/ar/dashboard`, which `aroute dashboard` resolves with no flags and nothing fetched from npm. The default install is `--lite`: the Rust binary only, and the weekly update timer never touches an installed dashboard.

The dist ships **without the ONNX model-inference stack**: `onnxruntime-node` (288 MB), `onnxruntime-web` (141 MB) and their `@huggingface`/`@atjsh/llmlingua-2` companions are ~450 MB for two features that are off unless an operator opts in — OmniRoute's vector memory is `enabled: false` by default, and LLMLingua compression is opt-in with a fail-open contract, so a missing module degrades those two features rather than breaking the server (verified: boot, combos and completions all serve with the stack stripped). `dashboard/enable-onnx.sh` fetches it into an installed dist when the memory or real-LLMLingua features are actually turned on.

When the dashboard *is* installed it comes up as its own unit, **stopped by default** — `--with-dashboard` writes the unit, starts nothing:

```sh
systemctl --user start aroute-dashboard   # UI up; the Node child holds ~700 MB RSS
systemctl --user stop  aroute-dashboard   # RSS reclaimed, port 20149 freed
systemctl --user enable aroute-dashboard  # optional: bring it up at boot instead
bash install.sh --check                    # reports dashboard + dashsvc state
```

The proxy has never needed the dashboard: `serve` reads config and env only, so on a memory-tight box the UI is a deliberate, reversible choice. An update while the unit is running stops it for the binary swap and restarts it on the new build (`dashsvc: restarted on aroute <version>`), so unattended updates never leave a stale dashboard or a half-swapped binary.

For the initial one-time import, `aroute import --from omniroute --combos <storage.sqlite>` reads real OmniRoute combos: nested `combo-ref` steps expand into the parent chain, per-step prompts, tags, connection allow-lists, weights and quota-exhaustion-only markers are preserved, and `fallbackOnlyOnQuotaExhaustion` steps are copied into the combo's `pool:` — without `--combos` the importer falls back to one combo per `provider/model`. One operational note: OmniRoute encrypts provider keys at rest (`enc:v1:` AES-256-GCM); a dashboard instance without `STORAGE_ENCRYPTION_KEY` cannot open them, and `aroute sync` deliberately skips such rows with a note rather than materialise ciphertext as a credential — decrypt the connections in the dashboard first, then re-sync. Every verified dispatch claim in this file comes from the [streaming battery](docs/26-benchmarks-parity-gaps.md#streaming-under-load).

## Documentation

Each crate under `crates/` has a README with its purpose, configuration, extension points, limitations, and the client-facing behaviour. The design documents are in `docs/`:

| Page | What it covers |
|---|---|

`AGENTS.md` holds the enforced Rust disciplines.

## Contributing

Read `AGENTS.md`, then:

```sh
cargo test --release
cargo clippy --all-targets --all-features --locked -- -D warnings
```

Both green, minimal diffs, no `unwrap` outside tests. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache-2.0](LICENSE) — © 2026 Ishan Parihar.

## ⭐ Support

If aroute saved you time, star the repo — it helps others find it.

Want to go further? Run `aroute donate`, or:

[![Sponsor](https://img.shields.io/badge/Sponsor-💜-purple)](https://github.com/sponsors/ishan-parihar)
