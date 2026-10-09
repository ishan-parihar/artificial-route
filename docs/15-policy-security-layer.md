# 15 — Policy & security layer upgrade plan

> **Status (2026-10-10):** Phases 1 and 2 have landed — `limits:` parses in
> `ar-config`, `ar-limit` buckets the requests, and `handle_chat` enforces both
> arms (429 with `Retry-After` after the gate, 402 after the cache, spend via
> the ledger's own `Ledger::admit_with`). The plan text below is kept as the
> decision record; where the landed shape diverges, the module docs in
> `crates/ar-limit` and `crates/ar-server/src/policy.rs` are the truth. The
> one deliberate divergence: Phase 1 as written projected cost before
> `resolve()`, but a projection can only price against a resolved row, so the
> 402 runs after resolve — and after the cache lookup, which a hit skips
> because the ledger never records a hit as spend.
>
> **Phase 3 landed the same day:** `aroute keys arm-gate` generates the 32-byte
> master into the credential store's `http-gate` row and prints the first
> client token (30-day TTL — the library's 15-minute default is an
> interactive-login figure); `aroute keys mint` issues more; `aroute serve`
> reads the row at boot and arms the gate; `aroute doctor` carries the armed /
> not-armed / unreadable row. The README's standing unauthenticated-listener
> warning now names the arming command instead of the absence of one.

Import the *worthy* agentgateway mechanisms into aroute, and nothing else. Every
row below was verified against both source trees on 2026-10-09.

## Decision record — what "worthy" means here

Our threat model is **loopback-first, single-operator**. The exposed production
surface is not this proxy: `scripts/aroute-to-agentgateway.sh` ports the whole
aroute catalog to the VPS agent-gateway, whose config preserves `llm.policies`
verbatim — agentgateway's own policy layer already governs that side. What we
lack is policy on the *aroute* side: the loopback listener and the config the
sync script reads.

Import rule: **port only a mechanism our threat model needs that we do not
already own.** That ruled out CEL, JWT, RBAC, ext-proc, OIDC and remote
rate-limiting (also triaged DELETE in [01-copy-from-agentgateway.md](01-copy-from-agentgateway.md)).

## Verified inventory — the delta is smaller than it looked

| Capability | Status today (verified in-tree) | agentgateway source | Decision |
|---|---|---|---|
| Spend caps (USD + tokens, 402 deny, `refuse_unpriced`) | Machinery **exists**: `ar-tokens/ledger.rs` `Cap`, `admit`, `DenyReason::{UsdCap,TokenCap,Unpriced}` — but `admit` is **never called** from `ar-server`, so caps are recorded, not enforced | `http/localratelimit.rs` `RateLimitType::Tokens` | **Wire what exists** (Phase 1). No port. |
| Request-count limiting (RPM) | Does not exist | `localratelimit.rs` `RateLimitType::Requests`: token bucket, bounded per-key buckets, LRU eviction | **Port the mechanism** (Phase 2), ~120 lines, stdlib only |
| Per-key bucket selection | n/a — one key per config | CEL `key:` expression (`jwt.sub`, `jwt.team`) | **Skip** — no JWT, no multi-tenant surface |
| API-key auth gate | `ar-server` gate exists, driven by `Components.master_key`; **no shipped command arms it** (README WARNING) | `http/apikey.rs` (589 LOC) | **Arm our own** (Phase 3), ~20 lines. No port. |
| Prompt-injection + credential guard | Exists, needle families at OmniRoute parity, `warn` by default | their guardrails are 3rd-party moderation services (OpenAI/Bedrock/Model Armor) | Keep. Different domain, stated non-goal. |
| Per-key cooldowns/backoff | Exists (`ar-route/attempt.rs`, `resilience.rs`) | (their equivalent lives in the LLM backend) | Keep. Upstream-side, orthogonal. |

## Phase 1 — enforce the caps we already have

The ledger records spend but nothing refuses against it. Wiring enforcement:

1. `crates/ar-server/src/config.rs` / `app.rs`: the ledger is already plumbed
   (`AppState.ledger: Option<Arc<Mutex<ar_tokens::Ledger>>>`); add the per-key
   `Cap` lookup beside it (same source the ledger reads).
2. `crates/ar-server/src/routes.rs`: call `ledger.admit(key_id, projected_tokens)`
   on the chat/messages/responses paths **before** `resolve()`, mirroring where
   `guard_body` already runs. Map `DenyReason` to a 402 envelope through the
   existing `error_because` shape, with an `x-ar-deny-reason` header naming the
   arm (never the amounts — the operator knows their own numbers, the log does
   not need them).
3. Tests, shaped like `refuses_a_prompt_injection_before_dispatch`:
   over-cap → 402 **and the provider is not called**; under-cap → passes;
   `refuse_unpriced` off → serve-and-record; on → 402.

Gate: `cargo test -p ar-server`, clippy, plus a live 402 against a capped key.

## Phase 2 — port the request-rate limiter (the actual import)

New crate `crates/ar-limit` — separate from `ar-tokens` because the lifecycle is
different (in-memory, no disk, no ledger) and mirroring agentgateway's own split.
**No new dependencies** (their `ratelimit` + `quick_cache` crates are not worth
the dep weight for what std does in 100 lines).

Mechanism, ported from `crates/agentgateway/src/http/localratelimit.rs`:

```rust
pub struct LimitSpec {
    pub max_tokens: u64,        // same field, same meaning as theirs
    pub tokens_per_fill: u64,   // =
    pub fill_interval: Duration // =
}
struct Bucket { tokens: f64, last: Instant }        // GCRA-equivalent refill
struct Limiter { spec: LimitSpec, buckets: Mutex<HashMap<String, Bucket>> }
impl Limiter {
    /// Ok(()) or Err(retry_after_seconds)
    fn check(&self, key: &str) -> Result<(), u64>;
}
```

- Refill = `elapsed / fill_interval * tokens_per_fill`, clamp at `max_tokens`;
  refuse with `Retry-After = ceil(deficit / tokens_per_fill * interval)`.
- Bounded state: cap at 10 000 buckets, evict oldest-`last` on overflow —
  their `MAX_BUCKETS`/LRU behavior, stated in a doc comment as the ported trade.
- **Same honest divergence as theirs**: buckets are per-process. A multi-instance
  deployment gets per-instance limits. Documented, not engineered around.

Config surface (`ar-config`), reusing the existing key grammar:

```yaml
limits:
  default: { rpm: 60 }
  keys: { openai: { rpm: 600 } }
```

Enforcement: an axum middleware in `app.rs` beside the existing gate; 429 +
`Retry-After` on refusal. Bucket key = the authenticated key id when the gate is
armed, `"anonymous"` otherwise. `x-ar-decision` already carries routing facts;
add nothing to it — the 429 envelope carries the reason.

Tests: refill math (injected clock), per-key isolation, eviction bound, 429 shape
and recovery after one interval, config round-trip through `ar-config`'s parser.

Gate: `cargo test --workspace`, clippy, live 429-and-recover against
`free-stack`.

## Phase 3 — arm the gate we already have

`aroute keys arm-gate` (or `aroute serve --arm-gate`): generate or accept a
32-byte master key into the credential store so `Components.master_key`
resolves, plus a `doctor` row reporting armed/not. Closes the standing README
WARNING. ~20 lines plus tests; no port.

## Non-goals (stated, not forgotten)

- CEL engine (`cel-fork`, 880 KB), JWT/RBAC/OIDC/CSRF/ext-proc/remote rate
  limiting — triaged DELETE in `docs/01` and deliberately not resurrected.
- Cross-instance shared limiting (Redis): single-node scope.
- Third-party moderation integrations: different domain from our local needle
  guard; revisit only if a client-facing deployment needs them.

## Order, gates, and debt

Phases ship independently: **1 → 2 → 3**. Each phase carries its own
`cargo test --workspace` + `clippy --all-targets --all-features --locked --
-D warnings` + `cargo fmt --all` gate, a live smoke against `free-stack`, a
README section, and an AUDIT-REPORT note closing the relevant gap. Each module
doc credits its source file — the porting convention this repo already keeps.

After Phase 2 the standing security-gap list in the README shrinks to one row
(unauthenticated listener, fixed by Phase 3) and the "policy layer" row of the
agentgateway comparison table flips from *absent* to *deliberately minimal*.
