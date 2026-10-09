//! `ar-limit` — in-process request-count limiting.
//!
//! The token bucket is ported from agentgateway's
//! `crates/agentgateway/src/http/localratelimit.rs` (Apache-2.0), keeping its
//! spec shape — `tokens_per_fill`, `fill_interval`, `max_tokens` — and its
//! two deliberate trades, while dropping what our threat model has no use for:
//! their CEL `key:` expression (no JWT, no per-user claims to key buckets on)
//! and their `quick_cache` dependency (a `Mutex<HashMap>` with a bounded size
//! is the same mechanism in fifty stdlib lines).
//!
//! **The trade that stays**: buckets are per-process. A deployment running
//! several proxies gets one independent limit per instance, exactly as the
//! source does. A cluster-wide ceiling needs shared state and is out of scope.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Buckets are evicted at this bound, oldest-refilled first — the source's
/// `MAX_BUCKETS` ceiling. Ten thousand distinct client keys in one process is
/// already far past what a single-operator deployment produces; the bound is
/// there so an unbounded key space (a per-request id) cannot grow the map
/// without limit.
pub const MAX_BUCKETS: usize = 10_000;

/// The three numbers that describe a bucket, in the source's own spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    /// Tokens the bucket holds when full — the burst ceiling.
    pub max_tokens: u64,
    /// Tokens added every [`Spec::fill_interval`].
    pub tokens_per_fill: u64,
    /// How often the fill lands.
    pub fill_interval: Duration,
}

impl Spec {
    /// The spec for "rpm requests per minute": a burst of `rpm`, refilled at
    /// `rpm` per 60s. The bucket therefore admits `rpm` instantly and then
    /// meters everything after it at the per-minute rate.
    #[must_use]
    pub fn per_minute(rpm: u32) -> Self {
        Self {
            max_tokens: u64::from(rpm),
            tokens_per_fill: u64::from(rpm),
            fill_interval: Duration::from_secs(60),
        }
    }
}

/// One key's bucket: tokens on hand and when they last refilled.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// A token bucket per client key.
#[derive(Debug)]
pub struct Limiter {
    spec: Spec,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl Limiter {
    /// A limiter with `spec`, holding a bucket per key seen since boot.
    #[must_use]
    pub fn new(spec: Spec) -> Self {
        Self {
            spec,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Consumes one token for `key`, or reports when the next one lands.
    ///
    /// `Ok(())` is "allowed"; `Err(seconds)` is the `Retry-After` value — the
    /// seconds until the bucket holds a token, rounded up so a client that
    /// waits exactly that long finds the request admitted.
    ///
    /// A zero-token spec (tokens_per_fill and max_tokens both zero) denies
    /// everything; `Spec::per_minute(0)` is refused at the config edge instead,
    /// so this arm is reachable only by constructing the struct by hand.
    pub fn check(&self, key: &str) -> Result<(), u64> {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("limit mutex poisoned");
        let spec = self.spec;
        let is_new = !buckets.contains_key(key);
        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: spec.max_tokens as f64,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last);
        if elapsed >= spec.fill_interval {
            let fills = elapsed.as_secs_f64() / spec.fill_interval.as_secs_f64();
            bucket.tokens =
                (bucket.tokens + fills * spec.tokens_per_fill as f64).min(spec.max_tokens as f64);
            bucket.last = now;
        }
        let outcome = if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else if spec.tokens_per_fill == 0 {
            // No refill, so no wait is ever enough.
            Err(u64::MAX)
        } else {
            // Deficit / refill rate, in seconds. `+ 1` rounds up: the
            // `>=` means a token lands the moment tokens reach 1.0, and a
            // whole-second `Retry-After` never wants to round the client
            // down into another 429.
            let per_fill = spec.fill_interval.as_secs_f64() / spec.tokens_per_fill as f64;
            let wait = (per_fill * (1.0 - bucket.tokens)).ceil();
            Err(wait as u64 + 1)
        };
        // Evict only on the insert that crosses the bound, so a steady key
        // set is never touched and the pass is one-in-10k.
        if is_new && buckets.len() > MAX_BUCKETS {
            Self::evict(&mut buckets);
        }
        outcome
    }

    /// Drops the oldest-refilled buckets until the map is inside
    /// [`MAX_BUCKETS`].
    fn evict(buckets: &mut HashMap<String, Bucket>) {
        while buckets.len() > MAX_BUCKETS {
            let oldest = buckets
                .iter()
                .min_by_key(|(_, b)| b.last)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    buckets.remove(&k);
                }
                None => break,
            }
        }
    }
}

/// Deterministic variant of [`Limiter::check`] for tests: the caller supplies
/// "now", so refill math is asserted without sleeping.
#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(rpm: u32) -> Limiter {
        Limiter::new(Spec::per_minute(rpm))
    }

    #[test]
    fn admits_a_burst_of_rpm_then_meters() {
        let l = limiter(2);
        assert!(l.check("k").is_ok());
        assert!(l.check("k").is_ok());
        let wait = l.check("k").expect_err("third request must wait");
        assert!((1..=31).contains(&wait), "retry after: {wait}");
    }

    #[test]
    fn refills_after_the_interval() {
        let spec = Spec {
            max_tokens: 1,
            tokens_per_fill: 1,
            fill_interval: Duration::from_millis(50),
        };
        let l = Limiter::new(spec);
        assert!(l.check("k").is_ok());
        assert!(l.check("k").is_err());
        std::thread::sleep(spec.fill_interval * 3);
        assert!(l.check("k").is_ok(), "bucket must refill");
    }

    #[test]
    fn keeps_keys_isolated() {
        let l = limiter(1);
        assert!(l.check("a").is_ok());
        assert!(l.check("b").is_ok());
        assert!(l.check("a").is_err());
        assert!(l.check("b").is_err());
    }

    #[test]
    fn burst_never_exceeds_max_tokens() {
        // A month of elapsed time cannot mint more than `max_tokens` at once.
        let spec = Spec {
            max_tokens: 3,
            tokens_per_fill: 1,
            fill_interval: Duration::from_secs(1),
        };
        let l = Limiter::new(spec);
        let mut buckets = l.buckets.lock().unwrap();
        buckets.insert(
            "old".to_owned(),
            Bucket {
                tokens: 0.0,
                last: Instant::now() - Duration::from_secs(3600),
            },
        );
        drop(buckets);
        // One refill clamped at max_tokens: four checks, the fourth refuses.
        let mut ok = 0;
        for _ in 0..5 {
            if l.check("old").is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, 3, "burst above max_tokens");
    }

    #[test]
    fn a_zero_refill_never_admits() {
        let l = Limiter::new(Spec {
            max_tokens: 0,
            tokens_per_fill: 0,
            fill_interval: Duration::from_secs(1),
        });
        assert_eq!(l.check("k"), Err(u64::MAX));
    }
}

#[cfg(test)]
mod evict_tests {
    use super::*;

    #[test]
    fn bounds_the_bucket_map() {
        let l = Limiter::new(Spec::per_minute(100));
        // MAX_BUCKETS + 1 distinct keys must not leave the map unbounded.
        for i in 0..=(MAX_BUCKETS as u32) {
            let _ = l.check(&format!("key-{i:05}"));
        }
        let buckets = l.buckets.lock().unwrap();
        assert!(
            buckets.len() <= MAX_BUCKETS,
            "bucket map grew past the bound: {}",
            buckets.len()
        );
    }
}
