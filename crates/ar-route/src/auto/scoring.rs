//! The 16-factor auto-combo scorer.
//!
//! Ported from `../OmniRoute/open-sse/services/autoCombo/scoring.ts`. The
//! factor set, the default weights and the per-variant weight packs are
//! transcribed as-is; the one deliberate deviation is the O(n²) fix the
//! upstream file documents but this port makes structural: the pool maxima are
//! computed once by [`pool_maxima`] and passed in, so scoring a pool is
//! linear in pool size. Upstream threads the same `precomputedMaxima` argument
//! for exactly this reason (the OOM incident in its comment); there is no
//! path here that recomputes them per candidate.
//!
//! Every factor is contractually `[0, 1]`. [`clamp01`] guards the whole
//! surface, so bad telemetry (a negative quota, a `NaN` price, an
//! out-of-range affinity) cannot produce a factor that distorts the ranking.

use crate::contract::{Candidate, ProviderId, Strng};

/// Clamps to `[0, 1]`, mapping non-finite to `0`.
///
/// The `NaN` arm is not defensive noise: `f64::clamp` propagates `NaN`, and a
/// `NaN` score sorts nondeterministically. Upstream's `clamp01` does the same
/// thing for the same reason.
fn clamp01(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Provider circuit-breaker state, as reported by the caller's telemetry.
///
/// This crate owns no breaker — `crate::Resilience` is backoff only, and a
/// cooling key is skipped by the attempt loop. An [`CircuitState::Open`] here
/// is a *fact about the upstream pool*, supplied by whoever owns the breaker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CircuitState {
    /// Healthy.
    #[default]
    Closed,
    /// Recovering; probing allowed.
    HalfOpen,
    /// Tripped.
    Open,
}

impl CircuitState {
    /// Canonical config spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "CLOSED",
            Self::HalfOpen => "HALF_OPEN",
            Self::Open => "OPEN",
        }
    }

    /// Parses the telemetry spelling, case-insensitively. Unknown → [`Self::Closed`].
    #[must_use]
    pub fn parse(name: &str) -> Self {
        match name.to_ascii_uppercase().as_str() {
            "HALF_OPEN" | "HALF-OPEN" | "HALFOPEN" => Self::HalfOpen,
            "OPEN" => Self::Open,
            _ => Self::Closed,
        }
    }

    /// `CLOSED` 1.0, `HALF_OPEN` 0.5, `OPEN` 0.0.
    #[must_use]
    pub fn health(self) -> f64 {
        match self {
            Self::Closed => 1.0,
            Self::HalfOpen => 0.5,
            Self::Open => 0.0,
        }
    }
}

/// Billing tier of the account behind a candidate.
///
/// Priority order is `ultra > pro > standard > free`, upstream's `T10` table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccountTier {
    /// Highest tier.
    Ultra,
    /// Mid-high tier.
    Pro,
    /// Mid-low tier; also the assumed default when the tier is unknown.
    Standard,
    /// Lowest tier.
    Free,
}

impl AccountTier {
    /// Whitelisted lowercase spellings, upstream `projectAccountTier`. Anything
    /// else — including `None` — is [`Self::Standard`], matching the
    /// `?? 0.33` fallback in `calculateTierScore`.
    #[must_use]
    pub fn parse(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "ultra" => Self::Ultra,
            "pro" => Self::Pro,
            "free" => Self::Free,
            _ => Self::Standard,
        }
    }

    fn base_score(self) -> f64 {
        match self {
            Self::Ultra => 1.0,
            Self::Pro => 0.67,
            Self::Standard => 0.33,
            Self::Free => 0.0,
        }
    }
}

/// Seconds in the 30-day reset window upstream divides by for its reset bonus.
const RESET_WINDOW_SECS: f64 = 2_592_000.0;

/// Combines account tier and quota-reset recency into one `[0, 1]` factor.
///
/// `None` tier is [`AccountTier::Standard`], and an absent or zero reset
/// interval earns no bonus. Ported from `calculateTierScore`.
fn tier_priority(tier: Option<AccountTier>, reset_secs: Option<u64>) -> f64 {
    let base = tier.map_or_else(
        || AccountTier::Standard.base_score(),
        AccountTier::base_score,
    );
    let bonus = match reset_secs {
        Some(s) if s > 0 => (1.0 - s as f64 / RESET_WINDOW_SECS).max(0.0),
        _ => 0.0,
    };
    (base * 0.8 + bonus * 0.2).min(1.0)
}

/// Bounds an observed rate to `[0, 1]`; absent or garbage reads as `0`.
///
/// Mirrors upstream `toBoundedRate`. The bound happens *before* the
/// subtraction in [`reliability_factor`] — `clamp01(1.0 - f64::NAN)` would be
/// `0.0`, i.e. "fails every call", which is the opposite of what corrupt
/// telemetry should mean.
fn bounded_rate(v: Option<f64>) -> f64 {
    match v {
        Some(x) if x.is_finite() && x >= 0.0 => x.min(1.0),
        _ => 0.0,
    }
}

/// `1 - failure rate`, preferring the explicit rate over the coarser error rate.
///
/// An unobserved candidate reads fully reliable (`1.0`), not neutral: it has
/// not failed anything. Same formula and precedence as upstream
/// `reliabilityFactor`.
fn reliability_factor(failure_rate: Option<f64>, error_rate: f64) -> f64 {
    clamp01(1.0 - bounded_rate(failure_rate.or(Some(error_rate))))
}

/// Pool-wide maxima used to normalise the relative factors.
///
/// Identical for every candidate in a pool, so it is computed once
/// ([`pool_maxima`]) and threaded through [`factors`] rather than
/// recomputed per candidate. The floors keep a single-candidate pool from
/// dividing by zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoolMaxima {
    /// Highest known input price, floor `0.001`.
    pub max_cost: f64,
    /// Highest p95 latency, floor `1`.
    pub max_latency_ms: f64,
    /// Highest latency standard deviation, floor `0.001`.
    pub max_stddev_ms: f64,
}

/// One scan of the pool for the relative-factor maxima.
#[must_use]
pub fn pool_maxima(pool: &[AutoCandidate]) -> PoolMaxima {
    let mut m = PoolMaxima {
        max_cost: 0.001,
        max_latency_ms: 1.0,
        max_stddev_ms: 0.001,
    };
    for c in pool {
        if let Some(price) = c.input_usd_per_mtok
            && price > m.max_cost
        {
            m.max_cost = price;
        }
        if c.p95_latency_ms > m.max_latency_ms {
            m.max_latency_ms = c.p95_latency_ms;
        }
        if c.latency_stddev_ms > m.max_stddev_ms {
            m.max_stddev_ms = c.latency_stddev_ms;
        }
    }
    m
}

/// One routable model plus the telemetry the 16 factors read.
///
/// Deliberately wider than [`crate::Candidate`]: the four lean strategies read
/// only provider, model, price and rank, and widening that struct would put
/// connection telemetry in the path of every `priority` pick. An
/// `AutoCandidate` is a superset and converts to a `Candidate` on demand.
#[derive(Clone, Debug, PartialEq)]
pub struct AutoCandidate {
    /// Provider that serves this model.
    pub provider: ProviderId,
    /// Provider-local model name.
    pub model: Strng,
    /// USD per 1M input tokens. `None` = price unknown, which scores as
    /// *worst*, never as free: routing blind to price is worse than routing
    /// to a known-cheap tier.
    pub input_usd_per_mtok: Option<f64>,
    /// Lower sorts first; breaks score ties in config order.
    pub rank: u32,
    /// Quota left, percent `0..=100`. Neutral default `100.0`.
    pub quota_remaining_pct: f64,
    /// Breaker state. Neutral default [`CircuitState::Closed`].
    pub breaker: CircuitState,
    /// p95 latency in ms. Neutral default `0.0` (every candidate equal).
    pub p95_latency_ms: f64,
    /// Latency standard deviation in ms. Neutral default `0.0`.
    pub latency_stddev_ms: f64,
    /// Observed failure rate; wins over [`Self::error_rate`] when present.
    pub failure_rate: Option<f64>,
    /// Coarser error rate, used when no failure rate is observed.
    pub error_rate: f64,
    /// Billing tier; `None` is treated as [`AccountTier::Standard`].
    pub account_tier: Option<AccountTier>,
    /// Quota reset interval in seconds; shorter earns a higher bonus.
    pub quota_reset_secs: Option<u64>,
    /// Affinity for staying on the current session's path. `None` → `0.5`.
    pub context_affinity: Option<f64>,
    /// Affinity for the account holding the cached prompt prefix. `None` → `0`.
    pub cache_affinity: Option<f64>,
    /// Whether the session's provider can take this request. `None` → `1`.
    pub session_availability: Option<f64>,
    /// Quota reset-window preference. `None` → `0.5`.
    pub reset_window_affinity: Option<f64>,
    /// Feedback-driven output quality. `None` → `0.5` (neutral, not penalised).
    pub quality: Option<f64>,
    /// Accounts on this provider. `None` → `1`.
    pub connection_pool_size: Option<u32>,
}

impl AutoCandidate {
    /// A candidate with every telemetry signal at its neutral value.
    ///
    /// Neutrals are chosen so that two freshly-built candidates differ *only*
    /// in what a caller explicitly sets: an unpriced model, a closed breaker,
    /// full quota, no observations. That is what makes a scoring test
    /// deterministic without hand-tuning twelve factors.
    #[must_use]
    pub fn new(provider: ProviderId, model: impl AsRef<str>) -> Self {
        Self {
            provider,
            model: Strng::from(model.as_ref()),
            input_usd_per_mtok: None,
            rank: 0,
            quota_remaining_pct: 100.0,
            breaker: CircuitState::Closed,
            p95_latency_ms: 0.0,
            latency_stddev_ms: 0.0,
            failure_rate: None,
            error_rate: 0.0,
            account_tier: None,
            quota_reset_secs: None,
            context_affinity: None,
            cache_affinity: None,
            session_availability: None,
            reset_window_affinity: None,
            quality: None,
            connection_pool_size: None,
        }
    }

    /// Sets the input price in USD per 1M tokens.
    #[must_use]
    pub fn with_price(mut self, usd_per_mtok: f64) -> Self {
        self.input_usd_per_mtok = Some(usd_per_mtok);
        self
    }

    /// Sets the tiebreak rank (lower = earlier).
    #[must_use]
    pub fn with_rank(mut self, rank: u32) -> Self {
        self.rank = rank;
        self
    }

    /// Sets the breaker state.
    #[must_use]
    pub fn with_breaker(mut self, breaker: CircuitState) -> Self {
        self.breaker = breaker;
        self
    }

    /// The lean-strategy view, for handing one entry to the attempt loop.
    ///
    /// Only the four fields the lean strategies read carry over. The
    /// quota / context / in-flight fields on [`Candidate`] are this crate's
    /// own later-phase metadata and are left at their constructor defaults:
    /// inventing values here would be a second source of truth for them.
    #[must_use]
    pub fn as_candidate(&self) -> Candidate {
        let mut c = Candidate::new(self.provider.clone(), self.model.clone()).with_rank(self.rank);
        if let Some(price) = self.input_usd_per_mtok {
            c = c.with_price(price);
        }
        c
    }
}

/// The 16 factors a candidate is scored on, each in `[0, 1]`.
///
/// Declaration order is the upstream `ScoringFactors` order **and** the order
/// [`Factors::as_pairs`] emits, so a trace is byte-stable across runs and a
/// positional read of this struct cannot disagree with the serialised one. The
/// last three are `connection_density`, `quality`, `reliability` — upstream puts
/// connection density *before* quality, and a struct that declared them the other
/// way round would be a lie the doc comment above used to tell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Factors {
    /// Remaining quota, `quota_remaining_pct / 100`.
    pub quota: f64,
    /// Breaker health.
    pub health: f64,
    /// Inverse price against the pool maximum.
    pub cost_inv: f64,
    /// Inverse p95 latency against the pool maximum.
    pub latency_inv: f64,
    /// Caller-supplied fitness of the model for the request.
    pub task_fit: f64,
    /// Inverse latency variance against the pool maximum.
    pub stability: f64,
    /// Account tier, plus a reset-recency bonus.
    pub tier_priority: f64,
    /// Manifest tier affinity. Constant until the manifest tier table lands.
    pub tier_affinity: f64,
    /// Manifest specificity match. Constant until the manifest tier table lands.
    pub specificity_match: f64,
    /// Sticking to the current session's path.
    pub context_affinity: f64,
    /// Staying on the account holding the cached prefix.
    pub cache_affinity: f64,
    /// Whether the session's provider can serve this request.
    pub session_availability: f64,
    /// Quota reset-window preference.
    pub reset_window_affinity: f64,
    /// Accounts behind the provider, normalised over 10.
    pub connection_density: f64,
    /// Feedback-driven output quality.
    pub quality: f64,
    /// Observed success share.
    pub reliability: f64,
}

impl Factors {
    /// The two factors `p2c` ranks on, in the order it reads them.
    ///
    /// Upstream's `getP2CTargetScore` scores a drawn pair on
    /// `successRate / 100` plus `1 / log10(avgLatency + 10)`
    /// (`combo/targetSorters.ts:126-140`). Those are the same two quantities as
    /// [`Self::reliability`] and [`Self::latency_inv`] after this crate's
    /// `[0, 1]` normalisation, so naming the pair here is what stops `p2c` from
    /// carrying a private copy of two of the sixteen and drifting from the set
    /// the rest of the scorer writes.
    #[must_use]
    pub const fn load_signals(&self) -> (f64, f64) {
        (self.reliability, self.latency_inv)
    }

    /// Field name and value pairs, in declaration order.
    ///
    /// This is the whole serialisation contract for `ar_explain_route` (P4
    /// `ar-mcp`): a fixed-order array of 16 pairs needs no map, no allocator
    /// and no `serde` in this crate, and renders deterministically in TOON.
    #[must_use]
    pub fn as_pairs(&self) -> [(&'static str, f64); 16] {
        [
            ("quota", self.quota),
            ("health", self.health),
            ("cost_inv", self.cost_inv),
            ("latency_inv", self.latency_inv),
            ("task_fit", self.task_fit),
            ("stability", self.stability),
            ("tier_priority", self.tier_priority),
            ("tier_affinity", self.tier_affinity),
            ("specificity_match", self.specificity_match),
            ("context_affinity", self.context_affinity),
            ("cache_affinity", self.cache_affinity),
            ("session_availability", self.session_availability),
            ("reset_window_affinity", self.reset_window_affinity),
            ("connection_density", self.connection_density),
            ("quality", self.quality),
            ("reliability", self.reliability),
        ]
    }
}

/// Per-factor weights. A scoring distribution, not a priority list.
///
/// Every preset in [`crate::AutoVariant::weights`] sums to `1.0` ± `0.01`,
/// which `weights_sum_to_one_when_each_preset` asserts. Passing a malformed
/// set does not break the ranking: the score is clamped to `[0, 1]`, and a set
/// summing to less than 1 just compresses the range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Weights {
    /// Weight of [`Factors::quota`].
    pub quota: f64,
    /// Weight of [`Factors::health`].
    pub health: f64,
    /// Weight of [`Factors::cost_inv`].
    pub cost_inv: f64,
    /// Weight of [`Factors::latency_inv`].
    pub latency_inv: f64,
    /// Weight of [`Factors::task_fit`].
    pub task_fit: f64,
    /// Weight of [`Factors::stability`].
    pub stability: f64,
    /// Weight of [`Factors::tier_priority`].
    pub tier_priority: f64,
    /// Weight of [`Factors::tier_affinity`].
    pub tier_affinity: f64,
    /// Weight of [`Factors::specificity_match`].
    pub specificity_match: f64,
    /// Weight of [`Factors::context_affinity`].
    pub context_affinity: f64,
    /// Weight of [`Factors::cache_affinity`].
    pub cache_affinity: f64,
    /// Weight of [`Factors::session_availability`].
    pub session_availability: f64,
    /// Weight of [`Factors::reset_window_affinity`].
    pub reset_window_affinity: f64,
    /// Weight of [`Factors::connection_density`].
    pub connection_density: f64,
    /// Weight of [`Factors::quality`].
    pub quality: f64,
    /// Weight of [`Factors::reliability`].
    pub reliability: f64,
}

impl Weights {
    /// Upstream `DEFAULT_WEIGHTS` — the balanced `auto` distribution.
    ///
    /// `health` was shifted down (0.1905 → 0.1605) to make room for
    /// `quality`; the sum is still 1.0. `cache_affinity`,
    /// `reset_window_affinity` and `reliability` are declared but silent
    /// here: their signals are not collected by default.
    #[must_use]
    pub const fn balanced() -> Self {
        Self {
            quota: 0.1429,
            health: 0.1605,
            cost_inv: 0.1429,
            latency_inv: 0.1143,
            task_fit: 0.0762,
            stability: 0.0476,
            tier_priority: 0.0476,
            tier_affinity: 0.0476,
            specificity_match: 0.0476,
            context_affinity: 0.0476,
            cache_affinity: 0.0,
            session_availability: 0.0476,
            reset_window_affinity: 0.0,
            connection_density: 0.0476,
            quality: 0.03,
            reliability: 0.0,
        }
    }

    /// Upstream `quality-first` — task fit and stability dominate. `auto/coding`.
    #[must_use]
    pub const fn quality_first() -> Self {
        Self {
            quota: 0.0752,
            health: 0.1714,
            cost_inv: 0.0276,
            latency_inv: 0.0476,
            task_fit: 0.3524,
            stability: 0.1429,
            tier_priority: 0.0276,
            tier_affinity: 0.0,
            specificity_match: 0.0,
            context_affinity: 0.0,
            cache_affinity: 0.0,
            session_availability: 0.0476,
            reset_window_affinity: 0.0,
            connection_density: 0.0476,
            quality: 0.03,
            reliability: 0.03,
        }
    }

    /// Upstream `ship-fast` — latency and health dominate. `auto/fast`.
    #[must_use]
    pub const fn ship_fast() -> Self {
        Self {
            quota: 0.1133,
            health: 0.2667,
            cost_inv: 0.0276,
            latency_inv: 0.3048,
            task_fit: 0.0952,
            stability: 0.0,
            tier_priority: 0.0376,
            tier_affinity: 0.0,
            specificity_match: 0.0,
            context_affinity: 0.0095,
            cache_affinity: 0.0,
            session_availability: 0.0476,
            reset_window_affinity: 0.0,
            connection_density: 0.0476,
            quality: 0.02,
            reliability: 0.03,
        }
    }

    /// Upstream `cost-saver` — price dominates. `auto/cheap`.
    #[must_use]
    pub const fn cost_saver() -> Self {
        Self {
            quota: 0.1133,
            health: 0.181,
            cost_inv: 0.3324,
            latency_inv: 0.0476,
            task_fit: 0.0952,
            stability: 0.0476,
            tier_priority: 0.0376,
            tier_affinity: 0.0,
            specificity_match: 0.0,
            context_affinity: 0.0,
            cache_affinity: 0.0,
            session_availability: 0.0476,
            reset_window_affinity: 0.0,
            connection_density: 0.0476,
            quality: 0.02,
            reliability: 0.03,
        }
    }

    /// Upstream `offline-friendly` — quota availability dominates, because the
    /// provider with quota left is the one that can still answer. `taskFit` goes
    /// to zero and `tierPriority` takes its share, which is the reference's own
    /// rebalance (`modePacks.ts:71-88`). `auto/offline`.
    #[must_use]
    pub const fn offline_friendly() -> Self {
        Self {
            quota: 0.3324,
            health: 0.2667,
            cost_inv: 0.0752,
            latency_inv: 0.0476,
            task_fit: 0.0,
            stability: 0.0952,
            tier_priority: 0.0376,
            tier_affinity: 0.0,
            specificity_match: 0.0,
            context_affinity: 0.0,
            cache_affinity: 0.0,
            session_availability: 0.0476,
            reset_window_affinity: 0.0,
            connection_density: 0.0476,
            quality: 0.02,
            reliability: 0.03,
        }
    }

    /// Upstream `chaos-mode` — health, stability and task fit dominate, quota
    /// almost silent. `auto/chaos` fans out over a panel, where quota
    /// diversity is secondary to picking the most stable providers.
    #[must_use]
    pub const fn chaos_mode() -> Self {
        Self {
            quota: 0.0376,
            health: 0.4,
            cost_inv: 0.014,
            latency_inv: 0.0186,
            task_fit: 0.1905,
            stability: 0.1714,
            tier_priority: 0.004,
            tier_affinity: 0.0,
            specificity_match: 0.0,
            context_affinity: 0.0186,
            cache_affinity: 0.0,
            session_availability: 0.0476,
            reset_window_affinity: 0.0,
            connection_density: 0.0476,
            quality: 0.02,
            reliability: 0.03,
        }
    }

    /// Sum of all sixteen weights. The distribution invariant, and the only
    /// thing a preset has to satisfy.
    #[must_use]
    pub fn sum(self) -> f64 {
        self.quota
            + self.health
            + self.cost_inv
            + self.latency_inv
            + self.task_fit
            + self.stability
            + self.tier_priority
            + self.tier_affinity
            + self.specificity_match
            + self.context_affinity
            + self.cache_affinity
            + self.session_availability
            + self.reset_window_affinity
            + self.connection_density
            + self.quality
            + self.reliability
    }
}

/// Computes the sixteen factors for one candidate.
///
/// `maxima` must come from [`pool_maxima`] over the same pool, so the
/// relative factors are comparable across candidates. `task_fitness` is the
/// caller's model/task hook — this crate ships no fitness table, because the
/// table is a catalog concern and the scorer must not be able to change the
/// request it is routing. Pass `|_| 0.5` for the neutral value.
pub fn factors<F>(c: &AutoCandidate, maxima: &PoolMaxima, task_fitness: &F) -> Factors
where
    F: Fn(&str) -> f64,
{
    // Unpriced candidates score as *worst* on price, mirroring the lean
    // `cost-optimized` rule: an unknown price must never win on "unknown is
    // probably free". With no priced peer, the pool maximum is the 0.001
    // floor and the comparison still lands in range.
    let cost_inv = match c.input_usd_per_mtok {
        Some(price) => 1.0 - price / maxima.max_cost,
        None => -1.0,
    };

    Factors {
        quota: clamp01(c.quota_remaining_pct / 100.0),
        health: c.breaker.health(),
        cost_inv: clamp01(cost_inv),
        latency_inv: clamp01(1.0 - c.p95_latency_ms / maxima.max_latency_ms),
        task_fit: clamp01(task_fitness(&c.model)),
        stability: clamp01(1.0 - c.latency_stddev_ms / maxima.max_stddev_ms),
        tier_priority: tier_priority(c.account_tier, c.quota_reset_secs),
        // Manifest-routing factors. Upstream returns a neutral 0.5 whenever no
        // `RoutingHint` is present, which is always, because the manifest tier
        // table is not in scope. A constant added to every candidate cancels
        // in the ranking; carrying the two fields keeps the factor set and the
        // trace shape at the upstream 16.
        tier_affinity: 0.5,
        specificity_match: 0.5,
        context_affinity: clamp01(c.context_affinity.unwrap_or(0.5)),
        cache_affinity: clamp01(c.cache_affinity.unwrap_or(0.0)),
        session_availability: clamp01(c.session_availability.unwrap_or(1.0)),
        reset_window_affinity: clamp01(c.reset_window_affinity.unwrap_or(0.5)),
        connection_density: clamp01(c.connection_pool_size.map_or(1.0, |n| n as f64 - 1.0) / 10.0),
        quality: clamp01(c.quality.unwrap_or(0.5)),
        reliability: reliability_factor(c.failure_rate, c.error_rate),
    }
}

/// Weighted sum of the factors, clamped to `[0, 1]`.
///
/// `weights` is borrowed, not copied: it is 128 bytes and this runs once per
/// candidate, so taking it by value would copy it per candidate.
#[must_use]
pub fn score(f: &Factors, weights: &Weights) -> f64 {
    clamp01(
        weights.quota * f.quota
            + weights.health * f.health
            + weights.cost_inv * f.cost_inv
            + weights.latency_inv * f.latency_inv
            + weights.task_fit * f.task_fit
            + weights.stability * f.stability
            + weights.tier_priority * f.tier_priority
            + weights.tier_affinity * f.tier_affinity
            + weights.specificity_match * f.specificity_match
            + weights.context_affinity * f.context_affinity
            + weights.cache_affinity * f.cache_affinity
            + weights.session_availability * f.session_availability
            + weights.reset_window_affinity * f.reset_window_affinity
            + weights.connection_density * f.connection_density
            + weights.quality * f.quality
            + weights.reliability * f.reliability,
    )
}

/// One scored candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct Scored {
    /// Provider that serves the model.
    pub provider: ProviderId,
    /// Provider-local model name.
    pub model: Strng,
    /// Weighted score in `[0, 1]`.
    pub score: f64,
    /// The sixteen factors behind [`Self::score`].
    pub factors: Factors,
}

/// Scores every candidate and returns them ranked, best first.
///
/// One pass to find the pool maxima, one pass to score, one sort. Ties keep
/// pool order, which is the same stability the lean `Priority` strategy
/// documents — a config with no explicit tiebreak must not reshuffle between
/// requests.
///
/// A tripped breaker is *not* filtered out here. It scores `health = 0.0`,
/// which is 0.16–0.40 of the distribution depending on the pack; a second
/// exclusion pass would re-rank the same set and cost a second scoring pass.
#[must_use]
pub fn score_pool<F>(pool: &[AutoCandidate], weights: &Weights, task_fitness: F) -> Vec<Scored>
where
    F: Fn(&str) -> f64,
{
    let maxima = pool_maxima(pool);
    let mut ranked: Vec<Scored> = pool
        .iter()
        .map(|c| {
            let factors = factors(c, &maxima, &task_fitness);
            Scored {
                provider: c.provider.clone(),
                model: c.model.clone(),
                score: score(&factors, weights),
                factors,
            }
        })
        .collect();
    // Two `Arc` bumps per candidate; the alternative is carrying indices
    // through the sort, which is more code for less than the 16 f64
    // multiplies the scorer already does per candidate.
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score));
    ranked
}

/// A factor set with every field healthy. Test-only, shared with the
/// selection tests so a 16-field literal is written once.
///
/// Deliberately not a `Default` impl: an all-zero `Factors` is not a neutral
/// factor set, and a `Default` would invite exactly that reading.
#[cfg(test)]
pub(crate) fn healthy_factors() -> Factors {
    Factors {
        quota: 1.0,
        health: 1.0,
        cost_inv: 1.0,
        latency_inv: 1.0,
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
        reliability: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccountTier, AutoCandidate, CircuitState, Factors, Weights, pool_maxima,
        reliability_factor, score, score_pool, tier_priority,
    };
    use crate::contract::ProviderId;

    fn all_weights() -> [Weights; 6] {
        [
            Weights::balanced(),
            Weights::quality_first(),
            Weights::ship_fast(),
            Weights::cost_saver(),
            Weights::offline_friendly(),
            Weights::chaos_mode(),
        ]
    }

    /// A factor set with every field at a healthy value. Not `Default`:
    /// an all-zero `Factors` is not a neutral factor set, and a `Default` impl
    /// would invite exactly that reading.
    fn healthy() -> Factors {
        super::healthy_factors()
    }

    #[test]
    fn weights_sum_to_one_when_each_preset() {
        for w in all_weights() {
            assert!((w.sum() - 1.0).abs() < 0.01, "sum was {}", w.sum());
        }
    }

    #[test]
    fn scores_cheapest_when_cost_inv_dominates() {
        let cheap = score(&healthy(), &Weights::cost_saver());
        let dear = score(
            &Factors {
                cost_inv: 0.0,
                ..healthy()
            },
            &Weights::cost_saver(),
        );
        assert!(cheap > dear, "{cheap} !> {dear}");
    }

    #[test]
    fn clamps_nan_factor_to_zero() {
        let f = Factors {
            quota: f64::NAN,
            ..healthy()
        };
        assert!(score(&f, &Weights::balanced()).is_finite());
    }

    #[test]
    fn treats_nan_rate_as_fully_reliable() {
        assert_eq!(reliability_factor(Some(f64::NAN), 0.0), 1.0);
    }

    #[test]
    fn reads_unobserved_candidate_as_fully_reliable() {
        assert_eq!(reliability_factor(None, 0.0), 1.0);
    }

    #[test]
    fn prefers_failure_rate_over_error_rate() {
        assert_eq!(reliability_factor(Some(0.5), 0.9), 0.5);
    }

    #[test]
    fn ranks_open_breaker_below_closed() {
        let pool = [
            AutoCandidate::new(ProviderId::new("p-open"), "m").with_breaker(CircuitState::Open),
            AutoCandidate::new(ProviderId::new("p-closed"), "m").with_breaker(CircuitState::Closed),
        ];
        let ranked = score_pool(&pool, &Weights::balanced(), |_| 0.5);
        assert_eq!(ranked[0].provider.as_str(), "p-closed");
    }

    #[test]
    fn scores_unpriced_candidate_as_worst_on_price() {
        let pool = [
            AutoCandidate::new(ProviderId::new("cheap"), "m").with_price(0.20),
            AutoCandidate::new(ProviderId::new("mystery"), "m"),
        ];
        let ranked = score_pool(&pool, &Weights::cost_saver(), |_| 0.5);
        assert_eq!(ranked[0].factors.cost_inv, 0.0);
    }

    #[test]
    fn keeps_pool_order_when_scores_tie() {
        let pool = [
            AutoCandidate::new(ProviderId::new("a"), "m").with_price(1.0),
            AutoCandidate::new(ProviderId::new("b"), "m").with_price(1.0),
        ];
        let ranked = score_pool(&pool, &Weights::balanced(), |_| 0.5);
        assert_eq!(ranked[0].provider.as_str(), "a");
    }

    #[test]
    fn floors_pool_maxima_against_empty_pool() {
        assert_eq!(
            pool_maxima(&[]),
            super::PoolMaxima {
                max_cost: 0.001,
                max_latency_ms: 1.0,
                max_stddev_ms: 0.001
            }
        );
    }

    #[test]
    fn ranks_ultra_tier_above_free_tier() {
        let mut ultra = AutoCandidate::new(ProviderId::new("u"), "m");
        ultra.account_tier = Some(AccountTier::Ultra);
        let mut free = AutoCandidate::new(ProviderId::new("f"), "m");
        free.account_tier = Some(AccountTier::Free);
        let ranked = score_pool(&[free, ultra], &Weights::balanced(), |_| 0.5);
        assert_eq!(ranked[0].provider.as_str(), "u");
    }

    #[test]
    fn parses_unknown_tier_as_standard() {
        assert_eq!(AccountTier::parse("enterprise"), AccountTier::Standard);
    }

    #[test]
    fn awards_reset_bonus_only_for_short_windows() {
        let soon = tier_priority(Some(AccountTier::Free), Some(60));
        let late = tier_priority(Some(AccountTier::Free), Some(2_592_000));
        assert!(soon > late, "{soon} !> {late}");
    }

    #[test]
    fn parses_breaker_state_case_insensitively() {
        assert_eq!(CircuitState::parse("half_open"), CircuitState::HalfOpen);
    }

    #[test]
    fn round_trips_every_factor_through_its_position() {
        // The struct's declaration order IS the serialisation order. Field `i`
        // carries `i / 16`, so a swap of any two neighbours — the
        // quality/connection_density pair this table used to have — shows up as a
        // name/value mismatch rather than as a silently different trace.
        let f = Factors {
            quota: 0.0,
            health: 1.0 / 16.0,
            cost_inv: 2.0 / 16.0,
            latency_inv: 3.0 / 16.0,
            task_fit: 4.0 / 16.0,
            stability: 5.0 / 16.0,
            tier_priority: 6.0 / 16.0,
            tier_affinity: 7.0 / 16.0,
            specificity_match: 8.0 / 16.0,
            context_affinity: 9.0 / 16.0,
            cache_affinity: 10.0 / 16.0,
            session_availability: 11.0 / 16.0,
            reset_window_affinity: 12.0 / 16.0,
            connection_density: 13.0 / 16.0,
            quality: 14.0 / 16.0,
            reliability: 15.0 / 16.0,
        };
        for (index, (name, value)) in f.as_pairs().iter().enumerate() {
            let expected = index as f64 / 16.0;
            assert!(
                (value - expected).abs() < f64::EPSILON,
                "pair {index} ({name}) was {value}, not {expected}"
            );
        }
    }

    #[test]
    fn emits_connection_density_before_quality_in_its_pairs() {
        // Upstream's `ScoringFactors` order, spelled out: the pair this crate
        // used to declare the other way round.
        let pairs = healthy().as_pairs();
        assert_eq!(pairs[13].0, "connection_density");
        assert_eq!(pairs[14].0, "quality");
    }
}
