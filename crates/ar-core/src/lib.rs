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

/// Borrows a [`Strng`] as `&str`.
///
/// The only spellings that used to exist were `&**s` (inscrutable) and
/// `(*s).to_owned()` (a read with an allocation in it). `Display` is not
/// implemented for `Arc<str>`, so there was no obvious third choice — now
/// there is one. Prefer this everywhere a [`Strng`] is compared, hashed, or
/// passed to a `&str` parameter; keep `.to_owned()` callers on the owned side.
#[must_use]
#[inline]
pub fn as_str(s: &Strng) -> &str {
    s
}

/// Interns `s` into a [`Strng`].
pub fn intern(s: &str) -> Strng {
    Strng::from(s)
}

#[cfg(test)]
mod tests {
    use super::{Strng, as_str, intern};

    #[test]
    fn as_str_borrows_without_allocating() {
        // The helper exists because the only other spellings were `&**s` and a
        // `.to_owned()` that allocates. One case proves the borrow outlives the
        // call and shares the `Arc`'s buffer rather than copying it.
        let s = intern("openai");
        let borrowed: &str = as_str(&s);
        assert_eq!(borrowed, "openai");
        assert!(std::ptr::eq(
            borrowed.as_ptr(),
            &*s as *const str as *const u8
        ));
        assert_eq!(as_str(&Strng::from("")), "");
    }
}
