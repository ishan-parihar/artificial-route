# 42 — Mid-body idle cuts on reasoning models (2026-10-22)

Status: root-caused and fixed 2026-10-22. Owner of the decision is this
file; `28-gap-ledger.md` §D points here.

Trigger: the primary gateway (racknerd, `aroute.service`, 0.1.20) logged
`upstream stream ended early error=Transport("upstream body idle for 120s")`
109 times in three days (25 Oct 20, 30 Oct 21, 54 Oct 22 — rising), and every
cut was followed by the in-band
`the upstream stream ended before it was complete` frame clients saw. First
occurrence 2026-10-20 17:21 IST, minutes after the 0.1.20 binary replaced
0.1.17 (the idle ceiling itself predates that; see "Why now" below).

## Evidence (journal, 7-day window)

- 109 `upstream body idle for 120s` warns; **no other stream-error kind**.
- Serving target at the cut: `nvidia/z-ai/glm-5.3` (60),
  `nvidia/stepfun-ai/step-5-preview` (43), `nemotron-3.5-super-vl` (1).
- All three are reasoning models on the NVIDIA NIM free tier. Their thinking
  phase sends no SSE frames: glm-5.3 paused for whole minutes mid-body before
  its answer resumed, then the 120s ceiling fired.
- `relayed=` spans 0 to 6643 — cuts happened at any point, including deep into
  long answers (6643 frames relayed, then silence > 120s).
- Not concurrency-bound: cuts are spread across single-digit request hours and
  peak hours alike; the per-hour histogram tracks *thinking* traffic, not
  in-flight count. The usage DB shows ~5.6k requests/day on these models.
- Client-visible counterpart: 96–116 `upstream stream cut after its headers`
  rows in `failures` over the same window (some cuts are the *same* event seen
  through the non-stream drain path).

## Root cause

`IDLE_BODY_TIMEOUT = 120s` (`ar-exec/src/lib.rs`, in `into_bytes` and
`into_sse`) is a ceiling on the silence between two body chunks. It was meant
to guard a *stalled connection* — but a reasoning model that pauses mid-body
while it thinks is silent in exactly the way the guard cannot distinguish from
a dead socket. Both cuts and thinking pauses grew together once glm-5.3 and
step-5-preview became the serving heads of `large-stack`.

docs/22 D10 deliberately raised the *per-combo stream deadline*
(`AR_STREAM_TIMEOUT_SECS=large-stack=150` on the VPS) so slow heads could
finish. That deadline bounds the whole request; the idle ceiling bounds the
gap between chunks — and the idle ceiling was the one that fired. D10's fix
could not help because the cut happened inside the body, not at the start.

## The fix

**AR_IDLE_BODY_TIMEOUT_SECS** (new): one global number, in seconds, read once
at `ArExec` construction and threaded through `ChatStream` into
`into_bytes`/`into_sse`. Default unchanged (120s) so every deployment behaves
as before until the operator says otherwise. A non-positive or non-numeric
value is dropped with a stderr warning, the same policy
`with_timeouts_from_env` applies to a bad deadline.

Why one number and not the per-model table `AR_STREAM_TIMEOUT_SECS` carries:
the idle ceiling guards a *connection*, and the socket a stalled request sits
on belongs to whichever target the router picked — decided after failover, so
a per-model entry could not name it in advance. A model that needs longer
mid-body pauses needs the raised global; that is what an operator means when
they say "these reasoning models think for minutes".

### VPS deployment (2026-10-22)

`~/.config/ar/ar.env` on racknerd now carries (with `ar.env.bak-20261022-idle`
preserved first):

```
AR_STREAM_TIMEOUT_SECS=large-stack=600
AR_IDLE_BODY_TIMEOUT_SECS=300
```

then `systemctl --user restart aroute.service` (no hot reload — the env is
read at construction). 300s is the idle ceiling; it only works paired with a
raised request deadline, which is the next section's arithmetic.

## The deadline interplay (the operator's arithmetic)

Three clocks bound one streamed attempt:

1. **Stream start** (`STREAM_START_TIMEOUT_CAP`, 110s): headers must arrive.
2. **Whole-request deadline** (`AR_STREAM_TIMEOUT_SECS`, 150s for
   large-stack on the VPS, 120s elsewhere): the router-level cut.
3. **Mid-body idle** (`AR_IDLE_BODY_TIMEOUT_SECS`, was a fixed 120s): the gap
   between two chunks.

Clock 2 ≥ clock 3 is the invariant a long-thinking stream needs: a pause only
survives if the *whole request* is allowed to outlast it. On the VPS,
`AR_STREAM_TIMEOUT_SECS=large-stack=150` still cuts a 3-minute think at 150s
— raising the idle ceiling alone cannot save it.

**Ruling: the VPS sets both.** `AR_STREAM_TIMEOUT_SECS=large-stack=600`
(10 minutes: a 5-minute think plus a 2–3x answer margin) alongside
`AR_IDLE_BODY_TIMEOUT_SECS=300`. The idle ceiling is now the *tighter* of the
two, which is the correct order: the request deadline is the outer bound the
client contracted for, the idle ceiling is the inner guard that turns a dead
socket into a named failure sooner than the outer bound would.

Local machine: unchanged (default 120s both clocks). Its traffic is
interactive coding, not long thinks, and it keeps the old behaviour until it
needs the new one.

## Why now (and not at 0.1.17)

The idle ceiling has been 120s since the crate existed. What changed is the
*traffic*: `large-stack` was rebuilt around nvidia reasoning models
(router-ops, 2026-10-19) and they became the serving heads. The cut rate
tracks that shift — the first cut lands 2026-10-20, the day after the lane
change settled in, and rises with thinking-heavy usage.

## Verification

- `cargo test -p ar-exec --lib`: 223 passed (3 new tests for the env parse:
  set, garbage, empty/unset).
- `cargo test --workspace --no-fail-fast`: **2317 passed, 0 failed**.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: clean
  (two pre-existing collapsible-`if`s in `dashboard.rs` from the docs/41
  stream collapsed; no behaviour change).
- **VPS deploy, measured 2026-10-22 22:57–23:06 IST**: static musl build of
  the tree at `485cf8c` installed over `aroute.bak-20261022-pre-idle`;
  `/proc/<pid>/environ` shows `AR_IDLE_BODY_TIMEOUT_SECS=300` and
  `AR_STREAM_TIMEOUT_SECS=large-stack=600` in the serving process.
  Streamed probes through the afflicted lane and its head models all ended
  at `[DONE]` with zero truncation frames: `large-stack` (served by
  `stepfun-ai/step-5-preview`), `glm-5.3` direct, `small-stack`, and a
  deliberate reasoning probe on `glm-5.3` (TTFB 34.1s, total 52.0s,
  `finish_reason: stop`). Journal from the restart to +8 min: **0**
  `body idle` / `ended early` warns (was ~13/hour before). The watchdog
  reports `unit=active loopback=200 public=200`.

## Not done, and why

- No per-model idle table (argued above: the socket's owner is decided after
  failover, so a table could not be consulted).
- No attempt to distinguish "thinking silence" from "dead socket" at the
  protocol level: SSE has no in-band heartbeat, and inventing a keepalive for
  the *upstream* leg would need provider cooperation. The keepalive the relay
  sends to the *client* (docs/22) already covers the client-side symptom.
