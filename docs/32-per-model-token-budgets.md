# 32 — Per-model token budgets: a ceiling that names the model it stopped

Status: **built, 2026-10-20.** Owner: this doc, for the *shape* of the feature.
Companions: [`docs/28-gap-ledger.md`](28-gap-ledger.md) (the state of every open
item), [`docs/15-policy-security-layer.md`](15-policy-security-layer.md) (the
policy layer's decision record),
[`docs/33-vps-limits-config.md`](33-vps-limits-config.md) (the ceilings the VPS
actually runs, and the measurements they came from), `TODO.md` (the register).

---

## 1. What the operator saw

```
Error: Retry budget exhausted after 10 retries: 402 this key has spent 5002339805 tokens, which
reaches its 5000000000-token ceiling
(type=billing_error param=payment_required)
```

On the VPS, `racknerd-harness` had this in `~/.config/ar/config.yaml`:

```yaml
limits:
  keys:
    racknerd-harness:
      tokens: 5000000000
```

Every harness on the box (omp, opencode, jcode) talks to the same gateway with
the same client key, so the key's *cumulative* token spend is the sum of
everything every harness has ever sent.

## 2. The root cause (measured, 2026-10-20)

**This 402 is aroute's own, not a provider's.** Three independent proofs:

1. The message text is `ar-tokens`'s own [`DenyReason::TokenCap`] prose
   (`crates/ar-tokens/src/ledger.rs:269`), verbatim.
2. The `failures` table on the VPS records it with `provider = '-'` — the
   sentinel for "no provider was tried" (`crates/ar-server/src/routes.rs:535`).
   A provider's own 402 would carry the provider's name.
3. The `code` is `payment_required` and the `reason` is `token_cap` — aroute's
   own envelope vocabulary (`crates/ar-server/src/routes.rs:3457`).

**The defect: the ceiling counts the wrong thing.**

`Limit.model` scopes *when* a row applies. `Limit.tokens` counts *what* the row
measures. Today both are **key-wide**: `Ledger::admit_with` compares the
ceiling against `spend(key_id)` — the sum over **every model** the key has ever
served (`crates/ar-tokens/src/ledger.rs:678`). So a row scoped to one model
still spends against the whole key's history.

That is the whole bug, and it is the shape the operator felt:

| what the operator wants | what the engine does today |
|---|---|
| "this key may spend 5 B tokens **on `minimax-m3.1-flash`**" | "this key may spend 5 B tokens **on everything, ever**" |
| a runaway model gets its own ceiling | the runaway model eats the *whole* key's ceiling, and the other models are collateral damage |
| the other models keep working | the other models are refused too — `large-stack`, `small-stack` and `glm-5.3` all 402 at once |

**The measured burn on the VPS** (the whole `racknerd-harness` key, all time):

| serving target | requests | tokens | share |
|---|---:|---:|---:|
| `kilocode/minimax/minimax-m3.1-flash:free` | 17 798 | 4 841 787 875 | **96.8 %** |
| `nvidia/z-ai/glm-5.3` | 614 | 151 583 073 | 3.0 % |
| `kilocode/reflection/beam-free` | 91 | 6 550 870 | 0.1 % |
| `nvidia/stepfun-ai/step-5-preview` | 7 | 2 025 579 | 0.04 % |

One model ate 96.8 % of the budget. The `max_tokens` distribution explains
why: 13 923 of those 17 798 requests carried **100 k–500 k** prompt tokens
(average 268 778), and 1 295 carried more than 500 k. A 1 M-token context
window lets a single request carry a million tokens; at 5 B tokens of ceiling
that is only 5 000 such requests.

**The refusal is therefore correct arithmetic on the wrong axis.** The engine
did its job; the *policy* was wrong.

**The second, compounding defect: a per-model ceiling cannot fail over.**

Even once the count is per-model, a refusal is currently terminal. The 402 arm
runs **before** the attempt loop (`crates/ar-server/src/routes.rs:755`), against
`plan.chain.first()` — one target, the chain's *winner*. So a chain whose
winner has exhausted its own budget is refused outright, even when a fallback
in the same chain has budget left. The operator's `large-stack` lists four
targets; one of them burning its budget should hand the request to the next
one, not end it.

**The third defect: the ceiling is not reset, so recovery means editing a
number by hand.** Every arm is cumulative against an append-only ledger
(`docs/15` Phase 1). A key that has spent 5 B tokens is refused *forever* until
someone edits `config.yaml` — and the ledger is append-only, so the spend can
never be "un-spent". There is no way to express "5 B tokens per day".

## 3. The shape of the fix

Three changes, each in the layer that owns it.

### 3.1 A per-target counter the ledger already has

The ledger records `provider` and `model` on every usage row — the **serving
target**, which is exactly the axis a model-specific budget is about (and the
axis `usage.combo` does *not* give: a combo is what the client asked for, and
two combos may share a target).

So the count is a `GROUP BY provider, model`, not new bookkeeping:

```sql
SELECT COALESCE(SUM(total_tokens), 0) FROM usage
 WHERE key_id = ?1 AND provider = ?2 AND model = ?3
```

**Cost.** Measured on the VPS's own 32 988-row ledger: the *existing* key-wide
`SUM` — which runs on **every request today** — costs 0.056 s cold and
~0.03 s warm, because `usage_by_key` is `(key_id, id)` and the sum has to
touch every row of the key. The grouped query with the new covering index
`(key_id, provider, model, total_tokens, created_at, cost_micros)` is
**0.017–0.018 s** warm, and its plan is `SEARCH usage USING COVERING INDEX
usage_by_target (key_id=?)`.

That is *faster* than the query it replaces, because the index carries every
column the aggregate needs and the grouping never visits the table. The new
index is therefore a net win on the request path, not a cost — and the
per-target arm comes with it for free, on the same pass.

### 3.2 The config: a per-target budget inside one row

A new optional arm on `ar_config::Limit`:

```yaml
limits:
  keys:
    racknerd-harness:
      tokens: 5000000000          # the key-wide ceiling, unchanged
      model_tokens:               # per-target ceilings
        "kilocode/minimax/minimax-m3.1-flash:free": 1000000000
        "nvidia/z-ai/glm-5.3": 500000000
      spend_window: 24h           # both arms measured over a rolling window
```

`model_tokens` is a `BTreeMap<String, u64>`, keyed by `provider/model` — the
same spelling the combo targets use, so the operator reads one vocabulary
everywhere. A target named here has its **own** ceiling, checked against the
ledger's spend **for that target only**.

**Semantics, stated so nothing is ambiguous:**

- **A target with a `model_tokens` entry is governed by that entry.** Its share
  of the key-wide `tokens` ceiling does not apply to it — the more specific
  rule replaces the general one, exactly as a scoped row replaces the default
  row today.
- **A target with no entry falls under the key-wide `tokens` ceiling**, counted
  as it is today (the whole key's spend). This preserves every existing
  config's behaviour exactly: a config with no `model_tokens` behaves as it
  did.
- **The key-wide ceiling still binds the key as a whole.** A key whose total
  spend passes `tokens` is refused for every target. The per-target arm is an
  *additional* constraint, not a replacement for the total.
- **A zero is refused at parse**, like every other arm: `0` reads as "never" to
  an operator and "unlimited" to the engine.
- **An unknown target is refused at load** — a `model_tokens` key naming a
  target no combo routes to is a typo that would arm a ceiling that never
  applies. The check is against the **resolved** target (the provider and the
  provider-local model the chain will actually ask for), so it lives in
  `ar-server`'s load rather than in `ar-config`'s parse: only the registry
  knows which model a bare provider name means. `aroute limits set` applies
  the same check at write time, so a typo never reaches the file.

### 3.3 The refusal is per-target, and the chain fails over

This is the part that turns "the model stopped" into "the request continued".

The 402 arm currently runs once, against `plan.chain.first()`. It must instead
run against **each target in the chain**, and:

- a target with budget left **stays in the chain**;
- a target over its own budget is **dropped from the chain** before the
  attempt loop starts, so the loop never dispatches to it;
- if **every** target is over budget, the request is refused with a 402 that
  names the models that refused it.

That turns the operator's failure into a failover:

```
large-stack  = [kilocode/minimax…, nvidia/stepfun-ai/step-5-preview, nvidia/z-ai/glm-5.3, nvidia/z-ai/glm-5.3-flash]
```

With `minimax-m3.1-flash` over its own 1 B-token ceiling, the request goes to
`step-5-preview` instead of dying. The `glm-5.3` target keeps its own 500 M
ceiling and is dropped only when it reaches it.

**The 402 the client sees** names the arm and the model, never the amounts:

```
402 Payment Required
x-ar-deny-reason: model_token_cap
{"error":{"type":"billing_error","code":"payment_required",
          "reason":"model_token_cap",
          "message":"kilocode/minimax/minimax-m3.1-flash:free has reached its token ceiling"}}
```

### 3.4 The refusal has to be *recoverable*

A cumulative ceiling with an append-only ledger is a one-way door: once the
key is over, it is over forever, and the only remedy is editing the config by
hand. The operator's incident is that door — their key is already past 5 B
tokens, so without this lever it stays dead.

The lever: **a reset window on the row.**

```yaml
      spend_window: 24h        # both spend arms reset on a rolling window
```

The window is **rolling** (the last 24 h of recorded spend), not calendar:
a rolling window is the shape that actually bounds a burn rate, and it is the
shape the dashboard's own `max_requests_per_minute` sibling uses. The ledger
already stores `created_at` on every row, so the rolling sum is one more
predicate on the same query — no new table, no new bookkeeping.

**It covers BOTH spend arms** — the key-wide `tokens` and the per-target
`model_tokens`. A window on only one of them is a trap: the other stays
cumulative against an append-only ledger, so it is blown forever and the
window never rescues the key. One window per key, read from the key's own row
(the `keys:` entry, else `default`), so the two arms can never disagree about
which spend is in force.

The window is **optional and off by default**. A config with no `spend_window`
keeps the cumulative semantics, so this change is additive and every existing
config is unaffected.

**The reset is not a scheduler.** The ceiling is evaluated per request against
the rolling window; nothing fires on a timer. A request at 23:59 sees the last
24 h; a request at 00:01 sees a window that has dropped the oldest hour. That
is deliberate — a reset job would need to run while the proxy is down, and a
budget that resets only when the proxy happens to be up is not a budget.

### 3.5 What is deliberately NOT in this change

- **Not a per-*combo* budget.** `usage.combo` records what the client asked
  for. A combo is not a spend class — `large-stack` and `claude-opus-5-5` route
  to the *same* four targets on the VPS, so a combo budget would be a second
  name for the same money.
- **Not a USD arm per target.** The VPS's providers are flat-rate (every
  `cost_micros` is 0), so a per-target dollar ceiling would be an arm that can
  never bite here. The token arm is the one that binds.
- **Not a "reset the spend" button.** The ledger is append-only by design
  (`docs/15`: the SQLite file *is* the audit trail). A rolling window solves
  the operator's need without weakening the audit property. Editing history to
  un-block a key would trade an audit trail for convenience.
- **Not a hard-coded provider list.** The target vocabulary comes from the
  config's own combos.

## 4. The surfaces

| surface | change |
|---|---|
| `ar-config` | `Limit.model_tokens: BTreeMap<String, u64>` + `Limit.spend_window: Option<String>`; parse-time validation (a zero ceiling and an unreadable window are refused) |
| `ar-tokens` | `Ledger::spend_breakdown(key_id, since)` — the key's total and each target's share in ONE grouped query; a covering index `usage_by_target` created on open; `Ledger::admit_against` so the key-wide comparison is made against a spend the caller already holds |
| `ar-server` | the 402 arm runs per target; over-budget targets are dropped from the chain; the refusal names the model; the load refuses a `model_tokens` entry naming a target nothing routes to |
| `ar-cli` | `aroute limits set --model-tokens provider/model=N --spend-window 24h`; the `aroute limits` view prints the per-target arms; the `/ui` keys pane edits them |
| `ar-route` | unchanged — the router must not know about budgets |

**One note on the shape of the change.** `DenyReason` stayed `Copy` and gained
no variant: a per-target refusal names a *model*, and a `String` in a `Copy`
enum is a cost across four crates. The refusal is therefore the policy layer's
own `model_token_cap` verdict rather than a ledger variant — the ledger keeps
the arithmetic, the policy layer keeps the naming, and the client's envelope
carries `reason: model_token_cap` either way.

**One read, not two.** The key-wide arm and the per-target arm both need the
key's spend, and the per-target arm needs it **split by target**. One grouped
query returns both, so the request path costs the same single pass the
key-wide `SUM` already cost — and the new covering index makes that pass
*cheaper* than before (measured below).

## 5. The config on the VPS, after this lands

```yaml
limits:
  keys:
    racknerd-harness:
      tokens: 5000000000
      model_tokens:
        "kilocode/minimax/minimax-m3.1-flash:free": 1000000000
        "nvidia/z-ai/glm-5.3": 500000000
        "nvidia/stepfun-ai/step-5-preview": 100000000
        "kilocode/reflection/beam-free": 100000000
      spend_window: 24h
```

Read: the key may spend 5 B tokens in total, but no single target may exceed
its own share inside any rolling 24-hour window. `minimax-m3.1-flash` — which
ate 96.8 % of the budget — is capped at 1 B per day, and the harness's real
work (GLM, step, beam-free) keeps 4 B.

## 6. The open decision

**Is the per-target ceiling the right axis, or should it be per-combo?**

The engine already knows both (`usage.combo` and `usage.provider`/`usage.model`).
The evidence for **per-target** is that the spend is a property of the *model
that served*, not of the name the client used: on the VPS, `large-stack`,
`claude-opus-5-5`, `claude-sonnet-5-5` and `claude-haiku-5-5` route to
overlapping target sets, so a combo budget would be charged several times over
for the same tokens — the same double-count the `combo` column was added to
fix in the other direction.

The operator owns the ruling if they want per-combo budgets as well; the
design above does not foreclose them (the ledger already carries the column).

## 7. The test plan

Each row is a behaviour a consumer can observe, not an implementation detail.

| # | behaviour | where |
|---|---|---|
| T1 | a key under every ceiling reaches the provider (no regression) | `ar-server` — the existing limits tests |
| T2 | a target over its own `model_tokens` ceiling is **dropped from the chain**, and the request is served by the next target | `a_target_over_its_own_budget_hands_the_request_to_a_sibling` |
| T3 | when every target in the chain is over budget, the request is 402, and the body names the model | `a_chain_with_no_budget_left_is_refused_and_names_the_models` |
| T4 | a target with no `model_tokens` entry still counts against the key-wide `tokens` ceiling (the old behaviour) | `a_target_with_no_budget_of_its_own_still_counts_against_the_key` |
| T5 | a key over its key-wide ceiling is refused for every target, including one with its own entry | `a_key_over_its_own_ceiling_is_refused_even_for_a_target_with_its_own` |
| T6 | the rolling window excludes spend older than the window, and counts spend inside it | `a_window_keeps_spend_outside_it_from_counting`, `a_window_counts_spend_inside_it` |
| T7 | a `model_tokens` entry naming no routed target is refused | `ar-cli` — `limits_set_refuses_a_per_target_ceiling_that_names_no_target`; `ar-server` — the load check |
| T8 | a zero `model_tokens` value is refused | `ar-config` — `a_zero_per_target_ceiling_is_refused_at_parse`; `ar-cli` — the malformed-input test |
| T9 | `aroute limits set --model-tokens` merges without dropping the other arms, and round-trips through the config parser | `limits_set_writes_a_per_target_ceiling_and_a_window`; `ar-cli/src/import/limits.rs` — the render round-trip |
| T10 | the ledger's per-target split sums to the key's whole spend | `ar-tokens` — `the_breakdown_splits_a_keys_spend_across_its_targets` |
| T11 | the panel writes and keeps the new arms, and refuses a bad one | `ar-cli/src/webui.rs` — the two HTTP tests |

### The shape of the test that matters most

T2 is the one that would have caught the original defect. It is not "the 402
names the right thing" — it is **"the request is served, by the other model"**.
A test that asserted only the status code would pass against the old code
too, because the old code also refused. The test has to observe *which
provider was dispatched to*, which is why the fixture's executor records what
it was asked for.
