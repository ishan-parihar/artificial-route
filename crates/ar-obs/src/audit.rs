//! The audit ledger: `redb`, redacted by default, raw behind an admin grant.
//!
//! `docs/04`: "Audit redacted default, raw 24h TTL `admin` only."
//!
//! # What redaction means here
//!
//! This crate masks nothing. `ar_keys::AuditLine` already cannot hold a
//! credential -- every field is a `&'static str` or an `ar_core::Strng`
//! identifier -- and `ar-keys/src/audit.rs` names this module as the persistence
//! half. So [`AuditLedger::append`] stores that record's redacted rendering and
//! nothing else. Re-implementing masking here is how a codebase ends up with two
//! masking widths and one of them wrong; prompt and secret redaction is
//! `ar-guard`'s, a sibling.
//!
//! # The raw path
//!
//! [`AuditLedger::append_raw`] exists, is gated, and is off unless asked for. The
//! gate is the part worth reviewing:
//!
//! * [`AuditLedger::open_raw`] compares the presented secret against the expected
//!   one in constant time and stores neither. A `read:*` grant cannot satisfy it,
//!   so it reaches [`AuditLedger::query`] on the redacted rows and nothing else --
//!   the `docs/04` acceptance criterion.
//! * Expiry is `written_at + [`RAW_TTL_SECS`]`, derived from the row's own
//!   timestamp rather than a second stored field, so a row cannot claim a longer
//!   life than its own stamp allows. [`AuditLedger::sweep`] enforces it on
//!   demand, and [`AuditLedger::query`] filters it on the way out -- an unswept
//!   file leaks nothing through this API.
//!
//! The gate is a secret comparison rather than an `ar_keys::Scope` because
//! `ar-keys` has no admin scope yet (its `Scope` is read/write/execute). That is
//! the one place to change when it lands.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use ar_keys::AuditLine;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::error::ObsError;

/// How long a raw row survives. 24h, per `docs/04-subsystems.md`.
pub const RAW_TTL_SECS: u64 = 24 * 60 * 60;

/// Append-only audit rows, keyed by a monotone sequence.
const ROWS: TableDefinition<u64, &[u8]> = TableDefinition::new("audit");

/// Leading byte on a raw row. Its absence is what "redacted" means on disk.
const RAW_MARK: u8 = 1;

/// Row layout: `[flags:u8]`, then `written_at` as 8 little-endian bytes, then
/// the payload. The timestamp is on every row, not only raw ones, because
/// `query(since)` has to work on a redacted ledger too.
fn encode(raw: bool, written_at: u64, payload: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(9 + payload.len());
    v.push(if raw { RAW_MARK } else { 0 });
    v.extend_from_slice(&written_at.to_le_bytes());
    v.extend_from_slice(payload.as_bytes());
    v
}

/// Decodes a stored row into `(is_raw, written_at, payload_bytes)`.
fn decode(bytes: &[u8]) -> (bool, u64, &[u8]) {
    let Some((&flags, rest)) = bytes.split_first() else {
        return (false, 0, &[]);
    };
    let raw = flags & RAW_MARK == RAW_MARK;
    match rest.get(..8) {
        Some(head) => (
            raw,
            u64::from_le_bytes(head.try_into().unwrap_or([0; 8])),
            rest.get(8..).unwrap_or_default(),
        ),
        None => (raw, 0, &[]),
    }
}

/// Whether a row's 24h has elapsed. A redacted row never expires: it holds a key
/// id and four enum labels, which is what the audit trail is for.
fn expired(raw: bool, written_at: u64, now: u64) -> bool {
    raw && written_at.saturating_add(RAW_TTL_SECS) <= now
}

/// Compares two secrets without an early exit.
///
/// Length is compared implicitly, the one leak this leaves: a caller learns the
/// expected length. Acceptable for a shared admin secret, and one more reason to
/// move this onto `ar-keys`' admin scope.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u32;
    for i in 0..a.len().max(b.len()) {
        diff |= u32::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// An append-only audit ledger over one `redb` file.
#[derive(Debug)]
pub struct AuditLedger {
    db: Database,
    raw: bool,
    /// Monotone row key, seeded from the highest key on disk so a restart cannot
    /// overwrite the rows it is about to append.
    next: AtomicU64,
}

impl AuditLedger {
    /// Opens a redacted-only ledger. This is what a `read:*` caller gets.
    pub fn open(path: &Path) -> Result<Self, ObsError> {
        Self::build(path, false)
    }

    /// Opens a ledger that also accepts raw rows.
    ///
    /// Refused unless `presented` matches `expected`. Neither is stored, and
    /// neither appears in the error, so a wrong guess cannot be read back out of
    /// a log line.
    pub fn open_raw(path: &Path, presented: &str, expected: &str) -> Result<Self, ObsError> {
        if !ct_eq(presented, expected) {
            return Err(ObsError::AdminScope);
        }
        Self::build(path, true)
    }

    fn build(path: &Path, raw: bool) -> Result<Self, ObsError> {
        // redb 4.x refuses to *open* a file that does not exist; `create` is the
        // call that makes one, so an absent ledger falls through to it. Same
        // two-step as `ar-cache`'s disk tier.
        let db = match Database::open(path) {
            Ok(db) => db,
            Err(_) => Database::create(path).map_err(ObsError::store)?,
        };
        // A redb file has no tables until a write transaction opens them, and
        // `open_table` on a *read* transaction is `TableDoesNotExist` until then
        // -- so the `begin_read` below would fail on a fresh ledger.
        {
            let write = db.begin_write().map_err(ObsError::store)?;
            drop(write.open_table(ROWS).map_err(ObsError::store)?);
            write.commit().map_err(ObsError::store)?;
        }
        let highest = {
            let read = db.begin_read().map_err(ObsError::store)?;
            let table = read.open_table(ROWS).map_err(ObsError::store)?;
            table
                .iter()
                .map_err(ObsError::store)?
                .filter_map(std::result::Result::ok)
                .map(|(k, _)| k.value())
                .max()
                .unwrap_or(0)
        };
        Ok(Self {
            db,
            raw,
            next: AtomicU64::new(highest + 1),
        })
    }

    /// Whether this ledger was opened with the raw path enabled.
    #[must_use]
    pub fn raw_enabled(&self) -> bool {
        self.raw
    }

    /// Appends the redacted rendering of one record. Always available.
    pub fn append(&self, line: &AuditLine) -> Result<u64, ObsError> {
        self.insert(false, line.at.max(0) as u64, &line.to_string())
    }

    /// Appends a raw excerpt, stamped with a hard 24h expiry.
    ///
    /// The excerpt is stored verbatim: this crate does not inspect, mask, or
    /// classify it. Passing a prompt through is the caller's decision to make
    /// with the admin grant in hand, and `ar-guard` is what decides what may
    /// legitimately be raw.
    pub fn append_raw(
        &self,
        line: &AuditLine,
        excerpt: &str,
        now: u64,
    ) -> Result<u64, ObsError> {
        if !self.raw {
            return Err(ObsError::AdminScope);
        }
        self.insert(true, now, &format!("{line} raw={excerpt}"))
    }

    fn insert(&self, raw: bool, written_at: u64, payload: &str) -> Result<u64, ObsError> {
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        let value = encode(raw, written_at, payload);
        let write = self.db.begin_write().map_err(ObsError::store)?;
        {
            let mut table = write.open_table(ROWS).map_err(ObsError::store)?;
            table.insert(key, value.as_slice()).map_err(ObsError::store)?;
        }
        write.commit().map_err(ObsError::store)?;
        Ok(key)
    }

    /// Every row written at or after `since`, oldest first, skipping raw rows
    /// whose 24h has elapsed by `now`.
    ///
    /// `now` is a parameter rather than a clock read so the TTL is testable and
    /// so a caller reading a historical window does not judge expiry against the
    /// window's start.
    pub fn query(&self, since: u64, now: u64) -> Result<Vec<String>, ObsError> {
        let read = self.db.begin_read().map_err(ObsError::store)?;
        let table = read.open_table(ROWS).map_err(ObsError::store)?;
        let mut rows = Vec::new();
        for row in table.iter().map_err(ObsError::store)? {
            let (_, value) = row.map_err(ObsError::store)?;
            let (raw, written_at, payload) = decode(value.value());
            if written_at < since || expired(raw, written_at, now) {
                continue;
            }
            rows.push(String::from_utf8_lossy(payload).into_owned());
        }
        Ok(rows)
    }

    /// Deletes every expired raw row and returns how many it removed.
    pub fn sweep(&self, now: u64) -> Result<usize, ObsError> {
        let write = self.db.begin_write().map_err(ObsError::store)?;
        let removed = {
            let mut table = write.open_table(ROWS).map_err(ObsError::store)?;
            // `redb` forbids mutating a table while a cursor over it is open, so
            // collect first. The ledger is bounded by its own retention, not by a
            // cap this crate enforces.
            let mut doomed = Vec::new();
            for row in table.iter().map_err(ObsError::store)? {
                let (key, value) = row.map_err(ObsError::store)?;
                let (raw, written_at, _) = decode(value.value());
                if expired(raw, written_at, now) {
                    doomed.push(key.value());
                }
            }
            let mut n = 0usize;
            for key in doomed {
                if table.remove(key).map_err(ObsError::store)?.is_some() {
                    n += 1;
                }
            }
            n
        };
        write.commit().map_err(ObsError::store)?;
        Ok(removed)
    }
}