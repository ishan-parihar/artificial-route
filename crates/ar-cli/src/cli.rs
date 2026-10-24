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

    /// Talk to the system (not user) systemd scope.
    ///
    /// Global, and therefore position-independent like `--config`:
    /// `aroute service --system status` and `aroute service status --system`
    /// must not be two different commands. `install.sh` writes the user scope
    /// by default, so `user` is the default here too.
    #[arg(long, global = true)]
    pub system: bool,

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
    ///   aroute doctor --json
    #[command(verbatim_doc_comment)]
    Doctor(DoctorArgs),

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

    /// Spend and token usage since the ledger's last reset, most recent first.
    ///
    /// Reads the same sqlite ledger `aroute serve` writes (`usage.sqlite` beside
    /// the credential store), through the same renderer the MCP's
    /// `ar_cost_report` uses, so the two surfaces cannot drift. It creates no
    /// file: a host with no ledger prints the definitive-empty form rather than
    /// a new empty sqlite that claims "0 rows" means "nothing spent".
    ///
    /// Examples:
    ///   aroute cost-report
    ///   aroute cost-report --limit 100
    ///   aroute cost-report --key prod-deploy
    #[command(verbatim_doc_comment)]
    CostReport(CostReportArgs),

    /// Manage the `aroute.service` / `aroute-dashboard.service` systemd
    /// units: status, start, stop, restart.
    ///
    /// The one lifecycle verb the CLI was missing (`docs/36` G6). `aroute sync`
    /// already knew how to restart the proxy, so the machinery existed; what
    /// did not was a way to ask, from an agent, whether the thing I am about
    /// to send traffic to is running — or to bring it back when it is not.
    ///
    /// `status` is the default and is read-only: two `systemctl show` reads,
    /// no mutation. The three changing verbs require naming the unit
    /// explicitly (`aroute service restart proxy`, not `aroute service
    /// restart`), because a CLI that can stop a production listener on a
    /// guess is a foot-gun. `--user` is the default scope, matching how
    /// `install.sh` writes the units; `--system` is accepted because the
    /// same units can be installed system-wide.
    Service(ServiceArgs),

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

    /// Support development 💜
    Donate,
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

    /// Emit the same rows as JSON instead of TOON.
    ///
    /// One query, two renderings: `--json` is the machine opt-in and TOON
    /// stays the default for humans (`docs/06`, "TOON on stdout, JSON inside" —
    /// this widens that to a CLI machine mode). The rows are identical, so a
    /// caller can switch formats without learning a second shape.
    ///
    /// A JSON value is never truncated: the ceiling exists to keep a
    /// human-readable table narrow, and a machine that needs a long value can
    /// parse it.
    #[arg(long)]
    pub json: bool,

    /// Print at most this many rows. `TOTAL` stays the full row count, so
    /// `count: N of TOTAL` tells the caller whether it saw everything.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Skip this many rows before printing. With `--limit` it pages.
    #[arg(long, value_name = "N")]
    pub offset: Option<usize>,

    /// Only models of this provider, e.g. `--provider openai`. Matches the
    /// model's own provider (`openai/gpt-4o-mini` -> `openai`), not the
    /// registry's wire dialect.
    #[arg(long, value_name = "ID")]
    pub provider: Option<String>,

    /// Only models this config routes — its combo targets.
    ///
    /// The complement of the 72 KB registry dump. `--routable` does NOT mean
    /// `status == routable`: that string requires both provider and model to be
    /// in the compiled-in registry, and filtering on it hid every target of a
    /// config whose providers were all custom. A target that cannot currently
    /// route is still listed, with the status that says why.
    #[arg(long)]
    pub routable: bool,

    /// Only models owned by this combo.
    #[arg(long, value_name = "ID")]
    pub combo: Option<String>,
}

/// The status vocabulary a check row can carry, as the CLI names it.
///
/// A `ValueEnum` so `--status bogus` is a clap usage error (exit 2) that lists
/// the valid values, exactly like `--fields bogus`. It is not merely a nicety:
/// without it a misspelling would filter to zero rows and print an empty table,
/// which an agent cannot distinguish from "this config is clean".
///
/// `skip` is a real state, not "ok": a check that could not run is not a
/// check that passed, and letting a caller filter on `ok` alone and trust the
/// exit code is the point of the enum being exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CheckStatusArg {
    Ok,
    Skip,
    Fail,
}

impl CheckStatusArg {
    /// The string `findings()` writes in column 2, so the filter and the
    /// renderer share one spelling each instead of two.
    pub fn as_row_value(self) -> &'static str {
        match self {
            CheckStatusArg::Ok => "ok",
            CheckStatusArg::Skip => "skip",
            CheckStatusArg::Fail => "fail",
        }
    }
}

/// Options for `aroute doctor`.
///
/// Separate from [`ListArgs`] because `doctor` has no `--fields`: its three
/// columns are the answer, not a projection of one.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Emit the same checks as JSON instead of TOON.
    ///
    /// One query, two renderings, the same rule as the list verbs: TOON stays
    /// the default for humans and `--json` is the machine opt-in.
    #[arg(long)]
    pub json: bool,

    /// Print only rows of this status, e.g. `--status fail`.
    ///
    /// A `ValueEnum`, not a `String`: an unrecognised value exits **2** with
    /// clap naming the valid ones inline, which is the same behaviour
    /// `--fields bogus` and `--limit abc` already have. Baking the vocabulary in
    /// also keeps the filter honest — the three values are exactly what
    /// `findings()` emits, so a typo cannot silently match nothing.
    ///
    /// The filter is over the PRINTED rows, never over the exit code: `doctor`
    /// still exits 1 when anything failed, whether or not the row was printed.
    /// A filter that also hid the failure would make `aroute doctor --status
    /// ok` look like a passing build, which is the exact class of silent
    /// success this CLI refuses everywhere else.
    #[arg(long, value_enum)]
    pub status: Option<CheckStatusArg>,

    /// Print at most this many rows. `TOTAL` stays the full check count.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Skip this many rows before printing. With `--limit` it pages.
    #[arg(long, value_name = "N")]
    pub offset: Option<usize>,
}

/// The two units `install.sh` writes, named the way an operator names them.
///
/// A closed enum rather than a free unit string: a typo'd unit name would make
/// `systemctl` fail with its own message, and worse, a caller who typed a
/// different service's name would get a confusing "not found" for something
/// that has nothing to do with aroute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ServiceUnit {
    /// The proxy and its `/v1` API — what every other verb talks to.
    Proxy,
    /// The bundled web dashboard.
    Dashboard,
}

impl ServiceUnit {
    /// The unit name as systemd knows it.
    pub fn unit_name(self) -> &'static str {
        match self {
            ServiceUnit::Proxy => "aroute.service",
            ServiceUnit::Dashboard => "aroute-dashboard.service",
        }
    }

    /// The port this unit serves.
    ///
    /// Reported by `status` because "active" and "answering" are not the same
    /// claim: the incident `docs/25` records was a service systemd called active
    /// while `/healthz` took 15,427 ms.
    pub fn port(self) -> u16 {
        match self {
            ServiceUnit::Proxy => 20128,
            ServiceUnit::Dashboard => 20149,
        }
    }

    /// The name as it is typed on the command line.
    ///
    /// Used in the `help:` sentence so the command printed is one the caller
    /// can paste, rather than the systemd unit name they would have to know.
    pub fn slug(self) -> &'static str {
        match self {
            ServiceUnit::Proxy => "proxy",
            ServiceUnit::Dashboard => "dashboard",
        }
    }
}

/// Options for `aroute service`.
///
/// The changing verbs take a positional unit, not a flag: `aroute service
/// restart proxy` reads as an instruction, and it means no invocation can stop
/// a listener without the caller having named which one.
#[derive(Debug, Args)]
pub struct ServiceArgs {
    /// What to do.
    ///
    /// `status` is the default and the only one that mutates nothing, so
    /// `aroute service` is safe to run for anyone at any time.
    #[command(subcommand)]
    pub action: Option<ServiceAction>,
}

#[derive(Debug, clap::Subcommand)]
pub enum ServiceAction {
    /// Report whether the units are running, and whether they answer.
    Status {
        /// Restrict to one unit; both are reported when omitted.
        #[arg(value_enum)]
        unit: Option<ServiceUnit>,
    },
    /// Start a unit. Reports a no-op when it is already running.
    Start {
        /// Which unit — required here, so no invocation starts something
        /// without naming it.
        #[arg(value_enum)]
        unit: ServiceUnit,
    },
    /// Stop a unit.
    Stop {
        /// Which unit — required for the same reason as `start`.
        #[arg(value_enum)]
        unit: ServiceUnit,
    },
    /// Restart a unit.
    Restart {
        /// Which unit — required for the same reason as `start`.
        #[arg(value_enum)]
        unit: ServiceUnit,
    },
    /// Enable a unit so it starts at boot, without starting it now.
    ///
    /// Deliberately NOT `--now`: `enable` answers "will it come back after a
    /// reboot", a different question from "is it running", and conflating
    /// them is how a read-looking command changes live state. `aroute service
    /// enable dashboard && aroute service start dashboard` is two honest
    /// steps. `status` reports the enabled state alongside the live one so the
    /// distinction is visible without remembering it.
    Enable {
        /// Which unit.
        #[arg(value_enum)]
        unit: ServiceUnit,
    },
    /// Disable boot persistence for a unit, without stopping it now.
    ///
    /// The mirror of `enable`: a disabled unit that is running keeps running
    /// until stopped or rebooted. This is the safe way to say "the dashboard
    /// should not come back after reboot" without interrupting a session that
    /// is using it right now.
    Disable {
        /// Which unit.
        #[arg(value_enum)]
        unit: ServiceUnit,
    },
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
    /// Serve StreamableHTTP at `/mcp` on this loopback address instead of stdio.
    /// Any non-loopback address is refused.
    #[arg(long, value_name = "ADDR")]
    pub http: Option<std::net::SocketAddr>,
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
    /// Report what the credential store actually holds, and what it cannot.
    ///
    /// Row names, OAuth sessions, the revocation count and the gate's own
    /// state. It prints no token material, because the store keeps no table of
    /// issued tokens — a list would be a fabrication — and it says so.
    ///
    /// Examples:
    ///   aroute keys list
    #[command(verbatim_doc_comment)]
    List,

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
    ///   aroute keys arm-gate --force
    ///
    /// Re-arming without `--force` is refused: the master is replaced whether
    /// or not anyone asked, and every token signed by the old one stops
    /// verifying.
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

    /// Revoke a token, so it stops working immediately.
    ///
    /// The token is dead the moment this returns, without waiting for its
    /// expiry: its session is written to the revocation table in the
    /// credential store, and every gate that consults that table refuses it.
    ///
    /// **The whole session dies, not one half of it.** A minted token is a
    /// pair — a short-lived access token and a long-lived refresh token — and
    /// both are valid bearer credentials. Revoking only the access half would
    /// leave the refresh half live for 30 days, which is the outcome this
    /// command exists to prevent.
    ///
    /// This is the remedy for a **leaked** token. The other remedy — rotating
    /// the master — invalidates every token at once, including the ones that
    /// are still wanted, so it is the blunt instrument. This one is the scalpel.
    ///
    /// The token is read from a file or from stdin rather than as an argument,
    /// because a token on a command line lands in the shell history and in the
    /// process table.
    ///
    /// Examples:
    ///   aroute keys revoke --token-file ~/.config/ar/leaked.jwt
    ///   aroute keys revoke --token-file - < leaked.jwt
    #[command(verbatim_doc_comment)]
    Revoke(RevokeArgs),
}

/// Options for `aroute keys revoke`.
#[derive(Debug, Args)]
pub struct RevokeArgs {
    /// A file holding the token to revoke. `-` reads stdin.
    ///
    /// A token is a secret, and a secret on a command line is in the shell
    /// history and in the process table for as long as the command runs.
    #[arg(long, value_name = "PATH")]
    pub token_file: String,
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

    /// Replace an existing gate master.
    ///
    /// Without it, arming over a live gate is refused: the old master is
    /// replaced whether or not anyone asked, and every token signed by it
    /// stops verifying immediately. Pass this when replacing is the intent, and
    /// expect the revocation that comes with it.
    #[arg(long)]
    pub force: bool,
}

/// Options for `aroute keys mint`.
#[derive(Debug, Args)]
pub struct MintArgs {
    /// Key id the minted token belongs to: what the usage ledger and the
    /// `limits.keys:` block key this credential by.
    #[arg(long, default_value = "default")]
    pub key_id: String,
    /// Lifetime of the token in days.
    ///
    /// The default is 30 days, which is a credential an operator has to
    /// remember to replace. A harness that reads its key from a file wants the
    /// opposite: one token, written once, read until the operator decides
    /// otherwise. `0` mints a token that never expires.
    #[arg(long, default_value_t = 30)]
    pub ttl_days: u32,
}

/// Options for `aroute limits`. No subcommand is the view.
#[derive(Debug, Args)]
pub struct LimitsCliArgs {
    #[command(subcommand)]
    pub command: Option<LimitsCommand>,

    /// Emit the same rows as JSON instead of TOON.
    ///
    /// One query, two renderings: TOON stays the default for humans and this is
    /// the machine opt-in, the same rule as the list verbs.
    #[arg(long)]
    pub json: bool,
}

/// Options for `aroute cost-report`.
///
/// The defaults are the ones the MCP uses (`ar-mcp/src/transport.rs:549`), so a
/// `cost-report` on the CLI and an `ar_cost_report` over MCP return the same
/// rows unless the caller narrows one of them.
#[derive(Debug, Args)]
pub struct CostReportArgs {
    /// Most recent rows to print. Rows beyond the limit still count in the
    /// totals, which are computed across the whole ledger.
    #[arg(long, default_value_t = 50, value_name = "N")]
    pub limit: usize,

    /// Only rows written under this key id.
    #[arg(long, value_name = "KEY")]
    pub key: Option<String>,

    /// Emit the same report as JSON instead of TOON.
    ///
    /// The rows are the ones `ar_cost_report` returns; the format is the CLI's
    /// choice, and JSON is the machine-readable one.
    #[arg(long)]
    pub json: bool,
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
    /// Scope the row to one combo id or `provider/model`, so its arms apply
    /// only to that model. `--model ""` clears the scope. The ledger still
    /// totals the key's whole spend (docs/23 G3).
    #[arg(long)]
    pub model: Option<String>,
    /// A per-target token ceiling, as `provider/model=N`.
    ///
    /// The target's OWN spend against this key, not the key's whole spend —
    /// which is what `tokens` measures. A model that burns its budget is
    /// dropped from the chain and the request fails over to a sibling that
    /// has budget left; only a chain with nothing left is refused.
    ///
    /// Repeatable: pass it once per target. The target must be one a combo
    /// routes to, checked at write time so a typo cannot arm a ceiling that
    /// never applies.
    #[arg(long = "model-tokens", value_name = "TARGET=CEILING")]
    pub model_tokens: Vec<String>,
    /// The window the spend arms are measured over, as `24h`.
    ///
    /// Off by default, which means the arms are cumulative: a key that has
    /// spent its budget is refused until the config is edited, because the
    /// ledger is append-only and the spend can never be un-spent. A window
    /// makes them rates — the spend inside the last N hours counts, and the
    /// key is inside its budget again once the old spend falls out of it.
    ///
    /// Covers both spend arms: the key-wide `tokens` and the per-target
    /// `model_tokens`.
    #[arg(long, value_name = "WINDOW")]
    pub spend_window: Option<String>,
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

    /// Show what WOULD change and write nothing. Ledger **F16** / register
    /// **DASH-10**: `--no-restart` only skips the service restart — the files
    /// are still rewritten — so an operator inspecting the bridge silently
    /// mutated their routing config. This computes the same output and prints
    /// a unified diff per file instead of writing them.
    #[arg(long, conflicts_with = "no_restart")]
    pub dry_run: bool,
}
