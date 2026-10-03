//! Routing error type. `thiserror` in a library, per `docs/03` ch.4.

use crate::strategy::Strategy;

/// Everything routing can refuse.
///
/// There is no catch-all variant: an unmatched error here means a real
/// unhandled case, and a `#[non_exhaustive]` enum would only move that
/// compile error somewhere less useful.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// No candidate providers were supplied, so there is nothing to route to.
    #[error("no candidate providers for this request")]
    NoCandidates,

    /// The requested strategy is a P2 strategy that this build does not carry.
    ///
    /// Reported instead of a `todo!()` panic so a P2-named config degrades to a
    /// specific 501 in the request path (`docs/02` deferred table).
    #[error("strategy `{0}` is deferred to P2 (full-strategies); this build is lean-routing")]
    DeferredStrategy(Strategy),

    /// A model name looked like an `auto/*` alias but is not one this build
    /// carries.
    ///
    /// `NotFound`, not `NotImplemented`: the caller named a model that does
    /// not exist, which is a client mistake, whereas a deferred *strategy* is
    /// a documented feature this binary does not have. The two are different
    /// bugs to chase and must not share a status.
    #[error(
        "unknown auto variant `{name}`; expected one of auto, auto/coding, auto/fast, auto/cheap, auto/smart, auto/chaos"
    )]
    UnknownAutoVariant {
        /// The name as it arrived, echoed back so an operator can fix the
        /// config without reading the source.
        name: String,
    },
}

impl RouteError {
    /// HTTP status this error maps to. `NotImplemented` for a deferred
    /// strategy is deliberate: the caller asked for a feature that exists in
    /// the design and not yet in this binary, which is not a client mistake.
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        match self {
            Self::NoCandidates => http::StatusCode::SERVICE_UNAVAILABLE,
            Self::DeferredStrategy(_) => http::StatusCode::NOT_IMPLEMENTED,
            Self::UnknownAutoVariant { .. } => http::StatusCode::NOT_FOUND,
        }
    }
}
