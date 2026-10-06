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

    /// Log a browser-session OAuth provider in, out, or check on.
    ///
    /// `login` prints the authorize URL to stdout and also tries `$BROWSER` /
    /// `xdg-open` / `open`, so it works on a headless VPS too: carry the URL to
    /// any device, complete consent there, and paste the redirected URL back on
    /// one line of stdin. That single read is the only interaction in the whole
    /// CLI — `--no-browser` forces it, and so does a callback listener that
    /// cannot bind.
    ///
    /// No token, code, or client secret is ever printed. `status` is read-only
    /// and redacted; `logout` deletes the session's credential rows.
    ///
    /// Examples:
    ///   ar auth status
    ///   ar auth login --provider codex
    ///   ar auth login --provider claude --no-browser < redirect.txt
    #[command(verbatim_doc_comment)]
    Auth(AuthArgs),

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

    /// Serve the MCP control plane over stdio (12 tools + tool_search).
    ///
    /// Needs `--features mcp`; without it this verb does not exist, so a host
    /// cannot discover a transport that was never linked in. Point an MCP client
    /// at `ar mcp`; `--list` prints the catalog without touching the config.
    ///
    /// `AR_MCP_SCOPE` narrows what this process may call (`read:*`,
    /// `write:combos`, …); unset means everything.
    ///
    /// Examples:
    ///   ar mcp --list
    ///   AR_MCP_SCOPE='read:*' ar mcp
    #[cfg(feature = "mcp")]
    #[command(verbatim_doc_comment)]
    Mcp(McpArgs),
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

/// Options for `ar mcp`.
#[cfg(feature = "mcp")]
#[derive(Debug, Args)]
pub struct McpArgs {
    /// Print the tool catalog and exit, without serving or reading the config.
    #[arg(long)]
    pub list: bool,
}

/// Subcommands of `ar auth`.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Authorise a provider in a browser and store the resulting tokens.
    ///
    /// Prints the authorize URL, opens it if a browser is available, waits for
    /// the redirect, exchanges the code, and writes the access + refresh rows
    /// into the credential store. Prints the row *names* it wrote, never their
    /// values.
    ///
    /// With `--no-browser` — or when the loopback callback listener cannot bind
    /// — the URL is printed and this reads exactly one line from stdin: the
    /// redirected URL, or a bare code. EOF before that line is an expiry, not a
    /// retry.
    ///
    /// Examples:
    ///   ar auth login --provider codex
    ///   ar auth login --provider codex --no-browser --timeout 600
    ///   ar auth login --provider claude --scope "openid profile"
    #[command(verbatim_doc_comment)]
    Login(AuthLoginArgs),

    /// Delete a session's credential rows from the store.
    ///
    /// Removes the access row, the refresh row, and the client-secret row this
    /// session declares — the `oauth:` block in `config.yaml` is left alone,
    /// because it holds no secret. Prints what was removed. A session with no
    /// rows is a no-op that exits 0.
    ///
    /// Examples:
    ///   ar auth logout --provider codex
    ///   ar auth logout --provider claude --config ./prod/config.yaml
    #[command(verbatim_doc_comment)]
    Logout(AuthProviderArgs),

    /// Report whether each OAuth session is armed, and why not when it is not.
    ///
    /// Read-only and redacted: rows come from the same `oauth_row` verdict
    /// `ar doctor` prints, so the two can never disagree. `unarmed` details name
    /// the exact `ar auth login --provider <id>` that fixes them.
    ///
    /// Examples:
    ///   ar auth status
    ///   ar auth status --provider codex
    #[command(verbatim_doc_comment)]
    Status(AuthProviderArgs),
}

/// Options for `ar auth login`.
#[derive(Debug, Args)]
pub struct AuthLoginArgs {
    /// Registry provider id to authorise, e.g. `codex`.
    #[arg(long, value_name = "ID")]
    pub provider: String,

    /// Loopback port for the redirect callback. `0` picks a free one.
    ///
    /// Only honoured when the session declares no `redirect_uri:` of its own: a
    /// registered redirect has to be named byte-for-byte on the authorize URL, so
    /// asking for a different port there is refused rather than ignored.
    #[arg(long, value_name = "PORT", default_value_t = 0)]
    pub port: u16,

    /// Do not try to open a browser; read the redirect from stdin instead.
    ///
    /// The right flag on a headless host, and the automatic path when the
    /// callback listener cannot bind.
    #[arg(long)]
    pub no_browser: bool,

    /// Seconds to wait for the redirect before giving up.
    #[arg(long, value_name = "SECONDS", default_value_t = 300)]
    pub timeout: u64,

    /// Scope to request, overriding the session's `scope:`.
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,
}

/// Options for the `ar auth` verbs that name one provider.
#[derive(Debug, Args)]
pub struct AuthProviderArgs {
    /// Registry provider id, e.g. `codex`. Omit on `status` for every session.
    #[arg(long, value_name = "ID")]
    pub provider: Option<String>,
}

/// Options for `ar auth`.
#[derive(Debug, Args)]
pub struct AuthArgs {
    /// Which half of the login lifecycle to run.
    #[command(subcommand)]
    pub command: AuthCommand,
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

    /// Read real combos from this OmniRoute `storage.sqlite` instead of
    /// synthesising one combo per `provider/model`.
    ///
    /// `--from omniroute` only. Without it the import reproduces the catalog and
    /// none of the operator's failover chains, weights or per-combo windows —
    /// the file parses and routes, just not the way their dashboard does.
    #[arg(long, value_name = "STORAGE_SQLITE")]
    pub combos: Option<PathBuf>,

    /// Directory the two files are written into. Created if absent.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub out_dir: PathBuf,
}
