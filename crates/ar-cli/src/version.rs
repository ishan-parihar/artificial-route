//! Leaf module for the `--version` fast path.
//!
//! Deliberately std-only: this module is what runs *before* the clap command
//! graph is built, so it must not drag in clap, serde, or the config loader.
//! `docs/06-axi-mcp.md` requires a bare `-v` / `-V` / `--version` to exit 0
//! ahead of that graph; OmniRoute does the same ahead of its Commander
//! registration chain.

use std::ffi::OsString;
use std::process::ExitCode;

/// Crate version, taken from the manifest at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Name the binary is invoked as in help and version output.
///
/// Hardcoded rather than `CARGO_PKG_NAME`: the crate is `ar-cli` but the binary
/// is `aroute`, and `env!("CARGO_PKG_NAME")` would print `ar-cli 0.1.4` in
/// version output and in usage strings. Not `ar` — the Rust `ar` crate (the .a
/// archiver, `cargo install ar`) owns that on $PATH, and an LLM proxy and an
/// archiver with one name is a coin flip for whoever is in a hurry.
pub const BIN: &str = "aroute";

/// Answers a bare version query without building the command graph.
///
/// Returns `None` for anything else, including `--version` alongside another
/// argument, so clap keeps ownership of the general case.
///
/// `-v` is version rather than verbosity: `docs/06` lists all three flags for
/// the fast path, and an agent-facing CLI has no use for a verbosity toggle.
pub fn fast_path(args: &[OsString]) -> Option<ExitCode> {
    let [only] = args else { return None };
    if only == "-v" || only == "-V" || only == "--version" {
        println!("{BIN} {VERSION}");
        return Some(ExitCode::SUCCESS);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn exits_zero_when_bare_version_flag() {
        assert_eq!(fast_path(&os(&["--version"])), Some(ExitCode::SUCCESS));
    }

    #[test]
    fn defers_to_clap_when_subcommand_present() {
        assert_eq!(fast_path(&os(&["serve", "--version"])), None);
    }
}
