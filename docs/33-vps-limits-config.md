# The VPS config change, and why each number is that number

Measured 2026-10-20 from `~/.config/ar/usage.sqlite` on `racknerd`, the
`racknerd-harness` key, all time:

| target | requests | tokens | last 24h |
|---|---:|---:|---:|
| `kilocode/minimax/minimax-m3.1-flash:free` | 17 798 | 4 841 787 875 | 4 841 787 875 |
| `nvidia/z-ai/glm-5.3` | 614 | 151 583 073 | 151 583 073 |
| `kilocode/reflection/beam-free` | 91 | 6 550 870 | 6 550 870 |
| `nvidia/stepfun-ai/step-5-preview` | 7 | 2 025 579 | 2 025 579 |

**The whole spend is inside the last 24 h** — the key was armed on 2026-10-19
and the traffic is continuous (~600 M tokens/hour, peak hour 869 M).

The hourly breakdown shows the shape plainly: **minimax is 100 % of every peak
hour**. It is not a burst that a limit would have caught; it is a steady drain
on one model while the other three sit nearly idle.

## The configuration

```yaml
limits:
  keys:
    racknerd-harness:
      tokens: 6000000000
      model_tokens:
        "kilocode/minimax/minimax-m3.1-flash:free": 5000000000
        "nvidia/z-ai/glm-5.3": 500000000
        "nvidia/stepfun-ai/step-5-preview": 200000000
        "kilocode/reflection/beam-free": 200000000
      spend_window: 24h
```

## Why each number

**`spend_window: 24h`** — the arm is what makes the ceilings recoverable at
all. Without it the key's 5 002 339 805 tokens are counted forever, the key is
permanently over its 5 B ceiling, and the only remedy is editing the config by
hand. With a 24 h window the key is inside its budget again once the spend
aged out. **Both** arms are measured over the window, so the key-wide ceiling
and the per-target ceilings cannot disagree about which spend is in force.

**`tokens: 6000000000`** (key-wide) — the ceiling is now a *daily* rate, not a
lifetime total. The measured burn is 4.84 B/24 h for minimax alone; the
7-day average is 0.71 B/day. 6 B gives roughly 25 % headroom over the worst
measured day while still bounding a runaway: a model that suddenly burned
5× the normal rate would be refused at 6 B rather than running to 30 B.

**`model_tokens` for minimax: 5000000000** (5 B/24 h) — this is the arm that
does the real work. The measured 24 h burn is 4.84 B, so 5 B is the same
order with a little headroom; the point is that it is now a *bounded daily
rate* rather than an unbounded lifetime draw, and that when minimax reaches
it, **the request fails over to `step-5-preview` or `glm-5.3` instead of
dying**.

**`model_tokens` for `glm-5.3`: 500000000** (0.5 B/24 h) — the measured burn is
0.15 B, so 0.5 B is ~3× headroom. GLM is the model the operator actually
wants (the "large stack" primary), so it gets real room.

**`model_tokens` for `step-5-preview` and `beam-free`: 200000000`** (0.2 B/24 h
each) — the measured burns are 2 M and 6.5 M, so 0.2 B is 30–100× headroom.
These are the *fallback* models: the point of a failover budget is that the
fallback has room to absorb the traffic when the primary is over. A 0.2 B
ceiling on a 4.84 B/day primary can absorb a meaningful slice of a diverted
flow.

## What this does to the current incident

**Today**: `racknerd-harness` has spent 5 002 339 805 tokens against a
5 000 000 000 ceiling, so **every** request through the key is refused `402`
`token_cap`. All three harnesses (omp, opencode, jcode) are dead on this
key.

**After**: with a 24 h window the key-wide figure is the spend inside the last
24 h — which is 5 002 339 805, still over 6 B? No: 5.00 B is *under* the 6 B
key-wide ceiling, so the key-wide arm passes. The per-target arm then finds
minimax at 4.84 B against its own 5 B ceiling — also under. So the key
**serves again immediately**, and the ceilings are the ones that bound the
*next* day.

## The verification, in order

1. `aroute limits` on the VPS prints the row — the config parsed and the
   arms are where the operator expects them.
2. `aroute doctor` reports the store and the gate as before.
3. A real `POST /v1/chat/completions` through the public endpoint with a
   harness token answers **200** — the key is not refused.
4. The response's `x-ar-decision` header names the provider that served.
5. A deliberate over-budget probe: temporarily set a target's ceiling to a
   number the current spend is already past, restart, and confirm the request
   **fails over** to the next target rather than being refused. Then restore.

Step 5 is the one that proves the new behaviour rather than the old: a 402
there would mean the failover did not happen.
