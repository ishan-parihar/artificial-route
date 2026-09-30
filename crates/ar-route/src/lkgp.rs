//! Last-Known-Good-Provider pins: session → provider, with a TTL.
//!
//! Ported from `../OmniRoute/open-sse/services/combo/` —
//! `sessionStickiness.ts` + `recordLkgpPin.ts` + `staleLkgpClear.ts`.
//! The TTL is the whole point of the port: a pin that outlives the provider it
//! names is a routing bug that reports itself as a 502 much later.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::contract::{ProviderId, Strng};

/// Default pin lifetime. Long enough to cover a work session, short enough
/// that a dead provider stops being pinned within one working day.
pub const DEFAULT_LKGP_TTL: Duration = Duration::from_secs(30 * 60);

/// A single pin and the instant it goes stale.
#[derive(Clone, Debug)]
struct Pin {
    provider: ProviderId,
    stored_at: Instant,
}

/// Session-keyed last-known-good providers.
///
/// Cheap to share: one `Mutex<HashMap>` behind an `&self` borrow, so the
/// router can hold it without an `Arc` per pin. Pins are swept on write, not
/// on a background timer — a `HashMap` this small does not need a janitor
/// task, and sweeping on write keeps the map bounded by traffic.
#[derive(Debug)]
pub struct LkgpPins {
    pins: Mutex<HashMap<Strng, Pin>>,
    ttl: Duration,
}

impl LkgpPins {
    /// Builds an empty pin table with an explicit TTL.
    #[must_use]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            pins: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Builds an empty pin table with [`DEFAULT_LKGP_TTL`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_LKGP_TTL)
    }

    /// Records `provider` as the last known good for `session`.
    ///
    /// Called on success only (`staleLkgpClear`'s counterpart). Any previous
    /// pin is overwritten and the map is swept first, so a burst of new
    /// sessions cannot leak expired entries.
    pub fn record(&self, session: &str, provider: &ProviderId) {
        let Ok(mut pins) = self.pins.lock() else {
            // A poisoned pin table costs stickiness, not correctness: the
            // request that hit it already succeeded. Skip rather than panic a
            // live request.
            tracing::warn!("lkgp pin table poisoned, skipping record");
            return;
        };
        sweep(&mut pins, self.ttl);
        pins.insert(
            Strng::from(session),
            Pin {
                provider: provider.clone(),
                stored_at: Instant::now(),
            },
        );
    }

    /// Returns the live pin for `session`, sweeping expired entries.
    ///
    /// A poisoned table reads as "no pin", which degrades `Lkgp` to `Priority`.
    #[must_use]
    pub fn get(&self, session: &str) -> Option<ProviderId> {
        let mut pins = self.pins.lock().ok()?;
        sweep(&mut pins, self.ttl);
        pins
            .get(session)
            .map(|p| p.provider.clone())
    }

    /// Drops the pin for `session`. Called when the pinned provider fails, so a
    /// single bad turn does not strand the session on a dead provider.
    pub fn clear(&self, session: &str) {
        if let Ok(mut pins) = self.pins.lock() {
            pins.remove(session);
        }
    }

    /// Number of live pins, for `/metrics` and tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pins.lock().map_or(0, |mut p| {
            sweep(&mut p, self.ttl);
            p.len()
        })
    }

    /// Whether the pin table holds no live pins.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for LkgpPins {
    fn default() -> Self {
        Self::new()
    }
}

/// Drops every pin older than `ttl`.
///
/// `Instant::elapsed` cannot go backwards, so `checked_sub` is only ever
/// `Some` — but the API forces the check and `map_or(0, ...)` keeps that from
/// silently pinning a stale entry.
fn sweep(pins: &mut HashMap<Strng, Pin>, ttl: Duration) {
    let now = Instant::now();
    pins.retain(|_, p| now.saturating_duration_since(p.stored_at) < ttl);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::LkgpPins;
    use crate::contract::ProviderId;

    #[test]
    fn returns_pinned_provider_after_record() {
        let pins = LkgpPins::new();
        pins.record("s1", &ProviderId::new("groq"));
        assert_eq!(pins.get("s1").map(|p| p.as_str().to_owned()), Some("groq".to_owned()));
    }

    #[test]
    fn returns_none_when_session_unseen() {
        let pins = LkgpPins::new();
        assert!(pins.get("nope").is_none());
    }

    #[test]
    fn expires_pin_after_ttl() {
        let pins = LkgpPins::with_ttl(Duration::from_nanos(1));
        pins.record("s1", &ProviderId::new("groq"));
        std::thread::sleep(Duration::from_millis(2));
        assert!(pins.get("s1").is_none());
    }

    #[test]
    fn clears_pin_on_demand() {
        let pins = LkgpPins::new();
        pins.record("s1", &ProviderId::new("groq"));
        pins.clear("s1");
        assert!(pins.get("s1").is_none());
    }

    #[test]
    fn counts_only_live_pins() {
        let pins = LkgpPins::with_ttl(Duration::from_nanos(1));
        pins.record("s1", &ProviderId::new("groq"));
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(pins.len(), 0);
    }
}
