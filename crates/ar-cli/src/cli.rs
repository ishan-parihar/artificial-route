//! clap command surface.
//!
//! Per `docs/06-axi-mcp.md`: a content-first bare `aroute`, per-command `--help`
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
    name = "aroute",
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
    ///   aroute serve
    ///   aroute serve --port 20129
    #[command(verbatim_doc_comment)]
    Serve(ServeArgs),

    /// List routable models: every combo target, plus registry-declared models.
    ///
    /// Examples:
    ///   aroute models
    ///   aroute models --fields id,combo
    #[command(verbatim_doc_comment)]
    Models(ListArgs),

    /// List configured providers, their wire dialect, and their key binding.
    ///
    /// Examples:
    ///   aroute providers
    ///   aroute providers --fields id,base_url
    #[command(verbatim_doc_comment)]
    Providers(ListArgs),

    /// List routing combos and their distinct provider sets.
    ///
    /// Examples:
    ///   aroute combo
    ///   aroute combo --fields id,strategy
    #[command(verbatim_doc_comment)]
    Combo(ListArgs),

    /// Check config, credentials and registry; exits 1 on any failure.
    ///
    /// Examples:
    ///   aroute doctor
    ///   aroute doctor --config ./config.yaml
    #[command(verbatim_doc_comment)]
    Doctor,

    /// One completion through the real router, printed to stdout.
    ///
    /// Examples:
    ///   aroute run -p 'hello'
    ///   aroute run --model cheap --full -p 'hello'
    #[command(verbatim_doc_comment)]
    Run(RunArgs),
    /// Show the effective configuration, after environment-variable resolution.
    ///
    /// Examples:
    ///   aroute configure
    ///   aroute configure --check
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
    ///   aroute auth status
    ///   aroute auth login --provider codex
    ///   aroute auth login --provider claude --no-browser < redirect.txt
    #[command(verbatim_doc_comment)]
    Auth(AuthArgs),

    /// Manage the HTTP gate's credentials: arm it, mint client tokens.
    ///
    /// The gate verifies signed tokens minted from a 32-byte master key.
    /// `arm-gate` generates that master into the credential store and prints
    /// the first client token; `mint` issues another from the stored master.
    /// `aroute serve` reads the master from the store at boot, and `aroute
    /// doctor` reports the gate armed once the row exists.
    ///
    /// Examples:
    ///   aroute keys arm-gate --key-id prod-deploy
    ///   aroute keys mint --key-id ci-runner
    #[command(verbatim_doc_comment)]
    Keys(KeysArgs),

    /// View and edit the `limits:` policy block: per-key rpm and spend ceilings.
    ///
    /// With no subcommand, prints the block as one row per key — the `default`
    /// row governs anonymous traffic and any key without one of its own.
    /// `set` merges the arms it is given into one row (`--key` names a client
    /// key; omit it for the default row) and rewrites the section; `clear`
    /// removes one key's row, or the whole block.
    ///
    /// Examples:
    ///   aroute limits
    ///   aroute limits set --key prod-deploy --rpm 600 --usd-micros 5000000
    ///   aroute limits clear --key prod-deploy
    #[command(verbatim_doc_comment)]
    Limits(LimitsCliArgs),

    /// Convert another tool's provider/model list into config.yaml + registry.json.
    ///
    /// Writes no credential: keys stay `$VAR` references. `omniroute` reads a
    /// models.dev-shaped provider map, fetching it live when `--path` is absent.
    ///
    /// config.yaml is picked up live (ar-config watches it). registry.json is
    /// the compile-time baseline, so it only changes after a rebuild — the live
    /// catalog from the same fetch is what `aroute models` reflects meanwhile.
    ///
    /// Examples:
    ///   aroute import --from litellm --path ./litellm.yaml
    ///   aroute import --from omniroute --out-dir ./config
    #[command(verbatim_doc_comment)]
    Import(ImportArgs),

    /// Pull the dashboard's combos, providers and API keys into the routing config.
    ///
    /// The web dashboard is the configuration surface: everything it saves lands
    /// in its integrated sqlite (`dashboard-data/storage.sqlite` beside the
    /// config). This verb is the bridge onto the routing engine — it rewrites
    /// the `combos:` and `custom_providers:` sections of config.yaml from that
    /// DB, and materialises every provider connection's API key into the env
    /// file the service loads. The proxy builds its routing tables at boot,
    /// so when anything changed sync restarts `aroute.service` through systemd
    /// (pass --no-restart to get the command printed instead).
    ///
    /// Examples:
    ///   aroute --config ~/.config/ar/config.yaml sync
    ///   aroute sync --db ~/.config/ar/dashboard-data/storage.sqlite --no-restart
    #[command(verbatim_doc_comment)]
    Sync(SyncArgs),

    /// Serve the MCP control plane over stdio (12 tools + tool_search).
    ///
    /// Needs `--features mcp`; without it this verb does not exist, so a host
    /// cannot discover a transport that was never linked in. Point an MCP client
    /// at `aroute mcp`; `--list` prints the catalog without touching the config.
    ///
    /// `AR_MCP_SCOPE` narrows what this process may call (`read:*`,
    /// `write:combos`, …); unset means everything.
    ///
    /// Examples:
    ///   aroute mcp --list
    ///   AR_MCP_SCOPE='read:*' ar mcp
    #[cfg(feature = "mcp")]
    #[command(verbatim_doc_comment)]
    Mcp(McpArgs),

    /// Run the bundled web UI server until interrupted.
    ///
    /// Spawns the compiled dashboard — a self-contained Node server (`server.js`
    /// plus its runtime, built by `dashboard/rebrand-dist.sh`) — on its own port;
    /// `aroute serve` keeps the `/v1` proxy API on `server.port`. The dist
    /// directory comes from `--path`, else `$AR_DASHBOARD_DIR`, else a
    /// `dashboard/dist` beside the binary.
    ///
    /// The child binds loopback only, and its data lives under `$DATA_DIR`
    /// (default `~/.config/ar/dashboard-data`), never inside aroute's config.
    ///
    /// Examples:
    ///   aroute dashboard
    ///   aroute dashboard --port 20150 --path ./dashboard/dist
    #[command(verbatim_doc_comment)]
    Dashboard(DashboardArgs),
}

/// Options for `aroute dashboard`.
#[derive(Debug, Args)]
pub struct DashboardArgs {
    /// Port for the dashboard server.
    #[arg(long, value_name = "PORT", default_value_t = DEFAULT_DASHBOARD_PORT)]
    pub port: u16,

    /// The compiled dashboard dist directory (the one containing `server.js`).
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,
}

/// Dashboard default port; `aroute serve` keeps `server.port` (20128).
pub const DEFAULT_DASHBOARD_PORT: u16 = 20149;

/// Options for `aroute serve`.
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

/// Options for `aroute run`.
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

/// Options for `aroute configure`.
#[derive(Debug, Args)]
pub struct ConfigureArgs {
    /// Exit 1 when any check fails, instead of only reporting.
    #[arg(long)]
    pub check: bool,
}

/// Options for `aroute mcp`.
#[cfg(feature = "mcp")]
#[derive(Debug, Args)]
pub struct McpArgs {
    /// Print the tool catalog and exit, without serving or reading the config.
    #[arg(long)]
    pub list: bool,
}

/// Options for `aroute keys`.
#[derive(Debug, Args)]
pub struct KeysArgs {
    #[command(subcommand)]
    pub command: KeysCommand,
}

/// Subcommands of `aroute keys`.
#[derive(Debug, Subcommand)]
pub enum KeysCommand {
    /// Arm the bearer gate: generate a 32-byte master key into the credential
    /// store, then mint and print the first client token.
    ///
    /// The master key itself is never printed — it lives encrypted in the
    /// store and `aroute serve` reads it from there at boot. The client token
    /// is printed once, with its expiry, because a token that is never shown
    /// cannot authorize anyone. `--key <hex>` stores an explicit 32-byte
    /// master instead of generating one.
    ///
    /// Examples:
    ///   aroute keys arm-gate
    ///   aroute keys arm-gate --key-id prod-deploy
    ///   aroute keys arm-gate --key $(openssl rand -hex 32)
    #[command(verbatim_doc_comment)]
    ArmGate(ArmGateArgs),

    /// Mint another client token from the already-stored gate master.
    ///
    /// The first token comes from `arm-gate`; this is for the second client
    /// onward. The token is printed once, with its expiry.
    ///
    /// Examples:
    ///   aroute keys mint --key-id ci-runner
    #[command(verbatim_doc_comment)]
    Mint(MintArgs),
}

/// Options for `aroute keys arm-gate`.
#[derive(Debug, Args)]
pub struct ArmGateArgs {
    /// Key id the minted token belongs to: what the usage ledger and the
    /// `limits.keys:` block key this credential by.
    #[arg(long, default_value = "default")]
    pub key_id: String,
    /// An explicit 32-byte master key, hex-encoded, instead of a generated one.
    #[arg(long, value_name = "HEX_32")]
    pub key: Option<String>,
}

/// Options for `aroute keys mint`.
#[derive(Debug, Args)]
pub struct MintArgs {
    /// Key id the minted token belongs to: what the usage ledger and the
    /// `limits.keys:` block key this credential by.
    #[arg(long, default_value = "default")]
    pub key_id: String,
}

/// Options for `aroute limits`. No subcommand is the view.
#[derive(Debug, Args)]
pub struct LimitsCliArgs {
    #[command(subcommand)]
    pub command: Option<LimitsCommand>,
}

/// Subcommands of `aroute limits`. `None` prints the block.
#[derive(Debug, Subcommand)]
pub enum LimitsCommand {
    /// Merge arms into one `limits:` row and rewrite the section.
    ///
    /// Named arms are set on the row; arms not passed stay as they were — the
    /// merge is per arm, so `set --rpm` cannot quietly drop a ceiling the row
    /// already carried. A zero arm is refused: `0` reads as "never" to an
    /// operator and "refill from empty forever" to the bucket, the one
    /// ambiguity the parser refuses too. To remove a row, `clear`.
    ///
    /// Examples:
    ///   aroute limits set --rpm 60
    ///   aroute limits set --key ci --rpm 120 --tokens 100000
    ///   aroute limits set --key ci --refuse-unpriced
    ///   aroute limits set --key ci --refuse-unpriced false
    #[command(verbatim_doc_comment)]
    Set(SetLimitsArgs),

    /// Remove one key's row (`--key`), or the whole `limits:` block.
    ///
    /// Examples:
    ///   aroute limits clear --key ci
    ///   aroute limits clear
    #[command(verbatim_doc_comment)]
    Clear(ClearLimitsArgs),
}

/// Options for `aroute limits set`.
#[derive(Debug, Args)]
pub struct SetLimitsArgs {
    /// The client key whose row this sets. Omit for the `default` row.
    #[arg(long)]
    pub key: Option<String>,
    /// Requests per minute, as a token bucket keyed by the credential.
    #[arg(long)]
    pub rpm: Option<u32>,
    /// Cumulative micro-dollar ceiling, against the usage ledger's spend.
    #[arg(long)]
    pub usd_micros: Option<u64>,
    /// Cumulative token ceiling, against the usage ledger's spend.
    #[arg(long)]
    pub tokens: Option<u64>,
    /// Refuse models with no pricing row. `--refuse-unpriced` alone is true;
    /// `--refuse-unpriced false` unsets it.
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = clap::value_parser!(bool)
    )]
    pub refuse_unpriced: Option<bool>,
}

/// Options for `aroute limits clear`.
#[derive(Debug, Args)]
pub struct ClearLimitsArgs {
    /// The client key whose row is removed. Omit for the whole block.
    #[arg(long)]
    pub key: Option<String>,
}

/// Subcommands of `aroute auth`.
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
    ///   aroute auth login --provider codex
    ///   aroute auth login --provider codex --no-browser --timeout 600
    ///   aroute auth login --provider claude --scope "openid profile"
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
    ///   aroute auth logout --provider codex
    ///   aroute auth logout --provider claude --config ./prod/config.yaml
    #[command(verbatim_doc_comment)]
    Logout(AuthProviderArgs),

    /// Report whether each OAuth session is armed, and why not when it is not.
    ///
    /// Read-only and redacted: rows come from the same `oauth_row` verdict
    /// `aroute doctor` prints, so the two can never disagree. `unarmed` details name
    /// the exact `aroute auth login --provider <id>` that fixes them.
    ///
    /// Examples:
    ///   aroute auth status
    ///   aroute auth status --provider codex
    #[command(verbatim_doc_comment)]
    Status(AuthProviderArgs),
}

/// Options for `aroute auth login`.
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

/// Options for the `aroute auth` verbs that name one provider.
#[derive(Debug, Args)]
pub struct AuthProviderArgs {
    /// Registry provider id, e.g. `codex`. Omit on `status` for every session.
    #[arg(long, value_name = "ID")]
    pub provider: Option<String>,
}

/// Options for `aroute auth`.
#[derive(Debug, Args)]
pub struct AuthArgs {
    /// Which half of the login lifecycle to run.
    #[command(subcommand)]
    pub command: AuthCommand,
}

/// Options for `aroute import`.
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

/// Options for `aroute sync`.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// The dashboard's integrated sqlite to read. Defaults to
    /// `dashboard-data/storage.sqlite` beside the config.
    #[arg(long, value_name = "STORAGE_SQLITE")]
    pub db: Option<PathBuf>,

    /// The env file the service loads (systemd `EnvironmentFile`), where
    /// provider keys are materialised. Defaults to `ar.env` beside the
    /// config. Values are written there, never to config.yaml.
    #[arg(long, value_name = "FILE")]
    pub env: Option<PathBuf>,

    /// Print the restart command instead of running it, for a sync that must
    /// not disturb in-flight traffic.
    #[arg(long)]
    pub no_restart: bool,
}
