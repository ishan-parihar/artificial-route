# Routing quality: the audit and the fixes

Dated-frozen: written `2026-10-19`. The state of every item below is in
[`docs/28-gap-ledger.md`](28-gap-ledger.md); this document owns the **rationale**
and the **evidence** for the routing work.

## What "production grade" means here

OmniRoute's routing is the reference bar, but it is a Next.js app with a SQLite
store, a live quota fetcher and a dashboard. This build is a Rust proxy with a
credential store, a compiled-in registry and no quota fetcher. So the bar is
**not** "copy OmniRoute". It is:

> Every strategy this build names must actually rank on a signal the server
> populates; every failure must be charged to the scope that owns it; and every
> decision must be reproducible from its inputs.

The audit that produced this document measured that bar and found it unmet.

## 1. The audit

Two read-only scouts mapped the two sides:

| map | what it found |
|---|---|
| OmniRoute's routing | 20 public combo strategies + 1 internal (`quota-share`), a **second** pluggable router family inside `auto` (`rules`, `score`, `cost`, `latency`, `sla-aware`, `lkgp`), and a **third** account-fallback picker in `auth.ts` |
| ours | 22 strategy names, 22 comparators, one attempt loop, one resilience table |

The reference's shape is the finding: **routing there is three layers**, and
each layer has its own signals. Ours has one layer with twenty-two names.

### 1.1 The signal census — the core finding

`ar-server`'s `candidate()` builder sets four fields: `rank`, `weight`,
`quota` (if the target has one) and `input_usd_per_mtok`. The audit then
checked, for every field the comparators read, whether **anything in the
production path** wrote it:

| signal | comparator that reads it | populated in production? |
|---|---|---|
| `quota` | `headroom`, `reset-window`, `reset-aware`, `quota-weighted`, `quota-share-fair`, `expiry-first` | **no** — `with_quota` was called only from tests |
| `in_flight` | `p2c`, `least-used` (tiebreak) | **no** — no `with_in_flight` call outside `ar-route`'s own tests |
| `context_window` | `context-optimized` | **no** — `0` on every candidate |
| `cached_prefix_tokens` | `cache-optimized` | **no** — `0` on every candidate |
| `reliability`, `latency_inv` | `p2c` | **no** — `TargetLoads::observe` had no production caller |

**Six of twenty-two strategies were degenerate.** A comparator that maximises a
signal nobody sets is `priority` in disguise: every candidate scores
identically and the tiebreak (`rank`) decides. So `headroom`, `reset-window`,
`reset-aware`, `quota-weighted`, `quota-share-fair`, `expiry-first`,
`context-optimized` and `cache-optimized` all did exactly what `priority` does.

Two more names were **exact duplicates**: `fill-first` shared `priority`'s
comparator, and `random` shared `round-robin`'s — a `fetch_add % len` cycle
wearing a random name. (`fill-first` ≡ `priority` is *faithful* to the
reference, whose `fill-first` is also a pass-through at the combo level; `random`
is not — the reference's is a Fisher-Yates shuffle.)

And one was **not wired at all**: `Strategy::Pipeline` had no branch in the
server, so a `strategy: pipeline` combo went through the ordinary attempt loop
where its stages became *fallbacks* — the second stage answered the client's
question instead of the first stage's.

**The reachable routing surface was roughly seven distinct behaviours, not
twenty-two.**

### 1.2 The failure-scope findings

| finding | why it matters |
|---|---|
| The **session never reached the strategy**. `ar-server`'s `order()` called `pick(strategy, None, …)` — a hardcoded `None` | `lkgp`, `context-relay` and `cache-optimized` were unreachable on the request path, while their unit tests passed, because the unit tests call `pick` directly *with* a session. The LKG pin was **written** on every success and **never read**. |
| The **stale pin was never cleared**. `LkgpPins::clear` existed, documented as "called when the pinned provider fails", and had **no caller** | a pinned provider that failed stayed pinned for the whole 30-minute TTL. The reference clears it in eight places (`executeTargetGates.ts`, `executeTargetClassify.ts`). |
| A **client abort was charged to the provider**. `ar-exec` had a precise `ExecError::Aborted`, and the bridge flattened it into an opaque string | the attempt loop could not tell a client disconnect from a dead socket, so it cooled the key, locked the model and ticked the breaker. A burst of cancelled requests would take a healthy provider out of rotation for every other client. The reference's breaker counts an `AbortError` as *not* a provider failure (`isLocalStreamLifecycleError`). |
| The success body was buffered **without a cap** | a 2xx with a 200 MB body had all of it resident in one `Vec` per request — the RAM blowup `docs/00-overview.md` budgets against. The *scan* only ever reads 2 KB. |
| The `auto/*` **strategy** name resolved to `Deferred` → **501** | `ar-config` parsed `auto/cheap` as `Auto`; `ar-route` parsed it as `Deferred`; the server converted through `as_str()`, so the router's answer won. A combo naming a strategy this build ships answered 501. |
| The UTF-8 cap **silently did not cap** | `body.get(..2048)` returns `None` when 2048 lands mid-character, and the `unwrap_or(body)` then scanned the **whole** body. |
| The error and failure bodies were **read whole, then truncated** | `response.bytes()` pulled the entire error page into memory before the 8 KB cap was applied. |
| The `expiry-first` comparator **read the wall clock twice per comparison** | a 40-candidate pool read the clock 78 times per request, and two candidates scored a second apart could disagree. It also made the one strategy whose decision `explain_route` could not reproduce. |
| The pipeline's **stage answer read one dialect** | `/message/content` only — so an OpenAI-shaped stage answer, the shape every provider here returns, was unreadable and the chain stopped after one stage. |

## 2. The fixes

Each has a regression test that **fails without the fix and passes with it**
(verified by reverting each fix and re-running).

| # | fix | the test that proves it |
|---|---|---|
| F1 | the session and the model reach the strategy: `order()` calls `pick_for_model(strategy, session, model, …)` | `the_lkgp_pin_reaches_the_live_routing_path` — the chain leads with the pinned provider, not the config order |
| F2 | the stale pin is cleared when the pinned target fails, **scoped** to that provider | `a_failed_target_clears_the_pin_that_names_it` and `a_failure_of_another_target_keeps_the_pin` |
| F3 | the success body is capped at 8 MiB for the scan, and the **whole** body is still relayed | `a_success_body_past_the_cap_is_truncated_for_the_scan_not_the_relay` |
| F4 | a client abort is a **typed** variant; it neither cools the provider nor ticks the breaker | `a_client_abort_neither_cools_nor_ticks_the_breaker`, with `a_transport_failure_is_still_charged_to_the_provider` as the contrast |
| F5 | `auto/*` is a live strategy that resolves through the virtual factory, over the combo's **own** targets | `an_auto_family_strategy_name_loads_as_a_routable_strategy` and `the_config_and_router_parsers_agree_on_an_auto_family_name` |
| F6 | the quota signal: the store's `quota_snapshots` table now reaches the candidate | `a_stored_quota_reading_reaches_the_target` and `a_quota_signal_makes_the_headroom_strategy_rank` |
| F7 | the load signal: the server observes each outcome and latency, and `p2c` reads it | `an_observed_outcome_reaches_the_load_table` and `p2c_prefers_the_observed_healthier_target` |
| F8 | the context signal: the registry's per-provider window reaches the candidate | `context-optimized` now has something to rank on |
| F9 | `random` is a Fisher-Yates shuffle, distinct from `round-robin` | `random_spreads_its_draws_across_the_whole_pool` and `random_is_not_round_robin` |
| F10 | `pipeline` is wired as a staged dispatch | `a_pipeline_stage_feeds_the_next_stage` — the second stage receives the **first stage's answer**, not the client's question |
| F11 | the bounds: the error body, the failure body, the judge body and the pipeline stage body are all capped **at the read** | the cap asymmetry is closed |

### 2.1 The two that changed the routing surface most

**F6 (the quota signal)** is the largest. The credential store has had a
`quota_snapshots` table since it was ported from OmniRoute's
`013_quota_snapshots.sql`, with a reader (`latest_quota_snapshot`) and a
staleness answer (`QuotaSnapshot::is_stale`). Nothing ever asked. Now the
config path reads the newest reading for a provider and puts it on the target,
so **six strategies have a signal to rank on** for the first time.

The store records a *fraction*; the router wants a *limit and a used count*,
because that is what a rollover is expressed in. The reading is scaled onto a
100-point window. A provider with no reading carries **no** window — unmetered,
not exhausted.

**F10 (the pipeline)** was a strategy that did not exist at request time. The
engine (`dispatch_pipeline`) was complete and fully unit-tested, and had zero
production callers: a `strategy: pipeline` combo silently became a fallback
chain. Wiring it surfaced F11's sibling — the stage answer only read the
Anthropic shape, so a pipeline against an OpenAI provider could never have run
two stages.

## 3. What was deliberately not changed

| not changed | why |
|---|---|
| `fill-first` ≡ `priority` | the reference's `fill-first` is **also** a pass-through at the combo level (`applyStrategyOrdering.ts`: "preserving priority order"). Its real effect is at the *account* level, which this build does not have. The duplication is faithful. |
| the random strategies draw from a deterministic cursor | a deliberate decision (`strategy.rs` module docs): a routing decision an operator cannot re-derive from its inputs is one nobody can debug. `random` now *shuffles* — it is a different distribution from `round-robin` — but it is still seeded deterministically. |
| `fusion`'s pick result is computed and discarded by the server | by design: the fan-out is the dispatch, and `pick` answers for the panel leader only. |
| no attempt-loop backoff | the reference retries the same target up to `maxRetries`; this port has none, and a failed stage ends the chain. A same-target retry in a *fallback* chain would re-spend the prompt against an endpoint that already refused it. The reference's own `failoverBeforeRetry` is opt-in for the same reason. |
| `MAX_ATTEMPTS = 3` | one winner plus two fallbacks. The reference allows 30 global attempts; three is the documented P0 number and the request deadline is the real budget. |

## 4. The remaining gap, stated honestly

**`cached_prefix_tokens` is still always 0.** `cache-optimized` therefore still
degrades to `context-relay` (an HRW affinity) and then to `priority`. The
signal needs a prefix-token count per provider per conversation, which means
the cache layer has to report what it cached — the `ar-cache` affinity digest
computes a prefix hash but does not publish its length. That is a change to
`ar-cache`, not to the router, and it is **not** done.

**The quota signal is coarse.** One window per provider, not per model or per
account. The reference keys on `executionKey` — provider, model *and* account —
so a combo repeating one model across accounts spreads per account. This build
routes by provider, and the account half of that key does not exist here.

**`in_flight` is now live.** The attempt loop acquires the target on dispatch
and releases it on every exit — success, abort, failover, transport failure and
client disconnect. The count is the one signal an *observation* cannot supply:
an observation is a past measurement, in-flight is a present count. `p2c`'s load
term was always zero before this, so the strategy could not prefer an idle
target over a busy one.

## 5. The live smoke

The fixes were exercised against the **real** stack (a scratch instance on
`127.0.0.1:20197` over the operator's own config and keys), not only in unit
tests. Two live targets: `nvidia/stepfun-ai/step-5-preview` (works) and
`seekai/moonshotai/kimi-k3` (the seekai key is disabled — the worklog records
it, and the live log confirms it: `401`).

**The LKG pin, live.** A combo with `strategy: lkgp` over both targets, one
session key, three requests:

| request | attempts | what it proves |
|---|---|---|
| 1 | **2** | no pin yet, so the dead seekai target was tried and failed over to nvidia |
| 2 | **1** | the pin named nvidia, so the dead target was **never tried again** |
| 3 (a *different* session) | **2** | the pin is **session-scoped**, not global — the dead target is tried again |

Request 2 is the proof of F1. Before the fix the session never reached the
strategy, so every request took 2 attempts and the pin was written and never
read. The server's own log shows the failover live:
`WARN ar_route::attempt: attempt failed over provider=seekai status=401`.

**The `auto/*` strategy, live.** A combo with `strategy: auto/cheap` over the
same two targets answered **200** with
`x-ar-decision: strategy=auto/cheap;outcome=ok;provider=nvidia`. Before F5 that
combo answered **501 Not Implemented**, naming a strategy this build ships.

**No regression.** The ordinary `small-stack` combo still answers correctly
(`provider=kilocode;model=reflection/beam-free;attempts=1`, 717 ms) with the
full `x-ar-*` header set: decision, usage, cache, compression, model, version,
tokens in/out, cost, latency and fallback attempts.

## 6. The commands

```
cargo test --workspace --locked          # 45 suites, all green
cargo clippy --all-targets --all-features --locked -- -D warnings
```

Every fix above has a test that was run against the **unfixed** code and
failed there. That is the claim this document makes: not "a test exists" but
"the test fails without the fix".
