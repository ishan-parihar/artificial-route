# 20 — Per-model dispatch attribution (diagnosis gap)

Status: **implemented 2026-10-17, shipped 0.1.16.** Triggered by "which member of
`free-stack` is hallucinating?" — answerable only by probing upstreams by hand,
because no aroute surface recorded the model that actually served a request.

## What landed

- `AttemptOutcome::Succeeded` gained `model: Strng` + `serving_model()`
  (`ar-route/src/attempt.rs`); the fusion construction site stamps it too.
- `x-ar-decision` gained `model=<serving>` (`-` when nothing served), same
  placeholder convention as the `provider` field.
- The usage ledger and trace JSONL record the **serving target** instead of the
  client's combo spelling (`UsageTee.model`, ar-server's non-stream accounting
  path both read it off the outcome).
- The three attempt-loop `WARN`s (`attempt failed over`, `2xx stream produced
  no bytes`, transport) stamp `model`.
- Old rows stay combo-keyed: no schema migration, no backfill.
- `/metrics` still carries no model labels — deliberate cardinality cap.

Verification: `aroute` 0.1.16 live emits
`x-ar-decision: strategy=least-used;outcome=ok;provider=nvidia;model=stepfun-ai/step-5-preview;attempts=1`.

## Problem

Every persisted/emitted surface names the *combo*, not the *target that won*:

| Surface | Fields | Serving model? |
|---|---|---|
| WARN attempt logs (`ar-route/src/attempt.rs:715,754,776`) | `provider`, `status`, `cooldown_ms`, `error` | no — `model` is in scope, unstamped |
| `x-ar-decision` (`routes.rs:1998`) | `strategy`, `outcome`, `provider`, `attempts` | no |
| `x-ar-model` (`routes.rs:2024`) | client's spelling — deliberately (test at `:5079`) | no |
| usage ledger (`routes.rs:798` passes `&canonical.model`) | combo id in the `model` column | no |
| trace JSONL (`routes.rs:915-934`) | combo id in the `model` field | no |
| audit ledger | key id, two enums, one `&'static str` | no |
| `/metrics` | no model labels, by design (cardinality cap, `metrics.rs:11`) | no |
| `explain_route` (MCP `ar_explain_route`) | provider + model, **dry run only**, `auto/*` combos | predicted, not served |

`attempts=N` proves *that* a failover happened, never *which* target failed.
Operational consequence observed 2026-10-17: a multi-hour `provider=nvidia
status=429` storm was unattributable; the only way to name the dead member was
to replay the combo's targets directly against `integrate.api.nvidia.com`
(glm-5.3: 429 on every call, ~0.9s; glm-5.3-flash: TTFT 99–124s vs
`STREAM_START_TIMEOUT_CAP` 110s; step-5-preview: 1–11s TTFT).

The serving model *is* present in the upstream echo (response body `model`,
per-chunk SSE `model`), so attribution is possible in principle — aroute just
never captures it into its own surfaces.

## Fix (in scope)

1. `AttemptOutcome::Succeeded` carries `model: Strng` beside `provider`
   (`ar-route/src/attempt.rs:732-736` is the construction site).
2. `x-ar-decision` gains `model=<serving>` (`routes.rs:1998`). `x-ar-model`
   keeps the client's spelling — clients compare against their own request.
3. The three WARN failover sites stamp `model` (already in scope at
   `attempt.rs:622`; one field each).
4. Trace JSONL `model` field records the serving target instead of the combo
   name (usage-ledger payload and `/metrics` unchanged — the ledger's model
   column feeds pricing lookups, so a semantic change there is a separate,
   cost-reporting-visible decision).

## Out of scope

- Usage ledger `model` column semantics (combo → serving model) moves every
  priced cost report's grouping.
- `/metrics` model labels: cardinality cap is deliberate.
- Content capture for hallucination *quality* checks: redaction is a feature
  (`ar-obs/src/audit.rs:5-13`). Model *health* is observable; response quality
  is not this layer's job.
