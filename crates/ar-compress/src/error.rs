//! The crate's only error type. `thiserror` in a library, per `docs/03` ch.4.

/// Everything in `ar-compress` that can refuse.
///
/// Plan resolution and the budget clamp are **total functions**: an
/// unrecognized header falls through to the next precedence layer, and an
/// unreachable budget is cut down rather than reported. Neither has a failure
/// mode, because a live request must not take a compression branch as an
/// outage.
///
/// The single fallible step is corpus ingestion, where a malformed case would
/// otherwise be scored as if it were valid and silently skew the fidelity
/// numbers the next release is promoted on.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompressError {
    /// A corpus case is malformed, or an ingested case failed vetting.
    #[error("eval corpus: case {id:?} {reason}")]
    CorpusCase {
        /// The offending case's id, or `"?"` when the case had none.
        id: String,
        /// Why the case was rejected.
        reason: &'static str,
    },
}
