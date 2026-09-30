# Artificial Route

Minimal-RAM Rust LLM proxy. One OpenAI-compatible endpoint over many providers.
P0 is the lean-routing spine: `File` config only, no UI, no control plane.

Read `docs/00-overview.md` then `docs/01..06` before changing anything.
`AGENTS.md` holds the enforced Rust disciplines.

## Status

Scaffold. The command surface, config loader, and provider registry exist; the
listener, translator, executor, and router are owned by sibling crates and are
`todo!()` seams (see `crates/ar-core/src/traits.rs`).

## Run

```sh
export OPENAI_API_KEY=sk-...          # config.yaml expands $VAR before parsing
export ANTHROPIC_API_KEY=sk-ant-...
cargo run --release -p ar-cli --bin ar -- doctor
```

`ar` with no arguments prints a content-first home view; `ar --help` lists the
commands. The config path defaults to `./config.yaml` and takes `--config <PATH>`.

```sh
ar doctor      # config, credential presence, registry — never secret values
ar providers   # configured providers, and whether each is in the registry
ar models      # routable models from the compiled-in registry
ar combo       # routing combos and their distinct provider sets
ar serve       # the listener; see "P0 routing spine" below
```

`--fields id,status` narrows any list. Output is TOON on stdout; errors are
structured on stdout with a `help:` line. Exit `0` success, `1` error, `2` usage.

## Gate

```sh
cargo test --release
cargo clippy --all-targets --all-features --locked -- -D warnings
```

## Layout

| Crate | Role |
|---|---|
| `ar-core` | `Strng`, the cross-crate trait seams, placeholder error |
| `ar-config` | `File`-mode YAML, `$VAR` injection, hot reload, `ArcSwap` snapshot |
| `ar-registry` | Compile-time provider catalog (`registry.json`) |
| `ar-cli` | The `ar` binary: AXI/TOON command surface |

---

# P0 routing spine — `ar-route` + `ar-server`

The listener, translator, executor and router that the section above calls
`todo!()` seams now exist. This section documents them; the section above stays
its own.

> **Integration note, for whoever wires `ar-cli` to this.** Two crates currently
> produce a binary named `ar` (`ar-cli` and `ar-server`), and the trait contract
> exists in two places with two shapes. Both are flagged under
> [Open seams](#open-seams) below. Neither is resolved here — `ar-cli` and
> `ar-core` are outside this crate's write scope.

## Status

**Lean-routing, not done routing.** Four strategies, one resilience layer, one
inbound dialect. The other 15 strategies, `auto/*`, quota-share and
`simulate_route` / `explain_route` are deferred to P2 and surface as
`Strategy::Deferred` — a config naming one gets a 501 with the strategy named,
never a panic in the request path. See `docs/05-roadmap.md` P2.

## Manual verify

```sh
cargo build --release -p ar-server
./target/release/ar serve --port 20128
# -> ar 0.1.0 listening on http://127.0.0.1:20128 (1 provider(s))

curl -s http://127.0.0.1:20128/healthz        # ok
curl -s http://127.0.0.1:20128/v1/models     # {"data":[…],"object":"list"}
curl -s http://127.0.0.1:20128/metrics       # ar_http_requests_total 0
```

### Idle RSS

`VmRSS` for one configured provider (`target/release`, `lto = true`,
`codegen-units = 1`, 6.99 MB binary), measured via `/proc/$PID/status`:

| State | VmRSS |
|---|---|
| idle after boot, zero traffic | **4.0 – 6.3 MB** across 4 runs |
| after 2 streamed chat completions | **7.2 MB** (+0.9 MB, ~0.5 MB per stream) |

`provisional:` the idle figure is a range, not a point — four boots gave 4000,
4008, 4152 and 6260 kB, the spread being allocator arena sizing rather than
anything the request path does. Re-measure on the target machine before treating
it as a gate.

Inside the `<35 MB` idle and `+<3 MB` per-stream budgets in
`docs/00-overview.md`. The headroom exists to be spent on `ar-cache` (32 MB
sharded) and `ar-mcp` (P1, +25 MB), neither of which is in this binary.

### Streaming chat, verified against a local SSE upstream

```sh
curl -sN -D - http://127.0.0.1:20128/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'x-ar-session: demo' \
  -d '{"model":"groq/llama-3.3-70b","stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

```
HTTP/1.1 200 OK
x-ar-decision: strategy=priority;outcome=ok;provider=groq;attempts=1
x-ar-usage: attempts=1
x-ar-cache: bypass
content-type: text/event-stream
x-ar-trace-id: 37c45cb3-82a0-48d3-aefc-38d778ab7ac7
transfer-encoding: chunked

data: {"id": "1", "model": "llama-3.3-70b", "stream": true}

data: {"delta": {"content": "Hel"}}

data: [DONE]
```

`transfer-encoding: chunked` is the point: frames are relayed as they arrive,
not buffered into one body. `x-ar-trace-id` is echoed from the request when the
client supplies one, and minted as a uuid v4 when it does not.

An unreachable upstream reports the decision that produced it, never a bare 502:

```
HTTP/1.1 502 Bad Gateway
x-ar-decision: strategy=priority;outcome=failover;provider=openai;attempts=1
{"error":{"code":"upstream_unavailable","message":"all providers failed; last was openai"}}
```

### CLI

```sh
./target/release/ar --version    # ar 0.1.0  (exits before the command graph loads)
./target/release/ar             # ar 0.1.0 — minimal-Rust LLM proxy | 1 provider(s), strategy=priority
./target/release/ar models --fields id,provider,model
```

```
models[1]{id,provider,model}
openai/gpt-4o-mini	openai	gpt-4o-mini
```

## Configuration

All from the environment. P0 has no config file, no hot reload, no auth.

| Variable | Required | Meaning |
|---|---|---|
| `AR_UPSTREAM_URL` | yes | Provider base URL, no trailing slash (e.g. `https://api.openai.com/v1`) |
| `AR_UPSTREAM_MODEL` | yes | Provider-local model name |
| `AR_API_KEY` | no | Bearer credential. Omit for keyless providers. |
| `AR_PROVIDER` | no | Provider id used in routing, cooldowns and the decision header. Default `default`. |
| `AR_STRATEGY` | no | `priority` \| `round-robin` \| `cost-optimized` \| `lkgp`. Default `priority`. |
| `AR_INPUT_USD_PER_MTOK` | no | Price used by `cost-optimized`. Absent = unknown = sorts last. |
| `AR_PORT` | no | Listen port. Default `20128`. |
| `RUST_LOG` | no | Log filter. Default `info`. |

With `AR_UPSTREAM_URL` unset the server still boots: `/healthz` and `/metrics`
answer, and `/v1/chat/completions` returns 503 naming the missing variable. A
proxy that refuses to start is harder to debug than one that says what is
missing.

The bind address is loopback-only. P0 has no auth layer (`ar-keys` is P1), so
binding `0.0.0.0` would publish an unauthenticated LLM proxy.

## Endpoints

| Route | Method | Behaviour |
|---|---|---|
| `/v1/chat/completions` | POST | OpenAI-compatible. SSE pass-through. Fallback chain. |
| `/v1/models` | GET | Catalog, stale-while-revalidate 60s. `x-ar-cache: fresh\|stale`. |
| `/healthz` | GET | Liveness. |
| `/metrics` | GET | Prometheus text exposition. |

Tower layers, outermost first: `trace_id` (mint/echo `x-ar-trace-id`) →
`RequestBodyLimitLayer` (2 MB) → `TimeoutLayer` (120 s, 504).

The timeout bounds the *request*, not the response body. `TimeoutLayer` resolves
when the handler returns a `Response`, which for a streamed completion is as soon
as upstream headers arrive — so a 20-minute generation is fine while a
20-minute *hang* is a 504. Wrapping the body instead would cut every long answer
off at 120 s.

## Routing

Four strategies, ported from
`../OmniRoute/open-sse/services/combo/{strategyDispatch,targetSorters}.ts`:

| Strategy | Behaviour |
|---|---|
| `priority` | Lowest `rank` wins. Stable on ties. |
| `round-robin` | One `AtomicU64` cursor, no lock. |
| `cost-optimized` | Cheapest known input price. **Unpriced sorts last** — routing blind to price is worse than routing to a known-cheap tier. |
| `lkgp` | Pin to the last provider that succeeded for this session (TTL 30 min), else `priority` ordering. A pin to a provider no longer in the candidate set is a config change, not a 500. |

The attempt loop tries `fallback_chain[0]` (where `pick` put the winner) then the
rest, at most 3 providers per request. One attempt's verdict is classified by
`classify_status`, a pure function of `(status, body)` — no clock, no state:

* **2xx** → serve it.
* **400** matching a stop row (`invalid message format`, `malformed`, `context`,
  `prompt`, `token`) → **Abort** 400. The body is wrong for every provider, so
  replaying it is pure latency. Context-overflow and parameter-validation 400s
  are checked *first* and still fail over: "maximum context length" trips the
  `context` row, and without the override a 200k prompt becomes a hard 400
  instead of a failover to a bigger model.
* **429 / 5xx / 401 / 403 / other 4xx / transport error** → **fail over**.

Resilience is one layer, per *key*: exponential backoff 3s → 300s, cleared by any
success, and a `Retry-After` longer than our own schedule wins (the provider
knows its own window). There is deliberately no circuit breaker, no quota-share
and no shadow traffic: a second layer that also skips a key is a second layer
that also has to be reasoned about, and P0 does not need it.

A throttle *anywhere* in the chain outranks a later transport failure. Reporting
only the last verdict turns "your key is busy until 08:14" into a 502, which
reads as "this gateway is broken" and sends the caller to the wrong dashboard.

## Metrics

Five counters, hand-rolled in `crates/ar-server/src/metrics.rs` — a
`prometheus` client would add a registry, a label-validation layer and a
protobuf dependency to print the same five lines. Labels are absent by
construction: `observe_*` takes only enums, so there is no field a caller could
fill with user content.

```
ar_http_requests_total        ar_route_failovers_total
ar_http_throttled_total       ar_upstream_attempts_total
ar_models_refresh_total
```

## Tests

```sh
cargo test -p ar-route --release                          # 41 tests + 1 doc-test
cargo test -p ar-server                                   # 42 lib + 6 bin + 5 e2e
cargo clippy -p ar-route -p ar-server --all-targets --all-features --locked -- -D warnings
```

All three clean. `e2e_single_provider_when_key_set` drives the real router
through every layer against a real mock upstream and asserts the SSE frames are
relayed with their boundaries intact, the bearer token and rewritten model
reached upstream, and `x-ar-decision` / `x-ar-usage` / `x-ar-cache` /
`x-ar-trace-id` are present.

The e2e suite found two real bugs during development, both fixed: the decision
header reported only the outcome and not the strategy that produced it, and a 429
on the first provider followed by a transport error on the second reported 502
instead of `Retry`.

## Open seams

Two things this crate could not close from inside its own write scope:

1. **Two `ar` binaries.** `ar-cli` and `ar-server` both declare `[[bin]] name =
   "ar"`. Cargo builds both; `target/{release,debug}/ar` is whichever linked
   last, and bare `cargo run --bin ar` is ambiguous. Use
   `cargo run -p ar-server --bin ar -- serve --port 20128` until they are
   merged, at which point `ar-cli`'s `serve` should delegate to
   `ar_server::server()` rather than re-implement a listener.

2. **Two shapes of the same trait contract.** `ar-core::traits` declares
   `ArTranslate` / `ArExec` / `ArRoute` over `serde_json::Value` and
   `RouteTarget`, with `TODO(#P0-translate)` / `TODO(#P0-route)` marking them
   for replacement once the concrete crates exist. `ar-route::contract` now
   declares `ArTranslate::to_canonical(&[u8]) -> Result<CanonicalRequest, _>`,
   `ArExec::post_chat(&ProviderId, &CanonicalRequest)`, and the object-safe
   `Executor` the attempt loop needs, with real types and real errors.

   `ar-route` does **not** implement `ar_core::traits::ArRoute`, deliberately:
   that signature carries no strategy, no session key, no price and no rank, and
   `RouteTarget` has no field for them — `cost-optimized` and `priority` are
   unimplementable against it. Adapting down would satisfy the letter of the
   placeholder and delete the feature. The reconciliation belongs in `ar-core`:
   either widen `RouteTarget` per its own TODO, or delete the three placeholder
   traits in favour of `ar_route::contract`.

Licence for both crates: Apache-2.0, matching `../agentgateway`.
