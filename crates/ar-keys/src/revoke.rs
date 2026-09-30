//! The revocation list: `jti` → `exp`, in a redb table, bounded by a cap.
//!
//! A signed token cannot be withdrawn — that is what signing is. The `jti` plus
//! this table is the other half: a token stays live for its full lifetime unless
//! something remembers it, and "stolen token expires" is only one of the two
//! guarantees. This is the one that makes a *reported* leak dead immediately.
//!
//! The bound is a cap, not a TTL alone. `docs/04-subsystems.md` says the list is
//! polled every 60s; polling bounds how *long* an entry lives, not how *many*
//! there are, and a request path that appends to an unbounded map is an OOM with
//! a long fuse. Entries are swept on every write, and because every entry
//! carries the token's own `exp` there is always something sweepable once the
//! list is genuinely full.
//!
//! When the cap is reached with nothing expired, the write is **refused**
//! ([`KeyError::RevokeListFull`]) rather than evicting a live entry. The two
//! convenient alternatives are both worse: dropping a live revocation
//! un-revokes a stolen token, and growing past the cap trades a bounded memory
//! budget for an unbounded one.

use std::path::Path;

use redb::{Database, ReadableTable, ReadableTableMetadata as _, TableDefinition};

use crate::error::KeyError;

/// Default cap on live entries.
pub const DEFAULT_CAP: usize = 4_096;

/// `jti` → token `exp` as a Unix timestamp.
const REVOKED: TableDefinition<&str, u64> = TableDefinition::new("revoked");

/// The revocation list.
///
/// Not `Clone`: it owns a redb `Database`, and handing a second handle to the
/// same file is a footgun (redb takes an exclusive lock, so it would fail at
/// runtime rather than compile time). Share it as `Arc<Revocation>`.
pub struct Revocation {
    db: Database,
    cap: usize,
}

impl Revocation {
    /// Opens (or creates) the store at `path`, capped at `cap` live entries.
    ///
    /// # Errors
    /// [`KeyError::Store`] if the file cannot be opened or the table cannot be
    /// created.
    pub fn open(path: &Path, cap: usize) -> Result<Self, KeyError> {
        let db = Database::create(path).map_err(store_err)?;
        {
            let txn = db.begin_write().map_err(store_err)?;
            txn.open_table(REVOKED).map_err(store_err)?;
            txn.commit().map_err(store_err)?;
        }
        Ok(Self { db, cap })
    }

    /// The cap on live entries.
    #[must_use]
    pub const fn cap(&self) -> usize {
        self.cap
    }

    /// Whether `jti` has been revoked and has not yet expired.
    ///
    /// # Errors
    /// [`KeyError::Store`] if the read transaction fails.
    pub fn is_revoked(&self, jti: &str) -> Result<bool, KeyError> {
        let txn = self.db.begin_read().map_err(store_err)?;
        let table = txn.open_table(REVOKED).map_err(store_err)?;
        Ok(match table.get(jti).map_err(store_err)? {
            Some(exp) => exp.value() >= now_epoch(),
            // Absent, or present-and-expired. The sweep on the next write will
            // collect the second case; treating it as live here would keep a
            // dead entry alive for the poll interval.
            None => false,
        })
    }

    /// Revokes `jti` until `expires_at` (the token's own `exp`).
    ///
    /// Re-revoking a live `jti` is a no-op, so a retried revoke does not consume
    /// a second slot.
    ///
    /// # Errors
    /// [`KeyError::RevokeListFull`] when the cap is reached with no entry
    /// eligible for sweep and `jti` is not already present.
    /// [`KeyError::Store`] on a redb failure.
    pub fn revoke(&self, jti: &str, expires_at: i64) -> Result<(), KeyError> {
        let expires_at = u64::try_from(expires_at).unwrap_or(0);
        let txn = self.db.begin_write().map_err(store_err)?;
        {
            let mut table = txn.open_table(REVOKED).map_err(store_err)?;
            // `is_some` consumes the guard, which ends the shared borrow of
            // `table` so the `insert` below can take it mutably.
            if !table.get(jti).map_err(store_err)?.is_some() {
                if table.len().map_err(store_err)? >= self.cap as u64 {
                    // Free what we can, then re-check: the entry about to be
                    // revoked may have expired, in which case there was no
                    // pressure after all.
                    let freed = sweep(&mut table)?;
                    if table.len().map_err(store_err)? >= self.cap as u64 && freed == 0 {
                        // Dropping `txn` here aborts the write, so a refused
                        // revoke leaves the table exactly as it was.
                        return Err(KeyError::RevokeListFull { cap: self.cap });
                    }
                }
                table.insert(jti, expires_at).map_err(store_err)?;
            }
        }
        txn.commit().map_err(store_err)
    }

    /// Drops every entry whose token has expired. Returns how many went.
    ///
    /// The 60-second poller in `docs/04-subsystems.md` calls this. Writes also
    /// sweep, so this is about reclaiming space when the process is idle rather
    /// than about correctness.
    ///
    /// # Errors
    /// [`KeyError::Store`] on a redb failure.
    pub fn purge_expired(&self) -> Result<usize, KeyError> {
        let txn = self.db.begin_write().map_err(store_err)?;
        let freed = {
            let mut table = txn.open_table(REVOKED).map_err(store_err)?;
            sweep(&mut table)?
        };
        txn.commit().map_err(store_err)?;
        Ok(freed)
    }

    /// Number of entries currently held, expired ones included.
    ///
    /// # Errors
    /// [`KeyError::Store`] on a redb failure.
    pub fn len(&self) -> Result<usize, KeyError> {
        let txn = self.db.begin_read().map_err(store_err)?;
        let table = txn.open_table(REVOKED).map_err(store_err)?;
        Ok(table.len().map_err(store_err)? as usize)
    }

    /// Whether the list is empty.
    ///
    /// # Errors
    /// [`KeyError::Store`] on a redb failure.
    pub fn is_empty(&self) -> Result<bool, KeyError> {
        Ok(self.len()? == 0)
    }
}

/// Removes every entry whose token has expired. Caller holds the write txn.
///
/// Collects the doomed keys into owned `String`s first: redb yields each entry
/// through a guard that is dropped at the end of the iteration, so borrowing the
/// key out of it does not live long enough to remove it — and `Table::remove`
/// needs `&mut` while the iterator borrows the table. Two phases is the
/// cheapest correct order; the allocation only happens for entries about to go.
fn sweep(table: &mut redb::Table<'_, &str, u64>) -> Result<usize, KeyError> {
    let now = now_epoch();
    let mut doomed: Vec<String> = Vec::new();
    for entry in table.iter().map_err(store_err)? {
        let (key, exp) = entry.map_err(store_err)?;
        if exp.value() < now {
            // A `jti` is a random id, not a credential, so owning it here does
            // not create a secret that needs zeroizing.
            doomed.push(key.value().to_string());
        }
    }
    for key in &doomed {
        table.remove(key.as_str()).map_err(store_err)?;
    }
    Ok(doomed.len())
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn store_err(e: impl std::fmt::Display) -> KeyError {
    KeyError::Store(e.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{Revocation};
    use crate::error::KeyError;

    /// A unique path per call, so parallel tests never share a redb file —
    /// redb takes an exclusive lock and the second open would fail.
    fn temp_path(tag: &str) -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ar-keys-revoke-{tag}-{}-{n}.redb", std::process::id()))
    }

    fn open(tag: &str, cap: usize) -> (Revocation, std::path::PathBuf) {
        let path = temp_path(tag);
        let _ = std::fs::remove_file(&path);
        (Revocation::open(&path, cap).expect("open"), path)
    }

    fn now() -> i64 {
        i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_secs()).expect("fits")
    }

    #[test]
    fn reports_revoked_after_revoking() {
        let (r, path) = open("basic", 16);
        r.revoke("jti-1", now() + 600).expect("revoke");
        assert!(r.is_revoked("jti-1").expect("lookup"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reports_not_revoked_for_an_unknown_jti() {
        let (r, path) = open("unknown", 16);
        assert!(!r.is_revoked("nope").expect("lookup"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn treats_an_expired_entry_as_not_revoked() {
        let (r, path) = open("expired", 16);
        r.revoke("jti-old", now() - 10).expect("revoke");
        assert!(!r.is_revoked("jti-old").expect("lookup"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn re_revoking_is_a_no_op() {
        let (r, path) = open("re-revoke", 2);
        r.revoke("jti-1", now() + 600).expect("first");
        r.revoke("jti-1", now() + 600).expect("second");
        assert_eq!(r.len().expect("len"), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn purges_only_expired_entries() {
        let (r, path) = open("purge", 16);
        r.revoke("live", now() + 600).expect("revoke");
        r.revoke("dead", now() - 1).expect("revoke");
        assert_eq!(r.purge_expired().expect("purge"), 1);
        assert!(r.is_revoked("live").expect("lookup"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sweeping_on_write_frees_space_at_the_cap() {
        // Cap 2, filled with two already-expired entries. A third live revoke
        // has nowhere to go until the sweep collects them, so this is the case
        // where refusing outright would strand the list forever.
        let (r, path) = open("sweep-on-write", 2);
        r.revoke("dead-a", now() - 10).expect("revoke");
        r.revoke("dead-b", now() - 10).expect("revoke");
        assert_eq!(r.len().expect("len"), 2);
        r.revoke("live-a", now() + 600).expect("an expired entry must be collectable at the cap");
        r.revoke("live-b", now() + 600).expect("now there is room");
        assert!(r.is_revoked("live-a").expect("lookup") && r.is_revoked("live-b").expect("lookup"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn refuses_when_full_of_live_entries() {
        let (r, path) = open("full", 2);
        r.revoke("live-a", now() + 600).expect("revoke");
        r.revoke("live-b", now() + 600).expect("revoke");
        assert!(matches!(r.revoke("live-c", now() + 600), Err(KeyError::RevokeListFull { cap: 2 })));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn never_drops_a_live_entry_to_make_room() {
        let (r, path) = open("no-evict", 2);
        r.revoke("live-a", now() + 600).expect("revoke");
        r.revoke("live-b", now() + 600).expect("revoke");
        let _ = r.revoke("live-c", now() + 600);
        assert!(r.is_revoked("live-a").expect("lookup"), "evicting a live revocation un-revokes a stolen token");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn entries_survive_a_reopen() {
        let path = temp_path("persist");
        let _ = std::fs::remove_file(&path);
        {
            let r = Revocation::open(&path, 16).expect("open");
            r.revoke("durable", now() + 600).expect("revoke");
        }
        let r = Revocation::open(&path, 16).expect("reopen");
        assert!(r.is_revoked("durable").expect("lookup"), "a restart must not un-revoke");
        let _ = std::fs::remove_file(path);
    }
}
