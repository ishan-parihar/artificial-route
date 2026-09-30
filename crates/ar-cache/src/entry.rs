//! Cached response bodies and the TTL policy `docs/04` fixes per status class.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;

/// Fixed codec header: `status:u16 | content_type_len:u32 | expires_at_ms:u64`.
pub(crate) const HEADER_LEN: usize = 2 + 4 + 8;

/// TTL for a `2xx` response: `200:5m` in `docs/04`.
pub const TTL_SUCCESS: Duration = Duration::from_secs(5 * 60);
/// TTL for a `4xx` response: `4xx:30s` in `docs/04`.
///
/// Short on purpose. A client error is frequently the caller's own bug and
/// changes on the next attempt; caching it for the success window would answer
/// a corrected request with the previous mistake.
pub const TTL_CLIENT_ERROR: Duration = Duration::from_secs(30);
/// The `5xx:no-store` marker from `docs/04`. A [`Duration::ZERO`] TTL means
/// "do not store", which is why it is a TTL rather than a separate flag.
pub const NO_STORE: Duration = Duration::ZERO;

/// One cached response.
///
/// Bodies are [`Bytes`], so a hit hands the caller a refcount bump and the
/// hot path never copies a payload that can be hundreds of kilobytes.
///
/// Not `serde`: the disk tier encodes this by hand (see
/// [`crate::tier`]) because `serde_json` renders a `Bytes` as an array of
/// decimal integers, which is four bytes of JSON per body byte and makes the
/// disk byte cap mean something other than what it says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Upstream status code, replayed verbatim.
    pub status: u16,
    /// Value for the response's `content-type`, if the caller supplies one.
    pub content_type: String,
    /// The response body.
    pub body: Bytes,
    /// Absolute wall-clock expiry, Unix milliseconds.
    ///
    /// Wall clock rather than [`std::time::Instant`] because the same field
    /// serialises into the `redb` tier, and an `Instant` has no cross-process
    /// meaning. The cost is one `SystemTime::now()` per operation (~25 ns),
    /// paid once per request rather than per cache shard.
    pub expires_at_ms: u64,
}

impl Entry {
    /// Builds an entry expiring at `now + ttl`.
    ///
    /// A zero `ttl` yields an already-expired entry, which is how
    /// "no-store" is represented without a separate flag: the caller simply
    /// never inserts it.
    #[must_use]
    pub fn new(
        status: u16,
        content_type: &str,
        body: impl Into<Bytes>,
        now: u64,
        ttl: Duration,
    ) -> Self {
        Self {
            status,
            content_type: content_type.to_owned(),
            body: body.into(),
            expires_at_ms: now.saturating_add(ttl.as_millis() as u64),
        }
    }

    /// Encodes into the fixed-header layout: `HEADER_LEN` bytes, then
    /// `content_type`, then `body`.
    ///
    /// Hand-rolled because the alternative is wrong, not merely verbose:
    /// `serde_json` serialises [`Bytes`] via `serialize_bytes`, which
    /// `serde_json` renders as `[110,121,44,...]` -- roughly 4x the payload,
    /// so a "256 MB" disk cap would hold ~64 MB of actual responses and the
    /// accounting would not match the file.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let ct = self.content_type.as_bytes();
        let mut out = Vec::with_capacity(HEADER_LEN + ct.len() + self.body.len());
        out.extend_from_slice(&self.status.to_le_bytes());
        out.extend_from_slice(&(ct.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.expires_at_ms.to_le_bytes());
        out.extend_from_slice(ct);
        out.extend_from_slice(&self.body);
        out
    }

    /// Decodes a row written by [`Entry::encode`].
    pub(crate) fn decode(raw: &[u8]) -> Result<Self, crate::error::CacheError> {
        use crate::error::CacheError;

        if raw.len() < HEADER_LEN {
            return Err(CacheError::Codec("row shorter than the fixed header"));
        }
        let status = u16::from_le_bytes([raw[0], raw[1]]);
        let ct_len = u32::from_le_bytes([raw[2], raw[3], raw[4], raw[5]]) as usize;
        let expires_at_ms = u64::from_le_bytes([
            raw[6], raw[7], raw[8], raw[9], raw[10], raw[11], raw[12], raw[13],
        ]);
        let payload = raw.len() - HEADER_LEN;
        if ct_len > payload {
            return Err(CacheError::Codec("content_type length overruns the row"));
        }
        let (ct, body) = raw[HEADER_LEN..].split_at(ct_len);
        Ok(Self {
            status,
            content_type: String::from_utf8_lossy(ct).into_owned(),
            body: Bytes::copy_from_slice(body),
            expires_at_ms,
        })
    }

    /// Whether this entry is past its expiry at `now`.
    ///
    /// `>=`, not `>`: a zero TTL is then expired at the instant it was
    /// created, so "no-store" cannot be stored by accident.
    #[must_use]
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at_ms
    }
}

/// Per-status-class TTL policy.
///
/// Three numbers, not a config tree. The `docs/04` spec is three numbers, and
/// a knob nobody has turned is a knob that only gets turned wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TtlPolicy {
    /// TTL for `2xx`.
    pub success: Duration,
    /// TTL for `4xx`.
    pub client_error: Duration,
    /// TTL for everything else -- `5xx`, and any `1xx`/`3xx` a provider invents.
    /// [`NO_STORE`] by default.
    pub other: Duration,
}

impl TtlPolicy {
    /// The `docs/04` policy: `200:5m 4xx:30s 5xx:no-store`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            success: TTL_SUCCESS,
            client_error: TTL_CLIENT_ERROR,
            other: NO_STORE,
        }
    }

    /// The TTL `status` earns, or [`NO_STORE`].
    ///
    /// `2xx` and `4xx` are matched on the class, so `207` and `429` are covered
    /// without enumerating codes. Everything else is "other", which defaults to
    /// no-store: an unrecognised class is more likely to be a provider fault
    /// than a cacheable answer.
    #[must_use]
    pub const fn for_status(&self, status: u16) -> Duration {
        match status {
            200..=299 => self.success,
            400..=499 => self.client_error,
            _ => self.other,
        }
    }
}

impl Default for TtlPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// Unix milliseconds, saturating at 0 if the clock is before the epoch.
///
/// Saturating rather than `unwrap_or_else(panic)`: a clock this far wrong is
/// not a reason to fail a request, and `0` makes every entry look expired,
/// which is the safe direction for a cache.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Entry, NO_STORE, TtlPolicy, now_ms};

    #[test]
    fn policy_gives_five_minutes_to_a_200() {
        assert_eq!(TtlPolicy::new().for_status(200), Duration::from_secs(300));
    }

    #[test]
    fn policy_gives_thirty_seconds_to_a_400() {
        assert_eq!(TtlPolicy::new().for_status(400), Duration::from_secs(30));
    }

    #[test]
    fn policy_refuses_to_store_a_503() {
        assert_eq!(TtlPolicy::new().for_status(503), NO_STORE);
    }

    #[test]
    fn policy_matches_the_class_not_the_code() {
        let policy = TtlPolicy::new();
        assert_eq!(policy.for_status(429), policy.for_status(400));
        assert_eq!(policy.for_status(201), policy.for_status(200));
    }

    #[test]
    fn zero_ttl_entry_is_expired_at_its_own_creation_instant() {
        let entry = Entry::new(200, "application/json", "{}", 1_000, NO_STORE);
        assert!(entry.is_expired(1_000));
    }

    #[test]
    fn entry_survives_one_millisecond_before_its_expiry() {
        let entry = Entry::new(200, "text/plain", "x", 1_000, Duration::from_millis(5));
        assert!(!entry.is_expired(1_004));
    }

    #[test]
    fn clock_never_reads_zero_after_boot() {
        // Sanity on `now_ms`: an `unwrap_or(0)`-shaped bug here would expire
        // every entry instantly, which looks exactly like a broken cache.
        assert!(now_ms() > 1_700_000_000_000);
    }
}
