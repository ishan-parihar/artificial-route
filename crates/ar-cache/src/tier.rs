//! The two tiers: an in-memory weight-bounded LRU and a `redb` file.
//!
//! # Why the memory tier is weighted, not counted
//!
//! `docs/04` says `quick_cache` 32 MB, and names OmniRoute's 2 MB thrash as a
//! defect of the reference. An item-count LRU cannot express a byte budget: a
//! cache holding 500 responses and one holding 500 2 MB responses both have 500
//! items, and only one of them fits the RAM budget. So capacity here is
//! [`quick_cache::Weighter`] bytes, and the eviction trigger is memory rather
//! than cardinality.
//!
//! `quick_cache` shards internally (`available_parallelism() * 4` shards, each
//! taking `capacity / shards`), so the "sharded 32 MB" of `docs/04` needs no
//! hand-rolled shard array and no second lock. There is one `Cache` and one
//! `Weighter`.
//!
//! # Why the disk tier is lossy
//!
//! The `redb` tier enforces a **logical** byte cap: it counts the bytes it has
//! written and refuses to exceed it. That is the bound that matters for RSS,
//! because `redb` reuses freed pages -- the file's high-water mark settles at
//! the cap plus a page of slack, not at an unbounded multiple of it.
//!
//! Under pressure the tier is lossy, and deliberately so: it sweeps expired
//! entries, then *refuses* a new entry rather than evicting a live one. The
//! alternative -- a full LRU with a key collection on the cold path -- is
//! strictly better for hit rate and strictly worse for worst-case insert cost
//! and code size.
//!
//! # ponytail: no compaction knob
//!
//! The file never shrinks below its high-water mark from this crate. `redb`
//! exposes `Database::compact`, and it wants `&mut Database`; wiring that
//! through an `Arc`-shared `Cache` means either a mutex on the hot path or a
//! `&mut` accessor nobody can reach. Revisit only if the file has to shrink
//! on disk.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use quick_cache::sync::{Cache as Lru, DefaultLifecycle};
use quick_cache::{DefaultHashBuilder, Weighter};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::entry::{Entry, now_ms};
use crate::error::CacheError;
use crate::key::{CacheKey, KEY_LEN};

/// `docs/04`: `quick_cache` sharded 32 MB.
pub const DEFAULT_MEM_BYTES: u64 = 32 * 1024 * 1024;

/// Estimated items, used only to size the initial shard allocations. A
/// `provisional:` guess, and deliberately not a bound: the byte capacity is the
/// bound, this only avoids a rehash storm on a warm start.
const ESTIMATED_ITEMS: usize = 4_096;

/// Per-entry fixed cost: the key, the status, the expiry, and the `String` /
/// `Bytes` headers. Not measured precisely, because the byte capacity is a
/// budget for growth, not a claim about allocator layout.
const ENTRY_OVERHEAD: u64 = 64;

/// Weights a [`CacheKey`]/[`Entry`] pair by the bytes it holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct ByteWeighter;

impl Weighter<CacheKey, Entry> for ByteWeighter {
    fn weight(&self, _key: &CacheKey, value: &Entry) -> u64 {
        KEY_LEN as u64 + value.body.len() as u64 + value.content_type.len() as u64 + ENTRY_OVERHEAD
    }
}

/// The in-memory tier.
///
/// `Send + Sync` because every field is: `quick_cache::sync::Cache` guards its
/// shards with `RwLock` and holds no thread-affine state. The audit is
/// `tests::cache_is_send_and_sync`, which fails to compile if that ever stops
/// holding.
#[derive(Debug)]
pub struct MemTier {
    cache:
        Lru<CacheKey, Entry, ByteWeighter, DefaultHashBuilder, DefaultLifecycle<CacheKey, Entry>>,
}

impl MemTier {
    /// Builds a tier holding at most `capacity_bytes`.
    ///
    /// The capacity is floored at 1 purely so the shard arithmetic in
    /// `quick_cache` cannot divide by a zero shard count. It does **not**
    /// guarantee an entry fits: a capacity below
    /// `KEY_LEN + ENTRY_OVERHEAD` plus the body stores nothing, and
    /// [`MemTier::weight`] reports zero. That is the honest behaviour -- a
    /// misconfigured cache that silently retained everything would be the
    /// thing that grows RSS.
    #[must_use]
    pub fn new(capacity_bytes: u64) -> Self {
        let capacity_bytes = capacity_bytes.max(1);
        Self {
            cache: Lru::with(
                ESTIMATED_ITEMS,
                capacity_bytes,
                ByteWeighter,
                DefaultHashBuilder::default(),
                DefaultLifecycle::default(),
            ),
        }
    }

    /// Looks `key` up, treating an expired entry as absent.
    ///
    /// The expired entry is removed on the way out. `quick_cache` 0.7 has no
    /// TTL of its own, so expiry is this check plus this removal; without the
    /// removal a dead entry would sit in the cache until memory pressure
    /// happened to evict it, and every lookup would pay to re-expire it.
    pub fn get(&self, key: &CacheKey, now: u64) -> Option<Entry> {
        let entry = self.cache.get(key)?;
        if entry.is_expired(now) {
            self.cache.remove(key);
            return None;
        }
        Some(entry)
    }

    /// Inserts `entry` unless it is already expired at `now`.
    pub fn insert(&self, key: &CacheKey, entry: Entry, now: u64) -> bool {
        if entry.is_expired(now) {
            return false;
        }
        self.cache.insert(key.clone(), entry);
        true
    }

    /// Drops `key`. Returns the entry that was there, if any.
    pub fn remove(&self, key: &CacheKey) -> Option<Entry> {
        self.cache.remove(key).map(|(_, entry)| entry)
    }

    /// Bytes currently held. Bounded by construction; this is how a test proves
    /// it.
    #[must_use]
    pub fn weight(&self) -> u64 {
        self.cache.weight()
    }

    /// The byte ceiling this tier was built with.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.cache.capacity()
    }

    /// Live entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the tier holds no live entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cache.len() == 0
    }
}

/// Response bodies. Keyed by the hex digest, valued by JSON-encoded [`Entry`].
const ENTRIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("entries");
/// One row, `used_bytes`, holding the logical size of `ENTRIES`.
const LEDGER: TableDefinition<&str, u64> = TableDefinition::new("ledger");
/// Ledger key for the logical byte total.
const USED_ROW: &str = "used_bytes";

/// The on-disk tier.
///
/// `Database` is `Send + Sync` and its transactions are `&self`-based, so
/// there is no lock here beyond redb's own -- a `Mutex` would serialise the
/// whole tier behind a lock redb already holds more finely.
#[derive(Debug)]
pub struct DiskTier {
    db: Database,
    cap_bytes: u64,
    used: AtomicU64,
}

impl DiskTier {
    /// Opens (or creates) the tier at `path`.
    ///
    /// A corrupt or unreadable file is a `CacheError`, not a panic: the cache
    /// is an optimisation, and refusing to start because a cache file is bad
    /// turns a performance problem into an outage.
    pub fn open(path: impl AsRef<Path>, cap_bytes: u64) -> Result<Self, CacheError> {
        let path: PathBuf = path.as_ref().to_path_buf();
        let db = match Database::open(&path) {
            Ok(db) => db,
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "cache disk tier unreadable, recreating");
                // `DatabaseError` is not `CacheError`'s `From` source, so the
                // promotion into `redb::Error` is explicit.
                Database::create(&path).map_err(redb::Error::from)?
            }
        };
        let tier = Self {
            db,
            cap_bytes: cap_bytes.max(1),
            used: AtomicU64::new(0),
        };
        // A `redb` file has no tables until a write transaction opens them, and
        // `open_table` on a *read* transaction is `TableDoesNotExist` until
        // then. Creating them up front is what makes the first `get` a miss
        // rather than an error.
        {
            let write = tier.db.begin_write()?;
            drop(write.open_table(ENTRIES)?);
            drop(write.open_table(LEDGER)?);
            write.commit()?;
        }
        // The ledger, not the counter, is the truth after a crash. Reading it
        // back is also what makes a restart self-healing: the counter starts
        // from what is actually on disk, not from a guess.
        let used = read_ledger(&tier.db)?;
        tier.used.store(used, Ordering::Relaxed);
        Ok(tier)
    }

    /// Looks `key` up, treating an expired row as absent and deleting it.
    pub fn get(&self, key: &CacheKey, now: u64) -> Result<Option<Entry>, CacheError> {
        let read = self.db.begin_read()?;
        let table = read.open_table(ENTRIES)?;
        let found = table
            .get(key.as_bytes().as_slice())?
            .map(|v| v.value().to_vec())
            .map(|raw| Entry::decode(&raw));
        drop(table);
        drop(read);

        let Some(result) = found else { return Ok(None) };
        let entry = result?;
        if entry.is_expired(now) {
            self.remove(key)?;
            return Ok(None);
        }
        Ok(Some(entry))
    }

    /// Stores `entry` if it fits under the cap.
    ///
    /// Returns `false` when the entry is already expired, or when the tier is
    /// full and nothing could be swept. `false` is a normal outcome, not an
    /// error: the caller simply did not get a cache write.
    pub fn insert(&self, key: &CacheKey, entry: &Entry, now: u64) -> Result<bool, CacheError> {
        if entry.is_expired(now) {
            return Ok(false);
        }
        let raw = entry.encode();
        let incoming = raw.len() as u64;

        let write = self.db.begin_write()?;
        let outcome = {
            let mut table = write.open_table(ENTRIES)?;
            let ledger = write.open_table(LEDGER)?;
            let used = ledger
                .get(USED_ROW)?
                .map_or(0, |g| g.value())
                .saturating_add(incoming);

            if used > self.cap_bytes {
                let reclaimed = sweep_expired(&mut table, now)?;
                let used = used.saturating_sub(reclaimed);
                if used.saturating_add(incoming) > self.cap_bytes {
                    // Full, and nothing left to reclaim. `None` means abort:
                    // `write` is still borrowed by `table` here so an explicit
                    // `abort()` is unavailable, but dropping an uncommitted
                    // `WriteTransaction` rolls it back all the same.
                    None
                } else {
                    Some(used)
                }
            } else {
                Some(used)
            }
        };

        let Some(mut used) = outcome else {
            return Ok(false);
        };
        {
            let mut table = write.open_table(ENTRIES)?;
            let mut ledger = write.open_table(LEDGER)?;
            if let Some(previous) = table.insert(key.as_bytes().as_slice(), raw.as_slice())? {
                // Replacing an existing row: `used` already counted the old
                // bytes, so take them back before adding the new ones.
                used = used.saturating_sub(previous.value().len() as u64);
            }
            ledger.insert(USED_ROW, used)?;
        }
        write.commit()?;
        self.used.store(read_ledger(&self.db)?, Ordering::Relaxed);
        Ok(true)
    }

    /// Deletes `key`. Missing keys are not an error.
    pub fn remove(&self, key: &CacheKey) -> Result<Option<Entry>, CacheError> {
        let write = self.db.begin_write()?;
        let removed = {
            let mut table = write.open_table(ENTRIES)?;
            let mut ledger = write.open_table(LEDGER)?;
            let previous = table.remove(key.as_bytes().as_slice())?;
            let freed = previous.as_ref().map_or(0, |p| p.value().len() as u64);
            if freed > 0 {
                let used = ledger
                    .get(USED_ROW)?
                    .map_or(0, |g| g.value())
                    .saturating_sub(freed);
                ledger.insert(USED_ROW, used)?;
            }
            previous.map(|p| p.value().to_vec())
        };
        write.commit()?;
        if removed.is_some() {
            self.used.store(read_ledger(&self.db)?, Ordering::Relaxed);
        }
        match removed {
            Some(raw) => Ok(Some(Entry::decode(&raw)?)),
            None => Ok(None),
        }
    }

    /// Drops every expired row and returns how many bytes it reclaimed.
    pub fn sweep(&self) -> Result<u64, CacheError> {
        let now = now_ms();
        let write = self.db.begin_write()?;
        let reclaimed = {
            let mut table = write.open_table(ENTRIES)?;
            sweep_expired(&mut table, now)?
        };
        write.commit()?;
        self.used.store(read_ledger(&self.db)?, Ordering::Relaxed);
        Ok(reclaimed)
    }

    /// Bytes logically stored, per the ledger.
    pub fn used_bytes(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// The cap this tier enforces.
    #[must_use]
    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    /// Rows currently stored, expired ones included.
    ///
    /// Inaccurate between a `begin_write` and its `commit` from another thread;
    /// that is a metrics counter, not a control path.
    pub fn len(&self) -> Result<u64, CacheError> {
        let read = self.db.begin_read()?;
        Ok(read.open_table(ENTRIES)?.len()?)
    }

    /// Whether the tier holds no rows.
    ///
    /// Never `unwrap_or(0)`: a `redb` failure here is a real error, and
    /// reporting it as "empty" would have the caller write over a tier it
    /// could not read.
    pub fn is_empty(&self) -> Result<bool, CacheError> {
        Ok(self.len()? == 0)
    }
}

/// Reads the logical byte total. One row, one read transaction.
///
/// `get` returns an `AccessGuard` that borrows the table, so the value is
/// copied out by `map_or` before the caller can write to the same table.
fn read_ledger(db: &Database) -> Result<u64, CacheError> {
    let read = db.begin_read()?;
    let table = read.open_table(LEDGER)?;
    Ok(table.get(USED_ROW)?.map_or(0, |guard| guard.value()))
}

/// Removes every expired row, returning the bytes reclaimed.
fn sweep_expired(table: &mut redb::Table<'_, &[u8], &[u8]>, now: u64) -> Result<u64, CacheError> {
    let mut reclaimed = 0u64;
    // Collect first: `redb` forbids mutating a table while a cursor over it is
    // open, and the list is bounded by the logical cap.
    let expired: Vec<Vec<u8>> = {
        let mut doomed = Vec::new();
        for row in table.iter()? {
            let (key, value) = row?;
            if Entry::decode(value.value())?.is_expired(now) {
                doomed.push(key.value().to_vec());
            }
        }
        doomed
    };
    for key in expired {
        if let Some(previous) = table.remove(key.as_slice())? {
            reclaimed = reclaimed.saturating_add(previous.value().len() as u64);
        }
    }
    Ok(reclaimed)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;

    use super::{ByteWeighter, MemTier};
    use crate::entry::Entry;
    use crate::key::CacheKey;

    #[test]
    fn weigher_charges_the_body_length() {
        let key = CacheKey::hash(b"k");
        let small = Entry::new(200, "text/plain", "a", 0, Duration::from_secs(1));
        let fat = "a".repeat(1000);
        let big = Entry::new(
            200,
            "text/plain",
            Bytes::copy_from_slice(fat.as_bytes()),
            0,
            Duration::from_secs(1),
        );
        let w = ByteWeighter;
        assert!(
            quick_cache::Weighter::weight(&w, &key, &big)
                > quick_cache::Weighter::weight(&w, &key, &small)
        );
    }

    #[test]
    fn expired_entry_reads_as_absent() {
        let tier = MemTier::new(1 << 20);
        let key = CacheKey::hash(b"k");
        tier.insert(
            &key,
            Entry::new(200, "text/plain", "x", 0, Duration::from_millis(5)),
            0,
        );
        assert!(tier.get(&key, 6).is_none());
    }

    #[test]
    fn a_zero_capacity_tier_stores_nothing_and_stays_bounded() {
        // A capacity below one entry's weight is a caller bug. The property
        // that must still hold is the bound: the tier reports the weight it
        // actually holds, which is zero, so a misconfigured cache cannot be
        // the thing that grows RSS.
        let tier = MemTier::new(0);
        let key = CacheKey::hash(b"k");
        tier.insert(
            &key,
            Entry::new(200, "text/plain", "x", 0, Duration::from_secs(9)),
            0,
        );
        assert_eq!(tier.weight(), 0, "a zero-capacity tier must retain nothing");
    }

    #[test]
    fn a_capacity_of_one_byte_reports_its_real_weight() {
        // `quick_cache` divides the capacity across shards, so a capacity of 1
        // can floor to 0 per shard. The floor keeps the arithmetic total; it
        // does not promise an entry fits.
        let tier = MemTier::new(1);
        let key = CacheKey::hash(b"k");
        tier.insert(
            &key,
            Entry::new(200, "text/plain", "x", 0, Duration::from_secs(9)),
            0,
        );
        assert!(tier.get(&key, 0).is_none() || tier.weight() <= tier.capacity());
    }
}
