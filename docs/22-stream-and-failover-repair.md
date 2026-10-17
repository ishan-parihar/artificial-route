# 22 — Stream and failover repair (2026-10-18)

Status: implemented 2026-10-18 (B1, B2, B3, B5, B6, T3, T5). B4, B7, T2 and
the operator rows are open, with reasons below. Owner of each decision is this
file; `21-remaining-gaps.md` points here for the rows it closes.

Trigger: `free-stack` returned `502 all providers failed; last was nvidia`
after a 110s silence, and streamed answers ended with
`the upstream stream ended before it was complete` even when the answer was
complete. Reproduced against a mock upstream (one behaviour per model) and
against the live gateway. Evidence summarised in the investigation notes for
this date; the mock harness lives outside the repo.

## Defects and their repairs

| ID | Defect | Repair | Decision |
|---|---|---|---|
| B1 | A transient failure (5xx, stall, transport, empty 2xx) cooled the **provider**, so a healthy sibling model on the same provider was skipped as "cooling". `free-stack` has two targets on one provider, so one hang left one target. | Transient failures lock `provider:model` (`LockReason::Unavailable`). The provider breaker still ticks and stays the endpoint-wide guard. | D1 |
| B2 | A stream that carried `finish_reason` but no `data: [DONE]` got an in-band `stream_error` after a complete answer. | Completion is `data: [DONE]` **or** a non-null `finish_reason` string (OpenAI dialect). | D2 |
| B3 | `[DONE]` split across two chunks was missed, so `stream_error` followed `[DONE]`. | Terminator detection runs over a carry window spanning chunk boundaries. | D2 |
| B4 | Pre-commit silence: headers wait for the first upstream byte, up to 110s per attempt. | **Not changed here.** Early commit changes the status contract (see below). | D8 |
| B5 | Retry, Failover and Abort responses carried no `Content-Type`. | JSON errors set `application/json`. | D3 |
| B6 | The in-band truncation frame was emitted without a log line. | Warn log with dialect, provider, model and relayed frame count. | D4 |
| B7 | A failover reports the last provider's status (e.g. 404) and `x-ar-fallback-attempts` counts dispatches only. | **Not changed here.** Pinned by existing contract tests; needs a decision. | D9 |
| T3 | Empty or degenerate 2xx passes as success. | Non-streaming classifier per `07-blockers.md` B2, one classified-empty failover per request. | D5 |
| T5 | Gemini non-streaming bodies reach OpenAI clients as `candidates`, with no `choices`. | Non-streaming Gemini bodies translate to the OpenAI chat envelope through `CanonicalResponse`. | D6 |
| B8 | Found while verifying T5: a non-streamed request without a `stream` field failed to encode on every non-OpenAI wire ("missing field `stream`"). `CanonicalChat.stream` had no serde default. | `#[serde(default)]`: absent means non-streaming. | D11 |
| B9 | Found while verifying T5: a Gemini stream relayed raw ends in `finishReason`, which the OpenAI completion check did not recognise, so a complete answer got a false `stream_error`. | Completion also accepts a `finishReason` string (D2). | D2 |
| B10 | Two stalls in one request exceed the router's 120s stream deadline, so the second is cut by `TimeoutLayer` and the client gets an empty 504. The same happens on multi-provider chains today. | **Ordering fixed.** The router layer and the model deadline both started at 120s, and the layer's timer started first, so the model-aware JSON 504 (`upstream_timeout`, `model_deadline`) never reached the client. The layer now outwaits the model deadline by `LAYER_GRACE` (5s). The budget itself is unchanged. | D10, D12 |
| B11 | Streamed requests wrote `latency_ms: 0` to the trace and audit rows. `UsageTee::record_obs` built its meta without a latency. | `with_latency(dispatched.elapsed())`, matching the work metric. | D12 |
| B12 | A mid-stream upstream failure was logged as `error decoding response body` with no cause. | The warn lines log the error with `?`, which carries the reqwest source chain. | D12 |
| T2 | Cline stream envelope. | **Not reproduced.** The API-key cline row streams plain OpenAI SSE (44 frames, usage, `[DONE]`). The unwrap stays unimplemented until an OAuth cline session streams the envelope. | D7 |

## Decisions

**D1 — Transient failures cool the model, not the provider.** A 5xx, a
no-headers stall, a transport failure, or a classified-empty 2xx locks
`provider:model` with `LockReason::Unavailable`: base `DEFAULT_BASE_BACKOFF`,
escalating per model through `lock_model`, capped at `DEFAULT_MAX_BACKOFF`
(five minutes) so a flaky model is not locked for the thirty-minute model
ceiling. A 400 keeps its breaker exemption. Connect, DNS and TLS are not
split from stalls: a refused connection fails in milliseconds, so trying the
sibling costs nothing. Endpoint-wide protection comes from the provider
breaker (`BreakerClass::Key`, 12 consecutive failures). Multi-credential
401/403 handling and 429 throttles are unchanged.

**D2 — Completion is a terminator, not one byte pattern.** The OpenAI dialect
completes on `data: [DONE]` at a line start, or on a `finish_reason` key whose
value is a JSON string (`null` does not count). A raw-relayed Gemini stream
spells the key `finishReason`, which also counts. A `\"finish_reason\"`
inside escaped content does not match. Anthropic (`event: message_stop`) and
Responses (`response.completed`) keep their terminators, matched through the
same window. Once a terminator is seen, no truncation frame is emitted, so
nothing follows `[DONE]`.

**D3 — Errors are JSON.** Retry, Failover and Abort responses set
`content-type: application/json`. The Ollama dialect is unaffected.

**D4 — Truncation is logged.** The warn line names the dialect, the provider,
the serving model and the relayed frame count, so a real cut and a missing
terminator can be told apart in logs.

**D5 — Empty-completion classifier (B2 spec).** A non-streaming 2xx is
classified empty when its first choice has no tool call and no reasoning, and
its content is blank, or degenerate: only `<|…|>` special tokens and
punctuation, with no alphanumerics and at most three characters after
stripping. A classified-empty attempt fails over like a 502. The loop allows
one classified-empty failover per request. A second empty answer is relayed
unchanged, so an empty-but-valid answer cannot loop. The streaming arm is not
classified: the first-byte gate (MW5) only sees bytes, and a stream cannot be
failed over after it commits.

Three consequences were chosen on purpose. The failover happens only when
another target exists, so a single-target combo still relays its empty answer.
Repeat detection (the spec's "known degenerate repeat") is not implemented: it
needs a measured corpus, and the current rule covers the special-token and
punctuation shapes the register recorded. If the remaining targets are all
skipped after the empty failover, the client gets the 502, not the held empty
answer.

**D6 — Gemini bodies are translated when they are not streamed.** A Gemini
`generateContent` body with text candidates becomes an OpenAI chat completion
through `CanonicalResponse` and `to_openai`. `finishReason` maps to the
canonical reasons: `MAX_TOKENS` to `length`, the safety and recitation family
to `content_filter`, anything else to `stop`. Thought parts are left out of the
content. A body with no text candidate, or with a `functionCall` part, is not
translated and relays as it does today. Tool-call translation is
a separate gap, because the canonical response has no tool-call field yet.
The streamed Gemini path relays unchanged, as the register records.

**D7 — Cline stream unwrap stays open.** The probe on 2026-10-18 found no
envelope on the API-key cline row. Implementing an unwrap without an observed
frame shape would be guesswork, and the register already says so.

**D8 — Pre-commit silence is a contract decision, not a fix.** Headers are sent
only after the loop commits, so a stalled upstream means silence for up to
110s per attempt. Removing it means sending 200 and SSE headers before the
upstream answers, and reporting total failure in-band rather than as a 502.
That changes what existing clients see on failure. Left open for an explicit
decision. The stall itself is now bounded by D1: the sibling is tried after
one stall, and no longer skipped.

**D10 — Two stalls outlast the stream deadline (open).** The start cap is 110s
per attempt (`ar-exec` `STREAM_START_TIMEOUT_CAP`) and the router deadline is
120s (`AR_STREAM_TIMEOUT_SECS`, default 120s everywhere). One stall fails over
in time. A second stall in the same request is cut at 120s with an empty 504.
Options, for an explicit choice: lower the per-attempt start cap, make the
router deadline longer than the chain's start budgets, or give the attempt loop
the deadline so it skips a target it cannot finish.

**Measured on 2026-10-18, direct to nvidia, three samples each, streamed:**
`z-ai/glm-5.3` sent headers in 1.37–2.09s and its first byte in 1.47–4.01s.
`z-ai/glm-5.3-flash` sent headers in 19.51–28.93s. `free-stack` lists glm-5.3
first and glm-5.3-flash second. One 110s stall on glm leaves flash 10s, which is
short of its 19–29s, so `free-stack` cannot recover from a first-target stall
under the 120s deadline. Two live probes on 2026-10-18 both returned 504 after
120s; the journal shows one glm stall at 110s.

The fix shipped for the ordering (B10). The budget is still open. The cheapest
fix with the current code is a per-combo entry in `timeouts:` for `free-stack`
(110 + 29 = 139s, so 150s covers it). The per-combo entry changes only that
combo's deadline. A lower start cap would change every provider and could cut
slow first bytes, so it needs more samples first.

**D12 — Fixes that need no decision.** The router layer outwaits the model
deadline by `LAYER_GRACE` (5s), so the model-aware JSON answer is what the
client sees. Streamed trace and audit rows carry the request's latency. Warn
lines carry the error's source chain.

**D11 — `stream` defaults to false.** `CanonicalChat.stream` is `#[serde(default)]`.
An OpenAI client that omits `stream` wants a whole reply, and every non-OpenAI
renderer now decodes it that way.

**D9 — Failover status is unchanged.** The last provider's status passes
through, and existing contract tests pin it. Changing it needs a decision on
what a model-not-found on the last target should look like to a client.

## Not executed, and why

- **T1** (12h soak): needs wall-clock time and an unattended run.
- **T4, T6, T7** (catalog pruning, guardrail deny-list, `modelRoles`): edit
  the live `~/.config/ar/config.yaml` that the running harness uses. Operator
  decisions, listed in the final report.
- **T8**: the register leans to closing it with rationale. Left for the owner.

## Verification (2026-10-18)

- `cargo test --workspace --no-fail-fast`: 1999 passed, 0 failed.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: clean.
- Unit tests were added for every decision. The attempt-loop tests cover model
  scope, the breaker still counting across models, the empty-completion
  classifier, and the one-failover cap. The relay tests cover the no-`[DONE]`
  completion, a `[DONE]` split across chunks, a split `finish_reason`, escaped
  answer text, and a real cut. Four existing tests that pinned provider-wide
  cooldown after a transient failure were moved to the model scope.
- Mock upstream, one behaviour per model, through a debug build of this tree:

| Case | Before | After |
|---|---|---|
| clean stream with `[DONE]` | streams | streams |
| complete answer, `finish_reason: stop`, no `[DONE]` | `stream_error` after the answer | no error |
| `[DONE]` split across two writes | `stream_error` after `[DONE]` | ends at `[DONE]` |
| real cut mid-answer | `stream_error` | `stream_error`, logged with provider, model and relayed count |
| first byte at 15s | headers at 10s, first byte at 15s | unchanged (D8) |
| empty 2xx stream, then healthy sibling | 502, `attempts 1` | 200 from sibling, `attempts 2` |
| stall, then healthy sibling | 502 at 110s, `attempts 1` | 200 from sibling at 110s, `attempts 2` |
| stall, then healthy sibling, two-key provider | 502 at 110s, `attempts 1` | 200 from sibling at 112s, `attempts 2` |
| two stalls in one request | 502 at 110s, `attempts 1` | empty 504 at 120s (B10, D10 open) |
| non-streamed empty answer, then healthy sibling | relayed empty | 200 from sibling, `attempts 2`, JSON |
| non-streamed empty answer, single target | relayed empty | relayed empty, unchanged (D5) |
| live `gemini-2.5-flash`, non-streamed, no `stream` field | 502, "missing field `stream`" | 200, OpenAI completion, served model, usage |
| live `gemini-2.5-flash`, streamed | false `stream_error` after the answer | ends at the final frame, no error |
| live `cline` API-key row, streamed | plain SSE, 44 frames (probed before the change) | not re-probed after the change |

- Not run: a 12-hour soak (T1), and any change to the live gateways on :20128
  and :21199. Both are still running their old builds. Restarting them onto this
  tree is an operator step.
- Repo hygiene: the rustfmt drift in `ar-cli` and in the pre-existing hunks of
  `attempt.rs`, `routes.rs` and `exec.rs` was left alone, so the diff carries
  only this work.
