//! The single resilience layer: per-key exponential backoff with 429
//! `Retry-After` honoured, plus the two narrower scopes a per-key table cannot
//! express — `provider:model` lockout and a per-provider circuit breaker.
//!
//! Ported from `../OmniRoute/open-sse/services/chatCore/connectionCooldown.ts`
//! (the backoff), `open-sse/services/accountFallback.ts` (lockout, escalation
//! window, credits-vs-terminal signals) and `src/shared/utils/circuitBreaker.ts`
//! (state machine, per-class thresholds, half-open probe budget, open-cycle
//! backoff escalation). `open-sse/config/constants.ts` supplies the threshold
//! numbers.
//!
//! A per-key table alone cooled too much: one 404 on one model took the whole
//! provider out for the length of the window, so its healthy sibling models
//! were skipped too. Three scopes now, each answering one question, each with
//! its own gate:
//!
//! | scope | keyed on | tripped by | gate |
//! |---|---|---|---|
//! | key cooldown | credential | 408/5xx/transport/429 | [`Resilience::is_cooling`] |
//! | model lockout | `provider`+`model` | 429, quota bodies, retired models | [`Resilience::is_cooling_for`] |
//! | provider breaker | provider | repeated non-throttle failures | [`Resilience::is_usable`] |
//!
//! `docs/02-port-from-omniroute.md` shipped **one** layer on purpose. The other
//! two answer questions the first one cannot: *is this model* the problem, and
//! *is this provider* down at all.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::contract::Strng;

/// Base cooldown for the first failure. Matches the upstream API-key value (3s);
/// OAuth's 5s base is P5, when browser-session providers exist.
pub const DEFAULT_BASE_BACKOFF: Duration = Duration::from_secs(3);

/// Ceiling on the exponential growth.
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Base cooldown for a model's first lockout. Upstream's
/// `modelLockoutSettings.baseCooldownMs`.
pub const LOCKOUT_BASE_COOLDOWN: Duration = Duration::from_secs(120);

/// Ceiling on model-lockout backoff. Upstream's `maxCooldownMs`.
pub const LOCKOUT_MAX_COOLDOWN: Duration = Duration::from_secs(1800);

/// Doublings before the lockout curve stops. Upstream's `maxBackoffSteps`.
pub const LOCKOUT_MAX_BACKOFF_STEPS: u32 = 10;

/// How long after one failure a *new* failure still counts as consecutive.
///
/// Upstream's `getFailureWindowMs` default. The load-bearing part is that the
/// window is measured from the last failure and then **extended by the cooldown
/// that failure applied** — see [`Resilience::lock_model`].
pub const LOCKOUT_ESCALATION_WINDOW: Duration = Duration::from_secs(1800);

/// Ceiling on a quota-exhaustion lockout.
///
/// Upstream clamps `getMsUntilTomorrow()` against the profile's
/// `maxCooldownMs`. We allow a full day, because a daily quota genuinely does
/// only clear at a day boundary — but the clamp still earns its place, because
/// the *escalation* on a repeat failure will happily ask for more.
pub const QUOTA_COOLDOWN_CAP: Duration = Duration::from_secs(24 * 60 * 60);

/// Completed `Open → probe → Open` cycles after which the breaker's reset
/// window doubles again, up to `1 << BREAKER_BACKOFF_STEPS` (16×). Upstream's
/// `maxBackoffMultiplier` default.
pub const BREAKER_BACKOFF_STEPS: u32 = 4;

/// One key's error state.
#[derive(Clone, Copy, Debug)]
struct Cooldown {
    /// When the key becomes usable again.
    until: Instant,
    /// Consecutive failures. Reset to 0 on any success.
    failures: u32,
}

/// One `provider:model` lockout.
#[derive(Clone, Copy, Debug)]
struct ModelLock {
    /// When the lockout expires.
    until: Instant,
    /// Consecutive failures inside the escalation window, 1-based.
    failures: u32,
    /// When the failure that applied this lock landed.
    last_failure: Instant,
    /// The cooldown that failure applied, so the escalation window can be
    /// measured past it.
    applied: Duration,
}

/// One provider's breaker.
#[derive(Clone, Copy, Debug)]
struct Breaker {
    state: BreakerState,
    class: BreakerClass,
    /// Consecutive failures. Mirrors the key table's count.
    failures: u32,
    /// When the current `Open` window started.
    opened_at: Instant,
    /// Completed `Open → probe → Open` cycles; each one lengthens the window.
    open_cycles: u32,
    /// Half-open probes still unspent.
    probes: u8,
}

/// Single-probe budget. Upstream's `halfOpenRequests: 1`.
const HALF_OPEN_PROBES: u8 = 1;

/// What the two narrower scopes own.
///
/// One mutex because retiring a provider and locking one of its models must not
/// interleave: a caller that retires and then locks the same `provider:model`
/// would otherwise be able to leave a lockout behind a retirement.
#[derive(Debug, Default)]
struct Scopes {
    locks: HashMap<Strng, ModelLock>,
    retired_models: HashSet<Strng>,
    retired_providers: HashSet<Strng>,
}

/// Why a model is locked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockReason {
    /// A throttle or a refusal that clears on its own: exponential lockout on
    /// this model alone, leaving the provider's other models serving.
    Throttled,
    /// The account's *allowance* is spent. A backoff step is the wrong unit —
    /// a quota-exhausted provider re-selected every few seconds until the money
    /// arrives is exactly the failure mode a long lockout removes.
    QuotaExhausted,
    /// Nothing recovers on a timer. The model (or the account) is retired, not
    /// cooled: no cooldown is long enough, so any of them is just latency.
    Terminal,
}

/// Which provider profile a breaker runs on.
///
/// Upstream's `PROVIDER_PROFILES` `circuitBreakerThreshold` /
/// `circuitBreakerReset` pair (`constants.ts`): an OAuth session recovers more
/// slowly than an API key, so it gets fewer attempts and a longer reset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BreakerClass {
    /// 8 failures, 60s reset.
    OAuth,
    /// 12 failures, 30s reset. The default: the router cannot see a
    /// provider's credential type, and an API key is the common case.
    #[default]
    Key,
}

impl BreakerClass {
    /// Consecutive failures before the breaker opens.
    #[must_use]
    pub const fn threshold(self) -> u32 {
        match self {
            Self::OAuth => 8,
            Self::Key => 12,
        }
    }

    /// How long the breaker stays open before it admits one probe.
    #[must_use]
    pub const fn reset(self) -> Duration {
        match self {
            Self::OAuth => Duration::from_secs(60),
            Self::Key => Duration::from_secs(30),
        }
    }
}

/// Breaker state.
///
/// Named `BreakerState` rather than `CircuitState` because `crate::auto`
/// already exports a `CircuitState` for the auto-combo health factor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakerState {
    /// Dispatching normally.
    Closed,
    /// Short-circuiting; the caller reroutes.
    Open,
    /// Admitting exactly one probe.
    HalfOpen,
}

/// Per-key cooldown table, plus the two narrower scopes.
///
/// The key table is the layer routing needs; the lockout and breaker tables are
/// the ones that stop it cooling too much. See the module docs for the split.
#[derive(Debug)]
pub struct Resilience {
    state: Mutex<HashMap<Strng, Cooldown>>,
    base: Duration,
    max: Duration,
    scopes: Mutex<Scopes>,
    breakers: Mutex<HashMap<Strng, Breaker>>,
    /// Test-only override of [`BreakerClass::reset`], so the state machine can
    /// be exercised without a 30s sleep.
    reset_override: Option<Duration>,
    /// Test-only override of [`LOCKOUT_ESCALATION_WINDOW`], same reason. The
    /// lockout *base* needs no knob: it arrives per call.
    lock_window: Duration,
}

impl Resilience {
    /// Builds a table with the default 3s → 300s exponential schedule.
    #[must_use]
    pub fn new() -> Self {
        Self::with_backoff(DEFAULT_BASE_BACKOFF, DEFAULT_MAX_BACKOFF)
    }

    /// Builds a table with an explicit exponential schedule.
    ///
    /// The bounds are calibration knobs, not internal constants: upstream
    /// rate-limit windows differ per provider, and a deployment that needs a
    /// different curve should not have to edit this file.
    #[must_use]
    pub fn with_backoff(base: Duration, max: Duration) -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
            base,
            max,
            scopes: Mutex::new(Scopes::default()),
            breakers: Mutex::new(HashMap::new()),
            reset_override: None,
            lock_window: LOCKOUT_ESCALATION_WINDOW,
        }
    }

    /// Records a failure for `key` and returns the cooldown now in effect.
    ///
    /// `retry_after` (a parsed upstream `Retry-After`) wins when it is longer
    /// than our own schedule: the provider knows its own window and guessing
    /// shorter just earns a second 429. Grows the failure count either way, so
    /// a provider that keeps sending `Retry-After: 1` still escalates.
    ///
    /// One lock for the whole read-modify-write: a three-lock version lets two
    /// concurrent failures both read `failures: 0` and both schedule `base`.
    pub fn record_failure(&self, key: &str, retry_after: Option<Duration>) -> Duration {
        let Ok(mut state) = self.state.lock() else {
            tracing::warn!("resilience table poisoned, failure not recorded");
            return Duration::ZERO;
        };
        let now = Instant::now();
        let failures = state.get(key).map_or(0, |c| c.failures);
        let backoff = self.backoff_for(failures);
        let until = now + backoff.max(retry_after.unwrap_or(Duration::ZERO));
        state.insert(
            Strng::from(key),
            Cooldown {
                until,
                failures: failures.saturating_add(1),
            },
        );
        until.saturating_duration_since(now)
    }

    /// Clears all error state for `key`. A success wipes the failure count, so
    /// the next failure starts from `base` again.
    pub fn record_success(&self, key: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.remove(key);
        }
    }

    /// Remaining cooldown for `key`, or `None` when the key is usable now.
    ///
    /// Expired entries are swept here, so a caller that keeps asking never
    /// grows the table. This is the *key-only* view; the model-scoped gate is
    /// [`Resilience::is_cooling_for`].
    #[must_use]
    pub fn cooling(&self, key: &str) -> Option<Duration> {
        let mut state = self.state.lock().ok()?;
        let now = Instant::now();
        state.retain(|_, c| c.until > now);
        let remaining = state.get(key)?.until.saturating_duration_since(now);
        (remaining > Duration::ZERO).then_some(remaining)
    }

    /// Whether `key` is currently in cooldown. The routing gate calls this
    /// before spending an attempt on a known-dead key.
    #[must_use]
    pub fn is_cooling(&self, key: &str) -> bool {
        self.cooling(key).is_some()
    }

    /// Consecutive failure count for `key`, 0 when healthy.
    #[must_use]
    pub fn failures(&self, key: &str) -> u32 {
        self.state
            .lock()
            .map_or(0, |s| s.get(key).map_or(0, |c| c.failures))
    }

    /// `base * 2^failures`, clamped to `max`.
    ///
    /// The shift is capped at 31 so `1u32 << failures` cannot overflow; at that
    /// point the product is already far past any realistic `max` and the clamp
    /// takes over.
    fn backoff_for(&self, failures: u32) -> Duration {
        let scaled = self.base.saturating_mul(1u32 << failures.min(31));
        scaled.min(self.max)
    }

    // ─── model lockout ───────────────────────────────────────────────

    /// Locks one `provider:model` and returns the cooldown now in effect.
    ///
    /// `cooldown` is the *base* for this class, escalated per consecutive
    /// failure — `cooldown * 2^min(failures-1, LOCKOUT_MAX_BACKOFF_STEPS)`,
    /// clamped to `LOCKOUT_MAX_COOLDOWN` for a throttle and
    /// `QUOTA_COOLDOWN_CAP` for a spent allowance. One curve, two ceilings:
    /// the ceiling is per-reason because a throttle and a quota are not the
    /// same quantity, and a shared ceiling would cap the daily one back to
    /// 30 minutes.
    ///
    /// The escalation window is `LOCKOUT_ESCALATION_WINDOW` **plus the
    /// cooldown the previous failure applied**, so a model that fails again the
    /// instant its lockout expires keeps escalating instead of restarting at 1
    /// — upstream's `now - lastFailureAt <= resetAfterMs + lastCooldownMs`
    /// (`accountFallback.ts`). The `+ applied` half is not decoration: a lockout
    /// longer than the window would otherwise reset the count on every single
    /// re-failure, which is precisely the case the long cooldown creates.
    ///
    /// A lock already longer than the new one is preserved — `lockModel`'s
    /// `existing.until > newUntil` guard — so a caller offering a small
    /// cooldown cannot shorten a long one.
    pub fn lock_model(
        &self,
        provider: &str,
        model: &str,
        reason: LockReason,
        cooldown: Duration,
    ) -> Duration {
        if reason == LockReason::Terminal {
            self.retire_model(provider, model);
            return Duration::ZERO;
        }
        let Ok(mut scopes) = self.scopes.lock() else {
            tracing::warn!("resilience scopes poisoned, model not locked");
            return Duration::ZERO;
        };
        let now = Instant::now();
        scopes.locks.retain(|_, l| l.until > now);
        let key = model_key(provider, model);
        let previous = scopes.locks.get(&key).copied();
        let failures = previous.map_or(1, |p| {
            let window = self.lock_window + p.applied;
            if now.saturating_duration_since(p.last_failure) <= window {
                p.failures.saturating_add(1)
            } else {
                1
            }
        });
        let max = match reason {
            LockReason::QuotaExhausted => QUOTA_COOLDOWN_CAP,
            _ => LOCKOUT_MAX_COOLDOWN,
        };
        let applied = cooldown
            .saturating_mul(1u32 << (failures - 1).min(LOCKOUT_MAX_BACKOFF_STEPS))
            .min(max);
        let until = now + applied;
        if let Some(p) = previous
            && p.until > until
        {
            return p.until.saturating_duration_since(now);
        }
        scopes.locks.insert(
            key,
            ModelLock {
                until,
                failures,
                last_failure: now,
                applied,
            },
        );
        applied
    }

    /// Remaining lockout for `provider:model`, or `None` when the model is
    /// usable now. A retired model is not "cooling" — it has no window left to
    /// report — so it reads `None` here and `true` on
    /// [`Resilience::is_cooling_for`].
    #[must_use]
    pub fn model_cooling(&self, provider: &str, model: &str) -> Option<Duration> {
        let mut scopes = self.scopes.lock().ok()?;
        let now = Instant::now();
        scopes.locks.retain(|_, l| l.until > now);
        let remaining = scopes.locks.get(&model_key(provider, model))?.until;
        Some(remaining.saturating_duration_since(now)).filter(|d| *d > Duration::ZERO)
    }

    /// Whether `provider:model` is unusable: the key is cooling, the model is
    /// locked, or either is retired.
    ///
    /// This is the lockout-aware sibling of [`Resilience::is_cooling`], and the
    /// gate a routing caller wants once it knows the model it intends to pick:
    /// a key whose other models are fine should not cost this one its turn.
    #[must_use]
    pub fn is_cooling_for(&self, provider: &str, model: &str) -> bool {
        self.is_cooling(provider)
            || self.model_cooling(provider, model).is_some()
            || self.is_retired_model(provider, model)
    }

    /// Retires `provider:model` outright.
    ///
    /// Upstream's `MODEL_PERMANENTLY_UNAVAILABLE_PATTERNS`: a retired model
    /// 404s/410s on *every* future request, so a cooldown — however long — just
    /// buys one wasted upstream call per window, and at volume that reads as
    /// abuse. Retirement is undone only by a restart.
    pub fn retire_model(&self, provider: &str, model: &str) {
        if let Ok(mut scopes) = self.scopes.lock() {
            let key = model_key(provider, model);
            scopes.locks.remove(&key);
            scopes.retired_models.insert(key);
        }
    }

    /// Retires the whole provider: a deactivated, disabled or suspended
    /// account. The credential behind the key is gone, so there is nothing left
    /// for the other models to fall back to.
    pub fn retire_provider(&self, provider: &str) {
        if let Ok(mut scopes) = self.scopes.lock() {
            scopes.retired_providers.insert(Strng::from(provider));
        }
    }

    /// Whether `provider:model` has been retired.
    #[must_use]
    pub fn is_retired_model(&self, provider: &str, model: &str) -> bool {
        self.scopes
            .lock()
            .is_ok_and(|s| s.retired_models.contains(&model_key(provider, model)))
    }

    /// Whether `provider` has been retired.
    #[must_use]
    pub fn is_retired_provider(&self, provider: &str) -> bool {
        self.scopes
            .lock()
            .is_ok_and(|s| s.retired_providers.contains(provider))
    }

    // ─── provider breaker ────────────────────────────────────────────

    /// Whether a request to `provider` may be dispatched *now*.
    ///
    /// This is the pre-dispatch seam: `!resilience.is_usable(p.as_str())`
    /// beside `resilience.is_cooling_for(p, model)` in the candidate filter
    /// `strategy.rs`'s `pick_filtered` already takes. Wiring it there is the
    /// caller's edit, not this file's.
    ///
    /// **It spends a probe.** In `HalfOpen` this decrements the single-probe
    /// budget, exactly like upstream's `execute()` gate: a check that did not
    /// charge would let a burst of concurrent callers all probe at once, which
    /// is the thundering herd the budget exists to prevent. Read the state
    /// without spending with [`Resilience::breaker_state`].
    #[must_use]
    pub fn is_usable(&self, provider: &str) -> bool {
        if self.is_retired_provider(provider) {
            return false;
        }
        let Ok(mut breakers) = self.breakers.lock() else {
            return true;
        };
        let now = Instant::now();
        let Some(b) = breakers.get_mut(provider) else {
            return true;
        };
        self.refresh(b, now);
        match b.state {
            BreakerState::Closed => true,
            BreakerState::Open => false,
            BreakerState::HalfOpen => {
                if b.probes == 0 {
                    false
                } else {
                    b.probes -= 1;
                    true
                }
            }
        }
    }

    /// Current breaker state, running the `Open → HalfOpen` refresh first.
    ///
    /// Read-only: spends no probe, so it is safe to call for a metric or a log
    /// line where [`Resilience::is_usable`] would not be.
    #[must_use]
    pub fn breaker_state(&self, provider: &str) -> BreakerState {
        if self.is_retired_provider(provider) {
            return BreakerState::Open;
        }
        let Ok(mut breakers) = self.breakers.lock() else {
            return BreakerState::Closed;
        };
        let Some(b) = breakers.get_mut(provider) else {
            return BreakerState::Closed;
        };
        self.refresh(b, Instant::now());
        b.state
    }

    /// Records one provider-scoped failure, opening the circuit at
    /// `class.threshold()`.
    ///
    /// A failed half-open probe re-opens immediately and counts as one open
    /// cycle, so the next window is longer — the backoff escalation that stops
    /// a provider which is not actually recovering from being re-probed on a
    /// fixed interval forever.
    pub fn record_provider_failure(&self, provider: &str, class: BreakerClass) {
        let Ok(mut breakers) = self.breakers.lock() else {
            return;
        };
        let now = Instant::now();
        let b = breakers.entry(Strng::from(provider)).or_insert(Breaker {
            state: BreakerState::Closed,
            class,
            failures: 0,
            opened_at: now,
            open_cycles: 0,
            probes: HALF_OPEN_PROBES,
        });
        b.class = class;
        if b.state == BreakerState::Open {
            return;
        }
        b.failures = b.failures.saturating_add(1);
        if b.state == BreakerState::HalfOpen {
            b.open_cycles = b.open_cycles.saturating_add(1);
            open_now(b, now);
        } else if b.failures >= class.threshold() {
            open_now(b, now);
        }
    }

    /// Records one provider-scoped success.
    ///
    /// A success out of `Open` or `HalfOpen` closes the circuit outright and
    /// clears the cycle count: a probe that worked is proof the provider is
    /// back, and letting the escalation survive a recovery would make the next
    /// unrelated blip wait twice as long for no reason. In `Closed` the count
    /// decays by one rather than resetting, so an intermittent provider is not
    /// declared healthy by a single lucky call.
    pub fn record_provider_success(&self, provider: &str) {
        let Ok(mut breakers) = self.breakers.lock() else {
            return;
        };
        let Some(b) = breakers.get_mut(provider) else {
            return;
        };
        if b.state == BreakerState::Closed {
            b.failures = b.failures.saturating_sub(1);
        } else {
            b.state = BreakerState::Closed;
            b.failures = 0;
            b.open_cycles = 0;
        }
    }

    /// `Open → HalfOpen` once the (escalated) window has elapsed.
    ///
    /// Nothing else moves here: `HalfOpen` is entered by this function and left
    /// by a probe's verdict, so a probe budget that runs out simply keeps
    /// reporting unusable rather than reopening on its own.
    fn refresh(&self, b: &mut Breaker, now: Instant) {
        if b.state != BreakerState::Open {
            return;
        }
        if now.saturating_duration_since(b.opened_at) >= self.reset_window(b.class, b.open_cycles) {
            b.state = BreakerState::HalfOpen;
            b.probes = HALF_OPEN_PROBES;
        }
    }

    /// The open window for a class, doubled per completed open cycle and
    /// capped at `1 << BREAKER_BACKOFF_STEPS`. Upstream's
    /// `_effectiveResetTimeout`.
    fn reset_window(&self, class: BreakerClass, open_cycles: u32) -> Duration {
        let base = self.reset_override.unwrap_or_else(|| class.reset());
        let ceiling = base.saturating_mul(1u32 << BREAKER_BACKOFF_STEPS);
        base.saturating_mul(1u32 << open_cycles.min(BREAKER_BACKOFF_STEPS))
            .min(ceiling)
    }

    /// Test-only: shrink the escalation window so escalation and sweep
    /// behaviour are observable without a thirty-minute sleep.
    #[cfg(test)]
    fn with_test_lockout_window(mut self, window: Duration) -> Self {
        self.lock_window = window;
        self
    }

    /// Test-only: override the breaker reset window.
    #[cfg(test)]
    fn with_test_breaker_reset(mut self, reset: Duration) -> Self {
        self.reset_override = Some(reset);
        self
    }
}

/// Enters `Open` at `now`.
fn open_now(b: &mut Breaker, now: Instant) {
    b.state = BreakerState::Open;
    b.opened_at = now;
    b.probes = 0;
}

/// `provider`+`model` as one map key.
///
/// The joiner is U+001F on purpose: model ids carry `/` (`openai/gpt-4o`) and
/// provider ids carry `-`, so the only unambiguous separator is one no registry
/// id contains.
fn model_key(provider: &str, model: &str) -> Strng {
    Strng::from(format!("{provider}\u{1f}{model}"))
}

/// Base cooldown for a quota-exhausted lockout: until the next UTC midnight.
///
/// Upstream's `getMsUntilTomorrow()`. A day boundary is the honest reading of
/// "your allowance is spent" — a three-second backoff step there is what makes
/// the provider get re-selected until the top-up lands. Locating the next day
/// boundary needs only the epoch read, so no calendar dependency comes with it.
#[must_use]
pub fn quota_cooldown() -> Duration {
    const DAY: u64 = 24 * 60 * 60;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Duration::from_secs(DAY - secs % DAY)
}

impl Default for Resilience {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        BreakerClass, BreakerState, LockReason, QUOTA_COOLDOWN_CAP, Resilience, quota_cooldown,
    };

    #[test]
    fn reports_cooling_after_failure() {
        let r = Resilience::new();
        r.record_failure("k1", None);
        assert!(r.is_cooling("k1"));
    }

    #[test]
    fn clears_cooldown_on_success() {
        let r = Resilience::new();
        r.record_failure("k1", None);
        r.record_success("k1");
        assert!(!r.is_cooling("k1"));
    }

    #[test]
    fn grows_backoff_with_consecutive_failures() {
        let r = Resilience::with_backoff(Duration::from_secs(1), Duration::from_secs(60));
        let first = r.record_failure("k1", None);
        let second = r.record_failure("k1", None);
        assert!(second > first);
    }

    #[test]
    fn clamps_backoff_at_max() {
        let r = Resilience::with_backoff(Duration::from_secs(1), Duration::from_secs(4));
        let mut last = Duration::ZERO;
        for _ in 0..8 {
            last = r.record_failure("k1", None);
        }
        assert_eq!(last, Duration::from_secs(4));
    }

    #[test]
    fn honors_retry_after_when_longer_than_backoff() {
        let r = Resilience::with_backoff(Duration::from_secs(1), Duration::from_secs(60));
        let got = r.record_failure("k1", Some(Duration::from_secs(30)));
        assert!(got >= Duration::from_secs(30));
    }

    #[test]
    fn prefers_exponential_when_retry_after_shorter() {
        // A provider that always says "retry in 1s" must still escalate, or a
        // hot loop is what the client gets.
        let r = Resilience::with_backoff(Duration::from_secs(1), Duration::from_secs(60));
        let first = r.record_failure("k1", Some(Duration::from_secs(1)));
        let second = r.record_failure("k1", Some(Duration::from_secs(1)));
        assert!(second > first);
    }

    #[test]
    fn keeps_keys_independent() {
        let r = Resilience::new();
        r.record_failure("k1", None);
        assert!(!r.is_cooling("k2"));
    }

    #[test]
    fn counts_consecutive_failures() {
        let r = Resilience::new();
        r.record_failure("k1", None);
        r.record_failure("k1", None);
        assert_eq!(r.failures("k1"), 2);
    }

    #[test]
    fn locks_one_model_without_locking_its_sibling() {
        // The whole reason the lockout table exists: a provider serving five
        // models loses one of them, not all five.
        let r = Resilience::new();
        let applied = r.lock_model("p", "dead", LockReason::Throttled, Duration::from_secs(120));
        assert_eq!(applied, Duration::from_secs(120));
        assert!(r.is_cooling_for("p", "dead"));
        assert!(
            !r.is_cooling_for("p", "alive"),
            "sibling model keeps serving"
        );
        assert!(!r.is_cooling("p"), "the provider key never cooled");
    }

    #[test]
    fn preserves_a_longer_existing_lock() {
        let r = Resilience::new();
        let long = r.lock_model("p", "m", LockReason::QuotaExhausted, quota_cooldown());
        assert!(long > Duration::from_secs(3600));
        let later = r.lock_model("p", "m", LockReason::Throttled, Duration::from_secs(120));
        assert!(
            later > Duration::from_secs(120),
            "a small later cooldown must not shorten a long lock, got {later:?}"
        );
    }

    #[test]
    fn quota_lockout_outlives_a_throttled_one() {
        let r = Resilience::new();
        let throttled = r.lock_model("p", "a", LockReason::Throttled, Duration::from_secs(120));
        let quota = r.lock_model("p", "b", LockReason::QuotaExhausted, quota_cooldown());
        assert_eq!(throttled, Duration::from_secs(120));
        assert!(
            quota > Duration::from_secs(3600),
            "a spent allowance waits for the day"
        );
        assert!(quota <= QUOTA_COOLDOWN_CAP);
    }

    #[test]
    fn repeated_quota_failures_are_capped_at_a_day() {
        // Escalation asks for more than a day after one repeat; the cap is what
        // stops a quota-exhausted model from being locked out for a week.
        let r = Resilience::new();
        let mut last = Duration::ZERO;
        for _ in 0..6 {
            last = r.lock_model("p", "m", LockReason::QuotaExhausted, quota_cooldown());
        }
        assert_eq!(last, QUOTA_COOLDOWN_CAP);
    }

    #[test]
    fn escalation_window_extends_past_the_applied_cooldown() {
        // Window zero: the only thing keeping the escalation alive is the
        // cooldown the previous failure applied.
        let r = Resilience::new().with_test_lockout_window(Duration::ZERO);
        let first = r.lock_model("p", "m", LockReason::Throttled, Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(40));
        let second = r.lock_model("p", "m", LockReason::Throttled, Duration::from_millis(100));
        assert_eq!(
            (first, second),
            (Duration::from_millis(100), Duration::from_millis(200))
        );
    }

    #[test]
    fn a_refailure_past_the_window_restarts_at_the_base() {
        // The other half of the same rule, and what makes the test above mean
        // something: once the elapsed time is past window+cooldown the count is
        // genuinely a new streak.
        let r = Resilience::new().with_test_lockout_window(Duration::ZERO);
        r.lock_model("p", "m", LockReason::Throttled, Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(90));
        let later = r.lock_model("p", "m", LockReason::Throttled, Duration::from_millis(50));
        assert_eq!(later, Duration::from_millis(50));
    }

    #[test]
    fn retires_a_terminal_model_instead_of_cooling_it() {
        let r = Resilience::new();
        let applied = r.lock_model("p", "m", LockReason::Terminal, Duration::from_secs(60));
        assert_eq!(
            applied,
            Duration::ZERO,
            "a retirement has no window to report"
        );
        assert!(r.is_retired_model("p", "m"));
        assert!(r.is_cooling_for("p", "m"));
        assert!(r.model_cooling("p", "m").is_none());
        assert!(!r.is_cooling_for("p", "other"));
    }

    #[test]
    fn retired_provider_makes_the_breaker_unusable() {
        let r = Resilience::new();
        assert!(r.is_usable("p"));
        r.retire_provider("p");
        assert!(!r.is_usable("p"));
        assert_eq!(r.breaker_state("p"), BreakerState::Open);
        assert!(r.is_usable("q"), "a sibling provider is untouched");
    }

    #[test]
    fn breaker_opens_at_the_class_threshold() {
        let r = Resilience::new();
        for _ in 0..BreakerClass::Key.threshold() - 1 {
            r.record_provider_failure("p", BreakerClass::Key);
        }
        assert!(r.is_usable("p"), "one failure short of the threshold");
        r.record_provider_failure("p", BreakerClass::Key);
        assert_eq!(r.breaker_state("p"), BreakerState::Open);
        assert!(!r.is_usable("p"));
    }

    #[test]
    fn oauth_class_trips_before_the_key_class() {
        assert!(BreakerClass::OAuth.threshold() < BreakerClass::Key.threshold());
        assert!(BreakerClass::OAuth.reset() > BreakerClass::Key.reset());
        let oauth = Resilience::new();
        for _ in 0..BreakerClass::OAuth.threshold() {
            oauth.record_provider_failure("p", BreakerClass::OAuth);
        }
        assert_eq!(oauth.breaker_state("p"), BreakerState::Open);
    }

    #[test]
    fn half_open_admits_one_probe_and_a_success_closes() {
        let r = Resilience::new().with_test_breaker_reset(Duration::from_millis(30));
        for _ in 0..BreakerClass::Key.threshold() {
            r.record_provider_failure("p", BreakerClass::Key);
        }
        assert!(!r.is_usable("p"), "open short-circuits");
        std::thread::sleep(Duration::from_millis(60));
        assert!(r.is_usable("p"), "half-open admits the probe");
        assert!(!r.is_usable("p"), "the probe budget is one request");
        r.record_provider_success("p");
        assert_eq!(r.breaker_state("p"), BreakerState::Closed);
        assert!(r.is_usable("p"));
    }

    #[test]
    fn a_failed_probe_reopens_with_a_longer_window() {
        let r = Resilience::new().with_test_breaker_reset(Duration::from_millis(30));
        for _ in 0..BreakerClass::Key.threshold() {
            r.record_provider_failure("p", BreakerClass::Key);
        }
        std::thread::sleep(Duration::from_millis(45));
        assert!(r.is_usable("p"), "first probe after the base window");
        r.record_provider_failure("p", BreakerClass::Key);
        // The window doubled with the cycle, so 45ms — past the 30ms base,
        // short of the 60ms escalated window — must not admit a probe.
        std::thread::sleep(Duration::from_millis(45));
        assert_eq!(r.breaker_state("p"), BreakerState::Open);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(r.breaker_state("p"), BreakerState::HalfOpen);
    }

    #[test]
    fn breaker_state_does_not_spend_the_probe() {
        let r = Resilience::new().with_test_breaker_reset(Duration::from_millis(20));
        for _ in 0..BreakerClass::Key.threshold() {
            r.record_provider_failure("p", BreakerClass::Key);
        }
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(r.breaker_state("p"), BreakerState::HalfOpen);
        assert_eq!(r.breaker_state("p"), BreakerState::HalfOpen);
        assert!(r.is_usable("p"), "reading the state left the budget intact");
    }

    #[test]
    fn expired_locks_sweep_so_the_table_cannot_grow_without_bound() {
        let r = Resilience::new().with_test_lockout_window(Duration::ZERO);
        r.lock_model("p", "m", LockReason::Throttled, Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(40));
        assert!(r.model_cooling("p", "m").is_none());
    }

    #[test]
    fn a_throttled_lockout_is_capped_at_thirty_minutes() {
        let r = Resilience::new();
        let mut last = Duration::ZERO;
        for _ in 0..8 {
            last = r.lock_model("p", "m", LockReason::Throttled, Duration::from_secs(120));
        }
        assert_eq!(last, super::LOCKOUT_MAX_COOLDOWN);
    }
}
