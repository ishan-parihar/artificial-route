//! The one error type this crate returns.
//!
//! Every `redb` failure collapses into [`CacheError::Disk`] because `redb::Error`
//! is already the umbrella type -- it has `From` impls for `StorageError`,
//! `TableError`, `DatabaseError`, `TransactionError`, `CommitError` and
//! `io::Error`. Re-wrapping each one would add seven variants that all mean
//! "the disk tier did not answer" and all get the same response: log it and
//! carry on without a cache.

/// A cache-tier failure.
///
/// None of these are fatal to a request. The memory tier has no fallible
/// constructor, and the disk tier's failures degrade to a miss -- see
/// [`crate::Cache`], which treats every tier error as "not cached".
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The `redb` tier could not be opened, read, written or committed.
    #[error("cache disk tier: {0}")]
    Disk(#[from] redb::Error),
    /// A stored row is not a decodable entry: shorter than the fixed header, or
    /// a `content_type` length that overruns the row.
    ///
    /// A `&'static str` rather than a wrapped error type because there are
    /// exactly two ways this happens and neither needs a backtrace. It also
    /// means the codec carries no serde dependency.
    #[error("cache entry codec: {0}")]
    Codec(&'static str),
}

// `redb` narrows its error type at every call site -- `begin_write` returns
// `TransactionError`, `open_table` returns `TableError`, `insert` returns
// `StorageError`. Each has a `From` into `redb::Error`, so `?` at a `redb`
// call would otherwise need `.map_err(redb::Error::from)` repeated at forty
// sites. These impls are the alternative: one line per redb error type here,
// rather than noise on every transaction.
//
// `redb::Error` itself keeps `#[from]` above because that is the one
// conversion a caller outside this module may want.
impl From<redb::DatabaseError> for CacheError {
    fn from(err: redb::DatabaseError) -> Self {
        Self::Disk(err.into())
    }
}

impl From<redb::TransactionError> for CacheError {
    fn from(err: redb::TransactionError) -> Self {
        Self::Disk(err.into())
    }
}

impl From<redb::TableError> for CacheError {
    fn from(err: redb::TableError) -> Self {
        Self::Disk(err.into())
    }
}

impl From<redb::StorageError> for CacheError {
    fn from(err: redb::StorageError) -> Self {
        Self::Disk(err.into())
    }
}

impl From<redb::CommitError> for CacheError {
    fn from(err: redb::CommitError) -> Self {
        Self::Disk(err.into())
    }
}

impl From<redb::CompactionError> for CacheError {
    fn from(err: redb::CompactionError) -> Self {
        Self::Disk(err.into())
    }
}
