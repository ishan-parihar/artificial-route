//! Shared vocabulary for every Artificial Route crate.
//!
//! P0 scope: the interned name type, the three cross-crate trait seams that
//! `ar-translate` / `ar-exec` / `ar-route` will implement, and the placeholder
//! error those seams return until each owning crate brings its own `thiserror`
//! enums (docs/03 assigns those enums to the owning crates, not here).

#![deny(missing_docs)]

use std::sync::Arc;

mod traits;

pub use traits::{ArError, ArExec, ArRoute, ArTranslate, RouteTarget};

/// Cheaply-cloned, immutable string used for provider, model, combo and key
/// names (docs/03: "`Strng` for names").
///
/// // ponytail: `Arc<str>` is 16 bytes, not the 8 bytes `agentgateway`'s
/// `arcstr::ArcStr` gets from its inline-capacity trick. At P0 scale the
/// registry holds a handful of entries, so the 8 bytes buy nothing measurable.
/// Swap in `arcstr` only if a profile of the 300+ provider registry shows the
/// name set matters.
pub type Strng = Arc<str>;

/// Interns `s` into a [`Strng`].
pub fn intern(s: &str) -> Strng {
    Strng::from(s)
}
