//! clap command surface.
//!
//! Per `docs/06-axi-mcp.md`: a content-first bare `ar`, per-command `--help`
//! with two worked examples, and no interactive prompts anywhere. Missing
//! required input fails with usage rather than blocking on a prompt, because a
//! prompt is a deadlock for an agent driving the binary non-interactively.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::import::ImportFrom;

/// Default config location, overridable with `--config` or `AR_CONFIG`.
pub const DEFAULT_CONFIG: &str = "config.yaml";

/// One OpenAI-compatible LLM proxy over many providers.
#[derive(Debug, Parser)]
#[command(
    name = "ar",
    version,
    about = "One OpenAI-compatible LLM endpoint over many providers",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Path to the YAML config.
    #[arg(long, short = 'c', global = true, value_name = "PATH", default_value = DEFAULT_CONFIG)]
    pub config: PathBuf,

    /// Subcommand. Omit for the home view.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The command surface. Seven verbs: one long-running, one network, five local.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the proxy listener until interrupted.
    ///
    /// Examples:
    ///   ar serve
    ///   ar serve --port 20129
    #[command(verbatim_doc_comment)]
    Serve(ServeArgs),

    /// List routable models: every combo target, plus registry-declared models.
    ///
    /// Examples:
    ///   ar models
    ///   ar models --fields id,combo
    #[command(verbatim_doc_comment)]
    Models(ListArgs),

    /// List configured providers, their wire dialect, and their key binding.
    ///
    /// Examples:
    ///   ar providers
    ///   ar providers --fields id,base_url
    #[command(verbatim_doc_comment)]
    Providers(ListArgs),

    /// List routing combos and their distinct provider sets.
    ///
    /// Examples:
    ///   ar combo
    ///   ar combo --fields id,strategy
    #[command(verbatim_doc_comment)]
    Combo(ListArgs),

    /// Check config, credentials and registry; exits 1 on any failure.
    ///
    /// Examples:
    ///   ar doctor
    ///   ar doctor --config ./config.yaml
    #[command(verbatim_doc_comment)]
    Doctor,

    /// One completion through the real router, printed to stdout.
    ///
    /// Examples:
    ///   ar run -p 'hello'
    ///   ar run --model cheap --full -p 'hello'
    #[command(verbatim_doc_comment)]
    Run(RunArgs),
    /// Show the effective configuration, after environment-variable resolution.
    ///
    /// Examples:
    ///   ar configure
    ///   ar configure --check
    #[command(verbatim_doc_comment)]
    Configure(ConfigureArgs),

    /// Convert another tool's provider/model list into config.yaml + registry.json.
    ///
    /// Writes no credential: keys stay `$VAR` references. `omniroute` reads a
    /// models.dev-shaped provider map, fetching it live when `--path` is absent.
    ///
    /// config.yaml is picked up live (ar-config watches it). registry.json is
    /// the compile-time baseline, so it only changes after a rebuild — the live
    /// catalog from the same fetch is what `ar models` reflects meanwhile.
    ///
    /// Examples:
    ///   ar import --from litellm --path ./litellm.yaml
    ///   ar import --from omniroute --out-dir ./config
    #[command(verbatim_doc_comment)]
    Import(ImportArgs),
}

/// Options for `ar serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Listen port, overriding `server.port` from the config.
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,
}

/// Shared options for the list commands.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Comma-separated columns to show instead of the default set.
    #[arg(long, value_name = "a,b,c")]
    pub fields: Option<String>,

    /// Print values in full instead of cutting them at the truncation ceiling.
    #[arg(long)]
    pub full: bool,
}

/// Options for `ar run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Combo to route through. Defaults to the first configured combo.
    #[arg(long, short = 'm', value_name = "ID")]
    pub model: Option<String>,

    /// The user message to send.
    #[arg(long, short = 'p', value_name = "TEXT")]
    pub prompt: String,

    /// Print the whole response body instead of truncating it.
    #[arg(long)]
    pub full: bool,
}

/// Options for `ar configure`.
#[derive(Debug, Args)]
pub struct ConfigureArgs {
    /// Exit 1 when any check fails, instead of only reporting.
    #[arg(long)]
    pub check: bool,
}

/// Options for `ar import`.
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// The upstream config shape to read.
    #[arg(long, value_enum, value_name = "omniroute|litellm")]
    pub from: ImportFrom,

    /// Read this file instead of fetching models.dev live.
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Directory the two files are written into. Created if absent.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub out_dir: PathBuf,
}
