//! The `rmcp` server. Compiled only with `--features mcp`.
//!
//! The module itself is `#[cfg(feature = "mcp")]` in `lib.rs` too, so a default
//! build has no `transport` in its public API at all rather than an empty one —
//! an empty public module reads as "there is something here" to a host doing
//! capability discovery, and there is not.
//!
//! # TODO(P1-mcp-stdio): wire stdio
//!
//! The feature gate and the dependency are real — `cargo tree -p ar-mcp
//! --features mcp` pulls `rmcp 3.5.0` in, and a default build has none of it —
//! but the handler is not written. Three things are missing: a transport feature
//! (`rmcp/transport-io`) plus `tokio`; a `ServerHandler` that lists
//! [`crate::Tool::ALL`] as tools and dispatches each into
//! [`crate::tools::guard`]; and the `ServiceExt::serve` call the SDK expects.
//!
//! Everything that handler needs is already here and SDK-free: the catalog, the
//! default-deny check, the audit row, the eight bodies (crate-private, so the
//! handler goes through `guard` like everything else). The handler is plumbing,
//! not logic.
//!
//! [`crate::Tool::ALL`]: crate::Tool::ALL
//! [`crate::tools::guard`]: crate::tools::guard

use crate::Error;

/// Serves the essential-8 over stdio.
///
/// # Errors
/// Always [`Error::NotWired`] until the TODO above lands, naming `"stdio"`.
///
/// A typed `Err` rather than the `unimplemented!()` this used to hold. The
/// difference matters: a panic aborts the process, and a control-plane host that
/// probes for a transport it cannot have should get something it can render as
/// a 501, not a dead connection. The signature is already the final one, so
/// filling in the body is a one-line change.
///
/// TODO(P1-mcp-stdio): `rmcp/transport-io` + `tokio` + a `ServerHandler` over
/// `crate::Tool::ALL` dispatching into `crate::guard`.
pub fn serve_stdio() -> Result<(), Error> {
    Err(Error::NotWired { what: "stdio" })
}

/// Whether the stdio server is wired. `false`, and it stays `false` until the
/// TODO above is done.
///
/// A host can read this instead of calling [`serve_stdio`] and catching the
/// error. It is a `const`, not a runtime probe, on purpose: the answer is a
/// property of the *build*, so it is decided at compile time and cannot go
/// stale relative to what was linked in.
pub const STDIO_WIRED: bool = false;

#[cfg(test)]
mod tests {
    use super::{STDIO_WIRED, serve_stdio};
    use crate::Error;

    #[test]
    fn serve_stdio_reports_not_wired_instead_of_panicking() {
        assert!(matches!(serve_stdio(), Err(Error::NotWired { what: "stdio" })));
    }

    #[test]
    fn stdio_wired_is_still_false() {
        // A `const` block, not a runtime assert: clippy is right that this has
        // no runtime work to do, and a build-time failure when someone flips
        // the constant is the check that was wanted anyway.
        const { assert!(!STDIO_WIRED) };
    }
}
