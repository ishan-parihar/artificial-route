# Remaining blockers: plan

Written 2026-10-05 against the ported `free-stack` / `small-stack` on `origin/main`
(`daddb0b`). Every number below was measured on this machine, not inherited from
a doc.

## What is measured

| measurement | result |
|---|---|
| combo targets answering | 16 of 17 |
| free-stack under load | 144 req / 3 min, 12 distinct models, RSS +2.2 MB |
| small-stack correct answers | 18 / 24 (75%) |
| load failures: guardrail answer | 4 / 24 |
| load failures: empty content | 2 / 24 |
| load failures: degenerate output | 1 / 24 |
| `opencode run --model hermes-router/small-stack` | answers end to end |

## B1 — a guardrail model answers ~17% of small-stack requests

`nvidia/nemotron-3.5-content-safety:free` replies `"User Safety: safe"` to
everything. It is a real entry in the KiloCode catalog, described there as
*"a compact 4B-parameter multimodal guardrail model … moderates both inputs to
and responses from LLMs"* — a moderation model exposed as a chat model, and
answerable: asked for directly it returns 200 with that text.

**Retraction.** An earlier draft of this file claimed Cline and KiloCode pick the
served model themselves. Measured, neither does: asked for a valid id each echoes
the request exactly — KiloCode 8/8, Cline 6/6, OpenRouter 10/10 — and a fabricated
id is rejected rather than silently substituted.

### Isolation (run 2026-10-05)

| condition | result |
|---|---|
| small-stack, concurrency 1, 12 requests | **12/12 correct**, zero guardrail |
| small-stack, concurrency 4, 12 requests | 10/12 correct, **2 guardrail** |
| small-stack, concurrency 4, 56 requests | 4 guardrail + 3 empty, **all `provider=kilocode`, `attempts=1`** |
| KiloCode direct, concurrency 4, 72 requests | **72/72 clean** |

So the substitution is **load-correlated and needs `ar` in the loop**. KiloCode
under the same concurrency, called directly, never substitutes — which is why the
isolation in this section only reproduces through the proxy.

Every occurrence is `outcome=ok; attempts=1`: the chain's first dispatchable
target is the one that answers wrongly, so nothing downstream ever sees it. That
is the shape B2 fixes, and it is why B2 is ranked first — it turns this failure
mode into failover rather than requiring the router to behave.

One occurrence is informative on its own: asked for `stealth/space-bunny-alpha`,
the upstream returned `poolside/laguna-s-2.1:free` with empty content. The
substitution swaps models, not just answers.

### Can the choice be pinned? No

Every OpenRouter-style routing field was tried at the concurrency where
substitution occurs, 24 requests each, all three clean:

| variant | result |
|---|---|
| plain | 24/24 OK |
| `provider: {sort: "throughput"}` | 24/24 OK |
| `provider: {allow_fallbacks: false}` | 24/24 OK |
| `models: [...]`, `route: "fallback"` | 24/24 OK at concurrency 1 |

None changes behaviour, and none is distinguishable from the baseline because the
baseline is already clean at that concurrency. **No request-side pin was found**,
so option (b) below — stop asking for the alias — is the only config-level lever.

### Fix options

1. **Filter** (preferred, and it is B2): treat a guardrail string as a failed
   attempt so it fails over. Local, honest, and it also catches the empty case.
2. **Pin by name**: drop `*/stealth/space-bunny-alpha` targets and name concrete
   models (`kilocode/poolside/laguna-s-2.1:free` was clean throughout). Loses the
   alias's capacity, so it is second.
3. **Drop kilocode** from small-stack. Cline served 6/6 at concurrency 1 and never
   appeared as a failure source, so this is a viable simplification — at the cost
   of half the free-tier capacity.

## B2 — kimi-k3 returns empty content about half the time

Measured 10/10 for `z-ai/glm-5.3` against **5/10** for `moonshotai/kimi-k3`
(3 empty, 2 degenerate, e.g. `'<|close|>!'`). Note NVIDIA serves `z-ai/glm-5.3`
when asked for `moonshotai/kimi-k3`, so free-stack's two targets largely resolve
to the same upstream model.

**This one is fixable in `ar` and is the highest-value item here.** An empty or
degenerate completion is a failed attempt wearing a 200, and `ar` currently
treats it as success — so `least-used` keeps selecting it and half of free-stack's
answers reach an agent blank. The load test showed the same class on other
targets.

### Steps

1. **Classify, do not retry blindly.** On a non-streaming 2xx, mark the attempt
   failed when the assistant message has no `content` **and** no reasoning that
   carries an answer, or when the content is a known degenerate repeat. Keep the
   existing in-band truncation frame untouched — that is a different signal.
2. **Fail over.** Reuse the existing attempt loop; a classified-empty attempt
   should consume one attempt slot and let the next target serve. That turns
   B2 into `kimi-k3` being deprioritised by measurement instead of by pruning.
3. **Bound it.** Cap at one classified-empty retry per request so a genuinely
   empty-but-valid answer (a refusal, a tool call with no text) cannot loop.
4. **Test.** A target that returns `{"content": null}` is failed over; a target
   returning `""` with reasoning is *not*; a genuinely complete short answer is
   not retried; the retry is bounded.

## B3 — `stealth/space-bunny-alpha` is labelled "retires Oct 5"

The KiloCode catalog describes it as *"Space Bunny Alpha (retires Oct 5)"* —
today. 12 of small-stack's 15 targets and both free-stack pool entries resolve
through it. Whatever replaces it is unknown, and it is the single largest
dependency in the ported config.

### Steps

1. Watch for the successor id in the catalog on the next discovery tick
   (60s, already running) rather than discovering it during an outage.
2. Add a standing check that every configured target still appears in its
   provider's live `/models`, reported by `ar doctor`. The per-target probe I ran
   by hand is exactly this and is not automated today.
3. When the successor appears, add it as a target and keep the old one in `pool:`
   until it starts failing, so the swap is a config change rather than an
   incident.

## B4 — gemini's response is not translated to the OpenAI envelope

Gemini now authenticates and returns a correct upstream answer, but framed as
`{"candidates":[{"content":{"parts":[{"text":"64"}]}}]}` rather than
`{"choices":[...]}`. An OpenAI client sees no choices. This is the translation
layer, not routing, and it is the one defect I fixed only halfway.

### Steps

1. Translate the Gemini non-streaming response into the canonical envelope when
   the inbound dialect is OpenAI. `ar-translate` already holds the
   inbound-facing envelopes for Anthropic and Responses; this is the third.
2. Leave the *streaming* path relaying upstream bytes unchanged — the module
   documents that pass-through as a deliberate contract.
3. Test: a Gemini 2xx body yields `choices[0].message.content` for an OpenAI
   client, and an upstream error still surfaces as an error rather than an empty
   choice.

## Not blockers, recorded so they are not re-litigated

- **The breaker tripping during testing.** Repeated probing pushed providers
  into escalating cooldown and produced `503 every provider in the chain is
  cooling down`. That is ar working. It is also a real operational note: a burst
  of failures escalates the cooldown, so a genuinely dead provider costs a
  long recovery. Worth watching under real load.
- **`~/bin/ar` shadowing `/usr/bin/ar`**, which breaks every Rust build on the box
  until `AR=/usr/bin/ar` is exported. Separate from these blockers.
- **Discovery's memory cost**: 34 MiB config-only → 120 MiB with 9,715
  discovered models. Documented in the unit, not a defect.