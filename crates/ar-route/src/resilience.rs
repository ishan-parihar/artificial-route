//! The single resilience layer: per-key exponential backoff with 429
//! `Retry-After` honoured.
//!
//! Ported from `../OmniRoute/open-sse/services/chatCore/connectionCooldown.ts`.
//! Per `docs/02-port-from-omniroute.md`, P0 ships **one** layer. There is
//! deliberately no circuit breaker, no quota-share, and no shadow traffic here:
//! a second layer that also skips a key is a second layer that also has to be
//! reasoned about, and P0 does not need it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::contract::Strng;

/// Base cooldown for the first failure. Matches the upstream API-key value (3s);
/// OAuth's 5s base is P5, when browser-session providers exist.
pub const DEFAULT_BASE_BACKOFF: Duration = Duration::from_secs(3);

/// Ceiling on the exponential growth.
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(300);

/// One key's error state.
#[derive(Clone, Copy, Debug)]
struct Cooldown {
    /// When the key becomes usable again.
    until: Instant,
    /// Consecutive failures. Reset to 0 on any success.
    failures: u32,
}

/// Per-key cooldown table.
///
/// Keyed on the *key* (a credential), not the provider: a provider with three
/// credentials keeps serving on the two that are healthy. That distinction is
/// the entire reason this layer exists separately from routing.
#[derive(Debug)]
pub struct Resilience {
    state: Mutex<HashMap<Strng, Cooldown>>,
    base: Duration,
    max: Duration,
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
            Cooldown { until, failures: failures.saturating_add(1) },
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
    /// grows the table.
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
        self.state.lock().map_or(0, |s| s.get(key).map_or(0, |c| c.failures))
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
}

impl Default for Resilience {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Resilience;

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
}
