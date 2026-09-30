//! Token/ledger error type. `thiserror` in a library, per `docs/03` ch.4.

/// Everything counting, pricing and ledger persistence can refuse.
///
/// Counting cannot fail — the exact path and the heuristic path both always
/// produce a number, which is the contract ported from
/// `../OmniRoute/src/shared/utils/tiktokenCounter.ts` ("never throws in a
/// counting path"). So the only error surface here is storage.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TokenError {
    /// The ledger database rejected a statement.
    #[error("ledger database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}
