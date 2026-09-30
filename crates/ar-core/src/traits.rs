//! Cross-crate trait seams.
//!
//! These three traits are the contract between `ar-cli`/`ar-http` (callers) and
//! `ar-translate` / `ar-exec` / `ar-route` (implementors). They are declared here
//! rather than in the implementing crate so the signature is pinned in one place
//! and no two agents can drift on it.
//!
//! Every method is `todo!()`: the owning crate fills the body. Bodies exchange
//! `serde_json::Value` / `&[u8]` rather than a canonical request struct because
//! that struct is `ar-translate`'s to define, and pinning it here would invert
//! the dependency. `// TODO(#P0-translate)` marks each seam to be tightened to
//! concrete types once `ar-translate` lands.

use crate::Strng;

/// One routable `(provider, model)` pair.
///
/// TODO(#P0-route): gain per-target weight and health once `ar-route` needs a
/// heterogeneous candidate list; keep it `Copy`-able while it stays this small.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouteTarget {
    /// Provider id, matching a key in the `ar-registry` catalog.
    pub provider: Strng,
    /// Provider-native model id.
    pub model: Strng,
}

/// Placeholder for the three seams' error type.
///
/// docs/03 gives each of `ar-translate` / `ar-exec` / `ar-route` its own
/// `thiserror` enums. Until those crates exist this is the only error the seams
/// can return; each `impl` should replace it with its crate's enum.
///
/// TODO(#P0-translate): delete once all three implementors carry their own error.
#[derive(Debug, thiserror::Error)]
pub enum ArError {
    /// An implementor reached a code path it does not support.
    #[error("unsupported by this implementor: {0}")]
    Unsupported(String),
}

/// Inbound-body -> canonical-request translation (implemented by `ar-translate`).
pub trait ArTranslate {
    /// Normalises an OpenAI-shaped inbound `body` for `provider`/`model`.
    ///
    /// `body` is the raw JSON body, already `$VAR`-expanded and parsed; the
    /// canonical shape `ar-exec` posts upstream is defined by the implementor.
    fn to_canonical(&self, body: serde_json::Value) -> Result<serde_json::Value, ArError>;
}

/// Upstream dispatch (implemented by `ar-exec`).
pub trait ArExec {
    /// POSTs an already-canonical `body` to `url` and returns the response body.
    ///
    /// Takes bytes so the implementor can forward a `BufList` without a copy
    /// (docs/03: "`ar-exec` (reqwest + `BufList`)").
    fn post_chat(&self, url: &str, body: &[u8]) -> Result<serde_json::Value, ArError>;
}

/// Target selection (implemented by `ar-route`).
pub trait ArRoute {
    /// Picks one target out of `candidates`.
    ///
    /// P0 ships the four lean-routing strategies (docs/05); the other 15 arrive
    /// in P2 alongside `simulate_route` / `explain_route`.
    fn pick(&self, candidates: &[RouteTarget]) -> Result<RouteTarget, ArError>;
}
