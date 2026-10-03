//! Exact response cache for Artificial Route: `blake3` keys, a 32 MB
//! `quick_cache` memory tier, a bounded `redb` file, a 24 h `Idempotency-Key`
//! store, and the prefix-pin affinity hook `ar-route` calls.
//!
//! Ported from `../OmniRoute/open-sse/` per `docs/02` §cache, with the hardened
//! spec of `docs/04` §cache. Four OmniRoute defects are fixed rather than
//! reproduced, and each is a reason this crate is not a transliteration:
//!
//! | Defect | Fix here |
//! |---|---|
//! | `cacheLayer.generateKey` sorts only top-level keys, and its "sort" is a `JSON.stringify` replacer array that drops nested keys | [`key::write_value`] sorts at every level; [`key`] has one canonical renderer |
//! | `LRUCache` bounds entries (50) *and* bytes (2 MB) separately, and an oversized single value drains the cache then inserts anyway | [`tier::MemTier`] bounds **bytes only**, via a [`quick_cache::Weighter`] |
//! | `idempotencyLayer` is an unbounded `Map` with a 5 s window | [`idempotency::IdempotencyStore`] is byte-capped with a 24 h window, and has the in-flight state the reference lacks |
//! | A client `Cache-Control: no-cache` overloads the cache instead of bypassing it | [`Cache::get`] takes a caller-decided [`CacheState::Bypass`]; this crate never reads request headers |
//!
//! # Bounded RSS
//!
//! Every allocation this crate can make is capped by construction, and the caps
//! are the ones `docs/00` budgets for:
//!
//! | Tier | Bound | Default |
//! |---|---|---|
//! | Memory | [`quick_cache::Weighter`] bytes, LRU-evicted | [`tier::DEFAULT_MEM_BYTES`] = 32 MB |
//! | Disk | logical bytes, swept then lossy | [`Config::disk_bytes`] = 256 MB |
//! | Idempotency | logical bytes, swept on write | [`idempotency::DEFAULT_IDEMPOTENCY_BYTES`] = 4 MB |
//! | Affinity | **none** -- [`affinity`] is a pure function and allocates nothing that outlives the call |
//!
//! So the ceiling is `32 MB + 4 MB` of anonymous memory plus whatever `redb`'s
//! page allocator holds, and the *file* settles at the disk cap plus a page of
//! slack because `redb` reuses freed pages. Nothing grows with traffic: the
//! memory tier evicts, the disk tier refuses, the idempotency store sweeps.
//!
//! ### Measured
//!
//! `cargo run -p ar-cache --release --example rss`, 8000 distinct 64 KB bodies
//! (512 MB pushed through a 32 MB cache), `VmRSS` from `/proc/self/status`:
//!
//! | state | bodies pushed in | entries | tier weight | `VmRSS` |
//! |---|---|---|---|---|
//! | baseline | 0 | 0 | 0 B | 2.5 MB |
//! | round 10 | 128 MB | 384 | 25 208 832 B | 27.0 MB |
//! | round 20 | 256 MB | 384 | 25 208 832 B | 27.2 MB |
//! | round 30 | 384 MB | 384 | 25 208 832 B | 27.3 MB |
//! | round 40 | 512 MB | 384 | 25 208 832 B | 27.3 MB |
//!
//! `VmRSS` is flat from round 10 onward after 4x more data has gone through,
//! and stays inside the `<35 MB` idle budget in `docs/00` with a disk tier and
//! an idempotency store attached. The settled weight is ~75% of the nominal
//! capacity because `quick_cache` evicts per shard and each shard reserves its
//! own headroom; the ceiling is a ceiling, not a fill target.
//!
//! `provisional:` re-run the example on the target machine. The absolute figure
//! moves with allocator arena sizing; the shape is the property.
//!
//! There is no compaction knob. See `tier`'s module docs.
//!
//! # What is deliberately absent
//!
//! **No semantic / vector cache.** `docs/02` defers `semanticCache.ts`'s
//! two-tier embedding side to P1-excluded work (`usearch` lands later, in its
//! own change). Caching "near enough" answers off an embedding index is a
//! different contract with different correctness properties, and folding it in
//! here would make "identical POST returns the identical body" untrue.
//!
//! # The two caches are separate on purpose
//!
//! [`Cache`] answers *"have I seen this exact request?"*. The `Idempotency-Key`
//! store answers *"is this the same client operation?"*. The reference keys them
//! differently (`sha256(model+messages+params)` vs `raw|provider|model|digest16`)
//! and they are not interchangeable: merging them would make a retried *failed*
//! request look like a cache hit for a *different* request that happened to
//! share a body.
//!
//! # Example
//!
//! ```
//! use ar_cache::{Cache, CacheConfig};
//!
//! let cache = Cache::with_config(CacheConfig::default()).expect("memory-only cache");
//! let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &serde_json::json!({
//!     "messages": [{"role": "user", "content": "hi"}]
//! }));
//!
//! assert_eq!(cache.get(&key).state, ar_cache::CacheState::Miss);
//! assert!(cache.store(&key, 200, "application/json", r#"{"ok":true}"#));
//! assert_eq!(cache.get(&key).state, ar_cache::CacheState::Hit);
//! ```

#![deny(missing_docs)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

pub mod affinity;
pub mod entry;
pub mod error;
pub mod idempotency;
pub mod key;
pub mod tier;

pub use entry::{Entry, NO_STORE, TTL_CLIENT_ERROR, TTL_SUCCESS, TtlPolicy, now_ms};
pub use error::CacheError;
pub use idempotency::{IdempotencyStore, Replay};
pub use key::CacheKey;
pub use tier::{DEFAULT_MEM_BYTES, DiskTier, MemTier};

/// The header `ar-server` sets. Kept here because this crate owns the verdict
/// and the server only serialises it.
pub const CACHE_HEADER: &str = "x-ar-cache";

/// What a lookup decided, as reported in [`CACHE_HEADER`].
///
/// Three states, not two: `hit`/`miss` alone cannot express "this request was
/// never eligible", and a `miss` that means "we decided not to cache" reads on
/// the client side as "we tried and failed", which is a different message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheState {
    /// The exact request was served from cache.
    Hit,
    /// Eligible, but not present.
    Miss,
    /// Not eligible. Streaming requests, and anything the caller scoped out.
    ///
    /// Emitted only by the caller: this crate reads no request headers, so it
    /// cannot be made to bypass itself. [`Cache::bypass`] is the constructor.
    Bypass,
}

impl CacheState {
    /// The header value for this verdict.
    #[must_use]
    pub const fn as_header(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Bypass => "bypass",
        }
    }
}

/// A lookup result: the verdict and, on a hit, the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lookup {
    /// What to report in [`CACHE_HEADER`].
    pub state: CacheState,
    /// The cached response. `Some` only when `state` is
    /// [`CacheState::Hit`].
    pub entry: Option<Entry>,
}

impl Lookup {
    /// A miss.
    #[must_use]
    pub fn miss() -> Self {
        Self { state: CacheState::Miss, entry: None }
    }

    /// A bypass.
    #[must_use]
    pub fn bypass() -> Self {
        Self { state: CacheState::Bypass, entry: None }
    }

    /// A hit carrying `entry`.
    #[must_use]
    pub fn hit(entry: Entry) -> Self {
        Self { state: CacheState::Hit, entry: Some(entry) }
    }

    /// The cached body, if this was a hit.
    #[must_use]
    pub fn body(&self) -> Option<&Bytes> {
        self.entry.as_ref().map(|e| &e.body)
    }
}

/// Everything tunable about a [`Cache`].
///
/// Defaults are memory-only. `docs/04` calls for a persistent `redb` file, but
/// a library that opens a file in the current directory the first time anyone
/// calls `Cache::new()` cannot be tested, cannot be embedded in a tool that
/// already owns its data directory, and cannot be run twice in one process
/// without a second failure mode. So the disk tier is opt-in by path via
/// [`CacheConfig::with_disk`], and the rest of the defaults are the `docs/04`
/// values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheConfig {
    /// Byte ceiling for the memory tier. [`DEFAULT_MEM_BYTES`] (32 MB) unless set.
    pub mem_bytes: u64,
    /// Where to keep the disk tier. `None` is memory-only.
    pub disk_path: Option<PathBuf>,
    /// Logical byte ceiling for the disk tier.
    pub disk_bytes: u64,
    /// Per-status-class TTLs. [`TtlPolicy::new`] (`200:5m 4xx:30s 5xx:no-store`)
    /// unless set.
    pub ttl: TtlPolicy,
    /// Idempotency window. 24 h unless set.
    pub idempotency_ttl: Duration,
    /// Idempotency byte ceiling.
    pub idempotency_bytes: u64,
}

/// Default disk cap: 256 MB.
///
/// Arbitrary but finite. It is a *disk* cap, not an RSS cap, so it does not
/// compete with the 32 MB memory budget -- it exists so a pathological client
/// cannot fill the volume the state directory lives on.
pub const DEFAULT_DISK_BYTES: u64 = 256 * 1024 * 1024;

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            mem_bytes: DEFAULT_MEM_BYTES,
            disk_path: None,
            disk_bytes: DEFAULT_DISK_BYTES,
            ttl: TtlPolicy::new(),
            idempotency_ttl: idempotency::DEFAULT_IDEMPOTENCY_TTL,
            idempotency_bytes: idempotency::DEFAULT_IDEMPOTENCY_BYTES,
        }
    }
}

impl CacheConfig {
    /// Memory-only at the default 32 MB.
    #[must_use]
    pub fn memory_only() -> Self {
        Self::default()
    }

    /// Enables the disk tier at `path`.
    #[must_use]
    pub fn with_disk(mut self, path: impl Into<PathBuf>) -> Self {
        self.disk_path = Some(path.into());
        self
    }

    /// Sets the memory byte ceiling.
    #[must_use]
    pub fn with_mem_bytes(mut self, bytes: u64) -> Self {
        self.mem_bytes = bytes;
        self
    }

    /// Sets the disk byte ceiling.
    #[must_use]
    pub fn with_disk_bytes(mut self, bytes: u64) -> Self {
        self.disk_bytes = bytes;
        self
    }

    /// Sets the TTL policy.
    #[must_use]
    pub fn with_ttl(mut self, ttl: TtlPolicy) -> Self {
        self.ttl = ttl;
        self
    }
}

/// A bounded, exact response cache.
///
/// Cheap to share: [`get`](Self::get) and [`store`](Self::store) take `&self`
/// and every field is internally synchronised, so the server holds one `Cache`
/// behind an `Arc` rather than passing `&mut` through a handler. `Send + Sync`
/// is asserted in `tests`.
#[derive(Debug)]
pub struct Cache {
    mem: MemTier,
    disk: Option<Arc<DiskTier>>,
    idempotency: IdempotencyStore,
    ttl: TtlPolicy,
}

impl Cache {
    /// Builds a cache from `config`.
    ///
    /// Fails only when a disk path was configured and the file could not be
    /// opened or created. A memory-only config cannot fail.
    pub fn with_config(config: CacheConfig) -> Result<Self, CacheError> {
        let disk = match &config.disk_path {
            Some(path) => Some(Arc::new(DiskTier::open(path, config.disk_bytes)?)),
            None => None,
        };
        Ok(Self::build(
            config.mem_bytes,
            disk,
            config.idempotency_ttl,
            config.idempotency_bytes,
            config.ttl,
        ))
    }

    /// Builds a memory-only cache at the default 32 MB.
    ///
    /// Infallible, so it is the constructor a caller with no state directory
    /// can reach without carrying a `Result` it cannot produce.
    #[must_use]
    pub fn memory_only() -> Self {
        let defaults = CacheConfig::default();
        Self::build(
            defaults.mem_bytes,
            None,
            defaults.idempotency_ttl,
            defaults.idempotency_bytes,
            defaults.ttl,
        )
    }

    /// The infallible tail of construction, shared by both constructors.
    ///
    /// Split out so [`Cache::memory_only`] needs neither a `Result` nor a
    /// `panic!` for an arm it cannot reach.
    fn build(
        mem_bytes: u64,
        disk: Option<Arc<DiskTier>>,
        idempotency_ttl: Duration,
        idempotency_bytes: u64,
        ttl: TtlPolicy,
    ) -> Self {
        Self {
            mem: MemTier::new(mem_bytes),
            disk,
            idempotency: IdempotencyStore::with_limits(idempotency_ttl, idempotency_bytes),
            ttl,
        }
    }

    /// Looks `key` up in the memory tier.
    ///
    /// Deliberately does **not** consult the disk tier: it does I/O, and this
    /// is on the hot path where every request passes. Use
    /// [`get_with_disk`](Self::get_with_disk) when the disk tier matters.
    ///
    /// Never fails. A tier that cannot answer reads as a miss, because a cache
    /// problem is not a request problem.
    #[must_use]
    pub fn get(&self, key: &CacheKey) -> Lookup {
        match self.mem.get(key, now_ms()) {
            Some(entry) => Lookup::hit(entry),
            None => Lookup::miss(),
        }
    }

    /// Looks `key` up, falling back to the disk tier off the async runtime.
    ///
    /// `redb` transactions are synchronous, so reading them on a runtime worker
    /// blocks a reactor thread for the duration of the read. This moves the
    /// fallback to `spawn_blocking` and leaves the memory tier -- which answers
    /// almost every hit -- entirely on the caller's thread.
    ///
    /// Takes the key by value because it crosses a `'static` boundary.
    pub async fn get_with_disk(&self, key: CacheKey) -> Lookup {
        if let Some(entry) = self.mem.get(&key, now_ms()) {
            return Lookup::hit(entry);
        }
        let Some(disk) = self.disk.clone() else {
            return Lookup::miss();
        };
        let now = now_ms();
        let probe = key.clone();
        match tokio::task::spawn_blocking(move || disk.get(&probe, now)).await {
            Ok(Ok(Some(entry))) => {
                // Promote to memory: the next request for this key should not
                // pay for the file again.
                self.mem.insert(&key, entry.clone(), now_ms());
                Lookup::hit(entry)
            }
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => Lookup::miss(),
        }
    }

    /// Stores a completed response.
    ///
    /// The TTL comes from [`Cache::ttl_policy`] for `status`, and a status
    /// whose TTL is [`NO_STORE`] is not stored at all -- `5xx:no-store` from
    /// `docs/04`. Returns whether it was stored, so a caller can log a
    /// deliberate non-write rather than guessing.
    ///
    /// A `4xx` *is* stored, for 30 s. It is usually the caller's own bug and
    /// changes on the next attempt, which is why the window is short -- but
    /// replaying it is still cheaper than re-running a validation round trip
    /// through three providers.
    ///
    /// # Do not call this for a truncated completion
    ///
    /// A response that stopped at the token ceiling is a *partial answer*. Serve
    /// it to the next identical request and the caller gets truncated output
    /// with a `200` and no way to tell. [`Cache::store_truncated`] is the named
    /// alternative and stores nothing.
    pub fn store(
        &self,
        key: &CacheKey,
        status: u16,
        content_type: &str,
        body: impl Into<Bytes>,
    ) -> bool {
        self.store_inner(key, status, content_type, body, None, false)
    }

    /// [`store`](Self::store) with a caller-requested TTL, clamped to the
    /// policy's.
    ///
    /// A request's cache-control headers may shorten an entry's life but never
    /// extend it past the operator's configured ceiling — the same clamp the
    /// reference added when its unbounded header let a client pin an entry far
    /// past the configured cache lifetime (`semanticCacheManager.ts`, #14484).
    /// `Duration::ZERO` here is a request asking to store nothing, and answers
    /// `false` without writing for exactly the reason [`Self::store_truncated`]
    /// does.
    pub fn store_with_ttl(
        &self,
        key: &CacheKey,
        status: u16,
        content_type: &str,
        body: impl Into<Bytes>,
        ttl: Duration,
    ) -> bool {
        self.store_inner(key, status, content_type, body, Some(ttl), false)
    }

    /// Declines to store a completion that was cut off at the token ceiling.
    ///
    /// Always returns `false` and writes nothing. It exists as a named method
    /// rather than a `truncated: bool` parameter because a bare boolean at a
    /// call site five arguments long is a boolean nobody reads.
    ///
    /// The reference guards this with
    /// `finish_reason ∈ {length, max_tokens}`
    /// (`../OmniRoute/src/lib/semanticCache.ts` `TRUNCATED_FINISH_REASONS`).
    pub fn store_truncated(
        &self,
        _key: &CacheKey,
        _status: u16,
        _content_type: &str,
        _body: impl Into<Bytes>,
    ) -> bool {
        false
    }

    /// Removes `key` from every tier. Returns whether it was cached.
    pub fn forget(&self, key: &CacheKey) -> bool {
        let hit = self.mem.remove(key).is_some();
        let disk_hit = match &self.disk {
            Some(disk) => disk.remove(key).ok().flatten().is_some(),
            None => false,
        };
        hit || disk_hit
    }

    /// A [`CacheState::Bypass`] lookup.
    ///
    /// The caller decides eligibility -- a streaming request, a request whose
    /// body the caller chose not to canonicalise -- and reports it through the
    /// same header as a hit or a miss.
    #[must_use]
    pub fn bypass() -> Lookup {
        Lookup::bypass()
    }

    /// The TTL policy in force.
    #[must_use]
    pub fn ttl_policy(&self) -> &TtlPolicy {
        &self.ttl
    }

    /// The memory tier.
    #[must_use]
    pub fn mem(&self) -> &MemTier {
        &self.mem
    }

    /// The disk tier, if one is configured.
    #[must_use]
    pub fn disk(&self) -> Option<&DiskTier> {
        self.disk.as_deref()
    }

    /// The `Idempotency-Key` store.
    #[must_use]
    pub fn idempotency(&self) -> &IdempotencyStore {
        &self.idempotency
    }

    fn store_inner(
        &self,
        key: &CacheKey,
        status: u16,
        content_type: &str,
        body: impl Into<Bytes>,
        ttl_override: Option<Duration>,
        truncated: bool,
    ) -> bool {
        let now = now_ms();
        let policy_ttl = self.ttl.for_status(status);
        // The caller's request can only shorten: `min` against the policy
        // ceiling, so a poisoned entry cannot be pinned past the configured
        // lifetime by a header that asked for more.
        let ttl = ttl_override.map_or(policy_ttl, |requested| requested.min(policy_ttl));
        if truncated || ttl == NO_STORE {
            return false;
        }
        let entry = Entry::new(status, content_type, body, now, ttl);
        let stored = self.mem.insert(key, entry.clone(), now);
        // The disk tier is best-effort: a full or broken tier must not turn a
        // successful response into an error.
        if let Some(disk) = &self.disk
            && let Err(err) = disk.insert(key, &entry, now)
        {
            tracing::warn!(error = %err, "cache disk write failed");
        }
        stored
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{CACHE_HEADER, Cache, CacheConfig, CacheState, Lookup};
    use crate::idempotency::{IdempotencyStore, Replay};
    use crate::key::CacheKey;

    /// `Send + Sync` audit on shared cache state (AGENTS.md §2).
    ///
    /// A compile-time assertion: if any field stops being `Send`/`Sync` -- a
    /// `Rc` in a tier, a `*mut` in a weigher, a non-`Sync` driver handle in the
    /// disk path -- this stops compiling rather than becoming a runtime
    /// "failed to spawn" on the first concurrent request.
    #[test]
    fn cache_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Cache>();
        assert_send_sync::<CacheConfig>();
        assert_send_sync::<IdempotencyStore>();
        assert_send_sync::<crate::tier::MemTier>();
        assert_send_sync::<crate::tier::DiskTier>();
        assert_send_sync::<CacheKey>();
        assert_send_sync::<Arc<Cache>>();
    }

    #[test]
    fn cache_state_renders_the_header_values_docs_04_names() {
        assert_eq!(CACHE_HEADER, "x-ar-cache");
        assert_eq!(CacheState::Hit.as_header(), "hit");
        assert_eq!(CacheState::Miss.as_header(), "miss");
        assert_eq!(CacheState::Bypass.as_header(), "bypass");
    }

    #[test]
    fn a_bypass_carries_no_entry() {
        assert_eq!(Cache::bypass().state, CacheState::Bypass);
        assert!(Cache::bypass().entry.is_none());
    }

    #[test]
    fn memory_only_construction_cannot_fail_the_builder() {
        let cache = Cache::with_config(CacheConfig::memory_only()).expect("memory-only");
        assert!(cache.disk().is_none());
    }

    #[test]
    fn config_with_disk_sets_the_path() {
        let cfg = CacheConfig::memory_only().with_disk("/tmp/ar-cache-test.redb");
        assert_eq!(cfg.disk_path.as_deref(), Some(std::path::Path::new("/tmp/ar-cache-test.redb")));
    }

    #[test]
    fn lookup_body_is_only_present_on_a_hit() {
        let hit = Lookup::hit(crate::Entry::new(
            200,
            "text/plain",
            "hi",
            0,
            std::time::Duration::from_secs(1),
        ));
        assert!(hit.body().is_some());
        assert!(Lookup::miss().body().is_none());
    }

    #[test]
    fn forget_reports_a_cached_key() {
        let cache = Cache::memory_only();
        let key = CacheKey::hash(b"k");
        assert!(!cache.forget(&key));
        cache.store(&key, 200, "text/plain", "x");
        assert!(cache.forget(&key));
        assert!(!cache.forget(&key));
    }

    #[test]
    fn replay_reaches_the_caller_through_the_facade() {
        let cache = Cache::memory_only();
        let entry = crate::Entry::new(
            200,
            "application/json",
            "{}",
            crate::now_ms(),
            std::time::Duration::from_secs(60),
        );
        assert_eq!(cache.idempotency().begin("t", "k"), Replay::Proceed);
        cache.idempotency().complete("t", "k", entry.clone());
        assert_eq!(cache.idempotency().begin("t", "k"), Replay::Cached(entry));
    }
}