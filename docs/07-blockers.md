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
and responses from LLMs"*. So it is a moderation model exposed as a chat model,
and it is answerable — asking for it directly returns 200 with that text.

**My first explanation was wrong and is retracted.** I claimed Cline and
KiloCode pick the served model themselves. Measured, they do not: asked for a
valid id, each echoes the request exactly — KiloCode 8/8, Cline 6/6, OpenRouter
10/10. The guardrail answer appears only under sustained load, so whatever
substitutes it is load-dependent. **The mechanism is not yet established** and
B1 is therefore an isolation task, not a fix task.

### Steps

1. **Isolate.** Replay the load test with the served-model field captured per
   request *and* the requested target, so the substitution is visible at the
   moment it happens rather than reconstructed after. Add the ar decision header
   (`x-ar-decision` carries `provider=`) alongside the body's `model`, so a
   substitution can be attributed to a target instead of guessed at.
2. **Test the retry hypothesis first.** If a guardrail answer is
   load-correlated, a single request at low concurrency should not produce one.
   Run 10 requests at concurrency 1 and 10 at concurrency 4 against the same
   combo and compare rates. If concurrency-1 is clean, this is a rate/fallback
   path upstream and B1 is not a routing bug at all.
3. **Only then choose a fix**, in preference order:
   - a. Filter: treat an answer that is recognisably a guardrail string as a
     failure and fail over. Cheap, local, and honest — but it is a heuristic on
     output text and needs a narrow match.
   - b. Pin: stop asking for `*/stealth/space-bunny-alpha` on the providers that
     advertise it as a routing alias, and name a concrete model instead.
   - c. Drop the alias targets. Loses capacity, so it is last.

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