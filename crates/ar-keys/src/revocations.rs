//! The revocation list: `jti` → `exp`, in the credential store's sqlite file.
//!
//! A signed token cannot be withdrawn — that is what signing is. The `jti` plus
//! this table is the other half: a token stays live for its full lifetime unless
//! something remembers it, and "stolen token expires" is only one of the two
//! guarantees. This is the one that makes a *reported* leak dead immediately.
//!
//! # Why sqlite and not a redb file
//!
//! The first cut of this was a separate redb file, and it could not work.
//! redb takes an **exclusive** `flock(LOCK_EX | LOCK_NB)` on its file
//! (`redb-2.6/src/tree_store/page_store/file_backend/unix.rs:37`) and returns
//! `DatabaseAlreadyOpen` when it cannot. The proxy holds the list open for the
//! life of the process, so `aroute keys revoke` — which has to run **while the
//! proxy is up**, because that is when a token needs revoking — could not open
//! it at all.
//!
//! sqlite is already the credential store's format, `rusqlite` is already a
//! dependency, and it supports **concurrent readers with one writer**: the
//! server reads a `jti` per request, the operator's revoke command writes one
//! row. That is exactly the access pattern, and putting the table in the same
//! file as the credentials settles the path question too — one file, one
//! master, one place to look.
//!
//! The journal mode is left at SQLite's default rollback journal, like the rest
//! of the store: WAL buys concurrent-reader throughput and costs two sidecar
//! files next to ones an operator has to keep out of git. The read is one point
//! lookup, so the default is enough.
//!
//! # The bound
//!
//! The bound is a cap, not a TTL alone. Entries are swept on every write, and
//! because every entry carries the token's own `exp` there is always something
//! sweepable once the list is genuinely full.
//!
//! When the cap is reached with nothing expired, the write is **refused**
//! ([`KeyError::RevokeListFull`]) rather than evicting a live entry. The two
//! convenient alternatives are both worse: dropping a live revocation
//! un-revokes a stolen token, and growing past the cap trades a bounded memory
//! budget for an unbounded one.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::error::KeyError;

/// Default cap on live entries.
pub const DEFAULT_CAP: usize = 4_096;

/// The revocation list, over a sqlite connection.
///
/// The connection is behind a `Mutex` because `rusqlite::Connection` is `Send`
/// but **not** `Sync`, and the server shares one list across every request
/// thread. The lock is held for one point query or one small write, so the
/// contention is on a single row and not on the request path's work.
///
/// Not `Clone`: the lock and the connection are the state, and a second handle
/// to the same file is what the sqlite format was chosen to avoid.
pub struct Revocations {
    conn: Mutex<Connection>,
    cap: usize,
}

impl Revocations {
    /// Opens (or creates) the list in the sqlite file at `path`, capped at `cap`
    /// live entries.
    ///
    /// The file is the **credential store's** file, so this must be the same
    /// path [`crate::CredentialStore`] uses. Opening it here creates the
    /// `revoked_tokens` table if an older file does not have it, which is the
    /// whole upgrade path.
    ///
    /// # Errors
    /// [`KeyError::Store`] if the file cannot be opened or the table cannot be
    /// created.
    pub fn open(path: &Path, cap: usize) -> Result<Self, KeyError> {
        let conn = Connection::open(path).map_err(sql)?;
        conn.execute_batch(SCHEMA).map_err(sql)?;
        Ok(Self {
            conn: Mutex::new(conn),
            cap,
        })
    }

    /// The cap on live entries.
    #[must_use]
    pub const fn cap(&self) -> usize {
        self.cap
    }

    /// Whether `jti` has been revoked and has not yet expired.
    ///
    /// # Errors
    /// [`KeyError::Store`] if the read fails, or if the lock was poisoned by a
    /// panic in another thread.
    pub fn is_revoked(&self, jti: &str) -> Result<bool, KeyError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(SELECT_REVOKED, [jti], |row| row.get::<_, i64>(0))
            .optional()
            .map_err(sql)?;
        Ok(match row {
            Some(exp) => exp >= now_epoch(),
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
    /// [`KeyError::Store`] on a sqlite failure.
    pub fn revoke(&self, jti: &str, expires_at: i64) -> Result<(), KeyError> {
        // A negative expiry reads as already expired, which is the safe
        // direction: a clock that went backwards cannot make a token live.
        let expires_at = expires_at.max(0);
        let conn = self.lock()?;
        // Sweep first: the cap is on **live** entries, and a store that has
        // only expired rows is not full. Sweeping before the count is what
        // makes the cap mean "live entries" rather than "rows ever written".
        conn.execute(DELETE_EXPIRED, params![now_epoch()])
            .map_err(sql)?;
        let live: i64 = conn
            .query_row(COUNT_REVOKED, [], |row| row.get(0))
            .map_err(sql)?;
        let already = conn
            .query_row(SELECT_REVOKED, [jti], |row| row.get::<_, i64>(0))
            .optional()
            .map_err(sql)?
            .is_some();
        if !already && live >= self.cap as i64 {
            return Err(KeyError::RevokeListFull { cap: self.cap });
        }
        conn.execute(UPSERT_REVOKED, params![jti, expires_at])
            .map_err(sql)?;
        Ok(())
    }

    /// Drops every entry whose token has expired. Returns how many went.
    ///
    /// The 60-second poller in `docs/04-subsystems.md` calls this. Writes also
    /// sweep, so this is about reclaiming space when the process is idle rather
    /// than about correctness.
    ///
    /// # Errors
    /// [`KeyError::Store`] on a sqlite failure.
    pub fn purge_expired(&self) -> Result<usize, KeyError> {
        let conn = self.lock()?;
        let gone = conn
            .execute(DELETE_EXPIRED, params![now_epoch()])
            .map_err(sql)?;
        Ok(gone)
    }

    /// How many entries are held, expired ones included.
    ///
    /// # Errors
    /// [`KeyError::Store`] on a sqlite failure.
    pub fn len(&self) -> Result<usize, KeyError> {
        let conn = self.lock()?;
        let n: i64 = conn
            .query_row(COUNT_REVOKED, [], |row| row.get(0))
            .map_err(sql)?;
        Ok(n as usize)
    }

    /// Whether the list holds no entries at all.
    ///
    /// # Errors
    /// [`KeyError::Store`] on a sqlite failure.
    pub fn is_empty(&self) -> Result<bool, KeyError> {
        Ok(self.len()? == 0)
    }

    /// The connection, or a store error when another thread panicked holding it.
    ///
    /// A poisoned lock means some other thread panicked mid-write. The data is
    /// still consistent (sqlite's own transaction is atomic), so the list is
    /// usable — but the panic is worth surfacing rather than hiding.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, KeyError> {
        self.conn.lock().map_err(|_| {
            KeyError::Store("the revocation list's lock was poisoned by a panic".to_owned())
        })
    }
}

/// The table, created on open so an older file gains it.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS revoked_tokens (
    jti        TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS revoked_tokens_by_expiry
    ON revoked_tokens (expires_at);
";

const SELECT_REVOKED: &str = "SELECT expires_at FROM revoked_tokens WHERE jti = ?1";

const COUNT_REVOKED: &str = "SELECT count(*) FROM revoked_tokens";

const UPSERT_REVOKED: &str = "
INSERT INTO revoked_tokens (jti, expires_at) VALUES (?1, ?2)
ON CONFLICT(jti) DO UPDATE SET expires_at = ?2
";

const DELETE_EXPIRED: &str = "DELETE FROM revoked_tokens WHERE expires_at < ?1";

/// The current Unix time, or 0 when the clock is before the epoch.
///
/// 0 is the safe direction: a token with an expiry of 0 reads as expired, so a
/// broken clock can only make a token dead, never live.
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A sqlite failure, as a store error.
///
/// The message is rusqlite's own, which names the table and the constraint and
/// never a stored value.
fn sql(e: rusqlite::Error) -> KeyError {
    KeyError::Store(e.to_string())
}
