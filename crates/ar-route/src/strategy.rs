//! `Strategy` and provider selection — the twenty concrete strategies plus the
//! deferred-name escape hatch.
//!
//! Ported from `../OmniRoute/open-sse/services/combo/` — `targetSorters.ts` for
//! the load-shaped set, `quotaStrategies` / `quotaScoring` / `headroomRanking` /
//! `quotaShareStrategy` for the quota-shaped set, `promptCacheAffinity` and
//! `comboStructure.sortTargetsByContextSize` for the context-shaped set, and
//! `dispatchPrelude.tryFusionDispatch` / `tryPipelineDispatch` for the
//! panel-shaped pair.
//!
//! An `auto/*` *strategy name* still resolves to [`Strategy::Deferred`], and
//! naming one gets a specific 501 instead of a `todo!()` panic in the request
//! path. The `auto/*` *model* surface is not deferred: the sixteen-factor
//! scorer, [`crate::virtual_combo`], [`crate::simulate_route`] and
//! [`crate::explain_route`] all live in [`crate::auto`], and
//! [`crate::auto_variant_for_model`] is the pure helper a server stream calls to
//! learn whether a requested model is an `auto` alias.
//!
//! # Five port decisions that are not the obvious ones
//!
//! **No clock.** The reference ranks quota strategies on `resetAt - now` and
//! takes one `now` snapshot per ranking (`quotaScoring.ts`, the `#9330` fix).
//! Here every candidate shares one `now`, so subtracting it changes no
//! *ordering* — only the tieband widths. Dropping it costs a tieband and buys a
//! quota arm with no time source to fake, which is why the quota-shaped strategies
//! landed without adding a parameter to [`pick`] and without editing `ar-server`. A
//! window rollover is the quota store's job, upstream of the sorter, exactly as
//! `QuotaStore` / `accountBuckets` do it upstream of `applyStrategyOrdering`.
//!
//! **The random strategies draw from the shared cursor**, so a fixed starting
//! value yields a fixed sequence. The reference's RNG is `node:crypto` with a
//! test-only float-source seam (`secureRandom.ts`); a hand-rolled splitmix64
//! over the `AtomicU64` this crate already owns is the same shape, one fewer
//! dependency, and a sequence that is a pure function of its seed across
//! versions — which is what makes these testable without a flaky seed.
//!
//! **The load-shaped pair reads a per-target ledger.** `p2c` scored on
//! in-flight alone and `least-used` ranked on in-flight alone, so both were
//! really the same strategy with a random draw in front, and neither could see
//! anything the sixteen-factor scorer already knew. [`TargetLoads`] holds the
//! cumulative served count and the [`Factors::load_signals`] pair per
//! `provider:model` execution key — the `#7015` keying shape, minus the account
//! half this crate's [`Candidate`] has no field for. It is process-wide rather
//! than a parameter because [`pick`]'s signature belongs to callers outside this
//! crate; that is also the shape upstream gets from its module-level
//! `getComboMetrics`.
//!
//! **The panel-shaped pair is not a comparator.** `fusion` fans out and
//! `pipeline` chains stages, so neither is "rank the list, return the head".
//! [`pick`] still returns a provider for both — the panel leader and the
//! chain's first stage — and [`dispatch_fusion`] / [`dispatch_pipeline`] do the
//! real work over an [`Executor`]. Both share one panel filter ([`viable`]), so
//! a fused answer and a chained answer are drawn from the same candidate set.
//!
//! **The reference's `context-relay` has no comparator at all.** It appears in
//! `HANDLED_COMBO_STRATEGIES` but there is no `strategy === "context-relay"`
//! branch in `applyStrategyOrdering`; what it actually is a *handoff trigger*
//! (`resolveContextRelayConfig` + `executeTargetAttempt`), i.e. summarise and
//! inject the conversation when it moves targets, over the default order. The
//! handoff machinery is a sibling's. What lands here is the routing half the
//! deferred table names for it — the prefix pin — and that is
//! [`Strategy::ContextRelay`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use ar_cache::affinity::{AffinityKey, AffinityTarget};
use bytes::Bytes;
use futures::StreamExt;
use http::StatusCode;

use crate::auto::Factors;
use crate::contract::{
    Candidate, CanonicalRequest, ChunkStream, ExecError, Executor, ProviderId, Strng, Upstream,
};
use crate::error::RouteError;

/// Model slot for the prefix-pin key when the caller supplied no model.
///
/// [`pick`] takes no `model`, so a caller on that path still shares one pin
/// across every model it asks for. The model-scoped entry points
/// ([`pick_for_model`], [`pick_filtered`]) pass the real requested model and do
/// not share a pin; this constant is what keeps [`pick`] a total function.
const MODEL_SCOPE: &str = "*";

/// Reference `RESET_AWARE_EXHAUSTION_GUARD_PERCENT` = 10 → `0.10`.
///
/// The share of a window below which a pool is discounted rather than merely
/// ranked last. Without it a pool at 4% looks *better* than one at 6%, and the
/// next request tips it over; with it the two are ordered by how close to
/// exhaustion they both are, which is the question an operator is asking.
const EXHAUSTION_GUARD: f64 = 0.10;

/// Reference `scoreResetAwareQuota`'s neutral score for an unknown quota.
///
/// `0.5`, not `0.0` and not "sort last": we do not know, and treating that as
/// knowing the pool is empty would route every unmonitored provider away from.
const NEUTRAL_SCORE: f64 = 0.5;

/// `p2c`'s in-flight penalty ceiling — the reference's `breakerPenalty` value.
///
/// The reference subtracts a flat `0.25` when the breaker is `HALF_OPEN`. This
/// crate has no breaker state on a [`Candidate`], so the term it subtracts is
/// live load instead, and it is applied *saturating*
/// (`ceiling * n / (1 + n)`, so it approaches `ceiling` and never crosses it)
/// rather than linearly. A linear term would let one busy target's own penalty
/// run away and decide the comparison on its own, which is the failure mode the
/// bounded reference term exists to prevent.
const P2C_LOAD_PENALTY: f64 = 0.25;

/// `getP2CTargetScore`'s score for a target nothing has been observed about.
///
/// The reference reads `0.5` and `0.25` here and this crate keeps both numbers,
/// so an unobserved target sits *between* a target observed failing everything
/// and a target observed perfect — the honest "we have not watched this one"
/// position. Zeroing them instead would make "never seen" indistinguishable
/// from "seen and always broken", and an unmeasured provider would be routed
/// away from on the strength of a silence.
const P2C_NEUTRAL_RELIABILITY: f64 = 0.5;
const P2C_NEUTRAL_LATENCY: f64 = 0.25;

/// One target's observed load and health, as `p2c` and `least-used` rank it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TargetLoad {
    /// Requests this router has handed to the target, cumulatively.
    ///
    /// The `least-used` primary key, and the counter that makes the strategy
    /// *least-used* rather than *least-busy*.
    pub served: u64,
    /// Observed success share in `0.0..=1.0`, or `None` when never observed.
    pub reliability: Option<f64>,
    /// Observed inverse latency in `0.0..=1.0`, or `None` when never observed.
    pub latency_inv: Option<f64>,
}

impl TargetLoad {
    /// What a target nothing is known about scores.
    fn unknown() -> Self {
        Self {
            served: 0,
            reliability: None,
            latency_inv: None,
        }
    }

    /// `p2c`'s score for this target under `in_flight` units of live load:
    /// success and inverse latency add, load subtracts. Higher wins.
    ///
    /// Mirrors `getP2CTargetScore`'s additive-success-plus-latency-minus-a-
    /// penalty shape, with the bounded penalty documented on
    /// [`P2C_LOAD_PENALTY`]. No clock is read: `in_flight` is a count the
    /// caller supplies and the two factors are already-computed `f64`s, so the
    /// same pool and the same counts always score the same way.
    fn p2c_score(&self, in_flight: u32) -> f64 {
        let success = self.reliability.unwrap_or(P2C_NEUTRAL_RELIABILITY);
        let latency = self.latency_inv.unwrap_or(P2C_NEUTRAL_LATENCY);
        let load = f64::from(in_flight);
        success + latency - P2C_LOAD_PENALTY * load / (1.0 + load)
    }
}

/// What one target has been observed to do, keyed `provider:model`.
///
/// The reference keys per-target usage on `executionKey` — provider, model
/// *and* account — precisely so a combo that repeats one model across distinct
/// accounts spreads its load per account instead of collapsing into the shared
/// model bucket and exhausting whichever account sorts first (#7015). This
/// crate routes by provider rather than by connection, so the account half of
/// that key does not exist and `provider:model` is the finest identity a
/// [`Candidate`] carries. It is the same execution key
/// [`affinity_target`] builds, so one string serves both.
fn execution_key(c: &Candidate) -> String {
    format!("{}/{}", c.provider.as_str(), c.model)
}

/// Per-target load and health, shared by `p2c` and `least-used`.
///
/// Two signals this crate had nowhere to put, on one key, in one table: the
/// cumulative served count `least-used` was missing, and the reliability and
/// latency factors `p2c` was missing. `pick`'s signature is fixed by the
/// callers this crate cannot edit (`ar-server`'s `order`, `ar-mcp`, `ar-cli`), so
/// the table is reached through a process-wide default rather than a parameter —
/// the same shape the reference gets from its module-level
/// `getComboMetrics(comboName)`, and the reason those metrics are per-combo
/// there and per-target here.
///
/// `observe` is the write side. It is deliberately not called from `score_pool`:
/// `simulate_route` and `explain_route` are *dry runs*, and a dry run that
/// published live routing state would make a simulation change the routing it
/// was describing. A caller that holds real telemetry calls it.
///
/// `ponytail:` one process-wide `Mutex<HashMap>`, one key per
/// `provider:model` ever routed — bounded by the config's target list, not by
/// traffic, so it cannot grow without limit. A per-combo table (as upstream
/// keys it) would multiply that by the combo count for no behavioural gain.
/// Take it when a caller needs per-combo isolation. One `String` is built per
/// key and handed to `Arc<str>` in place, so a served pick costs one allocation
/// rather than two.
#[derive(Debug, Default)]
pub struct TargetLoads {
    rows: Mutex<HashMap<Strng, TargetLoad>>,
}

impl TargetLoads {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what is known about `target` from an already-scored factor set.
    ///
    /// Takes [`Factors`] rather than two loose `f64`s so the two numbers cannot
    /// be transposed at the call site; [`Factors::load_signals`] is where the
    /// pair is named.
    ///
    /// `allow(dead_code)`: the producer is the server's telemetry path, which
    /// reads outcome and latency data this crate never sees and lives in files
    /// outside the change this landed in. Until it calls in, `p2c` reads the
    /// documented neutral pair and degrades to the load term it always had —
    /// the same contract every unpopulated optional signal in [`Candidate`]
    /// carries.
    #[allow(
        dead_code,
        reason = "write path for the caller's telemetry; no producer in-crate yet"
    )]
    pub fn observe(&self, target: &Candidate, factors: &Factors) {
        let (reliability, latency_inv) = factors.load_signals();
        if let Ok(mut rows) = self.rows.lock() {
            let row = rows
                .entry(Strng::from(execution_key(target)))
                .or_insert(TargetLoad::unknown());
            row.reliability = Some(reliability);
            row.latency_inv = Some(latency_inv);
        }
    }

    /// Records that this router has served one request on `target`.
    pub fn serve(&self, target: &Candidate) {
        if let Ok(mut rows) = self.rows.lock() {
            let row = rows
                .entry(Strng::from(execution_key(target)))
                .or_insert(TargetLoad::unknown());
            row.served = row.served.saturating_add(1);
        }
    }

    /// What is known about `target`; [`TargetLoad::unknown`] when nothing is.
    ///
    /// The `is_empty` guard is what keeps the common case allocation-free:
    /// naming a target costs a `String` for the key, and a proxy nobody has
    /// published observations for must not pay that once per candidate per
    /// request.
    #[must_use]
    pub fn load(&self, target: &Candidate) -> TargetLoad {
        let Ok(rows) = self.rows.lock() else {
            // A poisoned table costs two strategies their second signal, not
            // correctness: every candidate reads as unobserved, which is the
            // documented fallback. Skip rather than panic a live request.
            return TargetLoad::unknown();
        };
        if rows.is_empty() {
            return TargetLoad::unknown();
        }
        rows.get(execution_key(target).as_str())
            .copied()
            .unwrap_or_else(TargetLoad::unknown)
    }

    /// The table [`by_p2c`] and [`by_least_used`] read.
    #[must_use]
    pub fn global() -> &'static Self {
        static LOADS: OnceLock<TargetLoads> = OnceLock::new();
        LOADS.get_or_init(TargetLoads::new)
    }
}

/// The full strategy set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Strategy {
    // ---- lean-routing (P0) ----
    /// First-ranked candidate; drain it before the next.
    Priority,
    /// Cycle candidates in order. A single [`AtomicU64`] counter, no lock.
    RoundRobin,
    /// Cheapest known input price wins; unpriced candidates sort last.
    CostOptimized,
    /// Stick to the last provider that succeeded for this session, else
    /// fall back to [`Strategy::Priority`] ordering.
    Lkgp,

    // ---- load-shaped (targetSorters.ts) ----
    /// Roulette-wheel draw over `Candidate::weight`.
    Weighted,
    /// Keep the incoming order and let the attempt loop drain the head. An
    /// identity comparator in the reference too, and for the same reason.
    FillFirst,
    /// Power of two choices: draw two distinct, take the healthier one. Beats
    /// uniform random on tail latency for the same cost, which is why both
    /// exist. "Healthier" is [`TargetLoad::p2c_score`]: success rate and inverse
    /// latency add, live load subtracts, and an unobserved target sits at the
    /// reference's neutral so the draw degrades to a load comparison.
    P2c,
    /// Fewest requests served wins, then fewest in flight, then `rank`. The
    /// first key is cumulative and keyed per target, so a combo listing one
    /// model across several providers spreads across providers instead of
    /// draining whichever it started on (#7015).
    LeastUsed,
    /// Uniform draw.
    Random,
    /// Uniform draw over *distinct providers*, so two models on one provider
    /// cannot come up back to back.
    StrictRandom,

    // ---- quota-shaped (quotaStrategies / headroomRanking / quotaShare) ----
    /// Most free fraction of its window wins. Unmonitored pools rank first.
    Headroom,
    /// Soonest window rollover wins: that is the soonest free capacity.
    ResetWindow,
    /// Most free fraction, discounted as a pool nears exhaustion.
    ResetAware,
    /// Reset-aware score, load-weighted by live in-flight. An unknown quota
    /// ranks last but is never removed from the list.
    QuotaWeighted,
    /// DRR order by normalised weight, then power-of-two over live in-flight.
    QuotaShareFair,
    /// Spends the quota closest to being lost: highest usable fraction per hour
    /// until its window rolls, so a full window closing soon outranks an equally
    /// full one that holds for days.
    ///
    /// **Placement divergence** from the reference, which ranks a provider's
    /// multiple OAuth *connections* inside credential selection; this build is
    /// one credential per provider, so the strategy ranks combo targets
    /// instead. See `docs/audit-notes.md`.
    ExpiryFirst,

    // ---- context-shaped (promptCacheAffinity / sortTargetsByContextSize) ----
    /// Pure prefix pin: the HRW leader for this conversation.
    ContextRelay,
    /// Largest context window wins. A long prompt goes to a big model instead
    /// of discovering the limit as a 400.
    ContextOptimized,
    /// Most already-cached prefix tokens, affinity breaking ties.
    CacheOptimized,

    // ---- panel-shaped (dispatchPrelude) ----
    /// Panel leader: affinity, among the targets the pre-dispatch expansion
    /// kept.
    Fusion,
    /// First stage of a chain: config order, among the same kept targets.
    Pipeline,

    /// A strategy this build does not carry: an `auto/*` variant or a typo.
    Deferred(&'static str),
}

impl Strategy {
    /// Parses the wire/config spelling.
    ///
    /// Every landed name resolves to a real variant, and every name upstream
    /// spells *and* this build does not carry resolves to
    /// [`Strategy::Deferred`] carrying **that** name, so the 501 names the
    /// strategy the operator actually wrote. `auto/*` and typos stay
    /// [`Strategy::Deferred`] rather than crashing at startup — and a typo is
    /// reported as a typo, not filed against the P2 gate as a missing feature.
    ///
    /// `"quota-share"` is the reference's internal spelling of the
    /// fair-share strategy (`INTERNAL_ROUTING_STRATEGY_VALUES`, used by the
    /// auto-minted `qtSd/` combos); `"quota-share-fair"` is the visible one. Same
    /// strategy, both spellings — the variant keeps the visible name so
    /// `as_str` and the decision header never leak an internal value.
    ///
    /// The four aliases are the reference's `normalizeRoutingStrategy`
    /// (`routingStrategies.ts:68-73`): `usage`, `context`, `weekly-reset` and
    /// `reset-window-order` are spellings upstream *accepts on input* and this
    /// build resolves them to the same variants. Without them a combo ported
    /// from an OmniRoute config that used one would resolve to
    /// [`Strategy::Deferred`] and 501 on a strategy this build ships.
    #[must_use]
    pub fn parse(name: &str) -> Self {
        match name {
            "priority" => Self::Priority,
            "round-robin" => Self::RoundRobin,
            "cost-optimized" => Self::CostOptimized,
            "lkgp" => Self::Lkgp,
            "weighted" => Self::Weighted,
            "fill-first" => Self::FillFirst,
            "p2c" => Self::P2c,
            "least-used" | "usage" => Self::LeastUsed,
            "random" => Self::Random,
            "strict-random" => Self::StrictRandom,
            "headroom" => Self::Headroom,
            "reset-window" | "weekly-reset" | "reset-window-order" => Self::ResetWindow,
            "reset-aware" => Self::ResetAware,
            "quota-weighted" => Self::QuotaWeighted,
            "quota-share" | "quota-share-fair" => Self::QuotaShareFair,
            "context-relay" => Self::ContextRelay,
            "context-optimized" | "context" => Self::ContextOptimized,
            "cache-optimized" => Self::CacheOptimized,
            "fusion" => Self::Fusion,
            "pipeline" => Self::Pipeline,
            other if other.starts_with("auto") => Self::Deferred("auto"),
            "expiry-first" => Self::ExpiryFirst,
            _ => Self::Deferred("unknown"),
        }
    }

    /// Canonical config spelling, used in the `x-ar-decision` header.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::RoundRobin => "round-robin",
            Self::CostOptimized => "cost-optimized",
            Self::Lkgp => "lkgp",
            Self::Weighted => "weighted",
            Self::FillFirst => "fill-first",
            Self::P2c => "p2c",
            Self::LeastUsed => "least-used",
            Self::Random => "random",
            Self::StrictRandom => "strict-random",
            Self::Headroom => "headroom",
            Self::ResetWindow => "reset-window",
            Self::ResetAware => "reset-aware",
            Self::QuotaWeighted => "quota-weighted",
            Self::QuotaShareFair => "quota-share-fair",
            Self::ExpiryFirst => "expiry-first",
            Self::ContextRelay => "context-relay",
            Self::ContextOptimized => "context-optimized",
            Self::CacheOptimized => "cache-optimized",
            Self::Fusion => "fusion",
            Self::Pipeline => "pipeline",
            Self::Deferred(_) => "deferred",
        }
    }

    /// Every strategy this build names, for `ar combo` / `/models` discovery.
    ///
    /// Includes the deferred entry, because a surface that lists what it cannot
    /// do is more useful than one that silently omits it.
    #[must_use]
    pub fn all() -> &'static [Strategy] {
        use Strategy::{
            CacheOptimized, ContextOptimized, ContextRelay, CostOptimized, Deferred, ExpiryFirst,
            FillFirst, Fusion, Headroom, LeastUsed, Lkgp, P2c, Pipeline, Priority, QuotaShareFair,
            QuotaWeighted, Random, ResetAware, ResetWindow, RoundRobin, StrictRandom, Weighted,
        };
        &[
            Priority,
            RoundRobin,
            CostOptimized,
            Lkgp,
            Weighted,
            FillFirst,
            P2c,
            LeastUsed,
            Random,
            StrictRandom,
            Headroom,
            ResetWindow,
            ResetAware,
            QuotaWeighted,
            QuotaShareFair,
            ExpiryFirst,
            ContextRelay,
            ContextOptimized,
            CacheOptimized,
            Fusion,
            Pipeline,
            Deferred("auto"),
        ]
    }

    /// The concrete name a [`Strategy::Deferred`] stands for, when it is a
    /// known-unimplemented name. `None` for live strategies and for typos.
    #[must_use]
    pub fn deferred_name(self) -> Option<&'static str> {
        match self {
            Self::Deferred(name) => Some(name),
            _ => None,
        }
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deferred("unknown") => f.write_str("unknown"),
            Self::Deferred(name) => f.write_str(name),
            live => f.write_str(live.as_str()),
        }
    }
}

/// Chooses the provider for one request.
///
/// `session` is read by [`Strategy::Lkgp`] and by the two affinity-ranked
/// strategies, which use it as the prefix-pin key; pass `None` otherwise. It is
/// borrowed, never cloned — a hot path that allocates per request is how the
/// RAM budget in `docs/00-overview.md` goes out the window.
///
/// The pin is **not** model-scoped on this path: [`pick`] receives no model, so
/// every model reached through one session key shares a pin. Callers that hold
/// `canonical.model` should call [`pick_for_model`] instead; this stays the
/// model-free entry point so existing call sites keep compiling.
///
/// # Errors
/// - [`RouteError::NoCandidates`] when `candidates` is empty.
/// - [`RouteError::DeferredStrategy`] for [`Strategy::Deferred`].
///
/// [`docs/02`]: the retry/fail-over classification is in
/// [`crate::attempt::classify_status`], not here.
pub fn pick(
    strategy: Strategy,
    session: Option<&str>,
    candidates: &[Candidate],
    rr: &AtomicU64,
    lkgp: Option<&crate::lkgp::LkgpPins>,
) -> Result<ProviderId, RouteError> {
    route(strategy, session, MODEL_SCOPE, candidates, rr, lkgp, None)
}

/// [`pick`] with the requested `model` in scope, so the prefix pin is per-model.
///
/// Two models reached through one session key no longer share a pin: the model
/// is a component of the `ar-cache` affinity digest, so the same conversation
/// asked for `gpt-4o` and for `mixtral` resolves to two independent HRW
/// leaders. That is what a caller wants as soon as the conversation's model can
/// change — otherwise the second model's requests all land on whichever
/// provider the *first* model's pin named.
///
/// Every other parameter, and every strategy, behaves exactly as in [`pick`].
///
/// # Errors
/// As [`pick`].
pub fn pick_for_model(
    strategy: Strategy,
    session: Option<&str>,
    model: &str,
    candidates: &[Candidate],
    rr: &AtomicU64,
    lkgp: Option<&crate::lkgp::LkgpPins>,
) -> Result<ProviderId, RouteError> {
    route(strategy, session, model, candidates, rr, lkgp, None)
}

/// [`pick_for_model`] with a cooling gate in front of the ranking.
///
/// `skip` answers "is this candidate not usable right now?" — the caller's
/// `Resilience::is_cooling` is the intended argument. A skipped candidate is
/// removed before ranking, so a cooling provider cannot win `priority` and then
/// cost the attempt loop a round trip to discover it was already known dead.
///
/// When `skip` removes nothing (the common case) the caller's slice is borrowed
/// unchanged and the pick allocates nothing, exactly as [`pick`] does.
///
/// # Errors
/// - [`RouteError::NoCandidates`] when `candidates` is empty, or when `skip`
///   rejects every one of them.
pub fn pick_filtered(
    strategy: Strategy,
    session: Option<&str>,
    model: &str,
    candidates: &[Candidate],
    rr: &AtomicU64,
    lkgp: Option<&crate::lkgp::LkgpPins>,
    skip: &dyn Fn(&Candidate) -> bool,
) -> Result<ProviderId, RouteError> {
    route(strategy, session, model, candidates, rr, lkgp, Some(skip))
}

/// The one body behind all three `pick` entry points.
///
/// `skip` is `None` on the ungated paths so the common case never evaluates a
/// predicate and never copies a candidate.
fn route(
    strategy: Strategy,
    session: Option<&str>,
    model: &str,
    candidates: &[Candidate],
    rr: &AtomicU64,
    lkgp: Option<&crate::lkgp::LkgpPins>,
    skip: Option<&dyn Fn(&Candidate) -> bool>,
) -> Result<ProviderId, RouteError> {
    let pool = Pool::admitted(candidates, skip);
    let candidates = pool.as_slice();
    let Some(first) = candidates.first() else {
        return Err(RouteError::NoCandidates);
    };

    let winner = match strategy {
        Strategy::Priority | Strategy::FillFirst => by_rank(candidates, first),
        Strategy::RoundRobin | Strategy::Random => by_cursor(candidates, rr),
        Strategy::CostOptimized => by_price(candidates, first),
        Strategy::Lkgp => by_lkgp(session, candidates, lkgp, first),

        Strategy::Weighted => by_weight(candidates, rr),
        Strategy::P2c => by_p2c(candidates, rr),
        Strategy::LeastUsed => by_least_used(candidates, first),
        Strategy::StrictRandom => by_distinct_provider(candidates, rr),

        Strategy::Headroom => by_headroom(candidates, first),
        Strategy::ResetWindow => by_reset_window(candidates, first),
        Strategy::ResetAware => by_reset_aware(candidates, first),
        Strategy::QuotaWeighted => by_quota_weighted(candidates, first),
        Strategy::QuotaShareFair => by_fair_share(candidates, first),
        Strategy::ExpiryFirst => by_expiry_first(candidates, first),

        Strategy::ContextRelay => by_affinity(candidates, session, model, first),
        Strategy::ContextOptimized => by_context_window(candidates, first),
        Strategy::CacheOptimized => by_cached_prefix(candidates, session, model, first),
        Strategy::Fusion => fusion_leader(candidates, session, model, first),
        Strategy::Pipeline => pipeline_head(candidates, first),

        Strategy::Deferred(_) => return Err(RouteError::DeferredStrategy(strategy)),
    };

    // The served counter is `least-used`'s own accounting, so only `least-used`
    // writes it. Counting every strategy's picks here instead would make one
    // strategy's ranking depend on how much traffic an unrelated one had drawn,
    // and would let a combo flip between two schedulers and lose its history.
    if strategy == Strategy::LeastUsed {
        TargetLoads::global().serve(winner);
    }

    Ok(winner.provider.clone())
}

/// The candidate list after the cooling gate.
///
/// `All` borrows the caller's slice, which is what every ungated pick and every
/// request with nothing cooling gets: zero allocation on the hot path.
///
/// `Kept` owns a filtered copy. `ponytail:` it clones the survivors (a `Strng`
/// refcount bump each) rather than re-typing all twenty sorters over
/// `&[&Candidate]`; the gate is off by default, and one `Vec` per cooled request
/// is cheaper to reason about than a parallel set of `&[&Candidate]` signatures
/// that could drift from the `&[Candidate]` one.
enum Pool<'a> {
    All(&'a [Candidate]),
    Kept(Vec<Candidate>),
}

impl<'a> Pool<'a> {
    /// The candidates `skip` kept, or all of them when `skip` is absent or
    /// rejects nothing.
    fn admitted(candidates: &'a [Candidate], skip: Option<&dyn Fn(&Candidate) -> bool>) -> Self {
        let Some(skip) = skip else {
            return Self::All(candidates);
        };
        let kept: Vec<Candidate> = candidates.iter().filter(|c| !skip(c)).cloned().collect();
        if kept.len() == candidates.len() {
            Self::All(candidates)
        } else {
            Self::Kept(kept)
        }
    }

    /// The admitted candidates, as the slice every sorter takes.
    fn as_slice(&self) -> &[Candidate] {
        match self {
            Self::All(all) => all,
            Self::Kept(kept) => kept,
        }
    }
}

// ---------------------------------------------------------------------------
// lean-routing
// ---------------------------------------------------------------------------

/// `lkgp`: live pin, else [`Strategy::Priority`] ordering.
fn by_lkgp<'a>(
    session: Option<&str>,
    candidates: &'a [Candidate],
    lkgp: Option<&crate::lkgp::LkgpPins>,
    first: &'a Candidate,
) -> &'a Candidate {
    session
        .and_then(|s| lkgp.and_then(|p| p.get(s)))
        // A pin to a provider no longer in the candidate set is a config
        // change, not a reason to 500: fall through to priority.
        .and_then(|pinned| candidates.iter().find(|c| c.provider == pinned))
        .unwrap_or_else(|| by_rank(candidates, first))
}

// ---------------------------------------------------------------------------
// load-shaped
// ---------------------------------------------------------------------------

/// `weighted`: one draw over the weight total.
///
/// Cumulative rather than a rejection loop: a rejection loop spins forever if
/// every weight is zero, and the cumulative walk cannot. `Candidate::with_weight`
/// clamps `0` to `1`, so the total is at least `len`.
fn by_weight<'a>(candidates: &'a [Candidate], rr: &AtomicU64) -> &'a Candidate {
    let total: u64 = candidates.iter().map(|c| u64::from(c.weight)).sum();
    let mut ticket = splitmix(rr) % total.max(1);
    for c in candidates {
        let w = u64::from(c.weight);
        if ticket < w {
            return c;
        }
        ticket -= w;
    }
    // Unreachable: the walk consumes exactly `total` tickets. Falling back to
    // the head keeps the function total without an `unwrap`.
    &candidates[0]
}

/// `p2c`: draw two distinct, take the healthier one.
///
/// The two indices are built the reference's way — `r1 % n` and `r2 % (n-1)`
/// shifted past the first — so the pair is not uniform over unordered pairs.
/// Reproducing that is the point: a "cleaner" `(r1 % n, r2 % n)` would be a
/// different distribution, and this table exists to be comparable against the
/// thing it ports.
///
/// The *score* is the reference's too (`getP2CTargetScore`): success rate and
/// inverse latency add, a penalty subtracts, and the higher score wins. What
/// the two signals read is [`TargetLoad`] — the same two factors
/// [`Factors::load_signals`] names out of the sixteen, so the draw ranks on
/// health rather than on queue depth alone. The tiebreak stays the reference's:
/// `score(second) > score(first)`, so an exact tie keeps the *first* draw.
///
/// With nothing observed, both draws score the neutral pair and the comparison
/// collapses to the load term, which is what this function did before the
/// ledger existed — a strategy that has never been told about a provider
/// degrades to the signal it always had rather than to an error.
fn by_p2c<'a>(candidates: &'a [Candidate], rr: &AtomicU64) -> &'a Candidate {
    let n = candidates.len() as u64;
    if n < 2 {
        return &candidates[0];
    }
    let seed = splitmix(rr);
    let first = (seed % n) as usize;
    let mut second = ((seed >> 32) % (n - 1)) as usize;
    if second >= first {
        second += 1;
    }
    let (a, b) = (&candidates[first], &candidates[second]);
    let loads = TargetLoads::global();
    let score = |c: &'a Candidate| loads.load(c).p2c_score(c.in_flight);
    // Strict `>`, so a tie keeps `a` — the reference's `score(second) >
    // score(first) ? secondIndex : firstIndex` spelled the same way round.
    if score(b) > score(a) { b } else { a }
}

/// `least-used`: fewest requests served, then fewest in flight, then `rank`.
///
/// The first key is cumulative, which is the whole point of the variant and was
/// this function's gap: it ranked on `in_flight` alone, so a target that had
/// just drained looked identical to one that had never been used, and the combo
/// kept handing work back to whichever account it had already spent. Keyed on
/// the execution identity ([`execution_key`]) so a combo that lists one model
/// across several providers still spreads per target rather than per model
/// (#7015).
///
/// In-flight survives as the second key for the case the cumulative count cannot
/// see: two targets that have served equally and one is mid-burst right now.
/// `rank` is last, unchanged, so a config with no telemetry at all ranks exactly
/// as it did before.
fn by_least_used<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    let loads = TargetLoads::global();
    candidates
        .iter()
        .min_by_key(|c| (loads.load(c).served, c.in_flight, c.rank))
        .unwrap_or(first)
}

/// `strict-random`: uniform over *distinct providers*.
///
/// [`Strategy::Random`] draws over candidates, so two models on one provider
/// can come up back to back and the second attempt is spent re-reaching an
/// endpoint that already answered. Collapsing to providers first costs one
/// linear pass and removes that failure mode; `ar-server`'s `build_chain` does
/// it again on the way out.
///
/// `ponytail:` the reference's fuller version is a persisted deck that also
/// guarantees the last pick of cycle N is not the first of cycle N+1 and
/// reshuffles the fallback tail. That needs a per-combo deck table, which is
/// state this crate has nowhere to put — take it when a combo registry lands.
fn by_distinct_provider<'a>(candidates: &'a [Candidate], rr: &AtomicU64) -> &'a Candidate {
    let mut distinct: Vec<&Candidate> = Vec::with_capacity(candidates.len());
    for c in candidates {
        if !distinct.iter().any(|d| d.provider == c.provider) {
            distinct.push(c);
        }
    }
    // `distinct` is non-empty whenever `candidates` is, which `pick` has
    // already established, so the modulus never divides by zero.
    let n = distinct.len() as u64;
    distinct[(rr.fetch_add(1, Ordering::Relaxed) % n) as usize]
}

// ---------------------------------------------------------------------------
// quota-shaped
// ---------------------------------------------------------------------------

/// `headroom`: most free fraction of its window, descending.
///
/// A missing window scores `1.0` and therefore ranks *first* — the reference's
/// deliberate fail-open (`computeHeadroom` treats an absent saturation as
/// `util = 0`). It is the right default: no telemetry is not a constraint, and
/// sorting unknown-last would route away from every unmonitored provider, which
/// is how a fleet ends up with one provider carrying all the traffic.
fn by_headroom<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    candidates
        .iter()
        .max_by(|a, b| {
            a.quota
                .map_or(1.0, |q| q.headroom())
                .total_cmp(&b.quota.map_or(1.0, |q| q.headroom()))
                .then_with(|| b.rank.cmp(&a.rank))
        })
        .unwrap_or(first)
}

/// `reset-window`: soonest rollover first, ascending.
///
/// `Infinity` upstream, back of the list here: a pool whose window is unknown
/// cannot be said to free capacity soon, and leading with it would send the
/// first request into the unknown. The reference's 60 s tieband and its
/// round-robin rotation of that band need a shared counter table this crate does
/// not have, and dropping the band changes no winner decided outright.
///
/// Unknown is [`u64::MAX`], i.e. the same `Infinity` the reference sorts it as,
/// and — the part that was a bug — it is *ordered*, not *filtered*. A filter-out
/// emptied the candidate set whenever no pool had a known window and silently
/// returned `candidates[0]`, which looks like a decision and is not one. Now the
/// unknown pools stay in the list, last, and each is still reachable when it is
/// the only thing left to route to.
fn by_reset_window<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    candidates
        .iter()
        .min_by(|a, b| {
            rollover(a)
                .cmp(&rollover(b))
                .then_with(|| a.rank.cmp(&b.rank))
        })
        .unwrap_or(first)
}

/// The sort key for `reset-window`: the rollover instant, or `u64::MAX` when the
/// window is absent or its reset time was never reported.
fn rollover(c: &Candidate) -> u64 {
    c.quota
        .map(|q| {
            if q.reset_at_secs == 0 {
                u64::MAX
            } else {
                q.reset_at_secs
            }
        })
        .unwrap_or(u64::MAX)
}

/// `reset-aware`: most free fraction, discounted near exhaustion.
///
/// The reference blends remaining against reset *pressure*; the pressure term is
/// the only clock-dependent half, and dropping it leaves the exhaustion guard —
/// which is the part that changes winners, because a pool at 4% must not
/// outrank one at 6% when both are about to fail. An exhausted pool scores
/// `-inf` and an unknown one scores [`NEUTRAL_SCORE`], as upstream.
fn by_reset_aware<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    candidates
        .iter()
        .max_by(|a, b| {
            reset_aware_score(a)
                .total_cmp(&reset_aware_score(b))
                .then_with(|| {
                    a.quota
                        .map_or(0, |q| q.reset_at_secs)
                        .cmp(&b.quota.map_or(0, |q| q.reset_at_secs))
                })
                .then_with(|| b.rank.cmp(&a.rank))
        })
        .unwrap_or(first)
}

/// The `reset-aware` score for one candidate.
fn reset_aware_score(c: &Candidate) -> f64 {
    let Some(quota) = c.quota else {
        return NEUTRAL_SCORE;
    };
    if quota.remaining() == 0 {
        return f64::NEG_INFINITY;
    }
    let remaining = quota.headroom();
    if remaining >= EXHAUSTION_GUARD {
        return remaining;
    }
    remaining * (remaining / EXHAUSTION_GUARD).max(0.05)
}

/// The floor a window's free fraction must clear to count as spendable: at or
/// below 1% remaining, burning more requests against it wastes the capacity it
/// has left without meaningfully changing the balance.
const EXPIRY_FLOOR: f64 = 0.01;
/// The shortest deadline an urgency denominator may take, in hours. A window
/// that has already rolled (`reset_at_secs` in the past) is maximally urgent,
/// and dividing by a vanishing number of hours would otherwise make its
/// urgency unbounded.
const EXPIRY_MIN_HOURS: f64 = 0.25;

/// `expiry-first`: how much usable quota must be spent PER HOUR to avoid losing
/// it at the next reset.
///
/// The port of the reference's `scoreExpiryFirstQuota`; the scoring half
/// survives verbatim, the placement does not (see the variant's doc).
///
/// * `usable` is the tightest window's free fraction, because nested windows
///   all decrement together and no account can spend more than its most
///   constrained window allows.
/// * The deadline is the NEAREST reset: that is when the first tranche is lost.
/// * Exhausted scores `-inf`; no reset time at all falls back to plain
///   leftover, ranked below anything under a real deadline.
///
/// Deliberately not [`reset_aware_score`], which is a recovery signal favouring
/// a nearly-empty pool about to refresh — "who will be useful soon", where this
/// answers "whose quota is about to be thrown away".
fn by_expiry_first<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    candidates
        .iter()
        .max_by(|a, b| {
            expiry_first_score(a)
                .total_cmp(&expiry_first_score(b))
                .then_with(|| a.rank.cmp(&b.rank))
        })
        .unwrap_or(first)
}

/// The `expiry-first` score for one candidate: **higher is more urgent to spend.**
///
/// A window with no deadline ranks by leftover, and the clamping floors keep the
/// whole function finite: `score == f64::INFINITY` is legal for `total_cmp` but
/// two infinities would then fall through to `rank`, which is the ordering the
/// reference's tieband produces anyway — pinned by
/// `an_exhausted_window_never_outranks_a_spendable_one`.
fn expiry_first_score(c: &Candidate) -> f64 {
    let Some(quota) = c.quota else {
        // No quota record: nothing known to waste, so nothing to spend first.
        return 0.0;
    };
    let usable = quota.headroom();
    if usable <= EXPIRY_FLOOR {
        return f64::NEG_INFINITY;
    }
    match hours_until_reset(quota.reset_at_secs) {
        None => usable,
        Some(hours) => (usable / hours.max(EXPIRY_MIN_HOURS)).min(EXPIRY_MAX_SCORE),
    }
}

/// Hours until `reset_at_secs`, or `None` when the window reported no reset.
///
/// A past instant yields [`EXPIRY_MIN_HOURS`]: the snapshot predates the reset
/// it describes, and treating that as maximally urgent is the safe direction —
/// a just-rolled window is full, and re-reading it costs nothing.
fn hours_until_reset(reset_at_secs: u64) -> Option<f64> {
    if reset_at_secs == 0 {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    Some((reset_at_secs as f64 - now) / SECONDS_PER_HOUR)
}

/// Seconds in an hour. Named so the divisor above is a unit conversion and not
/// a bare 3600 that can be dropped.
const SECONDS_PER_HOUR: f64 = 3600.0;

/// The urgency ceiling, `1 / EXPIRY_MIN_HOURS`, expressed in score units: a
/// fully usable window reaching its deadline scores this. Clamped rather than
/// infinite so the ordering stays a total order over finite scores without a
/// special case.
const EXPIRY_MAX_SCORE: f64 = 4.0;

/// Ceiling on one fusion panel member's buffered body.
///
/// Sized against the RAM budget in `docs/00-overview.md`, not against what a
/// provider might return: `MAX_PANEL` x this must stay inside the ceiling for a
/// heavy concurrent request, so 256KB x 40 = 10MB against the <400MB-for-20
/// row. A completion answer is KBs, so the cap is not felt in normal operation;
/// what it bounds is a member streaming without end, which is the same heap case
/// `MAX_PANEL` exists to prevent one level down. Pinned by
/// `panel_bounds_fit_the_ram_budget`.
const PANEL_BODY_BYTES: usize = 256 * 1024;

/// `quota-weighted`: reset-aware score, divided by live load.
///
/// Ported from `orderTargetsByQuotaWeighted`
/// (`quotaStrategies.ts` L768). The reference's shape is
/// `weight = max(0, score) / (1 + inFlight)` over a pool it first splits into a
/// "has room and was observed recently" A pool and everything else, drawing one
/// winner with `pickWeightedIndex`.
///
/// Two things do not survive the port, and both are the reference's own
/// modelling, not its shape:
///
/// * **The draw is a sort.** The RNG is what makes a weighted draw a *draw*; this
///   crate's load-shaped strategies are seeded precisely so a decision is
///   reproducible, and a routing decision an operator cannot re-derive from the
///   inputs is the failure mode [`reset_aware_score`] exists to avoid. Ranking by
///   the same weight picks the same candidate the draw would pick most often, and
///   the tieband falls back to `in_flight` then `rank`.
/// * **No A/B split and no staleness window.** Both are properties of *when the
///   quota was last fetched*, a signal [`QuotaWindow`] does not carry — the
///   rollover is the quota store's job, upstream of the sorter.
///
/// The pool is also never emptied, which is the part the reference gets wrong
/// for a router: it filters to `remainingPercent > 0`, so a candidate with no
/// quota record scores `0` and is *dropped*, and when every candidate is
/// unmonitored the function returns an empty list. Here an unknown quota is a
/// last-place tier that stays in the list, behind every pool with known room and
/// ahead of a pool that has said it is empty. `remainingPercent == 0` upstream
/// means "no capacity"; it does not mean "do not dispatch to this provider".
fn by_quota_weighted<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    candidates
        .iter()
        .min_by(|a, b| {
            quota_weighted_key(a)
                .total_cmp(&quota_weighted_key(b))
                .then_with(|| a.rank.cmp(&b.rank))
        })
        .unwrap_or(first)
}

/// Sort key for [`by_quota_weighted`], **lower wins**.
///
/// `(tier, -weight)` folded into one `f64` so the whole ordering is a
/// `total_cmp`. Tiers are spaced by `2.0` and the weight is at most `1.0`, so a
/// tier can never interleave with the next: `-1.0..=0.0` a pool with known room,
/// `2.0` an unknown quota, `4.0` a pool that has already said it is empty. The
/// weight is the reset-aware score over `1 + in_flight`, so one unit of load
/// halves the score and a busy pool loses to a roomier idle one.
fn quota_weighted_key(c: &Candidate) -> f64 {
    let (tier, score) = match c.quota {
        Some(q) if q.remaining() == 0 => (4.0, 0.0),
        Some(_) => (0.0, reset_aware_score(c).max(0.0)),
        None => (2.0, 0.0),
    };
    tier - score / (1.0 + f64::from(c.in_flight))
}

/// `quota-share-fair`: DRR order, then power-of-two over live in-flight.
///
/// Two ported stages. **DRR** (`selectQuotaShareTarget` step 2) hands out quanta
/// of `weight / total` and the winner pays 1, so the long-run frequency
/// converges on the weight ratio; that needs a persisted deficit map, but its
/// *first* round from an empty map is "largest quantum wins", which is the
/// normalised-weight order below. **P2C** (step 3) then compares live in-flight
/// across the top two and keeps the DRR winner on a tie, which is the
/// work-conserving lend: a bigger pool wins its share even when the smaller one
/// is momentarily idle. The reference's gates — bucket saturation and
/// per-connection concurrency — are the one filter [`viable`] applies.
fn by_fair_share<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    let mut eligible = viable(candidates);
    if eligible.len() > 1 {
        eligible.sort_by(|a, b| b.weight.cmp(&a.weight).then_with(|| a.rank.cmp(&b.rank)));
        let (leader, runner_up) = (eligible[0], eligible[1]);
        if runner_up.in_flight < leader.in_flight {
            return runner_up;
        }
    }
    eligible.first().copied().unwrap_or(first)
}

/// Candidates with quota left, or all of them when none has any.
///
/// The reference drops quota-exhausted targets during pre-dispatch expansion and
/// keeps the list when that drop empties it (`eligible.length > 0 ? eligible :
/// targets`). Sending a request to a pool that has already said "no" to recover a
/// throttled answer is worse than sending it to a full one.
fn viable(candidates: &[Candidate]) -> Vec<&Candidate> {
    let kept: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.quota.is_none_or(|q| q.remaining() > 0))
        .collect();
    if kept.is_empty() {
        candidates.iter().collect()
    } else {
        kept
    }
}

// ---------------------------------------------------------------------------
// context-shaped and panel-shaped
// ---------------------------------------------------------------------------

/// `context-relay`: the pure prefix pin.
///
/// Delegates scoring to `ar_cache::affinity`, which is stateless — no pin table,
/// no TTL, no sweep — and where adding a candidate cannot disturb the relative
/// order of the ones already there. Without a session key there is no
/// conversation to relay, so this degrades to [`Strategy::Priority`] rather than
/// to "pin to the first target"; that would collapse every anonymous client onto
/// one provider.
fn by_affinity<'a>(
    candidates: &'a [Candidate],
    session: Option<&str>,
    model: &str,
    first: &'a Candidate,
) -> &'a Candidate {
    session
        .and_then(|s| affinity_key(s, model))
        .and_then(|key| affinity_leader(candidates.iter(), &key))
        .unwrap_or_else(|| by_rank(candidates, first))
}

/// `cache-optimized`: most already-cached prefix tokens, affinity breaking ties.
///
/// The reference's `applyPromptCacheAffinity` asks where the prefix *should*
/// live; the cached-token counter says where it *is*. Leading with the counter
/// and falling back to the hash is the same intent measured instead of derived,
/// and it is the difference from [`Strategy::ContextRelay`] — which pins
/// unconditionally, so it keeps sending turn N to whichever provider the hash
/// named even after a failover moved the conversation elsewhere.
fn by_cached_prefix<'a>(
    candidates: &'a [Candidate],
    session: Option<&str>,
    model: &str,
    first: &'a Candidate,
) -> &'a Candidate {
    let best = candidates
        .iter()
        .map(|c| c.cached_prefix_tokens)
        .max()
        .unwrap_or(0);
    if best == 0 {
        return by_affinity(candidates, session, model, first);
    }
    candidates
        .iter()
        .filter(|c| c.cached_prefix_tokens == best)
        .min_by_key(|c| c.rank)
        .unwrap_or(first)
}

/// `context-optimized`: largest context window wins.
///
/// A descending context-limit sort, as in `sortTargetsByContextSize`. Unknown
/// windows score `0` and land at the back, and when *no* target has a known
/// window the sort is a no-op — so this needs no session key and is the right
/// strategy for a long prompt whose length the router never inspects.
fn by_context_window<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    candidates
        .iter()
        .max_by_key(|c| (c.context_window, u32::MAX - c.rank))
        .unwrap_or(first)
}

/// `fusion`: the panel leader [`pick`] reports for a strategy that does not
/// rank.
///
/// The reference runs every panel member in parallel and lets `judgeModel`
/// synthesise, so it never *selects* a winner — there is no first. What a pure
/// comparator can still express is which member the attempt loop hits first, so
/// this is the leader of [`fusion_panel`]: the whole panel when a session is
/// pinned to one provider, config order otherwise. The fan-out itself is
/// [`dispatch_fusion`], which needs an [`Executor`] and therefore cannot live
/// behind [`pick`].
///
/// The panel comes from [`viable`], the same pre-dispatch filter the reference
/// applies, so a quota-exhausted account is never the member asked first.
fn fusion_leader<'a>(
    candidates: &'a [Candidate],
    session: Option<&str>,
    model: &str,
    first: &'a Candidate,
) -> &'a Candidate {
    match session.and_then(|s| affinity_key(s, model)) {
        Some(key) => {
            affinity_leader(fusion_panel(candidates, session, model), &key).unwrap_or(first)
        }
        None => fusion_panel(candidates, None, model)
            .into_iter()
            .next()
            .unwrap_or(first),
    }
}

/// The `fusion` panel in dispatch order: the pinned leader first, then the rest
/// in config order.
///
/// One `Vec` per request, over a panel that is already a filtered subset, and
/// only the strategies that fan out or chain ever build it.
fn fusion_panel<'a>(
    candidates: &'a [Candidate],
    session: Option<&str>,
    model: &str,
) -> Vec<&'a Candidate> {
    let mut panel = viable(candidates);
    if let Some(key) = session.and_then(|s| affinity_key(s, model))
        && let Some(leader) = affinity_leader(panel.iter().copied(), &key)
    {
        panel.sort_by_key(|c| {
            if c.provider == leader.provider {
                0u8
            } else {
                1u8
            }
        });
    } else {
        panel.sort_by_key(|c| c.rank);
    }
    panel
}

/// `pipeline`: the first stage [`pick`] reports, in config order.
///
/// Strictly sequential in definition order upstream — `pipelineTargets[0]` is the
/// first step after dedup, with no scoring and no randomness. Deliberately the
/// dullest of the twenty: a pipeline whose stage order is not its config order
/// is a pipeline nobody can debug. The one filter is the same pre-dispatch
/// expansion [`fusion_leader`] applies, so an exhausted account is not the stage
/// one. The chain itself is [`dispatch_pipeline`].
fn pipeline_head<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    viable(candidates)
        .into_iter()
        .min_by_key(|c| c.rank)
        .unwrap_or(first)
}

/// One panel member's verdict in a `fusion` fan-out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelVerdict {
    /// Provider that was asked.
    pub provider: ProviderId,
    /// Status it answered with, or `None` when the transport failed and produced
    /// no verdict at all.
    pub status: Option<u16>,
    /// Whether this member won — the first 2xx in panel order.
    pub winner: bool,
}

/// The result of one `fusion` fan-out.
#[derive(Debug)]
pub struct FusionOutcome {
    /// The winning member's response, ready to relay. `None` when no panel member
    /// answered 2xx.
    pub upstream: Option<Upstream>,
    /// Every member's verdict, in panel order.
    ///
    /// The full trace, not just the winner: a fused request costs N upstream
    /// calls, and an operator looking at one that took four seconds needs to see
    /// which members were slow, which refused, and which were never reached. The
    /// trace is provider ids and status codes only — no body, no prompt.
    pub trace: Vec<PanelVerdict>,
    /// Each successful member's assistant text, in panel order, paired with the
    /// provider that produced it.
    ///
    /// Collected from the same bodies the fan-out already buffered to find the
    /// winner, so the judge's second dispatch costs no extra upstream reads.
    /// `None` for a member whose body could not be parsed as a completion —
    /// the judge sees a panel with a hole rather than an invented answer, and
    /// `extract_panel_text` drops blank entries anyway.
    pub answers: Vec<(ProviderId, String)>,
}

impl FusionOutcome {
    /// The provider whose answer this fan-out returns.
    #[must_use]
    pub fn winner(&self) -> Option<&ProviderId> {
        self.trace.iter().find(|v| v.winner).map(|v| &v.provider)
    }

    /// Status to report to the client: the winner's own, `503` when the panel was
    /// empty, `502` when every member was asked and none answered 2xx.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.upstream.as_ref().map_or_else(
            || {
                if self.trace.is_empty() {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::BAD_GATEWAY
                }
            },
            |u| u.status,
        )
    }
}

/// `fusion`: fan the request out over the whole panel and return the first 2xx.
///
/// Ported from `dispatchPrelude.tryFusionDispatch` +
/// `fusion.ts::handleFusionChat`. Every panel member is asked
/// **concurrently** — `join_all` drives them on one task, so the wall time is
/// the slowest member rather than the sum — and the first member in panel order
/// to answer 2xx wins. Panel order is [`fusion_panel`]'s, so "first" is a
/// deterministic function of the inputs rather than a race; the reference reaches
/// the same place by collecting the panel and reading it in order.
///
/// This returns one panel answer, not a synthesis — and that is the reference's
/// own default: without a configured `judgeModel`, the panel's first 2xx IS the
/// answer. When a judge is configured, [`FusionOutcome::answers`] carries every
/// member's text (read from the same bodies this fan-out already buffered) and
/// [`crate::fusion_judge::synthesize`] runs the second dispatch over them.
///
/// # Errors
/// Never. A panel that cannot be reached at all is an [`FusionOutcome`] with no
/// winner, which the caller reports with [`FusionOutcome::status`].
pub async fn dispatch_fusion<E: Executor + ?Sized>(
    session: Option<&str>,
    model: &str,
    candidates: &[Candidate],
    canonical: &CanonicalRequest,
    exec: &E,
) -> FusionOutcome {
    let mut panel = fusion_panel(candidates, session, model);
    // The reference rejects an oversized panel BEFORE fan-out (#1905): every
    // member is called in parallel and its full body buffered at once, so a
    // large panel (reported: ~73 models) can exceed the heap ceiling and OOM the
    // whole process. Truncating to the ceiling is what this build does instead
    // of a 400, and the difference is deliberate and recorded: the router here
    // has no HTTP layer to answer from, so it keeps the first
    // [`crate::fusion_judge::MAX_PANEL`] members by panel order (the ranking a
    // caller can see in the trace) and lets the rest go unasked. A caller that
    // needs the refusal should validate panel size in config.
    if panel.len() > crate::fusion_judge::MAX_PANEL {
        panel.truncate(crate::fusion_judge::MAX_PANEL);
    }
    // Every panel member is asked NON-STREAMING, whatever the client asked for:
    // the fan-out buffers each member's complete answer to read its text (for
    // the winner's relay and for the judge's panel), and an SSE body is a
    // sequence of frames this module cannot read as one answer. The reference
    // builds the same `panelBody` (`{ ...rest, stream: false }`,
    // `fusion.ts::handleFusionChat`) and keeps the client's own stream flag for
    // the *judge's* response.
    //
    // Without this a streaming client gets a panel of empty texts and a judge
    // that synthesizes a confident answer from nothing — the worst possible
    // failure for a feature whose whole purpose is grounding.
    let panel_request = CanonicalRequest {
        stream: false,
        body: non_streaming_body(&canonical.body),
        ..canonical.clone()
    };
    let settled =
        futures::future::join_all(panel.iter().map(|c| exec.call(&c.provider, &panel_request)))
            .await;

    let mut trace = Vec::with_capacity(panel.len());
    let mut answers: Vec<(ProviderId, String)> = Vec::with_capacity(panel.len());
    let mut upstream = None;
    for (candidate, result) in panel.iter().zip(settled) {
        // A successful member's body is read here, once, for two consumers: the
        // winner's bytes are relayed and every member's text feeds the judge.
        // That read is why the fan-out already buffered these bodies, and why
        // the panel ceiling exists at all (#1905).
        let (status, text) = match result {
            Ok(mut u) if u.status.is_success() => {
                let body = match read_body(u.stream).await {
                    Ok(body) => body,
                    Err(ExecError(e)) => {
                        trace.push(PanelVerdict {
                            provider: candidate.provider.clone(),
                            status: Some(u.status.as_u16()),
                            winner: false,
                        });
                        tracing::warn!(provider = %candidate.provider, error = %e, "fusion panel member body could not be read");
                        continue;
                    }
                };
                let text = crate::fusion_judge::extract_panel_text(&body);
                u.stream = Box::pin(futures::stream::iter([body]));
                (Some(u.status.as_u16()), Some((text, u)))
            }
            Ok(u) => (Some(u.status.as_u16()), None),
            Err(ExecError(_)) => (None, None),
        };
        let winner = upstream.is_none() && text.is_some();
        if let Some((panel_text, response)) = text {
            if !panel_text.trim().is_empty() {
                answers.push((candidate.provider.clone(), panel_text));
            }
            if winner {
                upstream = Some(response);
            }
        }
        trace.push(PanelVerdict {
            provider: candidate.provider.clone(),
            status,
            winner,
        });
    }

    FusionOutcome {
        upstream,
        trace,
        answers,
    }
}

/// The same body with `stream` forced off, for the panel's non-streaming ask.
///
/// Setting [`CanonicalRequest::stream`] alone is not enough: providers honour the
/// `stream` field *inside* the body, so a body that still says `"stream":true`
/// gets SSE frames back regardless of what the struct claims. This is the one
/// place the router rewrites a request body, and the reference does the same
/// (`panelBody: { ...rest, stream: false }`) — every other path leaves the body
/// byte-identical, which is what keeps routing unable to change the request.
///
/// A body that is not JSON is passed through unchanged: it could not be
/// dispatched as canonical anyway, and a rewrite that fails must not be a second
/// reason for the fan-out to see nothing.
fn non_streaming_body(body: &Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("stream".to_owned(), serde_json::Value::Bool(false));
    }
    serde_json::to_vec(&value).map_or_else(|_| body.clone(), Bytes::from)
}

/// Buffers one upstream body. A fusion panel member is a complete non-stream
/// answer by construction (the fan-out asks the same body for every member), so
/// this is one body read once, not a relay.
///
/// Stops at [`PANEL_BODY_BYTES`] and reports the overflow, rather than growing
/// with the provider: a member over the cap is dropped from the panel (it can
/// never win, and its text never reaches the judge), which bounds a
/// `MAX_PANEL`-wide fan-out at `MAX_PANEL * PANEL_BODY_BYTES`. The alternative —
/// reading a member to completion — is exactly the #1905 heap case the panel
/// ceiling exists to prevent, and `MAX_PANEL` alone bounds only the member
/// *count*. Truncating instead would be worse than dropping: a JSON body cut
/// mid-answer does not parse, so the member would contribute an empty answer
/// while still costing its bytes.
async fn read_body(mut stream: ChunkStream) -> Result<Bytes, ExecError> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        if out.len() + chunk.len() > PANEL_BODY_BYTES {
            return Err(ExecError(format!(
                "panel member body exceeded {PANEL_BODY_BYTES} bytes"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(out))
}

/// One stage's verdict in a `pipeline` chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageVerdict {
    /// Provider that served the stage.
    pub provider: ProviderId,
    /// Status it answered with, or `None` when the transport failed.
    pub status: Option<u16>,
    /// Why this stage did not produce a usable turn: a transport error, an
    /// empty body, or a body whose turns could not be read. `None` on a stage
    /// whose output was read.
    pub error: Option<String>,
}

/// The result of one `pipeline` chain.
#[derive(Debug)]
pub struct PipelineOutcome {
    /// The final stage's response, ready to relay. `None` when an *intermediate*
    /// stage failed, in which case the chain stopped and nothing is returned.
    pub upstream: Option<Upstream>,
    /// Per-stage verdicts, in execution order.
    pub trace: Vec<StageVerdict>,
    /// The stage that ended the chain, if any. `None` when the chain completed.
    pub failed_at: Option<usize>,
}

impl PipelineOutcome {
    /// Status to report to the client: the final stage's own, `502` when an
    /// intermediate stage ended the chain, `503` when there were no stages.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.upstream.as_ref().map_or_else(
            || {
                if self.trace.is_empty() {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::BAD_GATEWAY
                }
            },
            |u| u.status,
        )
    }
}

/// `pipeline`: run the stages in order, each one's output feeding the next.
///
/// Ported from `dispatchPrelude.tryPipelineDispatch` +
/// `pipeline.ts::handlePipelineChat`. The chain is strictly sequential — stage
/// N+1's request body carries stage N's answer as its only turn, which is what
/// makes a pipeline a pipeline and not a fallback chain — and **only the final
/// stage's response is returned**. Stage order is [`viable`] then `rank`, the
/// config order, with no scoring and no randomness.
///
/// **No fallback.** Upstream's chain has one `maxRetries` knob for transient
/// statuses; this port has none, and a failed stage ends the chain rather than
/// trying another provider. A fallback here would be a different strategy wearing
/// a pipeline's name: the next provider answers a *different question* when it
/// receives the previous stage's output as its input, so silently substituting
/// one produces an answer to neither. The failed stage is in
/// [`PipelineOutcome::trace`] and named by [`PipelineOutcome::failed_at`].
///
/// # Errors
/// Never. A chain that cannot complete is a [`PipelineOutcome`] with
/// `failed_at`, which the caller reports with [`PipelineOutcome::status`].
pub async fn dispatch_pipeline<E: Executor + ?Sized>(
    candidates: &[Candidate],
    canonical: &CanonicalRequest,
    exec: &E,
) -> PipelineOutcome {
    let stages = pipeline_stages(candidates);
    let mut trace: Vec<StageVerdict> = Vec::with_capacity(stages.len());
    let mut prior: Option<String> = None;

    for (index, stage) in stages.iter().enumerate() {
        let is_final = index + 1 == stages.len();
        let request = match &prior {
            None => canonical.clone(),
            Some(answer) => match chained_request(canonical, answer) {
                Some(request) => request,
                None => {
                    trace.push(StageVerdict {
                        provider: stage.provider.clone(),
                        status: None,
                        error: Some("request body is not a JSON object".to_owned()),
                    });
                    return PipelineOutcome {
                        upstream: None,
                        trace,
                        failed_at: Some(index),
                    };
                }
            },
        };

        let response = match exec.call(&stage.provider, &request).await {
            Ok(response) => response,
            Err(ExecError(message)) => {
                trace.push(StageVerdict {
                    provider: stage.provider.clone(),
                    status: None,
                    error: Some(message),
                });
                return PipelineOutcome {
                    upstream: None,
                    trace,
                    failed_at: Some(index),
                };
            }
        };
        let status = response.status.as_u16();

        // The last stage is the answer, whatever it said: the caller relays it and
        // decides what a non-2xx means, exactly as `handlePipelineChat` returns
        // the final `Response` inside the loop.
        if is_final {
            trace.push(StageVerdict {
                provider: stage.provider.clone(),
                status: Some(status),
                error: None,
            });
            return PipelineOutcome {
                upstream: Some(response),
                trace,
                failed_at: None,
            };
        }

        if !response.status.is_success() {
            trace.push(StageVerdict {
                provider: stage.provider.clone(),
                status: Some(status),
                error: Some(format!("stage {} returned {status}", index + 1)),
            });
            return PipelineOutcome {
                upstream: None,
                trace,
                failed_at: Some(index),
            };
        }

        // A stage that produced nothing usable ends the chain. Feeding an empty
        // turn onward would hand the next provider a question with no answer in
        // it, which is how a pipeline quietly becomes a single stage.
        match stage_answer(response).await {
            Ok(answer) => {
                trace.push(StageVerdict {
                    provider: stage.provider.clone(),
                    status: Some(status),
                    error: None,
                });
                prior = Some(answer);
            }
            Err(error) => {
                trace.push(StageVerdict {
                    provider: stage.provider.clone(),
                    status: Some(status),
                    error: Some(error),
                });
                return PipelineOutcome {
                    upstream: None,
                    trace,
                    failed_at: Some(index),
                };
            }
        }
    }

    // Unreachable: the last stage returns inside the loop. An empty chain is the
    // only way here, and it is reported as "no stages" rather than a panic.
    PipelineOutcome {
        upstream: None,
        trace,
        failed_at: None,
    }
}

/// The `pipeline` chain in execution order: [`viable`], then config `rank`.
fn pipeline_stages(candidates: &[Candidate]) -> Vec<&Candidate> {
    let mut stages = viable(candidates);
    stages.sort_by_key(|c| c.rank);
    stages
}

/// Stage N+1's request: the original body with its turns replaced by stage N's
/// answer, and streaming off.
///
/// The shape is `handlePipelineChat`'s `buildTransformBody` for the
/// `messages` dialect, which is the only dialect [`crate::contract`]'s canonical
/// body has: an object with `messages`, each a `{role, content}` turn. `None`
/// when the body is not a JSON object, which the caller reports rather than
/// sending a stage an unrewritten body — that would silently run a four-stage
/// pipeline as four independent requests.
fn chained_request(canonical: &CanonicalRequest, answer: &str) -> Option<CanonicalRequest> {
    let mut body: serde_json::Value = serde_json::from_slice(&canonical.body).ok()?;
    let serde_json::Value::Object(ref mut fields) = body else {
        return None;
    };
    fields.insert(
        "messages".to_owned(),
        serde_json::json!([{ "role": "user", "content": answer }]),
    );
    // An intermediate stage must produce complete prose for the next one, which
    // is `stripStreaming` upstream.
    fields.insert("stream".to_owned(), serde_json::Value::Bool(false));
    let body = serde_json::to_vec(&body).ok().map(Bytes::from)?;
    Some(CanonicalRequest {
        model: canonical.model.clone(),
        body,
        stream: false,
        session: canonical.session.clone(),
    })
}

/// Drains one stage's response into the text the next stage is handed.
///
/// The canonical answer is `{"message":{"content":…}}`; the raw body is the
/// fallback for a dialect this crate does not model. A body that yields no text
/// at all is an error, not an empty turn — see [`dispatch_pipeline`].
async fn stage_answer(upstream: Upstream) -> Result<String, String> {
    let mut raw = Vec::new();
    let mut stream = upstream.stream;
    while let Some(chunk) = stream.next().await {
        raw.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => value
            .pointer("/message/content")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| "stage response carried no readable turn".to_owned()),
        // A non-JSON body is a provider that answered in its own dialect; the
        // whole body is the turn.
        Err(_) if text.trim().is_empty() => Err("stage response was empty".to_owned()),
        Err(_) => Ok(text),
    }
}

// ---------------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------------

/// The index the shared cursor points at.
///
/// One `fetch_add` per request. `fetch_add` wraps on overflow (documented),
/// which is fine: the modulus keeps the index valid.
fn by_cursor<'a>(candidates: &'a [Candidate], rr: &AtomicU64) -> &'a Candidate {
    &candidates[(rr.fetch_add(1, Ordering::Relaxed) % candidates.len() as u64) as usize]
}

/// Lowest `rank`, falling back to `first` on an empty slice.
///
/// `first` is always `Some` at every call site, so the fallback is dead code
/// there; it exists so this helper never has to `unwrap`.
fn by_rank<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    candidates.iter().min_by_key(|c| c.rank).unwrap_or(first)
}

/// Cheapest known input price, unpriced last.
fn by_price<'a>(candidates: &'a [Candidate], first: &'a Candidate) -> &'a Candidate {
    // `None` sorts after `Some(_)` — an unpriced model must never win on
    // "unknown is probably free".
    candidates
        .iter()
        .min_by(|a, b| {
            a.input_usd_per_mtok
                .unwrap_or(f64::INFINITY)
                .total_cmp(&b.input_usd_per_mtok.unwrap_or(f64::INFINITY))
        })
        .unwrap_or(first)
}

/// splitmix64: one uniform 64-bit word from the shared cursor.
///
/// Hand-rolled rather than a `rand` dep for two reasons that both matter here:
/// the crate has a hard dep budget (`docs/03`), and a seeded test needs the
/// sequence to be a pure function of the seed, which a general-purpose crate
/// does not promise across versions.
fn splitmix(rr: &AtomicU64) -> u64 {
    let mut z = rr
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The prefix-pin key for one session and one model, via the shared `ar-cache`
/// hook.
///
/// No message list is passed: this crate never inspects the request body, by
/// design (`CanonicalRequest::body` is opaque to the router). The session key is
/// the identity of the conversation, which is all the hook needs — and the
/// reference's explicit-key path, which skips prefix derivation entirely.
///
/// `model` is the second component of the digest, so the same conversation asked
/// for two models resolves to two independent leaders. [`pick`] passes
/// [`MODEL_SCOPE`] because it never sees a model; [`pick_for_model`] and
/// [`pick_filtered`] pass the real one.
fn affinity_key(session: &str, model: &str) -> Option<AffinityKey> {
    ar_cache::affinity::resolve_key(session, model, &[], Some(session))
}

/// A candidate as an `ar-cache` affinity target.
fn affinity_target(c: &Candidate) -> AffinityTarget {
    AffinityTarget {
        // This crate routes by provider, not by connection, and the hook falls
        // back to the execution key when the connection is unknown — which is
        // what the reference does when connection granularity is unavailable.
        connection_id: None,
        execution_key: format!("{}/{}", c.provider.as_str(), c.model),
        oauth: false,
        availability: 1.0,
    }
}

/// The highest-affinity candidate, or `None` for an empty iterator.
///
/// A fold rather than a `zip` over a parallel target vector so it serves both
/// the whole candidate list and [`viable`]'s filtered subset. Strict `>` on the
/// score keeps the *earlier* of equal maxima, which is the reference's original
/// index tiebreak.
fn affinity_leader<'a, I: IntoIterator<Item = &'a Candidate>>(
    candidates: I,
    key: &AffinityKey,
) -> Option<&'a Candidate> {
    let mut best: Option<(&'a Candidate, f64)> = None;
    for c in candidates {
        let score = ar_cache::affinity::score(key, &affinity_target(c));
        if best.is_none_or(|(_, seen)| score > seen) {
            best = Some((c, score));
        }
    }
    best.map(|(c, _)| c)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    use bytes::Bytes;
    use futures::StreamExt;
    use http::StatusCode;

    use super::{
        EXPIRY_MAX_SCORE, Factors, MODEL_SCOPE, PANEL_BODY_BYTES, Strategy, TargetLoad,
        TargetLoads, affinity_key, dispatch_fusion, dispatch_pipeline, expiry_first_score,
        hours_until_reset, pick, pick_filtered, pick_for_model, reset_aware_score, splitmix,
    };
    use crate::contract::{
        Candidate, CanonicalRequest, ExecError, Executor, ProviderId, QuotaWindow, Upstream,
    };
    use crate::error::RouteError;
    use crate::fusion_judge::MAX_PANEL;

    fn cands() -> Vec<Candidate> {
        vec![
            Candidate::new("openai".into(), "gpt-4o")
                .with_price(2.50)
                .with_rank(0),
            Candidate::new("groq".into(), "llama-3.3-70b")
                .with_price(0.59)
                .with_rank(1),
            Candidate::new("together".into(), "mixtral")
                .with_price(0.20)
                .with_rank(2),
        ]
    }

    /// Unix seconds now, for windows built relative to the current instant.
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs()
    }

    /// The provider id a strategy picks, by name.
    fn winner(strategy: &str, candidates: &[Candidate]) -> ProviderId {
        pick(
            Strategy::parse(strategy),
            None,
            candidates,
            &AtomicU64::new(0),
            None,
        )
        .unwrap_or_else(|e| panic!("{strategy} picked nothing: {e:?}"))
    }

    fn p(id: &str) -> ProviderId {
        ProviderId::new(id)
    }

    /// One-arg pick for the strategies that read no session and no pins.
    fn routed(strategy: Strategy, candidates: &[Candidate]) -> String {
        pick(strategy, None, candidates, &AtomicU64::new(0), None)
            .expect("candidates present")
            .as_str()
            .to_owned()
    }

    fn routed_seeded(strategy: Strategy, candidates: &[Candidate], seed: u64) -> String {
        pick(strategy, None, candidates, &AtomicU64::new(seed), None)
            .expect("candidates present")
            .as_str()
            .to_owned()
    }

    fn routed_session(strategy: Strategy, candidates: &[Candidate], session: &str) -> String {
        pick(
            strategy,
            Some(session),
            candidates,
            &AtomicU64::new(0),
            None,
        )
        .expect("candidates present")
        .as_str()
        .to_owned()
    }

    fn quota(limit: u64, used: u64, reset_at_secs: u64) -> QuotaWindow {
        QuotaWindow::new(limit, used, reset_at_secs)
    }

    // ---- the mock executor the panel-shaped dispatch is tested against ----

    /// A canonical chat request with `n` user turns, which is the body shape
    /// [`chained_request`] rewrites.
    fn request(turns: &[&str]) -> CanonicalRequest {
        let messages: Vec<serde_json::Value> = turns
            .iter()
            .map(|t| serde_json::json!({ "role": "user", "content": t }))
            .collect();
        let body = serde_json::json!({ "model": "m", "messages": messages, "stream": false });
        CanonicalRequest::new(
            "m",
            Bytes::from(serde_json::to_vec(&body).expect("body builds")),
        )
    }

    /// Records every call and answers a fixed script: 2xx with
    /// `stage-N` text, then whatever the script says after that.
    struct Recorder {
        script: Mutex<Vec<(StatusCode, String)>>,
        seen: Mutex<Vec<(String, Bytes)>>,
    }

    impl Recorder {
        fn ok(stages: usize) -> Self {
            Self {
                script: Mutex::new(
                    (0..stages)
                        .map(|n| (StatusCode::OK, format!("stage-{n} output")))
                        .collect(),
                ),
                seen: Mutex::new(Vec::new()),
            }
        }

        /// Answers `status` to the first call, 2xx to the rest.
        fn first_fails(status: StatusCode, stages: usize) -> Self {
            let mut script = vec![(status, "refused".to_owned())];
            script.extend((1..stages).map(|n| (StatusCode::OK, format!("stage-{n} output"))));
            Self {
                script: Mutex::new(script),
                seen: Mutex::new(Vec::new()),
            }
        }

        /// Answers `status` to every call.
        fn all_fail(status: StatusCode) -> Self {
            Self {
                script: Mutex::new(vec![(status, "refused".to_owned())]),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Bytes)> {
            self.seen.lock().map(|s| s.clone()).unwrap_or_default()
        }
    }

    impl Executor for Recorder {
        fn call<'a>(
            &'a self,
            provider: &'a ProviderId,
            canonical: &'a CanonicalRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Upstream, ExecError>> + Send + 'a>,
        > {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push((provider.as_str().to_owned(), canonical.body.clone()));
            }
            let index = self.calls().len() - 1;
            // Past the end of the script, hold the last verdict: a one-entry
            // script is then "every call gets this", which is how a panel-wide
            // outage is expressed without a per-call vector.
            let next = self
                .script
                .lock()
                .ok()
                .and_then(|s| s.get(index).or_else(|| s.last()).cloned())
                .unwrap_or((StatusCode::OK, String::new()));
            Box::pin(async move {
                let payload = Bytes::from(
                    serde_json::to_vec(&serde_json::json!({
                        "message": { "role": "assistant", "content": next.1 }
                    }))
                    .expect("body builds"),
                );
                Ok(if next.0.is_success() {
                    Upstream::success(Box::pin(futures::stream::iter([payload])))
                } else {
                    Upstream::failure(next.0, payload, None)
                })
            })
        }
    }

    /// `futures::executor::block_on`: the crate has no async runtime dependency
    /// and these futures never park on a real socket.
    fn block<F: std::future::Future>(f: F) -> F::Output {
        futures::executor::block_on(f)
    }

    /// Answers one member with a body of `bytes` real payload, so the reader's
    /// ceiling has something to overrun. Not part of [`Recorder`] because only
    /// the cap tests want a body that is not a completion JSON.
    struct Oversize(StatusCode, usize);

    impl Executor for Oversize {
        fn call<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _canonical: &'a CanonicalRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Upstream, ExecError>> + Send + 'a>,
        > {
            let (status, bytes) = (self.0, self.1);
            Box::pin(async move {
                // `b` is not JSON, which is the point: this member must be
                // dropped for its SIZE, not for failing to parse, so a test
                // that passed on a parse error would not be testing the cap.
                let payload = Bytes::from(vec![b'b'; bytes]);
                Ok(answer(status, payload))
            })
        }
    }

    /// Answers with `body` verbatim, for a boundary test whose payload has to be
    /// readable and not merely large: a filler body would make the assertion pass
    /// whether the reader kept the bytes or dropped them.
    struct Body(StatusCode, Bytes);

    impl Executor for Body {
        fn call<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _canonical: &'a CanonicalRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Upstream, ExecError>> + Send + 'a>,
        > {
            let (status, payload) = (self.0, self.1.clone());
            Box::pin(async move { Ok(answer(status, payload)) })
        }
    }

    /// A 2xx body is one frame of the stream; a refusal is a single error body.
    fn answer(status: StatusCode, payload: Bytes) -> Upstream {
        if status.is_success() {
            Upstream::success(Box::pin(futures::stream::iter([payload])))
        } else {
            Upstream::failure(status, payload, None)
        }
    }

    /// The turn texts the recorded requests actually carried, in call order.
    fn request_turns(recorder: &Recorder) -> Vec<Vec<String>> {
        recorder
            .calls()
            .into_iter()
            .map(|(_, body)| {
                let value: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                value
                    .get("messages")
                    .and_then(serde_json::Value::as_array)
                    .map(|turns| {
                        turns
                            .iter()
                            .filter_map(|t| {
                                t.get("content")
                                    .and_then(serde_json::Value::as_str)
                                    .map(ToOwned::to_owned)
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect()
    }

    // ---- pre-existing lean-routing proofs ----

    #[test]
    fn picks_cheapest_when_cost_optimized() {
        assert_eq!(routed(Strategy::CostOptimized, &cands()), "together");
    }

    #[test]
    fn skips_unpriced_candidate_when_cost_optimized() {
        let mut c = cands();
        c.insert(0, Candidate::new("mystery".into(), "unpriced"));
        assert_eq!(routed(Strategy::CostOptimized, &c), "together");
    }

    #[test]
    fn picks_lowest_rank_when_priority() {
        assert_eq!(routed(Strategy::Priority, &cands()), "openai");
    }

    #[test]
    fn cycles_when_round_robin() {
        let rr = AtomicU64::new(0);
        let c = cands();
        let seen: Vec<String> = (0..6)
            .map(|_| {
                pick(Strategy::RoundRobin, None, &c, &rr, None)
                    .expect("candidates present")
                    .as_str()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            seen,
            ["openai", "groq", "together", "openai", "groq", "together"]
        );
    }

    #[test]
    fn errors_when_no_candidates() {
        let got = pick(Strategy::Priority, None, &[], &AtomicU64::new(0), None);
        assert!(matches!(got, Err(RouteError::NoCandidates)));
    }

    // ---- deferred surface ----

    #[test]
    fn errors_when_strategy_deferred() {
        let got = pick(
            Strategy::parse("auto/coding"),
            None,
            &cands(),
            &AtomicU64::new(0),
            None,
        );
        assert!(matches!(
            got,
            Err(RouteError::DeferredStrategy(Strategy::Deferred("auto")))
        ));
    }

    #[test]
    fn parses_unknown_name_as_deferred() {
        assert_eq!(
            Strategy::parse("rotund-robin"),
            Strategy::Deferred("unknown")
        );
    }

    #[test]
    fn reaches_quota_share_under_both_spellings() {
        // `quota-share` is the reference's internal spelling (auto-minted `qtSd/`
        // combos); `quota-share-fair` is the visible one. Both must dispatch.
        assert_eq!(Strategy::parse("quota-share"), Strategy::QuotaShareFair);
        assert_eq!(
            Strategy::parse("quota-share-fair"),
            Strategy::QuotaShareFair
        );
    }

    /// The reference's `normalizeRoutingStrategy` aliases resolve to the variant
    /// they stand for, not to `Deferred`. A combo ported from an OmniRoute config
    /// spells its strategy the way that config spells it, and a strategy this
    /// build *does* ship must not 501 because the alias was missed.
    #[test]
    fn parses_the_reference_aliases_to_their_own_strategy() {
        // (routingStrategies.ts:71-73)
        for (alias, canonical) in [
            ("usage", "least-used"),
            ("context", "context-optimized"),
            ("weekly-reset", "reset-window"),
            ("reset-window-order", "reset-window"),
        ] {
            // Equality with the canonical spelling is the whole proof: the
            // canonical names are all routable, so an alias equal to one cannot
            // be deferred.
            assert_eq!(
                Strategy::parse(alias),
                Strategy::parse(canonical),
                "{alias} must resolve to {canonical}"
            );
        }
    }

    #[test]
    fn expiry_first_spends_the_window_that_rolls_sooner() {
        // The reference's own claim, made executable: two windows holding the
        // same share, one closing today and one in three days — the one about to
        // waste its quota must win, or the leftover is simply lost.
        let now = now_secs();
        let cands = [
            Candidate::new(p("a"), "m").with_quota(QuotaWindow::new(100, 50, now + 6 * 3600)),
            Candidate::new(p("b"), "m").with_quota(QuotaWindow::new(100, 50, now + 72 * 3600)),
        ];
        assert_eq!(winner("expiry-first", &cands).as_str(), "a");
    }

    #[test]
    fn expiry_first_prefers_more_usable_quota_at_the_same_deadline() {
        let now = now_secs();
        let cands = [
            Candidate::new(p("a"), "m").with_quota(QuotaWindow::new(100, 90, now + 3600)),
            Candidate::new(p("b"), "m").with_quota(QuotaWindow::new(100, 10, now + 3600)),
        ];
        assert_eq!(winner("expiry-first", &cands).as_str(), "b");
    }

    #[test]
    fn an_exhausted_window_never_outranks_a_spendable_one() {
        // The guard that keeps the urgency half finite: an exhausted pool has
        // zero to spend, so no deadline can make it a better first choice. This
        // also pins that the clamp keeps `total_cmp` a total order.
        let now = now_secs();
        let cands = [
            Candidate::new(p("a"), "m").with_quota(QuotaWindow::new(100, 100, now + 60)),
            Candidate::new(p("b"), "m").with_quota(QuotaWindow::new(
                100,
                20,
                now + 3600 * 24 * 365,
            )),
        ];
        assert_eq!(winner("expiry-first", &cands).as_str(), "b");
        let ranked = [expiry_first_score(&cands[0]), expiry_first_score(&cands[1])];
        assert!(
            ranked[0] == f64::NEG_INFINITY,
            "an exhausted window scores -inf by contract: {ranked:?}"
        );
        assert!(
            ranked[1].is_finite(),
            "the urgency clamp must keep a long-deadline score finite: {ranked:?}"
        );
    }

    #[test]
    fn expiry_first_ranks_on_leftover_when_no_window_reports_a_reset() {
        // No deadline means the urgency half is unknowable; leftover is the
        // honest fallback rather than an invented deadline.
        let cands = [
            Candidate::new(p("a"), "m").with_quota(QuotaWindow::new(100, 80, 0)),
            Candidate::new(p("b"), "m").with_quota(QuotaWindow::new(100, 20, 0)),
        ];
        assert_eq!(winner("expiry-first", &cands).as_str(), "b");
    }

    #[test]
    fn expiry_first_spends_a_just_rolled_window_first() {
        // A snapshot whose reset instant has passed describes a window that has
        // just refreshed: it is full, and it is the most perishable quota in the
        // pool. Bounded, not `inf` — see `expiry_first_score`.
        let now = now_secs();
        let cands = [
            Candidate::new(p("a"), "m").with_quota(QuotaWindow::new(100, 50, now - 3600)),
            Candidate::new(p("b"), "m").with_quota(QuotaWindow::new(100, 50, now + 30 * 3600)),
        ];
        assert_eq!(winner("expiry-first", &cands).as_str(), "a");
        assert!(
            expiry_first_score(&cands[0]) <= EXPIRY_MAX_SCORE,
            "a past reset must clamp, not diverge"
        );
    }

    #[test]
    fn expiry_first_scores_in_hours_so_the_floor_means_hours() {
        // The unit assertion, and the reason it exists: every other test here
        // compares ORDER, and order is invariant to a missing 3600x. This one
        // reads the magnitude — 80% of a window closing in one hour scores 0.8
        // per hour — so the constants are pinned by a measurement rather than
        // by prose, and a dropped divisor fails here instead of silently
        // pushing every real deadline into the clamp.
        let now = now_secs();
        let hour_away =
            Candidate::new(p("a"), "m").with_quota(QuotaWindow::new(100, 20, now + 3600));
        let score = expiry_first_score(&hour_away);
        assert!(
            (score - 0.8).abs() < 0.01,
            "80% usable one hour out must score ~0.8/h, got {score}"
        );

        // The two boundary readings the unit decides.
        assert_eq!(
            hours_until_reset(0),
            None,
            "no reset reported is no deadline"
        );
        let rolled = Candidate::new(p("a"), "m").with_quota(QuotaWindow::new(100, 20, now));
        assert!(
            expiry_first_score(&rolled) > score,
            "a just-rolled window must outrank an hour of runway"
        );
        assert!(
            expiry_first_score(&rolled) <= EXPIRY_MAX_SCORE,
            "the 15-minute floor must bound the urgency, not the 4.0 clamp"
        );
    }

    #[test]
    fn expiry_first_ignores_a_candidate_with_no_quota_record() {
        // Nothing is known to be wasted there, so it never leads — but it is
        // still routable when it is the only candidate.
        let now = now_secs();
        let cands = [
            Candidate::new(p("a"), "m").with_quota(QuotaWindow::new(100, 50, now + 3600)),
            Candidate::new(p("b"), "m"),
        ];
        assert_eq!(winner("expiry-first", &cands).as_str(), "a");
        assert_eq!(
            pick(
                Strategy::parse("expiry-first"),
                None,
                &[Candidate::new(p("solo"), "m")],
                &AtomicU64::new(0),
                None,
            )
            .expect("a single unmonitored candidate still routes"),
            p("solo")
        );
    }

    #[test]
    fn reaches_quota_weighted_by_name() {
        assert_eq!(Strategy::parse("quota-weighted"), Strategy::QuotaWeighted);
    }

    #[test]
    fn every_live_strategy_name_round_trips() {
        for s in Strategy::all()
            .iter()
            .filter(|s| s.deferred_name().is_none())
        {
            assert_eq!(Strategy::parse(s.as_str()), *s);
        }
    }

    // ---- load-shaped ----

    #[test]
    fn favours_the_heavier_candidate_when_weighted() {
        // 99:1 over 200 seeded draws. Seeded, so this is a fixed computation
        // rather than a coin flip.
        let c = vec![
            Candidate::new("heavy".into(), "m")
                .with_weight(99)
                .with_rank(1),
            Candidate::new("light".into(), "m")
                .with_weight(1)
                .with_rank(0),
        ];
        let heavy = (0..200)
            .filter(|seed| routed_seeded(Strategy::Weighted, &c, *seed) == "heavy")
            .count();
        assert!(heavy > 190, "heavy won only {heavy}/200");
    }

    #[test]
    fn ignores_every_signal_when_fill_first() {
        // The reference is an identity comparator: the attempt loop drains the
        // head. A cheaper, healthier, higher-weight target must not move it.
        let c = vec![
            Candidate::new("head".into(), "m")
                .with_price(9.0)
                .with_rank(0),
            Candidate::new("cheap".into(), "m")
                .with_price(0.01)
                .with_weight(99)
                .with_quota(quota(1_000_000, 0, 1))
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::FillFirst, &c), "head");
    }

    #[test]
    fn takes_the_quieter_of_two_draws_when_p2c() {
        // The idle provider wins the comparison in every seeded pair, so this
        // pins the rule rather than the draw. With nothing observed both draws
        // score the neutral pair, so the load term decides — the behaviour this
        // function had before the ledger existed.
        let c = vec![
            Candidate::new("busy".into(), "m")
                .with_in_flight(9)
                .with_rank(0),
            Candidate::new("idle".into(), "m")
                .with_in_flight(0)
                .with_rank(1),
        ];
        let idle = (0..200)
            .filter(|seed| routed_seeded(Strategy::P2c, &c, *seed) == "idle")
            .count();
        assert_eq!(idle, 200);
    }

    /// A factor set with every field healthy, so a test can override only the
    /// two `p2c` reads. A local copy of `scoring`'s own test helper because
    /// `Factors` deliberately has no `Default`: an all-zero factor set is not a
    /// neutral one, and a `Default` would invite exactly that reading.
    fn healthy(reliability: f64, latency_inv: f64) -> Factors {
        Factors {
            quota: 1.0,
            health: 1.0,
            cost_inv: 1.0,
            latency_inv,
            task_fit: 1.0,
            stability: 1.0,
            tier_priority: 1.0,
            tier_affinity: 0.5,
            specificity_match: 0.5,
            context_affinity: 0.5,
            cache_affinity: 0.0,
            session_availability: 1.0,
            reset_window_affinity: 0.5,
            connection_density: 0.0,
            quality: 0.5,
            reliability,
        }
    }

    #[test]
    fn prefers_the_healthy_fast_draw_when_p2c() {
        // The reference's own claim for `getP2CTargetScore`: a target that
        // answers reliably and quickly beats a quieter one that does not, in
        // every seeded pair, so the assertion is about the score and not the
        // draw. The healthy one is the *busier* of the two — the case a
        // load-only comparison gets backwards.
        let c = vec![
            Candidate::new("p2c-healthy".into(), "m")
                .with_in_flight(4)
                .with_rank(0),
            Candidate::new("p2c-unknown".into(), "m")
                .with_in_flight(0)
                .with_rank(1),
        ];
        TargetLoads::global().observe(&c[0], &healthy(1.0, 1.0));
        let wins = (0..200)
            .filter(|seed| routed_seeded(Strategy::P2c, &c, *seed) == "p2c-healthy")
            .count();
        assert_eq!(wins, 200, "health must outrank the load term");
    }

    #[test]
    fn reads_an_unobserved_target_above_one_observed_failing_when_p2c() {
        // "Not measured" is not "measured and broken". The reference's own
        // fallbacks are 0.5 and 0.25 for exactly this reason, and a scorer that
        // zeroed them would route away from every provider it had not watched.
        let c = vec![
            Candidate::new("p2c-broken".into(), "m")
                .with_in_flight(0)
                .with_rank(0),
            Candidate::new("p2c-silent".into(), "m")
                .with_in_flight(0)
                .with_rank(1),
        ];
        TargetLoads::global().observe(&c[0], &healthy(0.0, 0.0));
        let wins = (0..200)
            .filter(|seed| routed_seeded(Strategy::P2c, &c, *seed) == "p2c-silent")
            .count();
        assert_eq!(wins, 200);
    }

    #[test]
    fn p2c_scoring_is_deterministic_for_a_fixed_draw() {
        // No clock, no RNG beyond the draw itself: the same target and the same
        // in-flight count always score the same, which is what lets the two
        // tests above assert over 200 seeds instead of one.
        let load = TargetLoad {
            served: 0,
            reliability: Some(0.8),
            latency_inv: Some(0.6),
        };
        assert_eq!(load.p2c_score(3), load.p2c_score(3));
        assert!(
            load.p2c_score(0) > load.p2c_score(9),
            "more load, lower score"
        );
        let failing = TargetLoad {
            served: 0,
            reliability: Some(0.0),
            latency_inv: Some(0.0),
        };
        assert!(
            TargetLoad::unknown().p2c_score(0) > failing.p2c_score(0),
            "an unobserved target scores its neutral, not zero"
        );
    }

    #[test]
    fn returns_the_only_candidate_when_p2c_cannot_draw_two() {
        let c = vec![Candidate::new("solo".into(), "m")];
        assert_eq!(routed_seeded(Strategy::P2c, &c, 7), "solo");
    }

    #[test]
    fn picks_the_least_busy_when_least_used() {
        let c = vec![
            Candidate::new("busy".into(), "m")
                .with_in_flight(9)
                .with_rank(0),
            Candidate::new("idle".into(), "m")
                .with_in_flight(0)
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::LeastUsed, &c), "idle");
    }

    #[test]
    fn ranks_by_cumulative_served_before_in_flight_when_least_used() {
        // The #7015 shape. A long-busy account that has *drained* looks exactly
        // like a fresh one on in-flight alone, so the combo kept handing work
        // back to the account it had already spent; the cumulative count is
        // what finally moves it.
        let spent = Candidate::new("lu-spent".into(), "m").with_rank(0);
        let fresh = Candidate::new("lu-fresh".into(), "m").with_rank(1);
        let loads = TargetLoads::global();
        for _ in 0..5 {
            loads.serve(&spent);
        }
        assert_eq!(loads.load(&spent).served, 5);
        assert_eq!(loads.load(&fresh).served, 0);
        // Both are idle, so in-flight is a tie and only the served count can
        // separate them.
        assert_eq!(routed(Strategy::LeastUsed, &[spent, fresh]), "lu-fresh");
    }

    #[test]
    fn spreads_picks_across_targets_when_least_used() {
        // The counter has to actually accumulate, or the ordering above is a
        // one-shot coincidence: two idle targets, four picks, both used.
        let a = Candidate::new("lu-spread-a".into(), "m").with_rank(0);
        let b = Candidate::new("lu-spread-b".into(), "m").with_rank(1);
        let c = vec![a, b];
        let mut winners = std::collections::BTreeSet::new();
        for _ in 0..4 {
            winners.insert(routed(Strategy::LeastUsed, &c));
        }
        assert_eq!(winners.len(), 2, "both targets were served: {winners:?}");
    }

    #[test]
    fn draws_a_candidate_when_random() {
        let got = routed_seeded(Strategy::Random, &cands(), 11);
        assert!(["openai", "groq", "together"].contains(&got.as_str()));
    }

    #[test]
    fn draws_a_provider_once_when_strict_random() {
        // Two models on one provider: strict-random must never hand back the
        // same provider twice in a row, which is the whole point of the variant.
        let c = vec![
            Candidate::new("solo".into(), "model-a").with_rank(0),
            Candidate::new("solo".into(), "model-b").with_rank(1),
        ];
        assert_eq!(routed_seeded(Strategy::StrictRandom, &c, 3), "solo");
    }

    // ---- quota-shaped ----

    #[test]
    fn prefers_the_larger_free_fraction_when_headroom() {
        // 50% of 10k beats 10% of 1M: a ratio, not an absolute count.
        let c = vec![
            Candidate::new("big".into(), "m")
                .with_quota(quota(1_000_000, 900_000, 1))
                .with_rank(0),
            Candidate::new("small".into(), "m")
                .with_quota(quota(10_000, 5_000, 1))
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::Headroom, &c), "small");
    }

    #[test]
    fn ranks_an_unmonitored_pool_first_when_headroom() {
        // `computeHeadroom`'s deliberate fail-open: absent saturation is
        // `util = 0`, so no telemetry is not a constraint.
        let c = vec![
            Candidate::new("drained".into(), "m")
                .with_quota(quota(10, 10, 1))
                .with_rank(0),
            Candidate::new("unmonitored".into(), "m").with_rank(1),
        ];
        assert_eq!(routed(Strategy::Headroom, &c), "unmonitored");
    }

    #[test]
    fn prefers_the_soonest_rollover_when_reset_window() {
        let c = vec![
            Candidate::new("late".into(), "m")
                .with_quota(quota(10, 0, 9_000))
                .with_rank(0),
            Candidate::new("soon".into(), "m")
                .with_quota(quota(10, 0, 1_000))
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::ResetWindow, &c), "soon");
    }

    #[test]
    fn falls_back_when_no_candidate_knows_its_rollover() {
        // Unknown reset is `Infinity` upstream, i.e. back of the list.
        let c = vec![
            Candidate::new("a".into(), "m").with_quota(quota(10, 0, 0)),
            Candidate::new("b".into(), "m").with_quota(quota(10, 0, 0)),
        ];
        assert_eq!(routed(Strategy::ResetWindow, &c), "a");
    }

    #[test]
    fn keeps_an_unknown_window_reachable_when_reset_window() {
        // The regression, in the only shape that can observe it: when *no*
        // candidate reports a rollover, a filter-out leaves an empty list and the
        // answer silently becomes `candidates[0]`. So the unmonitored pool has to
        // be the one that wins a real comparison, which means it cannot also be
        // at index 0 — otherwise config order would be indistinguishable from the
        // fixed ranking.
        let c = vec![
            Candidate::new("reported-unknown".into(), "m")
                .with_quota(quota(10, 0, 0))
                .with_rank(1),
            Candidate::new("unmonitored".into(), "m").with_rank(0),
        ];
        assert_eq!(routed(Strategy::ResetWindow, &c), "unmonitored");
    }

    #[test]
    fn ranks_a_metered_pool_above_an_unknown_one_when_reset_window() {
        // The one candidate shape a filter-out made unreachable: a pool with no
        // quota record at all, behind a pool that does have one.
        let c = vec![
            Candidate::new("unmonitored".into(), "m").with_rank(0),
            Candidate::new("metered".into(), "m")
                .with_quota(quota(10, 0, 9_000))
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::ResetWindow, &c), "metered");
    }

    #[test]
    fn prefers_the_roomier_pool_when_quota_weighted() {
        // The reset-aware half: same in-flight, so the score decides.
        let c = vec![
            Candidate::new("tight".into(), "m")
                .with_quota(quota(1_000_000, 900_000, 1))
                .with_rank(0),
            Candidate::new("roomy".into(), "m")
                .with_quota(quota(1_000_000, 100, 1))
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::QuotaWeighted, &c), "roomy");
    }

    #[test]
    fn lends_the_roomier_pool_when_quota_weighted() {
        // The load half: identical quota, one request in flight against eight.
        let c = vec![
            Candidate::new("busy".into(), "m")
                .with_in_flight(8)
                .with_quota(quota(1_000, 0, 1))
                .with_rank(0),
            Candidate::new("idle".into(), "m")
                .with_in_flight(0)
                .with_quota(quota(1_000, 0, 1))
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::QuotaWeighted, &c), "idle");
    }

    #[test]
    fn keeps_an_unknown_quota_reachable_when_quota_weighted() {
        // The regression, in the shape that can observe it. The reference filters
        // to `remainingPercent > 0`, so an unmonitored pool is dropped and a fleet
        // of unmonitored pools resolves to an *empty* list. Unknown quota still
        // sorts last among dispatchable — but it is dispatchable, so it has to
        // beat a pool that has positively said it is empty. A single-candidate
        // pool would pass either way; the drained pool in front of it is what
        // makes the comparison real.
        let c = vec![
            Candidate::new("drained".into(), "m")
                .with_quota(quota(10, 10, 1))
                .with_rank(0),
            Candidate::new("unmonitored".into(), "m").with_rank(1),
        ];
        assert_eq!(routed(Strategy::QuotaWeighted, &c), "unmonitored");
    }

    #[test]
    fn keeps_a_whole_unmonitored_fleet_routable_when_quota_weighted() {
        // The other shape the reference's filter destroys: nothing is metered, so
        // the filter empties the list outright. A fleet with no quota reporting
        // still has to be routable, or the answer is a 503 for a config that is
        // perfectly well-formed.
        let c = vec![Candidate::new("unmonitored".into(), "m").with_rank(0)];
        assert_eq!(routed(Strategy::QuotaWeighted, &c), "unmonitored");
    }

    #[test]
    fn ranks_a_metered_pool_above_an_unknown_one_when_quota_weighted() {
        // ...and the other direction, so "last" does not mean "always last".
        let c = vec![
            Candidate::new("unmonitored".into(), "m").with_rank(0),
            Candidate::new("metered".into(), "m")
                .with_quota(quota(10, 0, 1))
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::QuotaWeighted, &c), "metered");
    }

    #[test]
    fn prefers_the_roomier_pool_when_reset_aware() {
        let c = vec![
            Candidate::new("tight".into(), "m")
                .with_quota(quota(1_000_000, 900_000, 1))
                .with_rank(0),
            Candidate::new("roomy".into(), "m")
                .with_quota(quota(1_000_000, 100, 1))
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::ResetAware, &c), "roomy");
    }

    #[test]
    fn discounts_a_pool_under_the_exhaustion_guard() {
        // 4% remaining must not outrank 6% remaining: both are about to fail.
        let almost = Candidate::new("almost".into(), "m").with_quota(quota(100, 96, 1));
        let slightly = Candidate::new("slightly".into(), "m").with_quota(quota(100, 94, 1));
        assert!(reset_aware_score(&slightly) > reset_aware_score(&almost));
    }

    #[test]
    fn scores_an_exhausted_pool_as_impossible_when_reset_aware() {
        let c = Candidate::new("gone".into(), "m").with_quota(quota(10, 10, 1));
        assert!(reset_aware_score(&c).is_infinite() && reset_aware_score(&c) < 0.0);
    }

    #[test]
    fn scores_an_unknown_pool_as_neutral_when_reset_aware() {
        let c = Candidate::new("unmonitored".into(), "m");
        assert!((reset_aware_score(&c) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn lends_a_bigger_pool_when_it_is_under_its_share() {
        // DRR says the 100-slot pool leads; P2C on live load agrees, because
        // 1 in flight against 100 slots beats 3 against 2.
        let c = vec![
            Candidate::new("big".into(), "m")
                .with_weight(100)
                .with_in_flight(1)
                .with_quota(quota(100, 0, 1))
                .with_rank(0),
            Candidate::new("small".into(), "m")
                .with_weight(2)
                .with_in_flight(3)
                .with_quota(quota(2, 0, 1))
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::QuotaShareFair, &c), "big");
    }

    #[test]
    fn lends_to_the_idler_when_the_share_leader_is_busy() {
        // Same DRR leader, but now it is the busy one: step 3 flips the pick.
        let c = vec![
            Candidate::new("big".into(), "m")
                .with_weight(100)
                .with_in_flight(8)
                .with_quota(quota(100, 0, 1))
                .with_rank(0),
            Candidate::new("small".into(), "m")
                .with_weight(2)
                .with_in_flight(1)
                .with_quota(quota(2, 0, 1))
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::QuotaShareFair, &c), "small");
    }

    // ---- context-shaped ----

    #[test]
    fn sends_one_conversation_to_a_single_provider_when_context_relay() {
        // The HRW leader is a pure function of (key, candidate set): a follower
        // resolves the same conversation to the same upstream.
        let c = cands();
        let mut seen: Vec<String> = (0..20)
            .map(|_| routed_session(Strategy::ContextRelay, &c, "s7"))
            .collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn falls_back_to_priority_when_context_relay_has_no_session() {
        // No conversation, no pin. "Pin to the first target" would collapse
        // every anonymous client onto one provider.
        assert_eq!(routed(Strategy::ContextRelay, &cands()), "openai");
    }

    #[test]
    fn prefers_the_widest_context_window_when_context_optimized() {
        let c = vec![
            Candidate::new("small".into(), "m")
                .with_context_window(8_000)
                .with_rank(0),
            Candidate::new("big".into(), "m")
                .with_context_window(200_000)
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::ContextOptimized, &c), "big");
    }

    #[test]
    fn sends_an_unknown_window_last_when_context_optimized() {
        let c = vec![
            Candidate::new("known".into(), "m")
                .with_context_window(8_000)
                .with_rank(0),
            Candidate::new("unknown".into(), "m").with_rank(1),
        ];
        assert_eq!(routed(Strategy::ContextOptimized, &c), "known");
    }

    #[test]
    fn prefers_the_cached_prefix_when_cache_optimized() {
        let c = vec![
            Candidate::new("cold".into(), "m")
                .with_cached_prefix(0)
                .with_rank(0),
            Candidate::new("warm".into(), "m")
                .with_cached_prefix(12_000)
                .with_rank(1),
        ];
        assert_eq!(routed(Strategy::CacheOptimized, &c), "warm");
    }

    // ---- panel-shaped ----

    #[test]
    fn moves_the_panel_leader_when_quota_runs_out() {
        // The one place `fusion` and `context-relay` differ: a pinned leader
        // with no quota left must stop being the leader.
        let c = vec![
            Candidate::new("pinned".into(), "m")
                .with_rank(0)
                .with_quota(quota(10, 10, 9_000)),
            Candidate::new("fresh".into(), "m")
                .with_rank(1)
                .with_quota(quota(10, 0, 9_000)),
        ];
        assert_eq!(routed(Strategy::Fusion, &c), "fresh");
    }

    #[test]
    fn takes_the_config_order_when_pipeline_has_no_quota() {
        assert_eq!(routed(Strategy::Pipeline, &cands()), "openai");
    }

    #[test]
    fn skips_an_exhausted_stage_when_pipeline() {
        let c = vec![
            Candidate::new("first".into(), "m")
                .with_rank(0)
                .with_quota(quota(10, 10, 1)),
            Candidate::new("second".into(), "m")
                .with_rank(1)
                .with_quota(quota(10, 0, 1)),
        ];
        assert_eq!(routed(Strategy::Pipeline, &c), "second");
    }

    #[test]
    fn asks_every_panel_member_when_fusion() {
        // A fused request costs one upstream call *per panel member*. The
        // regression is a fan-out that quietly degraded to "try the leader".
        let exec = Recorder::ok(3);
        let got = block(dispatch_fusion(
            None,
            "gpt-4o",
            &cands(),
            &request(&["hi"]),
            &exec,
        ));
        assert_eq!(exec.calls().len(), 3);
        assert_eq!(got.trace.len(), 3);
    }

    #[test]
    fn returns_the_first_2xx_when_fusion() {
        // The winner is a function of panel order, not of who answered first:
        // `together` is asked last and still cannot outrank a 2xx ahead of it.
        let exec = Recorder::ok(3);
        let got = block(dispatch_fusion(
            None,
            "gpt-4o",
            &cands(),
            &request(&["hi"]),
            &exec,
        ));
        assert_eq!(got.winner().map(ProviderId::as_str), Some("openai"));
        assert_eq!(got.status(), StatusCode::OK);
    }

    #[test]
    fn records_every_verdict_when_fusion() {
        // A full trace, not just the winner: `groq` is asked and refused, and
        // that has to be visible without re-running the request.
        let exec = Recorder::first_fails(StatusCode::TOO_MANY_REQUESTS, 3);
        let got = block(dispatch_fusion(
            None,
            "gpt-4o",
            &cands(),
            &request(&["hi"]),
            &exec,
        ));
        let refusals: Vec<Option<u16>> = got.trace.iter().map(|v| v.status).collect();
        assert_eq!(refusals, [Some(429), Some(200), Some(200)]);
        assert_eq!(
            got.trace.iter().filter(|v| v.winner).count(),
            1,
            "exactly one member may be marked the winner"
        );
    }

    #[test]
    fn caps_the_fan_out_at_the_reference_ceiling() {
        // #1905: every member is called in parallel and its body buffered, so
        // an unbounded panel is an OOM with extra steps. The reference rejects
        // with a 400; this build has no HTTP layer here and truncates to
        // `MAX_PANEL` in panel order, which is what this pins.
        let many: Vec<Candidate> = (0..50)
            .map(|i| Candidate::new(format!("p{i}").into(), "m").with_rank(i))
            .collect();
        let exec = Recorder::ok(1);
        let got = block(dispatch_fusion(
            None,
            "gpt-4o",
            &many,
            &request(&["hi"]),
            &exec,
        ));
        assert_eq!(
            got.trace.len(),
            crate::fusion_judge::MAX_PANEL,
            "fan-out must stop at the ceiling"
        );
        assert_eq!(exec.calls().len(), crate::fusion_judge::MAX_PANEL);
        assert_eq!(
            got.trace[0].provider.as_str(),
            "p0",
            "truncation keeps panel order, so the leader still leads"
        );
    }

    /// The panel ceiling bounds the member COUNT, not the bytes: a panel of
    /// members whose replies are unbounded is the same heap case one level down.
    /// An over-cap member is dropped from the panel rather than truncated — a
    /// body cut mid-answer does not parse, so truncating would keep the cost and
    /// lose the answer.
    /// The cap and the panel ceiling are independent bounds on the same buffer, so
    /// their PRODUCT is the real ceiling and only one of them being pinned proves
    /// nothing: `AUTO_VARIANTS` and `ROUTE_STRATEGIES` both drifted to numbers that
    /// were individually plausible and jointly wrong. This is the budget row of
    /// `docs/00-overview.md` — 20 heavy concurrent requests under 400MB — so the
    /// fan-out's share is asserted rather than asserted-in-prose.
    #[test]
    fn panel_bounds_fit_the_ram_budget() {
        let panel = crate::fusion_judge::MAX_PANEL * PANEL_BODY_BYTES;
        assert!(
            panel <= 32 * 1024 * 1024,
            "one fan-out buffers up to {panel} bytes ({MAX_PANEL} x {PANEL_BODY_BYTES}); \
         20 of those must fit docs/00's <400MB heavy-request row"
        );
    }

    #[test]
    fn drops_a_panel_member_whose_body_exceeds_the_cap() {
        let cands = vec![
            Candidate::new(p("small"), "m").with_rank(0),
            Candidate::new(p("huge"), "m").with_rank(1),
        ];
        // The first member answers with a real completion, well under the cap;
        // the second overruns it. Under-cap must still win the request.
        let exec = Oversize(StatusCode::OK, PANEL_BODY_BYTES + 1);
        let got = block(dispatch_fusion(
            None,
            "gpt-4o",
            &cands,
            &request(&["hi"]),
            &exec,
        ));

        assert!(
            got.upstream.is_none(),
            "a panel whose only member overruns the cap has no winner"
        );
        assert!(
            got.answers.is_empty(),
            "an over-cap member must not reach the judge's panel"
        );
        assert_eq!(
            got.trace.len(),
            2,
            "both members are asked and both are traced, so the operator sees why"
        );
        assert!(
            got.trace.iter().all(|v| !v.winner),
            "no member may be marked winner when its body could not be read"
        );
    }

    /// The cap is a ceiling, not a quota: a body exactly at it is still read and
    /// its text reaches the judge. A `>` where `<=` belonged would drop this
    /// member and the panel would carry no text at all — which is the assertion
    /// below, and why the payload is a real completion body, not filler: filler
    /// makes it pass whether the bytes were kept or dropped.
    #[test]
    fn reads_a_panel_member_whose_body_is_exactly_at_the_cap() {
        let wrap = |pad: usize| {
            serde_json::to_vec(&serde_json::json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "x".repeat(pad) }
                }]
            }))
            .expect("body builds")
        };
        let overhead = wrap(0).len();
        let body = Bytes::from(wrap(PANEL_BODY_BYTES - overhead));
        assert_eq!(
            body.len(),
            PANEL_BODY_BYTES,
            "the fixture must sit EXACTLY at the cap, since that is the boundary under test"
        );

        let exec = Body(StatusCode::OK, body);
        let got = block(dispatch_fusion(
            None,
            "gpt-4o",
            &[Candidate::new(p("edge"), "m").with_rank(0)],
            &request(&["hi"]),
            &exec,
        ));

        assert_eq!(
            got.answers.len(),
            1,
            "a body of exactly the cap is read and its text reaches the judge"
        );
        assert!(
            got.upstream.is_some(),
            "so the member also wins the request"
        );
    }

    #[test]
    fn reports_bad_gateway_when_no_panel_member_answers_when_fusion() {
        let exec = Recorder::all_fail(StatusCode::BAD_GATEWAY);
        let got = block(dispatch_fusion(
            None,
            "gpt-4o",
            &cands(),
            &request(&["hi"]),
            &exec,
        ));
        assert!(got.upstream.is_none() && got.winner().is_none());
        assert_eq!(got.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn runs_every_stage_when_pipeline() {
        // A chain that answers the client is N upstream calls, not one.
        let exec = Recorder::ok(3);
        let got = block(dispatch_pipeline(&cands(), &request(&["hi"]), &exec));
        assert_eq!(exec.calls().len(), 3);
        assert_eq!(got.failed_at, None);
        assert_eq!(got.trace.len(), 3);
    }

    #[test]
    fn feeds_each_stage_output_into_the_next_when_pipeline() {
        // The whole difference between a pipeline and a fallback chain: stage 2's
        // request carries stage 1's *answer*, not the client's original body.
        let exec = Recorder::ok(3);
        block(dispatch_pipeline(&cands(), &request(&["hi"]), &exec));
        let turns = request_turns(&exec);
        assert_eq!(turns[0], ["hi"]);
        assert_eq!(turns[1], ["stage-0 output"]);
        assert_eq!(turns[2], ["stage-1 output"]);
    }

    #[test]
    fn returns_only_the_final_stage_when_pipeline() {
        // Stage 0 said "stage-0 output"; the answer the client gets is stage 2's.
        let exec = Recorder::ok(3);
        let got = block(dispatch_pipeline(&cands(), &request(&["hi"]), &exec));
        let Some(upstream) = got.upstream else {
            panic!("a completed chain returns its last stage");
        };
        let mut body = Vec::new();
        let mut stream = upstream.stream;
        futures::executor::block_on(async {
            while let Some(chunk) = stream.next().await {
                body.extend_from_slice(&chunk);
            }
        });
        let value: serde_json::Value =
            serde_json::from_slice(&body).expect("canonical response body");
        assert_eq!(value["message"]["content"], "stage-2 output");
    }

    #[test]
    fn ends_the_chain_when_an_intermediate_stage_refuses_when_pipeline() {
        // No fallback: a failed stage stops the chain instead of quietly asking a
        // different provider a question the previous stage's answer set up.
        let exec = Recorder::first_fails(StatusCode::INTERNAL_SERVER_ERROR, 3);
        let got = block(dispatch_pipeline(&cands(), &request(&["hi"]), &exec));
        assert_eq!(exec.calls().len(), 1);
        assert_eq!(got.failed_at, Some(0));
        assert_eq!(got.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn runs_the_stages_in_config_order_when_pipeline() {
        // A pipeline whose stage order is not its config order is a pipeline
        // nobody can debug, so the chain follows `rank` and nothing else.
        let exec = Recorder::ok(3);
        block(dispatch_pipeline(&cands(), &request(&["hi"]), &exec));
        let providers: Vec<String> = exec.calls().iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(providers, ["openai", "groq", "together"]);
    }

    // ---- the shared pieces ----

    #[test]
    fn splitmix_is_deterministic_for_a_fixed_seed() {
        let a = splitmix(&AtomicU64::new(42));
        assert_eq!(a, splitmix(&AtomicU64::new(42)));
    }

    #[test]
    fn derives_an_affinity_key_from_the_session() {
        assert!(affinity_key("s1", "gpt-4o").is_some());
    }

    #[test]
    fn scopes_the_affinity_key_with_a_model_slot() {
        // The key is opaque, so the testable claim is that the slot is wired in
        // at all: an empty scope would let two models share a pin by accident.
        assert_ne!(MODEL_SCOPE, "");
    }

    #[test]
    fn derives_a_different_affinity_key_per_model() {
        // Same conversation, two models: the model is a digest component, so the
        // two keys are independent and the pins cannot collide.
        let a = affinity_key("s1", "gpt-4o");
        let b = affinity_key("s1", "mixtral");
        assert_ne!(a.map(|k| k.key), b.map(|k| k.key));
    }

    #[test]
    fn routes_two_models_of_one_session_to_different_providers() {
        // The user-visible consequence of the digest component. HRW could agree by
        // chance on a single session, so this sweeps 64 sessions over four
        // candidates: agreement on all of them is the failure the scoping fixes.
        let c = cands();
        let diverged = (0..64).any(|n| {
            let session = format!("s{n}");
            let a = pick_for_model(
                Strategy::ContextRelay,
                Some(&session),
                "gpt-4o",
                &c,
                &AtomicU64::new(0),
                None,
            )
            .expect("candidates present");
            let b = pick_for_model(
                Strategy::ContextRelay,
                Some(&session),
                "mixtral",
                &c,
                &AtomicU64::new(0),
                None,
            )
            .expect("candidates present");
            a != b
        });
        assert!(diverged, "every session pinned both models to one provider");
    }

    #[test]
    fn skips_a_cooling_candidate_when_filtered() {
        // The point of the gate: `priority` would hand back `openai`, which the
        // attempt loop would then skip without spending an attempt.
        let c = cands();
        let got = pick_filtered(
            Strategy::Priority,
            None,
            "gpt-4o",
            &c,
            &AtomicU64::new(0),
            None,
            &|candidate: &Candidate| candidate.provider.as_str() == "openai",
        )
        .expect("candidates present");
        assert_eq!(got.as_str(), "groq");
    }

    #[test]
    fn picks_the_head_when_nothing_is_cooling() {
        // The gate is off by default; a predicate that rejects nothing must not
        // change a single decision.
        let c = cands();
        let got = pick_filtered(
            Strategy::Priority,
            None,
            "gpt-4o",
            &c,
            &AtomicU64::new(0),
            None,
            &|_| false,
        )
        .expect("candidates present");
        assert_eq!(got.as_str(), "openai");
    }

    #[test]
    fn errors_when_every_candidate_is_cooling() {
        // Filtering everything out is "no candidates", not "try the head anyway".
        let got = pick_filtered(
            Strategy::Priority,
            None,
            "gpt-4o",
            &cands(),
            &AtomicU64::new(0),
            None,
            &|_| true,
        );
        assert!(matches!(got, Err(RouteError::NoCandidates)));
    }

    #[test]
    fn does_not_reorder_a_single_candidate() {
        let c = vec![Candidate::new("solo".into(), "m")];
        assert_eq!(routed_session(Strategy::ContextRelay, &c, "s1"), "solo");
    }

    #[test]
    fn falls_back_to_priority_when_lkgp_pin_names_a_departed_provider() {
        // A pin to a departed provider is a config change, not a 500.
        let pins = crate::lkgp::LkgpPins::new();
        pins.record("s1", &ProviderId::new("departed"));
        let got = pick(
            Strategy::Lkgp,
            Some("s1"),
            &cands(),
            &AtomicU64::new(0),
            Some(&pins),
        )
        .expect("candidates present");
        assert_eq!(got.as_str(), "openai");
    }
}
