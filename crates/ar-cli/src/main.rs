//! The `ar` binary.
//!
//! AXI contract from `docs/06-axi-mcp.md`:
//!
//! * `--version` on a bare invocation exits 0 before the command graph loads
//!   (see [`version`], which is std-only for exactly that reason).
//! * `0` success, `1` error, `2` usage. Never anything else.
//! * stdout carries data *and* structured errors; stderr carries diagnostics.
//! * No interactive prompt, anywhere: a blocking read is a deadlock for an agent
//!   driving this non-interactively.

#![deny(missing_docs)]

mod auth;
mod cli;
mod commands;
mod import;
#[cfg(feature = "mcp")]
mod mcp;
mod serve;
mod toon;
mod version;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;
use clap::error::{ContextKind, ErrorKind};

use crate::cli::Cli;

/// Exit code for a handled error, per `docs/06`.
const EXIT_ERROR: u8 = 1;
/// Exit code for misuse, per `docs/06`. Matches what clap returns for its own
/// parse failures.
const EXIT_USAGE: u8 = 2;

/// jemalloc rather than glibc malloc: the 00-overview RAM budget is <35MB idle,
/// and decay behaviour is the lever that keeps a long-lived proxy flat. Set here
/// rather than in a library because a global allocator must be declared by the
/// final binary.
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn main() -> ExitCode {
    // `try_init` rather than `init`: a library that already installed a
    // subscriber is not a reason to abort the CLI.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    run(std::env::args_os().skip(1))
}

/// Runs one invocation and returns its exit code.
///
/// Takes the arguments rather than reading `argv` so the whole surface, exit
/// codes included, is testable in-process.
fn run<I: Iterator<Item = OsString>>(args: I) -> ExitCode {
    let args: Vec<OsString> = args.collect();

    // Fast path first: answering "what version is this" must not build the
    // command graph, touch the filesystem, or load the config.
    if let Some(code) = version::fast_path(&args) {
        return code;
    }

    let parsed = Cli::try_parse_from(
        std::iter::once(OsString::from(version::BIN)).chain(args.iter().cloned()),
    );
    let cli = match parsed {
        Ok(cli) => cli,
        Err(e) => {
            // clap already formats help and usage; it goes to stderr because it
            // is diagnostics, not data.
            let code = commands::usage_exit_code(e.kind());
            let _ = e.print();
            // An unknown flag also gets a line on stdout, which is where an agent
            // reads errors and suggestions from (`docs/06`). clap's own output
            // names the flag but leaves the valid set to a `--help` round trip.
            if e.kind() == ErrorKind::UnknownArgument {
                let invalid = e
                    .get(ContextKind::InvalidArg)
                    .map_or_else(|| "<argument>".to_owned(), ToString::to_string);
                println!("error: {}", commands::unknown_flag_hint(&args, &invalid));
            }
            return ExitCode::from(code);
        }
    };

    // A `--fields` value clap cannot validate: the valid set depends on the
    // command, so this is the same class of mistake as an unknown flag.
    if let Some(message) = commands::check_fields(&cli) {
        println!("error: {message}");
        return ExitCode::from(EXIT_USAGE);
    }

    match commands::dispatch(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Errors are data for an agent, so they go to stdout (docs/06).
            println!("error: {e}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(args: &[&str]) -> ExitCode {
        run(args.iter().map(OsString::from))
    }

    #[test]
    fn exits_zero_when_version_requested() {
        assert_eq!(code(&["--version"]), ExitCode::SUCCESS);
    }

    #[test]
    fn exits_two_when_flag_unknown() {
        assert_eq!(code(&["--nope"]), ExitCode::from(EXIT_USAGE));
    }

    #[test]
    fn exits_two_when_subcommand_flag_unknown() {
        assert_eq!(code(&["models", "--nope"]), ExitCode::from(EXIT_USAGE));
    }

    #[test]
    fn exits_zero_when_help_requested() {
        assert_eq!(code(&["--help"]), ExitCode::SUCCESS);
    }

    #[test]
    fn exits_zero_when_subcommand_help_requested() {
        assert_eq!(code(&["run", "--help"]), ExitCode::SUCCESS);
    }

    #[test]
    fn exits_two_when_required_prompt_missing() {
        assert_eq!(code(&["run"]), ExitCode::from(EXIT_USAGE));
    }

    #[test]
    fn exits_two_when_fields_value_unknown() {
        assert_eq!(
            code(&["providers", "--fields", "nope"]),
            ExitCode::from(EXIT_USAGE)
        );
    }
}
