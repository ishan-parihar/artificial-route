//! Every refusal this crate can produce. `thiserror` in a library, per
//! `docs/03-crates-and-deps.md` ch.4. No catch-all variant, mirroring
//! `ar_keys::KeyError`: an unmatched error means a real unhandled case.

use std::fmt;
use std::io;

/// Why an observability write was refused.
#[derive(Debug, thiserror::Error)]
pub enum ObsError {
    /// A trace directory or file could not be opened, written, or pruned.
    #[error("trace io: {0}")]
    Io(#[from] io::Error),

    /// The `redb` store refused a transaction or a value.
    ///
    /// `redb` splits failures across `StorageError` / `TableError` /
    /// `DatabaseError` and exposes `Database::create` as a bare
    /// `io::Error`, none of which compose into one `#[from]`. Rendering the
    /// `Display` keeps `?` at the call sites and this a `thiserror` enum.
    #[error("obs store: {0}")]
    Store(String),

    /// The raw audit path was reached without an admin grant.
    ///
    /// This is the check that makes a stolen `read:*` token useless for
    /// traffic: a read grant reaches [`crate::AuditLedger::open`] and its
    /// redacted rows, and nothing else.
    #[error("raw audit requires an `admin` grant; `read:*` is not enough")]
    AdminScope,

    /// A global tracing subscriber is already installed.
    #[error("a global tracing subscriber is already installed")]
    Subscriber,
}

impl ObsError {
    /// Wraps a store failure without threading its concrete error type through
    /// every call site.
    pub(crate) fn store(e: impl fmt::Display) -> Self {
        Self::Store(e.to_string())
    }
}