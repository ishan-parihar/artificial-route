//! The audit trail: one append-only `redb` row per guarded tool call —
//! **including the ones refused**.
//!
//! # Row shape
//!
//! `docs/06-axi-mcp.md` names `tool|duration|key-id` plus `blake3(input)` and
//! `truncate(output, 200)`. The stored row is those four fields, unchanged and
//! still leading, followed by three more:
//!
//! ```text
//! tool|duration_ms|key_id|input_hash|outcome|out_len|truncated|output
//! ```
//!
//! * `outcome` — `ok`, `error` or `denied`. A scope denial is a row, not a
//!   silence: "who tried to do what they were not allowed to do" is the whole
//!   reason the trail exists, and a trail that records only successes cannot
//!   answer it.
//! * `out_len` — the output's byte length **before** truncation, so a reader can
//!   tell a short answer from a cut one without trusting the cut itself.
//! * `truncated` — `true` only when the stored `output` is shorter than
//!   `out_len`. Without it a truncated row and a complete row are
//!   indistinguishable, which is how a 200-char stub of a 4KB provider error
//!   ends up read as the whole story.
//!
//! Nothing stores the untruncated output: the row is the trail, and the trail
//! is bounded.
//!
//! # Why `blake3` and not `DefaultHasher`
//!
//! `std::hash::DefaultHasher` is **not stable across Rust releases** — its
//! algorithm is explicitly unspecified and has changed before. A row written by
//! one build therefore cannot be matched by another, which is fine for
//! in-process correlation and wrong for anything a reader is meant to be able to
//! reproduce later. `docs/06` asks for `blake3(input)`, and `blake3 1.8.7` is
//! already in `Cargo.lock` via `ar-cache`, so the stable hash costs no new
//! transitive dependency. [`input_hash`] keeps its `u64` signature and returns
//! the leading 8 bytes of the 32-byte digest, so a caller holding a `u64` from
//! the old build keeps its type and a reader can always recompute the full
//! digest from the same input.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

/// How much of a tool's output an audit row keeps.
pub const AUDIT_OUTPUT_LIMIT: usize = 200;

const AUDIT: TableDefinition<'static, u64, String> = TableDefinition::new("ar_mcp_audit");

/// How a guarded call ended. Every call writes a row; this is the third field
/// that says which kind it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    /// The body ran and returned.
    Ok,
    /// The body ran and failed. The row's `output` holds the rendered error.
    Error,
    /// The body never ran: the held scope did not cover the tool's need.
    /// Audited *before* the `Err` is returned, so a denial cannot be lost.
    Denied,
}

impl CallOutcome {
    /// The one-word token stored in the row.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Denied => "denied",
        }
    }
}

/// Everything that can go wrong touching the audit table. One variant for the
/// four error types `redb` funnels into `redb::Error`, one for the commit,
/// which `redb` keeps separate. Five variants would be five identical messages.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// The database, a transaction, a table, a read or a write failed.
    #[error("audit: {0}")]
    Redb(#[from] redb::Error),
    /// The write transaction could not be committed.
    #[error("audit commit: {0}")]
    Commit(#[from] redb::CommitError),
}

fn redb_err<E: Into<redb::Error>>(e: E) -> AuditError {
    AuditError::Redb(e.into())
}

/// Truncates to `limit` bytes without splitting a UTF-8 code point.
fn truncate(s: &str, limit: usize) -> &str {
    if s.len() <= limit {
        return s;
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The correlation digest for a tool's input: the leading 8 bytes of
/// `blake3(input)`, big-endian. See the module docs for why this is not
/// `DefaultHasher`.
///
/// The return type is `u64` so an existing caller keeps its type. 64 bits of a
/// 256-bit digest is ample for correlating two rows of the same prompt, and
/// nothing in this crate authorizes on the value.
#[must_use]
pub fn input_hash(input: &str) -> u64 {
    let digest = blake3::hash(input.as_bytes());
    u64::from_be_bytes(digest.as_bytes()[..8].try_into().unwrap_or([0; 8]))
}

/// Handle on the audit database. Cheap to share: `redb::Database` is
/// `Send + Sync` and a write is one transaction with one `insert`.
#[derive(Debug)]
pub struct Audit {
    db: Database,
    /// Next sequence number. An `AtomicU64` rather than a table length: one
    /// relaxed load beats a table scan, and this is the write path. Seeded by
    /// [`Audit::open`] from the table's highest key so a restart appends
    /// instead of overwriting.
    next: AtomicU64,
}

impl Audit {
    /// Opens, creating if absent, the audit database at `path`.
    ///
    /// Resumes the sequence from the highest key already stored, so a restart
    /// continues the trail rather than writing over it: a fresh handle starting
    /// at 0 would `insert` over rows 0, 1, 2 … and the ledger is append-only in
    /// intent. One scan at open is the whole price.
    ///
    /// # Errors
    /// Fails if the file cannot be created or opened, or if the resume scan
    /// cannot be read.
    pub fn open(path: &Path) -> Result<Self, AuditError> {
        let db = Database::create(path).map_err(redb_err)?;
        let next = match db.begin_read() {
            Ok(tx) => match tx.open_table(AUDIT) {
                // Keys ascend, so the last one is the max. `iter()` is a
                // provided method on `ReadableTable` that hands back a `Range`,
                // which is the double-ended one -- so this is a one-element
                // walk from the end, not a full scan.
                Ok(table) => {
                    match table.iter().map_err(redb_err)?.next_back().transpose().map_err(redb_err)? {
                        Some((k, _)) => k.value().saturating_add(1),
                        None => 0,
                    }
                }
                // A brand-new file has no tables at all. That is an empty
                // trail, not a failure: the first `record` creates the table.
                Err(redb::TableError::TableDoesNotExist(_)) => 0,
                Err(e) => return Err(redb_err(e)),
            },
            // A read transaction that will not open is a database that will not
            // take a write either; fail loudly here rather than resume at 0 and
            // overwrite the trail.
            Err(e) => return Err(redb_err(e)),
        };
        Ok(Self { db, next: AtomicU64::new(next) })
    }

    /// Appends one row and returns its sequence number.
    ///
    /// The row is truncated to [`AUDIT_OUTPUT_LIMIT`] and *marked* — see the
    /// module docs for the field list.
    ///
    /// # Errors
    /// Fails if the transaction or the write cannot be completed.
    pub fn record(
        &self,
        tool: &str,
        duration: Duration,
        key_id: &str,
        input: &str,
        output: &str,
        outcome: CallOutcome,
    ) -> Result<u64, AuditError> {
        let seq = self.next.fetch_add(1, Ordering::Relaxed);
        let cut = truncate(output, AUDIT_OUTPUT_LIMIT);
        let row = format!(
            "{}|{}|{}|{:016x}|{}|{}|{}|{}",
            tool,
            duration.as_millis(),
            key_id,
            input_hash(input),
            outcome.as_str(),
            output.len(),
            cut.len() < output.len(),
            cut,
        );
        let tx = self.db.begin_write().map_err(redb_err)?;
        tx.open_table(AUDIT).map_err(redb_err)?.insert(seq, row).map_err(redb_err)?;
        tx.commit()?;
        Ok(seq)
    }

    /// Reads one row back, or `None` if that sequence was never written.
    ///
    /// # Errors
    /// Fails if the transaction, the table or the read cannot be completed.
    pub fn get(&self, seq: u64) -> Result<Option<String>, AuditError> {
        let tx = self.db.begin_read().map_err(redb_err)?;
        let t = tx.open_table(AUDIT).map_err(redb_err)?;
        Ok(t.get(seq).map_err(redb_err)?.map(|v| v.value()))
    }

    /// The next sequence number this handle will hand out, i.e. the number of
    /// rows in the table. Across a restart this starts at the resumed value,
    /// not at zero — see [`Audit::open`].
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next.load(Ordering::Relaxed)
    }

    /// How many rows are in the table. See [`Audit::next_seq`], which is the
    /// same number under a name that says what it is for.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.next_seq()
    }

    /// Whether the table holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{AUDIT_OUTPUT_LIMIT, Audit, CallOutcome, input_hash};
    use std::time::Duration;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("ar-mcp-tests");
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn audits_when_tool_called() {
        let audit = Audit::open(&tmp("basic.redb")).expect("open");
        let seq = audit
            .record("ar_get_health", Duration::from_millis(3), "k1", "{}", "ok", CallOutcome::Ok)
            .expect("record");
        let row = audit.get(seq).expect("get").expect("row");
        assert!(row.starts_with("ar_get_health|3|k1|"), "{row}");
    }

    #[test]
    fn truncates_output_to_the_documented_limit() {
        let audit = Audit::open(&tmp("trunc.redb")).expect("open");
        let seq = audit
            .record("ar_cost_report", Duration::ZERO, "k1", "{}", &"x".repeat(500), CallOutcome::Ok)
            .expect("record");
        let row = audit.get(seq).expect("get").expect("row");
        assert_eq!(row.matches('x').count(), AUDIT_OUTPUT_LIMIT);
    }

    #[test]
    fn marks_a_truncated_row_with_its_true_length() {
        let audit = Audit::open(&tmp("trunc-mark.redb")).expect("open");
        let seq = audit
            .record("ar_cost_report", Duration::ZERO, "k1", "{}", &"x".repeat(500), CallOutcome::Ok)
            .expect("record");
        let row = audit.get(seq).expect("get").expect("row");
        let fields: Vec<&str> = row.split('|').collect();
        assert_eq!(fields[5], "500", "out_len is the pre-truncation length: {row}");
        assert_eq!(fields[6], "true", "truncated is an explicit marker: {row}");
    }

    #[test]
    fn leaves_a_short_row_unmarked() {
        let audit = Audit::open(&tmp("no-trunc-mark.redb")).expect("open");
        let seq = audit
            .record("ar_get_health", Duration::ZERO, "k1", "{}", "ok", CallOutcome::Ok)
            .expect("record");
        let row = audit.get(seq).expect("get").expect("row");
        assert_eq!(row.split('|').nth(6), Some("false"), "{row}");
    }

    #[test]
    fn records_a_denial_without_the_body_having_run() {
        let audit = Audit::open(&tmp("denied.redb")).expect("open");
        let seq = audit
            .record("ar_switch_combo", Duration::ZERO, "k1", "{}", "", CallOutcome::Denied)
            .expect("record");
        let row = audit.get(seq).expect("get").expect("row");
        assert_eq!(row.split('|').nth(4), Some("denied"), "{row}");
    }

    #[test]
    fn resumes_the_sequence_after_a_restart_instead_of_overwriting() {
        let path = tmp("resume.redb");
        let first = Audit::open(&path).expect("open");
        let a = first
            .record("ar_list_models", Duration::ZERO, "k1", "{}", "one", CallOutcome::Ok)
            .expect("record");
        drop(first);

        // A second handle over the same file: the old default started at 0 and
        // would have overwritten row 0.
        let second = Audit::open(&path).expect("reopen");
        let b = second
            .record("ar_list_models", Duration::ZERO, "k1", "{}", "two", CallOutcome::Ok)
            .expect("record");
        assert_eq!((a, b), (0, 1));
        assert_eq!(second.len(), 2);
        assert!(second.get(a).expect("get").expect("row").ends_with("one"));
    }

    #[test]
    fn input_hash_is_a_stable_blake3_prefix() {
        // blake3("") = af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262;
        // the leading 8 bytes, big-endian. Pinned so a dependency bump that
        // changes the digest is a failing test rather than a silently
        // unmatchable trail.
        assert_eq!(input_hash(""), 12615508044239446438);
    }

    #[test]
    fn input_hash_distinguishes_inputs() {
        assert_ne!(input_hash("{}"), input_hash("{\"n\":2}"));
    }
}
