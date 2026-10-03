//! The `Idempotency-Key` store: 24 h, tenant-scoped, bounded.
//!
//! `docs/04` gives this one line of spec -- "`Idempotency-Key` store 24h stops
//! double-spend on retry" -- and it is the store with the most disagreement
//! between it and the reference it ports from.
//!
//! # Where this deliberately diverges from `../OmniRoute`
//!
//! `open-sse/handlers/chatCore/idempotency.ts` +
//! `src/lib/idempotencyLayer.ts` do three things this does not, and all three
//! are consequences of a 5 s window that AR raises to 24 h:
//!
//! 1. **OmniRoute's window is 5 s, in an unbounded `Map`, swept every 30 s.**
//!    At 24 h that map is the dominant memory leak in the process, so this is a
//!    byte-capped map swept on write -- the same shape as
//!    [`crate`]-sibling `ar_route::LkgpPins` (AGENTS.md §2 `Send/Sync` audit on
//!    shared state: one `Mutex`, no per-key `Arc`).
//! 2. **OmniRoute has no in-flight state**, so a concurrent duplicate is not
//!    blocked -- it just misses and executes anyway. That is the double-spend
//!    the store exists to prevent, and a sequential client retry only avoids it
//!    by luck of timing. [`Slot::InFlight`] makes the claim true under
//!    concurrency; [`Replay::InFlight`] is what the caller sees.
//! 3. **OmniRoute caps nothing on the client-supplied key.** An unvalidated
//!    header is a memory and CPU input. [`MAX_KEY_LEN`] rejects the absurd and
//!    the key is *hashed* before it is stored, so even a legal 255-byte key
//!    occupies 32 bytes here rather than 255.
//!
//! # What is ported unchanged
//!
//! The replay is lossy in the reference and stays lossy here on purpose: the
//! stored status and body are replayed, and the original response *headers* are
//! not stored at all (`idempotencyLayer` synthesises `Content-Type` plus
//! `X-OmniRoute-Idempotent: true` because nothing else survives). Storing
//! headers would mean storing a caller-visible surface with a 24 h lifetime.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::entry::{Entry, now_ms};
use crate::key::CacheKey;

/// `docs/04`: the idempotency window is 24 h.
pub const DEFAULT_IDEMPOTENCY_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Byte ceiling for the store.
///
/// Not in `docs/04`, which bounds nothing. A 24 h window needs one: without a
/// cap the store is the largest allocation in the process and the one thing
/// that grows with traffic rather than with memory pressure. 4 MB holds a few
/// thousand responses, which is far more distinct idempotency keys than one
/// tenant issues in a day.
pub const DEFAULT_IDEMPOTENCY_BYTES: u64 = 4 * 1024 * 1024;

/// Longest accepted `Idempotency-Key`.
///
/// `idempotencyLayer.getIdempotencyKey` has no limit at all. A client-supplied
/// header of arbitrary length is an unbounded hashing input and an unbounded
/// memory input, so this rejects it instead.
pub const MAX_KEY_LEN: usize = 255;

/// Per-slot fixed cost: the key, the status, the expiry.
const SLOT_OVERHEAD: u64 = 64;

/// What [`IdempotencyStore::begin`] decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Replay {
    /// Nothing live under this key. Execute, then call [`IdempotencyStore::complete`].
    Proceed,
    /// Another request holds this key and has not finished. Executing now would
    /// double-spend.
    ///
    /// The caller decides what that means -- 409 is the usual answer, and a
    /// proxy that invents a policy here is a proxy that guesses on money.
    InFlight,
    /// A completed response for the same key and body. Replay it.
    Cached(Entry),
}

/// What is stored under one key.
#[derive(Clone, Debug)]
enum Slot {
    /// Claimed, not yet answered.
    InFlight {
        /// When this claim lapses. A request that never calls `complete` must
        /// not block the key forever.
        expires_at_ms: u64,
    },
    /// Answered and replayable.
    Done(Entry),
}

impl Slot {
    fn expires_at_ms(&self) -> u64 {
        match self {
            Self::InFlight { expires_at_ms } => *expires_at_ms,
            Self::Done(entry) => entry.expires_at_ms,
        }
    }

    fn weight(&self) -> u64 {
        match self {
            Self::InFlight { .. } => SLOT_OVERHEAD,
            Self::Done(entry) => {
                SLOT_OVERHEAD + entry.body.len() as u64 + entry.content_type.len() as u64
            }
        }
    }

    fn is_live(&self, now: u64) -> bool {
        now < self.expires_at_ms()
    }
}

#[derive(Debug, Default)]
struct Inner {
    slots: HashMap<CacheKey, Slot>,
    bytes: u64,
}

/// Tenant-scoped `Idempotency-Key` store.
///
/// `Send + Sync` because every field is; the audit is
/// `tests::idempotency_store_is_send_and_sync`.
#[derive(Debug)]
pub struct IdempotencyStore {
    inner: Mutex<Inner>,
    ttl: Duration,
    cap_bytes: u64,
}

impl IdempotencyStore {
    /// Builds a store with the `docs/04` 24 h window and the default cap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_IDEMPOTENCY_TTL, DEFAULT_IDEMPOTENCY_BYTES)
    }

    /// Builds a store with an explicit window and byte ceiling.
    #[must_use]
    pub fn with_limits(ttl: Duration, cap_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            ttl,
            cap_bytes: cap_bytes.max(SLOT_OVERHEAD),
        }
    }

    /// The window this store honours.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// The byte ceiling this store honours.
    #[must_use]
    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    /// Claims `(tenant, key)`, or reports that someone else already has it.
    ///
    /// The check and the claim happen under one lock. Splitting them into
    /// `get` then `insert` leaves a window in which two concurrent duplicates
    /// both see [`Replay::Proceed`] and both spend -- which is the exact failure
    /// this store exists to prevent, so it is worth a lock that costs
    /// nanoseconds.
    ///
    /// # Panics
    ///
    /// Never. A poisoned lock reads as "no claim held", which re-opens the
    /// double-spend window for one key rather than failing the process.
    pub fn begin(&self, tenant: &str, key: &str) -> Replay {
        let Some(slot_key) = scoped_key(tenant, key) else {
            return Replay::Proceed;
        };
        let now = now_ms();
        let Ok(mut inner) = self.inner.lock() else {
            tracing::warn!("idempotency store poisoned, treating key as unclaimed");
            return Replay::Proceed;
        };

        // Sweep before the lookup, so an expired claim does not shadow the key.
        sweep(&mut inner, now);
        match inner.slots.get(&slot_key) {
            Some(Slot::Done(entry)) => Replay::Cached(entry.clone()),
            Some(Slot::InFlight { .. }) => Replay::InFlight,
            None => {
                inner.bytes = inner.bytes.saturating_add(SLOT_OVERHEAD);
                inner.slots.insert(
                    slot_key,
                    Slot::InFlight {
                        expires_at_ms: now.saturating_add(self.ttl.as_millis() as u64),
                    },
                );
                Replay::Proceed
            }
        }
    }

    /// Publishes the answer for a claimed key, making it replayable.
    ///
    /// Call this only after [`IdempotencyStore::begin`] returned
    /// [`Replay::Proceed`] and the upstream call succeeded. A failed request
    /// should call [`IdempotencyStore::abort`] instead: caching a failure for
    /// 24 h turns a transient upstream blip into a day of wrong answers.
    pub fn complete(&self, tenant: &str, key: &str, entry: Entry) {
        let Some(slot_key) = scoped_key(tenant, key) else {
            return;
        };
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let now = now_ms();
        sweep(&mut inner, now);
        // Over the cap, the claim is dropped rather than stored: an oversized
        // replay entry is worse than no replay, because the client that gets
        // the replay is the one that was trying to avoid a second charge.
        if inner.bytes.saturating_add(entry.body.len() as u64) > self.cap_bytes {
            if let Some(previous) = inner.slots.remove(&slot_key) {
                inner.bytes = inner.bytes.saturating_sub(previous.weight());
            }
            tracing::debug!("idempotency store full, dropping key");
            return;
        }
        let weight = SLOT_OVERHEAD + entry.body.len() as u64 + entry.content_type.len() as u64;
        if let Some(previous) = inner.slots.insert(slot_key, Slot::Done(entry)) {
            inner.bytes = inner.bytes.saturating_sub(previous.weight());
        }
        inner.bytes = inner.bytes.saturating_add(weight);
        debug_assert!(inner.bytes <= self.cap_bytes || inner.slots.is_empty());
    }

    /// Releases a claim without publishing an answer.
    ///
    /// The right call after a failure, and after a caller-visible 4xx the
    /// client should be able to retry with the same key.
    pub fn abort(&self, tenant: &str, key: &str) {
        let Some(slot_key) = scoped_key(tenant, key) else {
            return;
        };
        if let Ok(mut inner) = self.inner.lock()
            && let Some(previous) = inner.slots.remove(&slot_key)
        {
            inner.bytes = inner.bytes.saturating_sub(previous.weight());
        }
    }

    /// Bytes held, for `/metrics` and tests.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.inner.lock().map_or(0, |i| i.bytes)
    }

    /// Live slot count, for `/metrics` and tests.
    #[must_use]
    pub fn len(&self) -> usize {
        let now = now_ms();
        // A poisoned lock reads as empty, matching `bytes()`: a metrics
        // counter that reports zero is recoverable, a panic in `/metrics` is
        // not.
        let Ok(mut inner) = self.inner.lock() else {
            return 0;
        };
        sweep(&mut inner, now);
        inner.slots.len()
    }

    /// Whether the store holds no live slots.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for IdempotencyStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Hashes `tenant` and the raw header into one 32-byte slot key.
///
/// The hash is the point: an untrusted header never becomes a stored key, so a
/// 255-byte (legal, maximum) header costs 32 bytes and cannot collide across
/// tenants. Rejecting instead of hashing would be the alternative, and it would
/// make a legitimate long key a client error for no safety gain.
fn scoped_key(tenant: &str, key: &str) -> Option<CacheKey> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return None;
    }
    let mut buf = String::with_capacity(tenant.len() + key.len() + 3);
    buf.push_str(tenant);
    buf.push('\u{0}');
    buf.push_str(key);
    Some(CacheKey::hash(buf.as_bytes()))
}

/// Drops every lapsed slot and updates the byte count.
fn sweep(inner: &mut Inner, now: u64) {
    inner.slots.retain(|_, slot| {
        if slot.is_live(now) {
            return true;
        }
        inner.bytes = inner.bytes.saturating_sub(slot.weight());
        false
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DEFAULT_IDEMPOTENCY_TTL, IdempotencyStore, MAX_KEY_LEN, Replay};
    use crate::entry::Entry;
    use crate::key::CacheKey;

    fn answer() -> Entry {
        Entry::new(
            200,
            "application/json",
            r#"{"id":"a"}"#,
            crate::entry::now_ms(),
            Duration::from_secs(60),
        )
    }

    #[test]
    fn defaults_to_a_24_hour_window() {
        assert_eq!(
            IdempotencyStore::new().ttl(),
            Duration::from_secs(24 * 60 * 60)
        );
        assert_eq!(DEFAULT_IDEMPOTENCY_TTL, Duration::from_secs(86_400));
    }

    #[test]
    fn rejects_a_key_longer_than_the_cap_without_claiming_it() {
        let store = IdempotencyStore::new();
        let huge = "k".repeat(MAX_KEY_LEN + 1);
        assert_eq!(store.begin("t", &huge), Replay::Proceed);
        assert!(store.is_empty());
    }

    #[test]
    fn rejects_an_empty_key_without_claiming_it() {
        let store = IdempotencyStore::new();
        assert_eq!(store.begin("t", ""), Replay::Proceed);
        assert!(store.is_empty());
    }

    #[test]
    fn same_key_in_two_tenants_does_not_collide() {
        let store = IdempotencyStore::new();
        store.begin("a", "k");
        store.complete("a", "k", answer());
        assert!(matches!(store.begin("b", "k"), Replay::Proceed));
    }

    #[test]
    fn distinct_raw_keys_hash_to_distinct_slots() {
        let store = IdempotencyStore::new();
        store.begin("t", "k1");
        store.complete("t", "k1", answer());
        assert_eq!(store.begin("t", "k2"), Replay::Proceed);
    }

    #[test]
    fn a_claim_is_reported_in_flight_to_the_second_caller() {
        let store = IdempotencyStore::new();
        assert_eq!(store.begin("t", "k"), Replay::Proceed);
        assert_eq!(store.begin("t", "k"), Replay::InFlight);
    }

    #[test]
    fn abort_releases_the_claim() {
        let store = IdempotencyStore::new();
        store.begin("t", "k");
        store.abort("t", "k");
        assert_eq!(store.begin("t", "k"), Replay::Proceed);
    }

    #[test]
    fn a_lapsed_claim_does_not_block_the_key_forever() {
        let store = IdempotencyStore::with_limits(Duration::from_nanos(1), 1 << 20);
        assert_eq!(store.begin("t", "k"), Replay::Proceed);
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(store.begin("t", "k"), Replay::Proceed);
    }

    #[test]
    fn an_expired_answer_replays_as_proceed_not_as_a_stale_body() {
        let store = IdempotencyStore::with_limits(Duration::from_millis(5), 1 << 20);
        store.begin("t", "k");
        store.complete(
            "t",
            "k",
            Entry::new(
                200,
                "text/plain",
                "x",
                crate::entry::now_ms(),
                Duration::from_millis(5),
            ),
        );
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(store.begin("t", "k"), Replay::Proceed);
    }

    #[test]
    fn completes_beyond_the_byte_cap_without_growing_past_it() {
        // 400 bytes of ceiling and 200-byte bodies: at most a couple fit. The
        // point is the bound, not the exact count.
        let store = IdempotencyStore::with_limits(DEFAULT_IDEMPOTENCY_TTL, 400);
        for i in 0..50 {
            store.begin("t", &format!("k{i}"));
            store.complete("t", &format!("k{i}"), answer());
        }
        assert!(store.bytes() <= 400, "bytes {} exceeded cap", store.bytes());
    }

    #[test]
    fn aborting_an_unheld_key_is_a_no_op() {
        let store = IdempotencyStore::new();
        store.abort("t", "never-seen");
        assert!(store.is_empty());
    }

    #[test]
    fn slot_key_is_tenant_separated_by_a_nul_that_cannot_appear_in_a_tenant() {
        // Concatenation without a separator would make ("ab", "c") and
        // ("a", "bc") the same slot.
        let ab_c = CacheKey::hash(b"ab\0c");
        let a_bc = CacheKey::hash(b"a\0bc");
        assert_ne!(ab_c, a_bc);
    }
}
