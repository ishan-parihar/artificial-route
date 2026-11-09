//! Writes a Codex `model_catalog_json` from this router's own combos.
//!
//! # Why this exists
//!
//! Codex CLI will happily talk to any Responses endpoint, but it refuses to
//! *recognise* a model it has no metadata for. Every startup then prints:
//!
//! ```text
//! warning: Model metadata for `large-stack` not found. Defaulting to
//! fallback metadata; this can degrade performance and cause issues.
//! ```
//!
//! and the request is served at a fallback window (272K rather than the 1M the
//! combo actually has), so Codex compacts far earlier than it needs to. It also
//! decides which *hosted tools* to declare — the fallback entry carries
//! `supports_search_tool`, which is what makes Codex ask for a hosted
//! `web_search` tool this router cannot run.
//!
//! The one documented seam is the top-level `model_catalog_json` config key,
//! which points at a file in Codex's own catalog schema. Writing one entry per
//! combo turns the warning off, restores the real context window, and lets the
//! `/model` picker list the router's ids.
//!
//! # Why the entries are cloned, not invented
//!
//! Codex validates each catalog entry strictly, and a bundled entry carries a
//! version-specific system prompt (`base_instructions` / `model_messages`) that
//! runs to tens of kilobytes. An entry synthesised from scratch loses that
//! prompt, and the model loses the instructions that make it Codex-shaped.
//! So each combo entry is a **clone of the installed Codex's own bundled
//! catalog entry**, taken from `codex debug models --bundled`, with the fields
//! that stop it from applying retargeted. This is the same approach OmniRoute
//! takes, and it is the only one that survives a Codex upgrade: a new release
//! with new prompts is picked up by re-running this command.
//!
//! # What is retargeted, and why each one
//!
//! | Field | Set to | Reason |
//! |---|---|---|
//! | `slug`, `display_name` | the combo id | the router's id IS the model name |
//! | `description` | `via aroute` | the GPT descriptions are wrong for a lane |
//! | `context_window`, `max_context_window` | the combo's own figure | Codex must compact at the lane's real window |
//! | `visibility` | `list` | otherwise `/model` does not show it |
//! | `upgrade`, `availability_nux` | `null` | a GPT retirement notice on a router lane is nonsense |
//! | `supports_search_tool`, `web_search_tool_type` | `false` / `null` | see below |
//! | `tool_mode`, `use_responses_lite`, `multi_agent_*`, `experimental_supported_tools` | cleared | see below |
//! | `service_tiers`, `additional_speed_tiers` | `[]` | OpenAI services tiers do not exist on arbitrary upstreams |
//!
//! **The hosted search tool.** A bundled entry declares
//! `supports_search_tool: true`, which is what makes Codex add a hosted
//! `web_search` tool to its request. That tool runs *on the provider*, and this
//! router has no web access, so the request is refused outright — the lane is
//! unusable, over a tool nobody asked for. Setting it off makes Codex stop
//! declaring it. (The router independently refuses the tool if a client still
//! sends one; this is the half that stops it being sent.)
//!
//! **Code Mode and responses-lite.** `tool_mode: "code_mode_only"` and
//! `use_responses_lite: true` are behaviours only the real Codex upstream
//! understands. Cloning them onto a lane whose targets are ordinary
//! OpenAI-compatible providers produces tools the model calls and the provider
//! rejects as an unsupported call. The plain shell toolset is what a
//! non-Codex lane needs, so they are cleared.
//!
//! # Why the base config selects the provider
//!
//! A catalog entry alone is not enough. Codex sends the request to whichever
//! `model_provider` is active, and the default is OpenAI's own backend. That
//! backend has never heard of `large-stack`, and it answers any slug it does
//! not know with
//!
//! ```text
//! ERROR: The 'large-stack' model is not supported when using Codex with a
//! ChatGPT account.
//! ```
//!
//! which reads like a plan or account problem and is neither: the same error
//! comes back for a model name that does not exist anywhere. The catalog makes
//! Codex *recognise* the lane; only the provider selection makes it *reach*
//! the router. So this command writes `model_provider = "aroute"` into the
//! base config, and the profile keeps the same value for the `--profile`
//! path.
//!
//! # Why the daemon is restarted
//!
//! Codex 0.167 runs an `app-server daemon` that outlives the CLI. That daemon
//! reads `model_catalog_json` once, at startup, and never rereads it — the
//! config key is documented upstream as "applied on startup only". A daemon
//! that started before this command ran therefore keeps the old catalog, and
//! the `/model` picker lists the models that were current then: the bundled
//! GPT entries, with no router lane among them. The catalog on disk is
//! correct and the picker still disagrees with it.
//!
//! The daemon is the only thing that can load a new catalog, so this command
//! restarts it. A restart interrupts work in flight, so it is opt-out
//! (`--no-daemon-restart`) and it is reported either way.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::commands::{fail, load_edit};

/// The bundled catalog template to prefer: a plain shell-tool entry, not a
/// Code Mode one, so the clone starts from something a non-Codex lane can
/// already run.
const DEFAULT_TEMPLATE_SLUG: &str = "gpt-5.5";

/// The Codex subcommand that dumps the installed CLI's own bundled catalog.
const CODEX_BUNDLED_ARGS: [&str; 3] = ["debug", "models", "--bundled"];

/// Fields a non-Codex lane must not inherit from its template, and the value
/// each takes instead.
///
/// These two are the ones the schema accepts as `false` and `[]`. The fields
/// that must *disappear* rather than become `null` are in
/// [`REMOVED_OVERRIDES`].
const CODE_MODE_OVERRIDES: [(&str, serde_json::Value); 2] = [
    ("use_responses_lite", serde_json::Value::Bool(false)),
    (
        "experimental_supported_tools",
        serde_json::Value::Array(Vec::new()),
    ),
];

/// The fields a non-Codex lane must not inherit, and which are *removed*
/// rather than set to `null`.
///
/// Removing is how Codex's own catalog spells "this lane has no such
/// feature", and it is the only spelling that parses: `tool_mode`,
/// `multi_agent_version`, `multi_agent_reasoning_effort` and
/// `web_search_tool_type` are typed as a string or a map, so a `null` makes
/// Codex reject the entire catalog file with
/// `invalid type: null, expected string or map`.
const REMOVED_OVERRIDES: [&str; 4] = [
    "tool_mode",
    "multi_agent_version",
    "multi_agent_reasoning_effort",
    "web_search_tool_type",
];

/// Options for `aroute codex`.
#[derive(Debug, clap::Args)]
pub struct CodexArgs {
    /// Which Codex home to write into. Defaults to `$CODEX_HOME`, else `~/.codex`.
    #[arg(long, value_name = "DIR")]
    pub codex_home: Option<PathBuf>,

    /// Overwrite the catalog file without asking.
    #[arg(long)]
    pub force: bool,

    /// Print what would be written, and write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Print the result as JSON, and nothing else.
    #[arg(long)]
    pub json: bool,

    /// The provider base URL to record in `[model_providers.aroute]`.
    ///
    /// Only used when the block is absent, or when this flag is given
    /// explicitly: an existing `base_url` is the operator's own choice and a
    /// default must not silently repoint Codex at a different router.
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,

    /// The combo to configure as the profile's model. Defaults to `large-stack`.
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,

    /// Do not restart the Codex app-server daemon after writing the catalog.
    ///
    /// The daemon reads the catalog once, at startup, so it keeps serving the
    /// old model list until it restarts. A restart interrupts work in flight,
    /// so this flag is the way to decline it: the picker will then keep
    /// listing the models that were current when the daemon started.
    #[arg(long)]
    pub no_daemon_restart: bool,

    /// Do not make aroute the default provider in the base config.
    ///
    /// Without this, the base config names `model_provider = "aroute"`, and a
    /// plain `codex` talks to the router. With it, the router is reachable
    /// only through `codex --profile aroute`.
    #[arg(long)]
    pub no_default_provider: bool,

    /// Read the router's API key through a command instead of `$AROUTE_API_KEY`.
    ///
    /// Codex resolves `env_key` from the process environment, so the key has
    /// to be in the environment of every shell that starts Codex. A key read
    /// from a file the shell sources is in *that* shell's environment and no
    /// other, which is why `Missing environment variable: AROUTE_API_KEY`
    /// appears in a session started any other way. This flag writes Codex's
    /// command-backed `auth` instead, and Codex runs the command itself, so
    /// the key reaches every session whatever started it.
    #[arg(long)]
    pub auth_command: bool,
}

/// The subcommand a Codex provider block names in its `auth.command`.
///
/// Codex's command-backed auth has one contract: the command's stdout,
/// trimmed, is the token. This is that command. It prints the key and nothing
/// else, so it never becomes a diagnostic the router would reject as a
/// credential.
#[derive(Debug, Clone, clap::Args)]
pub struct TokenArgs {
    /// Which environment file to read the key from.
    ///
    /// The operator's shell sources `~/.config/jcode/provider-aroute.env`,
    /// so that is where the key lives. The flag names an alternative for a
    /// key stored somewhere else.
    #[arg(long, value_name = "PATH")]
    pub env_file: Option<std::path::PathBuf>,
}

/// The provider block's own table name in `config.toml`.
const PROVIDER_TABLE: &str = "model_providers.aroute";

/// The provider id, as it appears in `model_provider` and in the provider
/// block's own name.
const PROVIDER_ID: &str = "aroute";

/// The value `web_search` takes to keep the hosted search tool out of the
/// request.
///
/// The router has no web access and refuses the hosted tool outright, so the
/// switch has to be client-side: the router's refusal arrives after Codex has
/// already declared the tool, and it takes the whole request down with it.
const WEB_SEARCH_DISABLED: &str = "disabled";

/// The reasoning efforts the router runs.
///
/// The router's own set, in its own words: `crates/ar-translate` reads
/// `minimal`, `low`, `medium` and `high`, and refuses anything else with
/// `reasoning.effort names an effort this build does not run`. The catalog
/// must offer no more than this, because an effort the picker offers and the
/// router refuses is a request that fails for the whole turn.
const SUPPORTED_REASONING_EFFORTS: [&str; 4] = ["minimal", "low", "medium", "high"];

/// The reasoning effort a lane uses when nothing else is chosen.
///
/// `medium`: the router's middle setting, and the one the operator's own
/// profile corpus uses most.
const DEFAULT_REASONING_EFFORT: &str = "medium";

/// The default provider base URL: the local router.
///
/// A default, not a policy: an existing `base_url` in the operator's
/// `config.toml` wins, because that is where they pointed Codex on purpose.
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:20128/v1";

/// The combo a profile uses when the operator names none.
const DEFAULT_MODEL: &str = "large-stack";

/// The share of a lane's window Codex compacts at, in percent.
///
/// The rest is the room the answer itself needs. A limit at the ceiling
/// leaves nowhere to put the reply, and the request fails rather than
/// compacting — which is the failure the limit exists to avoid.
///
/// 85% is the figure the operator's own profile corpus uses: 6467 of the 6469
/// sibling profiles in a Codex home compact at exactly this share, across
/// every window size they declare.
const COMPACT_PERCENT: u64 = 85;

/// One combo, in the shape the catalog builder needs.
struct Lane {
    /// The combo id, which is also the model a client asks for.
    id: String,
    /// The combo's declared context window, when it declared one.
    context_length: Option<u32>,
}

/// Runs `aroute codex`.
pub fn codex(cli: &crate::cli::Cli, args: &CodexArgs) -> Result<()> {
    let cfg = load_edit(cli)?;
    let lanes = lanes(&cfg);

    let home = codex_home(args)?;
    let bundled = load_bundled(&home)?;
    let catalog = build(&bundled, &lanes)?;

    let listed = catalog
        .get("models")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|row| row.get("visibility").and_then(serde_json::Value::as_str) == Some("list"))
                .count()
        })
        .unwrap_or(0);

    let model = args
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
    // The lane the profile uses. A named lane that the config does not have
    // falls back to the lane with the largest declared window, so a config
    // that has renamed `large-stack` still yields a working profile instead
    // of an error. The choice is reported, because a fallback that is not
    // announced is a silent substitution.
    let (lane, fallback) = pick_lane(&lanes, &model)?;
    let window = lane.context_length.map(u64::from).unwrap_or(0);

    let catalog_path = catalog_path(&home);
    let profile_path = profile_path(&home);
    let config_path = home.join("config.toml");

    if args.dry_run {
        let body = serde_json::to_string_pretty(&catalog).unwrap_or_default();
        println!("{body}");
        println!("\n--- would write ---");
        println!("catalog: {}", catalog_path.display());
        println!("profile: {}", profile_path.display());
        println!("config:  {}", config_path.display());
        return Ok(());
    }

    let mut changed: Vec<String> = Vec::new();

    // The catalog. An existing file is only replaced when the operator says so,
    // because the catalog carries the system prompt of the Codex version that
    // wrote it and a stray overwrite throws that away.
    if catalog_path.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            catalog_path.display()
        );
    }
    write_atomic(&catalog_path, &catalog)?;
    changed.push(catalog_path.display().to_string());

    // The profile layer: `$CODEX_HOME/<name>.config.toml`, which
    // `codex --profile <name>` layers over the base config.
    let profile = profile_body(lane);
    write_text_atomic(&profile_path, &profile)?;
    changed.push(profile_path.display().to_string());

    // The base config: the catalog key and the provider block.
    let base_url = args.base_url.clone().unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    let config = read_config(&config_path)?;
    let (edited, mut notes) = edit_config(
        &config,
        &catalog_path,
        &EditOptions {
            base_url,
            explicit_base_url: args.base_url.is_some(),
            default_provider: !args.no_default_provider,
            auth_command: args.auth_command,
        },
    );
    if edited != config {
        backup_once(&config_path, &config)?;
        write_text_atomic(&config_path, &edited)?;
        changed.push(config_path.display().to_string());
    }

    // The daemon serves the `/model` picker, and it reads the catalog once,
    // at startup. A daemon that was already running when the catalog was
    // written keeps serving the old list, so the catalog and the picker
    // disagree until it restarts. See the module docs.
    let daemon = if args.no_daemon_restart {
        DaemonOutcome::Declined
    } else {
        restart_daemon(&home)
    };

    // The fallback is announced. A profile that names a lane the operator
    // did not ask for is a silent substitution unless it is said out loud.
    if fallback {
        notes.push(format!(
            "the config has no combo named {model:?}; the profile uses {} instead",
            lane.id
        ));
    }

    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "catalog": catalog_path.display().to_string(),
                "profile": profile_path.display().to_string(),
                "config": config_path.display().to_string(),
                "listed": listed,
                "total": catalog.get("models").and_then(serde_json::Value::as_array).map(|r| r.len()).unwrap_or(0),
                "model": lane.id,
                "requested_model": model,
                "fallback": fallback,
                "context_window": window,
                "changed": changed,
                "notes": notes,
                "daemon": daemon.as_str(),
            })
        );
    } else {
        for path in &changed {
            println!("wrote {path}");
        }
        for note in &notes {
            println!("note: {note}");
        }
        println!(
            "wrote {} ({listed} routable ids)",
            catalog_path.display()
        );
        println!("daemon: {}", daemon.as_str());
        println!("next: codex");
    }
    Ok(())
}

/// The lane a profile should use, and whether that was a fallback.
///
/// A named lane the config does not have falls back to the lane with the
/// largest declared window, and the second element of the pair says so. A
/// config that has renamed `large-stack` still yields a working profile, and
/// the operator is told which lane was chosen rather than discovering it by
/// looking at the file.
fn pick_lane<'a>(lanes: &'a [Lane], model: &str) -> Result<(&'a Lane, bool)> {
    if let Some(lane) = lanes.iter().find(|lane| lane.id == model) {
        return Ok((lane, false));
    }
    if let Some(lane) = lanes.iter().max_by_key(|lane| lane.context_length) {
        return Ok((lane, true));
    }
    Err(fail(
        format!("the config has no combo named {model:?}"),
        "run `aroute combo` to see the routable ids, or pass --model",
    ))
}

/// What happened to the Codex app-server daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonOutcome {
    /// The daemon was restarted, so it now serves the new catalog.
    Restarted,
    /// No daemon was running, so there was nothing to restart.
    NotRunning,
    /// The daemon was running but the restart failed.
    Failed,
    /// The operator declined the restart.
    Declined,
}

impl DaemonOutcome {
    /// The outcome as a word, for the human and JSON outputs.
    fn as_str(self) -> &'static str {
        match self {
            Self::Restarted => "restarted",
            Self::NotRunning => "not running",
            Self::Failed => "restart failed",
            Self::Declined => "declined",
        }
    }
}

/// Restarts the Codex app-server daemon, so it picks up the new catalog.
///
/// A daemon that is not running needs no restart: the next Codex launch reads
/// the catalog itself. A daemon that *is* running is the whole problem, so it
/// is restarted, and a failure is reported rather than swallowed: the catalog
/// on disk is then correct while the picker is not, and the operator has to
/// know that.
fn restart_daemon(home: &Path) -> DaemonOutcome {
    // A missing `codex` means there is no daemon to restart either: the daemon
    // is the Codex CLI's own process.
    let Ok(codex) = which_codex() else {
        return DaemonOutcome::NotRunning;
    };

    // `daemon version` is the only read-only way to ask whether a daemon is
    // running: it prints `{"status":"running",...}` when one is, and fails
    // when one is not.
    if !daemon_is_running(&codex, home) {
        return DaemonOutcome::NotRunning;
    }

    let restarted = std::process::Command::new(&codex)
        .args(["app-server", "daemon", "restart"])
        .env("CODEX_HOME", home)
        .output();
    match restarted {
        Ok(out) if out.status.success() => DaemonOutcome::Restarted,
        _ => DaemonOutcome::Failed,
    }
}

/// Whether a Codex app-server daemon is running for this home.
fn daemon_is_running(codex: &Path, home: &Path) -> bool {
    std::process::Command::new(codex)
        .args(["app-server", "daemon", "version"])
        .env("CODEX_HOME", home)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// The compact limit for a window.
///
/// 85% of the window, plain. The remaining 15% is the room the answer itself
/// needs: compacting at the ceiling leaves nowhere to put the reply, and the
/// request fails instead of compacting.
///
/// 85% is not a preference but a measurement. Across every sibling profile in
/// this Codex home — 6469 of them, each naming both a window and a limit —
/// 6467 compact at exactly 85% of their window, across every window size
/// including the power-of-two ones: 1048576 -> 891289, 262144 -> 222822,
/// 131072 -> 111411. Those figures are 85% of the raw binary window and of
/// nothing else, so the rule is one line and carries no exception.
fn compact_limit(window: u64) -> u64 {
    window * COMPACT_PERCENT / 100
}

/// The profile layer's body.
///
/// The window and the compact limit come from the lane, not from a constant,
/// so a lane with a smaller window compacts at its own scale instead of at a
/// figure borrowed from another lane.
///
/// The compact limit leaves room for the answer itself: compacting at the
/// ceiling leaves nowhere to put the reply, and the request fails instead of
/// compacting.
fn profile_body(lane: &Lane) -> String {
    let window = lane.context_length.unwrap_or(0);
    let compact = compact_limit(u64::from(window));
    format!(
        "# codex --profile aroute\n\
         # aroute (Artificial Route)\n\
         model                          = \"{id}\"\n\
         model_provider                 = \"aroute\"\n\
         model_context_window           = {window}\n\
         model_auto_compact_token_limit = {compact}\n\
         tool_output_token_limit        = 32768\n\
         \n\
         # The reasoning effort is pinned, not inherited. The base config's own\n\
         # value may name an effort the router refuses — `xhigh` is one — and\n\
         # the router answers that with a 400 for the whole request. The\n\
         # catalog offers only the efforts the router runs, so the profile\n\
         # names one of those.\n\
         model_reasoning_effort         = \"{effort}\"\n\
         \n\
         # aroute has no web access and refuses the hosted tool, so Codex must not\n\
         # ask for it. The switch is client-side: it keeps the `web_search` tool out\n\
         # of the request aroute sees.\n\
         web_search                     = \"disabled\"\n",
        id = lane.id,
        window = window,
        compact = compact,
        effort = DEFAULT_REASONING_EFFORT,
    )
}


/// The Codex home this command writes into.
fn codex_home(args: &CodexArgs) -> Result<PathBuf> {
    if let Some(home) = &args.codex_home {
        return Ok(home.clone());
    }
    if let Some(home) = std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
    {
        return Ok(home);
    }
    let home = dirs_home()?.join(".codex");
    if home.is_dir() {
        return Ok(home);
    }
    bail!("no Codex home found: set CODEX_HOME or pass --codex-home")
}

/// `$HOME`, resolved without a `dirs` dependency this crate does not carry.
fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set, so `~/.codex` cannot be resolved")
}

/// Where the catalog is written, inside a Codex home.
fn catalog_path(home: &Path) -> PathBuf {
    home.join("aroute-models.json")
}

/// Where the profile layer is written, inside a Codex home.
///
/// The name is the profile name: `codex --profile aroute` layers
/// `$CODEX_HOME/aroute.config.toml` over the base `config.toml`. This is the
/// mechanism Codex 0.166.0 actually has — not a `[profiles.*]` table.
fn profile_path(home: &Path) -> PathBuf {
    home.join("aroute.config.toml")
}

/// The combos that are NOT chat models, and so are not lanes a client can pick.
///
/// These route to decision and systemone endpoints: `/v1/decisions`,
/// `/v1/systemone`, `/api/alpha/decisions`. They do not speak
/// chat-completions at all — a request to one is refused with a complaint
/// about a missing `state` or `questions` field, not answered — so a catalog
/// entry for one offers the `/model` picker something that cannot work.
///
/// The signal is structural rather than a name list: a combo with no declared
/// context window, whose every target names a custom provider whose endpoint
/// path is one of the decision paths. A combo that serves a chat model always
/// declares a window, because the window is what a chat lane is for.
const DECISION_ENDPOINTS: [&str; 3] = [
    "/v1/decisions",
    "/v1/systemone",
    "/api/alpha/decisions",
];

/// The router's combos, as lanes Codex can select.
///
/// Only combos are listed. A registry model is directly addressable but is not
/// a routing lane, and a catalog entry that names one offers the client
/// something the `/model` picker cannot usefully switch to. The decision
/// endpoints are not chat models, so they are absent here by construction.
fn lanes(cfg: &ar_config::Config) -> Vec<Lane> {
    let decision_providers: std::collections::HashSet<&str> = cfg
        .custom_providers
        .iter()
        .filter(|provider| {
            provider.endpoint.as_deref().is_some_and(|endpoint| {
                DECISION_ENDPOINTS
                    .iter()
                    .any(|decision| endpoint.contains(decision))
            })
        })
        .map(|provider| provider.id.as_str())
        .collect();

    cfg.combos
        .iter()
        .filter(|combo| {
            // A lane with no context window is not a chat lane. The decision
            // endpoints declare none, and every real lane declares one.
            combo.context_length.is_some()
                // And every one of its targets must be a chat target: a combo
                // that routes to a decision endpoint cannot be answered by a
                // chat client whichever window it claimed.
                && !combo.targets.iter().any(|target| {
                    let provider = target.split('/').next().unwrap_or_default();
                    decision_providers.contains(provider)
                })
        })
        .map(|combo| Lane {
            id: combo.id.clone(),
            context_length: combo.context_length,
        })
        .collect()
}

/// Reads the installed Codex's own bundled catalog by running it.
///
/// Cloning from the installed CLI rather than shipping a template is the whole
/// point: the bundled entry carries that version's system prompt, and it is
/// regenerated every time this runs.
fn load_bundled(home: &Path) -> Result<serde_json::Value> {
    let codex = which_codex()?;
    let out = std::process::Command::new(&codex)
        .args(CODEX_BUNDLED_ARGS)
        .env("CODEX_HOME", home)
        .output()
        .with_context(|| format!("could not run `{}`", codex.display()))?;
    if !out.status.success() {
        return Err(fail(
            format!("`codex debug models --bundled` exited {}", out.status),
            "install Codex CLI, or pass --codex-home to the home that has it",
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| {
        fail(
            format!("Codex's bundled catalog is not JSON: {e}"),
            "upgrade Codex CLI; the `debug models --bundled` subcommand is new",
        )
    })
}

/// Locates the `codex` executable on PATH.
fn which_codex() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        for name in ["codex", "codex.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(fail(
        "`codex` is not on PATH",
        "the catalog is cloned from the installed Codex, so it has to be installed",
    ))
}

/// Builds the catalog: one listed entry per lane, plus the bundled entries kept
/// hidden so Codex's own slugs still resolve.
fn build(bundled: &serde_json::Value, lanes: &[Lane]) -> Result<serde_json::Value> {
    let entries = bundled
        .get("models")
        .and_then(serde_json::Value::as_array)
        .context("Codex's bundled catalog has no `models` array")?;
    let template = pick_template(entries)
        .context("Codex's bundled catalog has no usable template entry")?;

    let mut listed = Vec::with_capacity(lanes.len());
    let mut seen = std::collections::HashSet::new();
    for lane in lanes {
        if lane.id.trim().is_empty() || !seen.insert(lane.id.clone()) {
            continue;
        }
        listed.push(entry_from(&template, lane, listed.len() + 1)?);
    }

    // The bundled entries are kept, hidden, so a template slug Codex itself
    // names (a `gpt-*` in another config, say) still resolves. They are shifted
    // past the router's block so the picker leads with the lanes.
    let offset = i64::try_from(listed.len()).unwrap_or(i64::MAX) + 1;
    let hidden: Vec<serde_json::Value> = entries
        .iter()
        .filter(|row| {
            row.get("slug")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|slug| !seen.contains(slug))
        })
        .map(|row| {
            let mut row = row.clone();
            if let Some(object) = row.as_object_mut() {
                object.insert(
                    "visibility".to_owned(),
                    serde_json::Value::String("hide".to_owned()),
                );
                let priority = object
                    .get("priority")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0)
                    .max(0);
                object.insert("priority".to_owned(), serde_json::json!(offset + priority));

                // The hidden entries are kept so a template slug Codex itself
                // names still resolves, and they are held to the same tool
                // rules as the lanes: a hidden entry is still an entry Codex
                // can resolve by slug, and one that declares the hosted
                // search tool or the freeform apply_patch tool would draw the
                // same refusal a listed entry draws.
                object.insert(
                    "supports_search_tool".to_owned(),
                    serde_json::Value::Bool(false),
                );
                for key in REMOVED_OVERRIDES {
                    object.remove(key);
                }
                object.remove("apply_patch_tool_type");
                object.insert(
                    "use_responses_lite".to_owned(),
                    serde_json::Value::Bool(false),
                );

                // The hidden entries are held to the same reasoning rules as
                // the lanes. The base config makes aroute the active
                // provider, so a hidden entry is still an entry Codex can
                // resolve by slug and send to this router — and the router
                // refuses an effort above `high` with a 400 that fails the
                // whole turn. See [`entry_from`] for the reasoning cut.
                cut_reasoning_levels(object);
            }
            row
        })
        .collect();

    let mut models = listed;
    models.extend(hidden);
    Ok(serde_json::json!({ "models": models }))
}

/// The template to clone: the plain shell-tool entry, else the first listed
/// non-code-mode one, else whatever leads the picker.
fn pick_template(entries: &[serde_json::Value]) -> Option<serde_json::Value> {
    let is_entry = |row: &serde_json::Value| {
        row.get("slug")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|slug| !slug.is_empty())
    };
    let listed = |row: &serde_json::Value| {
        row.get("visibility").and_then(serde_json::Value::as_str) == Some("list")
    };
    let priority = |row: &serde_json::Value| {
        row.get("priority")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(i64::MAX)
    };
    let is_code_mode = |row: &serde_json::Value| {
        row.get("tool_mode").and_then(serde_json::Value::as_str) == Some("code_mode_only")
            || row.get("use_responses_lite").and_then(serde_json::Value::as_bool) == Some(true)
    };

    entries
        .iter()
        .filter(|row| is_entry(row))
        .find(|row| {
            row.get("slug")
                .and_then(serde_json::Value::as_str)
                == Some(DEFAULT_TEMPLATE_SLUG)
        })
        .or_else(|| {
            entries
                .iter()
                .filter(|row| is_entry(row) && listed(row) && !is_code_mode(row))
                .min_by_key(|row| priority(row))
        })
        .or_else(|| {
            entries
                .iter()
                .filter(|row| is_entry(row) && listed(row))
                .min_by_key(|row| priority(row))
        })
        .or_else(|| {
            entries
                .iter()
                .filter(|row| is_entry(row))
                .min_by_key(|row| priority(row))
        })
        .cloned()
}

/// Clones `template` into a catalog entry naming `lane`.
///
/// `priority` is the lane's own rank in the picker. Codex sorts the catalog by
/// this field, and the template's value is the rank of a *GPT* model, so
/// every clone would otherwise share it and the picker's order would be
/// decided by a tie-break rather than by anything the router said. Each lane
/// gets its own rank, in the order the router lists them.
fn entry_from(
    template: &serde_json::Value,
    lane: &Lane,
    priority: usize,
) -> Result<serde_json::Value> {
    let mut entry = template.clone();
    let Some(object) = entry.as_object_mut() else {
        bail!("the bundled template entry is not a JSON object");
    };

    object.insert("slug".to_owned(), serde_json::json!(lane.id));
    object.insert("display_name".to_owned(), serde_json::json!(lane.id));
    object.insert(
        "description".to_owned(),
        serde_json::json!(format!("{} via aroute", lane.id)),
    );
    object.insert(
        "visibility".to_owned(),
        serde_json::Value::String("list".to_owned()),
    );
    object.insert("priority".to_owned(), serde_json::json!(priority));
    // A bundled entry's `upgrade` names a *newer OpenAI model* to move to; on a
    // router lane that is an invitation to leave for a model the router does
    // not serve.
    object.insert("upgrade".to_owned(), serde_json::Value::Null);
    object.insert("availability_nux".to_owned(), serde_json::Value::Null);

    // The hosted search tool: see the module docs. It is what makes Codex add a
    // `web_search` declaration the router cannot run.
    object.insert("supports_search_tool".to_owned(), serde_json::Value::Bool(false));

    for (key, value) in CODE_MODE_OVERRIDES {
        object.insert(key.to_owned(), value);
    }

    // The fields that must disappear rather than become `null`. See
    // [`REMOVED_OVERRIDES`]: Codex's schema types them as a string or a map,
    // so a `null` makes it reject the whole catalog file.
    for key in REMOVED_OVERRIDES {
        object.remove(key);
    }

    // `freeform` is what makes Codex declare `apply_patch` as a `custom` tool:
    // a grammar-constrained tool the model writes patch syntax into, rather
    // than a JSON-argument function. The router cannot relay one — its tool
    // translation refuses a `custom` declaration with `unsupported tool type` —
    // and the refusal is a 400 for the whole request, not a dropped tool. The
    // entry that omits this field keeps the JSON shell tool instead, which is
    // the one the router can run.
    object.remove("apply_patch_tool_type");

    // OpenAI's service tiers are an OpenAI concept; a router lane has none.
    object.insert(
        "service_tiers".to_owned(),
        serde_json::Value::Array(Vec::new()),
    );
    object.insert(
        "additional_speed_tiers".to_owned(),
        serde_json::Value::Array(Vec::new()),
    );

    // The reasoning levels the entry offers. The template is a GPT model's,
    // and it offers levels this router does not run: `xhigh`, `max` and
    // `ultra` are OpenAI's, and the router refuses any effort above `high`
    // with a 400 that reads `reasoning.effort names an effort this build
    // does not run`. A level the router will refuse is not a level the
    // catalog may offer, so the list is cut to the router's own.
    //
    // The cut is here rather than in the router because the router's refusal
    // is right: a request that asked for more thinking than it gets should
    // say so. The catalog is the wrong place to promise it.
    cut_reasoning_levels(object);

    // The reasoning *summary* default, the other half of the thinking story.
    // Codex only sends `reasoning.summary` when the catalog's
    // `default_reasoning_summary` is not `none` — and the bundled GPT
    // template ships `none`, which means a lane cloned from it asks for no
    // summary at all. But the router now renders the upstream's
    // `reasoning_content` as summary events regardless, and Codex's TUI
    // shows thinking from those events. Setting the default to `auto` makes
    // Codex ask for summaries on the request too, which is the honest shape:
    // the request says what it wants and the reply path can now deliver it.
    object.insert(
        "default_reasoning_summary".to_owned(),
        serde_json::json!("auto"),
    );

    // The template's window belongs to a GPT model. The lane's own figure is
    // the one Codex must compact at, so it wins whenever the combo declared one.
    if let Some(window) = lane.context_length {
        object.insert("context_window".to_owned(), serde_json::json!(window));
        object.insert(
            "max_context_window".to_owned(),
            serde_json::json!(window),
        );
        // A percentage of a window Codex applies on top; keeping the template's
        // is right, but it must not outlive a window we just replaced.
        if let Some(percent) = object
            .get("effective_context_window_percent")
            .and_then(serde_json::Value::as_i64)
            && percent > 100
        {
            object.insert(
                "effective_context_window_percent".to_owned(),
                serde_json::json!(100),
            );
        }
    }

    Ok(entry)
}

/// Cuts a catalog entry's reasoning levels down to the router's own.
///
/// A Codex model entry offers `xhigh`, `max` and `ultra`; this router runs
/// `minimal`, `low`, `medium` and `high` and refuses anything else with a
/// 400 for the whole request. An entry that offers a level the router
/// refuses is an entry whose picker row produces a request that cannot
/// work, so the levels are cut and the default is moved onto one the router
/// runs.
///
/// Every entry is cut, the hidden ones included: the base config makes
/// aroute the active provider, so a hidden entry is still an entry Codex can
/// resolve by slug and send here.
fn cut_reasoning_levels(object: &mut serde_json::Map<String, serde_json::Value>) {
    let levels: Vec<serde_json::Value> = object
        .get("supported_reasoning_levels")
        .and_then(serde_json::Value::as_array)
        .map(|levels| {
            levels
                .iter()
                .filter(|level| {
                    level
                        .get("effort")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|effort| SUPPORTED_REASONING_EFFORTS.contains(&effort))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    object.insert(
        "supported_reasoning_levels".to_owned(),
        serde_json::Value::Array(levels),
    );

    // The default may be a level the list no longer carries. `medium` is the
    // router's middle setting and the one the operator's own profile corpus
    // uses most, so it is the default when the entry's own is not offered.
    let default_is_offered = object
        .get("default_reasoning_level")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|effort| SUPPORTED_REASONING_EFFORTS.contains(&effort));
    if !default_is_offered {
        object.insert(
            "default_reasoning_level".to_owned(),
            serde_json::json!(DEFAULT_REASONING_EFFORT),
        );
    }
}

/// Writes `value` to `path` atomically: a temp file beside it, then a rename.
///
/// A catalog that is half-written is read by the next Codex launch as a corrupt
/// file, which is the failure this avoids. The rename is atomic within one
/// filesystem, and the temp file is written beside the target so it is on that
/// filesystem.
fn write_atomic(path: &Path, value: &serde_json::Value) -> Result<()> {
    let parent = path
        .parent()
        .context("the catalog path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("aroute-models.json")
    ));
    let body = serde_json::to_vec_pretty(value).context("the catalog is not serialisable")?;
    std::fs::write(&temp, &body).with_context(|| format!("could not write {}", temp.display()))?;
    std::fs::rename(&temp, path)
        .with_context(|| format!("could not move {} into place", temp.display()))?;
    Ok(())
}

/// Reads a file, treating a missing file as empty.
///
/// A missing `config.toml` is not an error: Codex creates one on first run,
/// and `aroute codex` must work before Codex has ever been launched.
fn read_config(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(body),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => {
            Err(error).with_context(|| format!("could not read {}", path.display()))
        }
    }
}

/// Backs `config.toml` up once, to `config.toml.aroute.bak`.
///
/// Once, not every time: the backup is the state *before aroute first touched
/// the file*, which is what an operator needs to undo the change by hand. A
/// second run's backup would be the already-edited file, and restoring it
/// would restore the edit rather than undo it.
fn backup_once(path: &Path, body: &str) -> Result<()> {
    let backup = path.with_extension("toml.aroute.bak");
    if backup.exists() {
        return Ok(());
    }
    write_text_atomic(&backup, body)?;
    Ok(())
}

/// Writes text to `path` atomically: a temp file beside it, then a rename.
///
/// The temp file is written beside the target so the rename is within one
/// filesystem, and so it is atomic: Codex never reads a half-written config.
fn write_text_atomic(path: &Path, body: &str) -> Result<()> {
    let parent = path
        .parent()
        .context("the config path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml")
    ));
    std::fs::write(&temp, body).with_context(|| format!("could not write {}", temp.display()))?;
    std::fs::rename(&temp, path)
        .with_context(|| format!("could not move {} into place", temp.display()))?;
    Ok(())
}

/// The environment variable the router's key is stored under.
const API_KEY_ENV: &str = "AROUTE_API_KEY";

/// The environment file the operator's shell sources for the router's key.
///
/// The same file the jcode provider profile reads through `env_file`, so the
/// shell, the profile and this command are on one value.
const DEFAULT_ENV_FILE: &str = ".config/jcode/provider-aroute.env";

/// Prints the router's API key on stdout, for Codex's command-backed auth.
///
/// Codex's contract is narrow and this command keeps to it: the key on
/// stdout, nothing else. A diagnostic on stdout would become part of the
/// token, and the router would answer the whole request with a 401 that
/// names a key nobody can read.
///
/// The key comes from the environment when it is there, and from the
/// operator's env file when it is not. The environment is preferred because
/// a shell that has already sourced the file has the freshest value; the
/// file is the fallback that makes the key reachable from a session started
/// any other way.
pub fn token(args: &TokenArgs) -> Result<()> {
    if let Ok(value) = std::env::var(API_KEY_ENV) {
        let value = value.trim();
        if !value.is_empty() {
            println!("{value}");
            return Ok(());
        }
    }

    let path = match &args.env_file {
        Some(path) => path.clone(),
        None => dirs_home()?.join(DEFAULT_ENV_FILE),
    };
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;

    let value = body
        .lines()
        .filter_map(|line| line.trim().strip_prefix(API_KEY_ENV))
        .find_map(|rest| {
            let rest = rest.trim();
            let rest = rest.strip_prefix('=')?.trim();
            let value = rest.trim_matches(|c| c == '"' || c == '\'');
            (!value.is_empty()).then(|| value.to_owned())
        })
        .with_context(|| {
            format!(
                "{} holds no {API_KEY_ENV}; the key is not where Codex should look",
                path.display()
            )
        })?;

    println!("{value}");
    Ok(())
}

/// What the base-config edit should do.
///
/// The options are named rather than positional: there are several, they are
/// all booleans, and a sixth positional `true`/`false` would say nothing at
/// the call site.
#[derive(Debug, Default)]
struct EditOptions {
    /// The router's base URL, used when the provider block has none.
    base_url: String,
    /// Whether an existing `base_url` may be replaced. Only an explicit
    /// `--base-url` earns that: the operator's own is a choice.
    explicit_base_url: bool,
    /// Whether aroute becomes the active provider.
    default_provider: bool,
    /// Whether the provider block gets the command-backed auth.
    auth_command: bool,
}

/// Edits `config.toml`: adds the catalog key at the top level, selects the
/// aroute provider, and makes sure the provider block exists, without
/// disturbing what is already there.
///
/// The editor is line-oriented rather than a TOML parser, because the repo
/// does not depend on a TOML crate and this command must not add one. It only
/// ever needs to recognise three things: a top-level key, a table header, and
/// the keys of one table. Everything else it passes through byte for byte.
fn edit_config(
    body: &str,
    catalog: &Path,
    options: &EditOptions,
) -> (String, Vec<String>) {
    let mut notes = Vec::new();
    let mut out = body.to_owned();
    let catalog_value = catalog.display().to_string();
    let base_url = options.base_url.as_str();
    let explicit_base_url = options.explicit_base_url;
    let default_provider = options.default_provider;
    let auth_command = options.auth_command;

    // The catalog key. It is top-level, not under a table: the catalog is
    // global, and a `model_catalog_json` inside `[model_providers.aroute]`
    // would be read as a provider key and ignored.
    //
    // A key that already points somewhere else is *preserved*, not
    // repointed. The path is the operator's own choice, and overwriting it
    // would silently point Codex at a catalog it did not ask for. The
    // consequence is stated plainly, because it is not a small one: Codex
    // will keep reading the *other* catalog, so the lanes this command just
    // wrote may not appear in its picker at all.
    if let Some(existing) = top_level_value(&out, "model_catalog_json") {
        if existing == catalog_value {
            notes.push("model_catalog_json already points at the catalog".to_owned());
        } else {
            notes.push(format!(
                "model_catalog_json still points at {existing}, NOT at {catalog_value}: \
                 Codex will read the other catalog, so the router's lanes may not appear in \
                 its picker. Point it at {catalog_value} to use them."
            ));
        }
    } else {
        out = insert_top_level(&out, "model_catalog_json", &catalog_value);
        notes.push(format!("added model_catalog_json = {catalog_value}"));
    }

    // The active provider. This is the difference between a lane Codex
    // *recognises* and a lane Codex can *reach*: the default provider is
    // OpenAI's own backend, and it answers every router model name with
    // "not supported when using Codex with a ChatGPT account".
    if default_provider {
        let existing = top_level_value(&out, "model_provider");
        match existing.as_deref() {
            Some(PROVIDER_ID) => {
                notes.push("model_provider already selects aroute".to_owned());
            }
            Some(other) => {
                out = replace_top_level(&out, "model_provider", PROVIDER_ID);
                notes.push(format!(
                    "model_provider now selects aroute (was {other})"
                ));
            }
            None => {
                out = insert_top_level(&out, "model_provider", PROVIDER_ID);
                notes.push(format!("added model_provider = {PROVIDER_ID}"));
            }
        }

        // The hosted search tool, which this router cannot run. It is written
        // here and not only in the profile because the base-config path is the
        // one that works without `--profile`, and the catalog's
        // `supports_search_tool: false` is NOT enough on its own: measured
        // against the live router, a request with that key absent is refused
        // outright with `unsupported tool type "web_search": this build does
        // not run it`, and the same request with it present is answered.
        //
        // An operator's own value is *replaced*, not left alone. The router
        // refuses the hosted tool either way, so a request with hosted search
        // enabled does not fail softly: it fails for the whole request, and
        // it takes the MCP tools in the same request down with it. A value
        // that cannot work here is not a preference to preserve.
        let existing = top_level_value(&out, "web_search");
        match existing.as_deref() {
            Some(WEB_SEARCH_DISABLED) => {
                notes.push("web_search is already disabled".to_owned());
            }
            Some(other) => {
                out = replace_top_level(&out, "web_search", WEB_SEARCH_DISABLED);
                notes.push(format!(
                    "web_search is now disabled (was {other}; the router cannot run the hosted tool)"
                ));
            }
            None => {
                out = insert_top_level(&out, "web_search", WEB_SEARCH_DISABLED);
                notes.push(format!("added web_search = {WEB_SEARCH_DISABLED}"));
            }
        }

        // The reasoning effort, for the same reason and the same rule. The
        // router runs `minimal`, `low`, `medium` and `high`, and refuses
        // anything else with a 400 for the whole request. The operator's own
        // base config may name `xhigh` — a level OpenAI's models offer and
        // this router does not — and every plain `codex` turn would fail on
        // it. An effort the router runs is left alone: that is a preference
        // that works.
        let existing = top_level_value(&out, "model_reasoning_effort");
        match existing.as_deref() {
            Some(effort) if SUPPORTED_REASONING_EFFORTS.contains(&effort) => {
                notes.push(format!("model_reasoning_effort is {effort}, which the router runs"));
            }
            Some(other) => {
                out = replace_top_level(
                    &out,
                    "model_reasoning_effort",
                    DEFAULT_REASONING_EFFORT,
                );
                notes.push(format!(
                    "model_reasoning_effort is now {DEFAULT_REASONING_EFFORT} (was {other}; \
                     the router refuses any other effort)"
                ));
            }
            None => {
                out = insert_top_level(
                    &out,
                    "model_reasoning_effort",
                    DEFAULT_REASONING_EFFORT,
                );
                notes.push(format!(
                    "added model_reasoning_effort = {DEFAULT_REASONING_EFFORT}"
                ));
            }
        }
    }

    // The provider block.
    edit_provider(
        &mut out,
        base_url,
        explicit_base_url,
        auth_command,
        &mut notes,
    );

    // The file's *shape* is the operator's too: a config that did not end
    // with a newline does not gain one. The keys above are written with the
    // body's own line ending, so this only has to put the last terminator
    // back the way it was — the content of the final line is ours, but the
    // choice to end the file without a newline was theirs.
    if !body.ends_with('\n') {
        while out.ends_with('\n') {
            out.pop();
            if out.ends_with('\r') {
                out.pop();
            }
        }
    }

    (out, notes)
}

/// The provider block's own keys, in the order Codex documents them.
///
/// The secret is never written: `env_key` names the environment variable, so
/// the key itself stays in the operator's shell and never lands in a file.
///
/// The command-backed auth replaces `env_key` entirely — Codex refuses a
/// provider that carries both — so the key is left out when the auth command
/// owns the credential.
fn provider_keys(base_url: &str, auth_command: bool) -> Vec<(&'static str, String)> {
    let mut keys = vec![
        ("name", "aroute".to_owned()),
        ("base_url", base_url.to_owned()),
    ];
    if !auth_command {
        keys.push(("env_key", "AROUTE_API_KEY".to_owned()));
    }
    keys.push(("wire_api", "responses".to_owned()));
    keys
}

/// A TOML basic string, quoted and escaped.
///
/// Every value the provider block carries is a string, and TOML strings must
/// be quoted: an unquoted `https://x/v1` is not a string but a syntax error,
/// and a config Codex cannot parse is a config Codex ignores.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The value of a TOML basic string, without its quotes.
///
/// A value that is not a quoted string is returned as it stands: the editor
/// only ever writes quoted strings, and a value it did not write is one it
/// must not try to interpret.
fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(character) = chars.next() {
            if character != '\\' {
                out.push(character);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        }
        return out;
    }
    trimmed.to_owned()
}

/// Makes sure `[model_providers.aroute]` exists and is complete, in place.
///
/// A block that exists is left alone except for the keys it is missing: the
/// operator's own `base_url` is a choice, and a command that rewrites it
/// repoints Codex at a different router without asking.
///
/// The missing keys go *inside* the block, at its end, and not at the end of
/// the file: a key appended after the next table header would belong to that
/// other table, and Codex would read the provider as missing its `env_key`.
fn edit_provider(
    body: &mut String,
    base_url: &str,
    explicit_base_url: bool,
    auth_command: bool,
    notes: &mut Vec<String>,
) {
    // The credential form is decided by the config, not only by the flag. A
    // config that already carries the command-backed auth keeps it: adding
    // `env_key` beside it produces a config Codex refuses outright
    // (`provider auth cannot be combined with env_key`), and a refused
    // provider makes the whole config fail to load. So a run without
    // `--auth-command` must not undo a run with it.
    let auth_command = auth_command || !table_keys(body, AUTH_TABLE).is_empty();

    let existing = table_keys(body, PROVIDER_TABLE);
    if existing.is_empty() {
        notes.push("added [model_providers.aroute]".to_owned());
        // The body's own ending, and a new one only when the file needs it:
        // appending to a file with no trailing newline would otherwise join
        // two lines together or, with a CRLF file, rewrite it as LF.
        let ending = line_ending(body);
        if !body.is_empty() && !body.ends_with('\n') {
            body.push_str(ending);
        }
        body.push_str(ending);
        body.push_str(&format!("[model_providers.aroute]{ending}"));
        for (key, value) in provider_keys(base_url, auth_command) {
            body.push_str(&format!("{key} = {}{ending}", toml_string(&value)));
        }
        if auth_command {
            edit_auth_command(body, notes);
        }
        return;
    }

    // The block is there. Add the keys it lacks, and leave the rest.
    let missing: Vec<(&str, String)> = provider_keys(base_url, auth_command)
        .into_iter()
        .filter(|(key, _)| !existing.iter().any(|(name, _)| name == key))
        .collect();
    if missing.is_empty() {
        notes.push("[model_providers.aroute] already complete".to_owned());
    } else {
        for (key, value) in missing {
            insert_into_table(body, PROVIDER_TABLE, key, &value);
            notes.push(format!("added {key} to [model_providers.aroute]"));
        }
    }

    // The base URL is the one key an explicit flag may replace: the operator
    // named it on the command line, so it is a deliberate choice.
    if explicit_base_url {
        let current = table_keys(body, PROVIDER_TABLE)
            .into_iter()
            .find(|(name, _)| name == "base_url")
            .map(|(_, value)| value);
        if current.as_deref() != Some(base_url) {
            replace_in_table(body, PROVIDER_TABLE, "base_url", base_url);
            notes.push(format!("base_url in [model_providers.aroute] is now {base_url}"));
        }
    }

    // The command-backed auth, which replaces the environment-variable form
    // rather than sitting beside it.
    //
    // Codex runs the command itself, so the key reaches every session whatever
    // started it — which the `env_key` form cannot promise, because Codex
    // reads `env_key` from the process environment and a key the shell sources
    // is in one shell's environment only.
    //
    // The two forms are mutually exclusive. Codex refuses the combination
    // outright — `provider auth cannot be combined with env_key` — so a config
    // that keeps both fails to load *at all*, and every turn fails with it.
    if auth_command {
        if table_keys(body, PROVIDER_TABLE)
            .iter()
            .any(|(name, _)| name == "env_key")
        {
            remove_from_table(body, PROVIDER_TABLE, "env_key");
            notes.push(
                "removed env_key from [model_providers.aroute] (auth replaces it)".to_owned(),
            );
        }
        edit_auth_command(body, notes);
    }
}

/// The nested table Codex reads the provider's auth command from.
///
/// Codex's schema puts `auth` inside the provider block, so the table is
/// `[model_providers.aroute.auth]`.
const AUTH_TABLE: &str = "model_providers.aroute.auth";

/// The program Codex runs to fetch the router's API key.
///
/// Codex's `auth.command` is a *program path*, and its arguments go in
/// `auth.args` beside it. A command string with a space in it names a
/// program with a space in its name, which does not exist: Codex answers
/// `provider auth command \`aroute token\` failed to start: No such file or
/// directory`. So the program is `aroute` and the subcommand is an argument.
const AUTH_COMMAND: &str = "aroute";

/// The argument that makes the program print the key.
///
/// `aroute token` reads the key from the environment, or from the file the
/// operator's shell sources, and prints it on stdout. That is exactly the
/// contract Codex's command-backed auth has: the command's stdout, trimmed,
/// is the token.
const AUTH_ARGS: [&str; 1] = ["token"];

/// Writes the provider's command-backed auth, in place.
///
/// The table is written whole rather than filled in key by key: it is a
/// mechanism this command owns end to end, and a half-written `auth` block
/// is a provider Codex cannot authenticate against.
///
/// `command` and `args` are *replaced* rather than left alone. They are the
/// one part of the block this command owns, and a value left behind from an
/// earlier shape is worse than a missing one: a `command = "aroute token"`
/// names a program with a space in its name, and Codex answers
/// `provider auth command \`aroute token\` failed to start: No such file or
/// directory`.
fn edit_auth_command(body: &mut String, notes: &mut Vec<String>) {
    let existing = table_keys(body, AUTH_TABLE);
    let wanted = [
        ("command", AUTH_COMMAND.to_owned()),
        ("timeout_ms", "10000".to_owned()),
    ];

    // The arguments are a TOML array of strings, and are written as one line
    // so the table stays the shape Codex's own docs show.
    let args_value = format!(
        "[{}]",
        AUTH_ARGS
            .iter()
            .map(|argument| toml_string(argument))
            .collect::<Vec<String>>()
            .join(", ")
    );

    let missing: Vec<(&str, String)> = wanted
        .iter()
        .filter(|(key, _)| !existing.iter().any(|(name, _)| name == key))
        .map(|(key, value)| (*key, value.clone()))
        .collect();
    let has_args = existing.iter().any(|(name, _)| name == "args");
    let command_is_wrong = existing
        .iter()
        .any(|(name, value)| name == "command" && value.as_str() != AUTH_COMMAND);
    let args_are_wrong = existing
        .iter()
        .any(|(name, value)| name == "args" && value.as_str() != args_value);

    if missing.is_empty() && has_args && !command_is_wrong && !args_are_wrong {
        notes.push("[model_providers.aroute.auth] already complete".to_owned());
        return;
    }

    if existing.is_empty() {
        notes.push("added [model_providers.aroute.auth]".to_owned());
        let ending = line_ending(body);
        if !body.is_empty() && !body.ends_with('\n') {
            body.push_str(ending);
        }
        body.push_str(ending);
        body.push_str(&format!("[{AUTH_TABLE}]{ending}"));
        body.push_str(&format!(
            "command = {}{ending}",
            toml_string(AUTH_COMMAND)
        ));
        body.push_str(&format!("args = {args_value}{ending}"));
        body.push_str(&format!("timeout_ms = 10000{ending}"));
        return;
    }

    // A value that is already there but wrong is replaced. Leaving it would
    // keep a config Codex cannot authenticate against, and the report would
    // claim the block is complete.
    if command_is_wrong {
        replace_raw_in_table(body, AUTH_TABLE, "command", &toml_string(AUTH_COMMAND));
        notes.push(format!(
            "command in [{AUTH_TABLE}] is now {AUTH_COMMAND} (it named a program Codex cannot run)"
        ));
    }
    if args_are_wrong {
        replace_raw_in_table(body, AUTH_TABLE, "args", &args_value);
        notes.push(format!("args in [{AUTH_TABLE}] is now {args_value}"));
    }

    for (key, value) in missing {
        let value = if key.contains("command") {
            toml_string(&value)
        } else {
            value
        };
        insert_into_table(body, AUTH_TABLE, key, &value);
        notes.push(format!("added {key} to [{AUTH_TABLE}]"));
    }
    if !has_args {
        insert_raw_into_table(body, AUTH_TABLE, "args", &args_value);
        notes.push(format!("added args to [{AUTH_TABLE}]"));
    }
}

/// A body's lines, each keeping its own terminator.
///
/// `str::lines` drops the terminator, so an editor built on it has to guess
/// one and writes the whole file with the guess. A hand-edited config can be
/// CRLF, and a guess turns a one-key edit into a whole-file rewrite — the
/// operator's own `config.toml` is theirs to keep, byte for byte, apart from
/// the keys this command owns.
fn lines_with_endings(body: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (index, character) in body.char_indices() {
        if character == '\n' {
            out.push(&body[start..=index]);
            start = index + 1;
        }
    }
    if start < body.len() {
        out.push(&body[start..]);
    }
    out
}

/// A line's content, without its terminator.
fn line_content(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|rest| rest.strip_suffix('\r').unwrap_or(rest))
        .unwrap_or(line)
}

/// A line's terminator: CRLF, LF, or empty for a last line that has none.
fn line_terminator(line: &str) -> &str {
    match line.strip_suffix('\n') {
        Some(rest) if rest.ends_with('\r') => "\r\n",
        Some(_) => "\n",
        None => "",
    }
}

/// The ending a body uses: CRLF if any line has one, LF otherwise.
///
/// A body with mixed endings is not this editor's to normalise, and CRLF is
/// the one worth preserving, because it is what a Windows editor wrote.
fn line_ending(body: &str) -> &'static str {
    if body.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Inserts `key = value` at the end of `table`'s block.
///
/// The end of the block is the line before the next table header, or the end
/// of the file when the table is the last one.
fn insert_into_table(body: &mut String, table: &str, key: &str, value: &str) {
    insert_raw_into_table(body, table, key, &toml_string(value));
}

/// Inserts a key whose value is already written the way TOML wants it.
///
/// A value that is not a string — an array, a number — is written verbatim,
/// because quoting it would change its type and a `args = "[\"token\"]"` is a
/// string where Codex expects an array.
fn insert_raw_into_table(body: &mut String, table: &str, key: &str, value: &str) {
    let header = format!("[{table}]");
    let ending = line_ending(body);
    let new_line = format!("{key} = {value}{ending}");
    let mut out = String::with_capacity(body.len() + new_line.len());
    let mut inside = false;
    let mut inserted = false;
    // Blank lines inside the block are held back rather than written
    // immediately: the key's own place is not known until a later line says
    // whether more keys follow. Holding them back is what puts the key after
    // the block's last *key* line instead of after a trailing blank line
    // that a stricter reader would treat as the end of the block.
    let mut held_blank: Vec<&str> = Vec::new();
    let flush = |held_blank: &mut Vec<&str>, out: &mut String| {
        for held in held_blank.drain(..) {
            out.push_str(held);
        }
    };
    for line in lines_with_endings(body) {
        let trimmed = line_content(line).trim();
        if trimmed.starts_with('[') {
            // The next table starts: the key goes before it, and the blanks
            // that came before the header go with them.
            if inside && !inserted {
                flush(&mut held_blank, &mut out);
                out.push_str(&new_line);
                inserted = true;
            }
            inside = trimmed == header;
            held_blank.clear();
            out.push_str(line);
            continue;
        }
        if inside {
            if trimmed.is_empty() {
                held_blank.push(line);
                continue;
            }
            if !inserted {
                out.push_str(&new_line);
                inserted = true;
            }
            flush(&mut held_blank, &mut out);
        } else {
            held_blank.clear();
        }
        out.push_str(line);
    }
    if inside && !inserted {
        flush(&mut held_blank, &mut out);
        // A last line with no terminator would otherwise swallow the key.
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(ending);
        }
        out.push_str(&new_line);
    }
    *body = out;
}

/// Replaces the value of `key` inside `table`.
fn replace_in_table(body: &mut String, table: &str, key: &str, value: &str) {
    replace_raw_in_table(body, table, key, &toml_string(value));
}

/// Replaces the value of `key` inside `table`, writing it verbatim.
///
/// A value that is not a string — an array, a number — is written as it is,
/// because quoting it would change its type.
fn replace_raw_in_table(body: &mut String, table: &str, key: &str, value: &str) {
    let header = format!("[{table}]");
    let mut out = String::with_capacity(body.len());
    let mut inside = false;
    for line in lines_with_endings(body) {
        let trimmed = line_content(line).trim();
        if trimmed.starts_with('[') {
            inside = trimmed == header;
        }
        if inside
            && let Some((name, _)) = split_key(trimmed)
            && name == key
        {
            // The line keeps its own terminator, so a CRLF file stays CRLF.
            out.push_str(&format!("{key} = {value}{}", line_terminator(line)));
            continue;
        }
        out.push_str(line);
    }
    *body = out;
}

/// Removes a key from a table, leaving the rest of the file alone.
///
/// The line goes entirely, terminator and all, so the table keeps its shape
/// and the file keeps its ending. A key that is not there is not an error:
/// the caller's question is whether the key is present afterwards, and it is
/// not.
fn remove_from_table(body: &mut String, table: &str, key: &str) {
    let header = format!("[{table}]");
    let mut out = String::with_capacity(body.len());
    let mut inside = false;
    for line in lines_with_endings(body) {
        let trimmed = line_content(line).trim();
        if trimmed.starts_with('[') {
            inside = trimmed == header;
        }
        if inside
            && let Some((name, _)) = split_key(trimmed)
            && name == key
        {
            continue;
        }
        out.push_str(line);
    }
    *body = out;
}

/// The keys of a table, as `(name, value)` pairs.
///
/// A table named `model_providers.aroute` is the header `[model_providers.aroute]`.
/// The scan stops at the next header, because a key after another header
/// belongs to that other table.
fn table_keys(body: &str, table: &str) -> Vec<(String, String)> {
    let header = format!("[{table}]");
    let mut keys = Vec::new();
    let mut inside = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == header;
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((name, value)) = split_key(trimmed) {
            keys.push((name, unquote(&value)));
        }
    }
    keys
}

/// Splits `name = value` into its parts, for a line that is not a comment.
fn split_key(line: &str) -> Option<(String, String)> {
    let (name, value) = line.split_once('=')?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some((name.to_owned(), value.trim().to_owned()))
}

/// The value of a top-level key, or `None` when it is absent.
///
/// "Top level" means before the first table header: a key written after a
/// header belongs to that table, and `model_catalog_json` after a header
/// would be read as part of that table.
fn top_level_value(body: &str, key: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            return None;
        }
        if let Some((name, value)) = split_key(trimmed)
            && name == key
        {
            return Some(unquote(&value));
        }
    }
    None
}

/// Replaces the value of a top-level key.
///
/// The line keeps its own terminator, so a CRLF file stays CRLF and a file
/// with no trailing newline does not gain one.
fn replace_top_level(body: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut replaced = false;
    for line in lines_with_endings(body) {
        let trimmed = line_content(line).trim();
        if !replaced
            && let Some((name, _)) = split_key(trimmed)
            && name == key
        {
            out.push_str(&format!(
                "{key} = {}{}",
                toml_string(value),
                line_terminator(line)
            ));
            replaced = true;
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Inserts a top-level key at the very top of the file.
///
/// At the top, not at the end: a key appended after the last table header
/// belongs to that table, and the catalog key must be global. The new line
/// takes the body's own ending, so a CRLF file stays CRLF.
fn insert_top_level(body: &str, key: &str, value: &str) -> String {
    let mut out = format!("{key} = {}{}", toml_string(value), line_ending(body));
    out.push_str(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The edit options a test wants: the default base URL, no explicit URL,
    /// aroute as the active provider, and no command-backed auth.
    fn options() -> EditOptions {
        EditOptions {
            base_url: "http://127.0.0.1:20128/v1".to_owned(),
            explicit_base_url: false,
            default_provider: true,
            auth_command: false,
        }
    }
    fn bundled() -> serde_json::Value {
        json!({
            "models": [
                {
                    "slug": "code-mode-entry",
                    "visibility": "list",
                    "priority": 1,
                    "tool_mode": "code_mode_only",
                    "use_responses_lite": true,
                    "supports_search_tool": true,
                    "web_search_tool_type": "text_and_image",
                    "context_window": 272000,
                    "max_context_window": 272000,
                    "base_instructions": "the prompt",
                    "service_tiers": ["default"],
                    "upgrade": {"to": "gpt-9"},
                },
                {
                    "slug": DEFAULT_TEMPLATE_SLUG,
                    "visibility": "list",
                    "priority": 13,
                    "tool_mode": null,
                    "use_responses_lite": false,
                    "supports_search_tool": true,
                    "web_search_tool_type": "text_and_image",
                    "context_window": 272000,
                    "max_context_window": 272000,
                    "base_instructions": "the prompt",
                    "service_tiers": ["default"],
                    "upgrade": {"to": "gpt-9"},
                    "default_reasoning_level": "xhigh",
                    "supported_reasoning_levels": [
                        {"effort": "low", "description": "Fast"},
                        {"effort": "medium", "description": "Balanced"},
                        {"effort": "high", "description": "Deeper"},
                        {"effort": "xhigh", "description": "Extra deep"},
                        {"effort": "max", "description": "Maximum"},
                        {"effort": "ultra", "description": "Maximum, delegated"},
                    ],
                },
            ]
        })
    }

    fn lane(id: &str) -> Lane {
        Lane {
            id: id.to_owned(),
            context_length: Some(1_048_576),
        }
    }

    #[test]
    fn prefers_the_plain_shell_template_over_a_code_mode_entry() {
        let template = pick_template(bundled().get("models").unwrap().as_array().unwrap());
        assert_eq!(
            template.and_then(|row| row.get("slug").and_then(serde_json::Value::as_str).map(str::to_owned)),
            Some(DEFAULT_TEMPLATE_SLUG.to_owned()),
        );
    }

    #[test]
    fn clones_the_template_so_the_system_prompt_survives() {
        let catalog = build(&bundled(), &[lane("large-stack")]).expect("built");
        let entry = &catalog["models"][0];

        assert_eq!(
            entry["base_instructions"], "the prompt",
            "the prompt must be cloned, not invented"
        );
        assert_eq!(entry["slug"], "large-stack");
        assert_eq!(entry["display_name"], "large-stack");
        assert_eq!(entry["visibility"], "list");
    }

    #[test]
    fn carries_the_lanes_own_context_window() {
        let catalog = build(&bundled(), &[lane("large-stack")]).expect("built");

        assert_eq!(catalog["models"][0]["context_window"], 1_048_576);
        assert_eq!(catalog["models"][0]["max_context_window"], 1_048_576);
    }

    #[test]
    fn turns_off_the_hosted_search_tool_the_router_cannot_run() {
        let catalog = build(&bundled(), &[lane("large-stack")]).expect("built");

        assert_eq!(catalog["models"][0]["supports_search_tool"], false);
        // The type is *removed*, not nulled: Codex's schema types it as a
        // string or a map, and a `null` makes Codex reject the whole file.
        assert!(
            catalog["models"][0].get("web_search_tool_type").is_none(),
            "the search tool's type must be absent, not null"
        );
    }

    #[test]
    fn writes_no_null_where_codex_expects_a_string_or_a_map() {
        // Codex's schema types `tool_mode`, `multi_agent_version`,
        // `multi_agent_reasoning_effort` and `web_search_tool_type` as a string
        // or a map. A `null` there makes Codex refuse the whole catalog with
        // `invalid type: null, expected string or map`, so the entry is worth
        // nothing however correct it looks.
        let catalog = build(&bundled(), &[lane("large-stack")]).expect("built");
        let entry = &catalog["models"][0];

        for field in REMOVED_OVERRIDES {
            assert!(
                entry.get(field).is_none(),
                "{field} must be absent, not null"
            );
        }
        // The two the schema does accept as `null` are the ones the bundled
        // catalog itself uses that way.
        assert!(entry["availability_nux"].is_null());
        assert!(entry["upgrade"].is_null());
    }

    #[test]
    fn does_not_ask_codex_for_the_freeform_apply_patch_tool() {
        // `apply_patch_tool_type = "freeform"` is what makes Codex declare
        // `apply_patch` as a `custom` tool: a grammar-constrained tool whose
        // arguments are patch syntax rather than JSON. The router refuses a
        // `custom` declaration outright — `unsupported tool type "custom":
        // this build does not run it` — and that refusal is a 400 for the
        // whole request, so the MCP tools in the same request never run.
        let catalog = build(&bundled(), &[lane("large-stack")]).expect("built");

        assert!(
            catalog["models"][0].get("apply_patch_tool_type").is_none(),
            "the freeform apply_patch tool must not be declared"
        );
    }

    #[test]
    fn no_entry_declares_a_tool_the_router_cannot_run() {
        // Every entry, listed or hidden: a hidden entry is still an entry
        // Codex resolves by slug, and one that declares the hosted search tool
        // or the freeform apply_patch tool draws the same refusal a listed
        // entry draws.
        let catalog = build(&bundled(), &[lane("large-stack")]).expect("built");
        let rows = catalog["models"].as_array().expect("an array");

        for row in rows {
            let slug = row.get("slug").and_then(serde_json::Value::as_str).unwrap_or("?");
            assert_eq!(
                row.get("supports_search_tool").and_then(serde_json::Value::as_bool),
                Some(false),
                "{slug} declares the hosted search tool"
            );
            assert!(
                row.get("apply_patch_tool_type").is_none(),
                "{slug} declares the freeform apply_patch tool"
            );
            for field in REMOVED_OVERRIDES {
                assert!(
                    row.get(field).is_none(),
                    "{slug} writes a null where Codex expects a string or a map: {field}"
                );
            }
        }
    }

    #[test]
    fn clears_code_mode_behaviour_a_non_codex_lane_cannot_use() {
        let catalog = build(&bundled(), &[Lane {
            id: "copycat".to_owned(),
            context_length: None,
        }])
        .expect("built");

        // No window declared, so the template's own stays — but the code-mode
        // flags are cleared either way.
        assert_eq!(catalog["models"][0]["context_window"], 272000);
        assert!(
            catalog["models"][0].get("tool_mode").is_none(),
            "the code mode flag must be absent, not null"
        );
        assert_eq!(catalog["models"][0]["use_responses_lite"], false);
    }

    #[test]
    fn hides_the_bundled_entries_after_the_lanes() {
        let catalog = build(&bundled(), &[lane("a"), lane("b")]).expect("built");
        let rows = catalog["models"].as_array().expect("array");

        assert_eq!(rows.len(), 4, "two lanes plus two bundled");
        assert_eq!(rows[0]["slug"], "a");
        assert_eq!(rows[1]["slug"], "b");
        assert_eq!(rows[2]["visibility"], "hide");
        assert_eq!(rows[3]["visibility"], "hide");
        assert!(
            rows[2]
                .get("priority")
                .and_then(serde_json::Value::as_i64)
                .is_some_and(|priority| priority > 2),
            "hidden entries are shifted past the router's block"
        );
    }

    #[test]
    fn drops_an_empty_or_duplicate_lane_id() {
        let catalog = build(
            &bundled(),
            &[
                Lane {
                    id: " ".to_owned(),
                    context_length: None,
                },
                lane("dup"),
                lane("dup"),
            ],
        )
        .expect("built");

        let listed: Vec<&str> = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["visibility"] == "list")
            .map(|row| row["slug"].as_str().unwrap())
            .collect();
        assert_eq!(listed, vec!["dup"]);
    }

    #[test]
    fn puts_the_catalog_key_at_the_top_level() {
        let (edited, notes) = edit_config(
            "[model_providers.aroute]\nbase_url = \"https://example/v1\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: false,
                auth_command: false,
            },
        );

        assert!(
            edited.starts_with("model_catalog_json = \"/home/x/.codex/aroute-models.json\""),
            "the key must be before the first table header, got: {edited}"
        );
        assert!(notes.iter().any(|note| note.contains("model_catalog_json")));
    }

    #[test]
    fn keeps_an_existing_base_url() {
        let (edited, _) = edit_config(
            "[model_providers.aroute]\nbase_url = \"https://aroute.ishanparihar.com/v1\"\nenv_key = \"AROUTE_API_KEY\"\nwire_api = \"responses\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: false,
                auth_command: false,
            },
        );

        assert!(
            edited.contains("base_url = \"https://aroute.ishanparihar.com/v1\""),
            "the operator's own base URL must survive, got: {edited}"
        );
        assert!(!edited.contains("127.0.0.1:20128"), "the default must not overwrite it");
    }

    #[test]
    fn replaces_the_base_url_only_when_it_is_named() {
        let (edited, notes) = edit_config(
            "[model_providers.aroute]\nbase_url = \"https://old/v1\"\nenv_key = \"AROUTE_API_KEY\"\nwire_api = \"responses\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "https://new/v1".to_owned(),
                explicit_base_url: true,
                default_provider: false,
                auth_command: false,
            },
        );

        assert!(edited.contains("base_url = \"https://new/v1\""), "got: {edited}");
        assert!(!edited.contains("https://old/v1"));
        assert!(notes.iter().any(|note| note.contains("base_url")));
    }

    #[test]
    fn adds_the_missing_provider_keys_inside_the_block() {
        let (edited, notes) = edit_config(
            "[model_providers.aroute]\nbase_url = \"https://aroute.ishanparihar.com/v1\"\n\n[profiles.work]\nmodel = \"gpt-5.5\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: false,
                auth_command: false,
            },
        );

        // The keys must land inside the provider block, not after the next
        // table header, or Codex would read the provider as missing them.
        let block = edited
            .split("[model_providers.aroute]")
            .nth(1)
            .and_then(|tail| tail.split("[profiles.work]").next())
            .expect("the provider block");
        assert!(block.contains("env_key = \"AROUTE_API_KEY\""), "got: {block}");
        assert!(block.contains("wire_api = \"responses\""), "got: {block}");
        assert!(block.contains("name = \"aroute\""), "got: {block}");
        assert!(notes.iter().any(|note| note.contains("env_key")));
    }

    #[test]
    fn leaves_a_complete_provider_block_alone() {
        let before = "[model_providers.aroute]\nname = \"aroute\"\nbase_url = \"https://x/v1\"\nenv_key = \"AROUTE_API_KEY\"\nwire_api = \"responses\"\n";
        let (edited, notes) = edit_config(
            before,
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "https://x/v1".to_owned(),
                explicit_base_url: false,
                default_provider: false,
                auth_command: false,
            },
        );

        // Only the catalog key is added; the block is untouched.
        let block = edited
            .split("[model_providers.aroute]")
            .nth(1)
            .expect("the provider block");
        assert!(!block.contains("name = \"aroute\"\nname"), "got: {block}");
        assert_eq!(block.matches("base_url").count(), 1, "got: {block}");
        assert!(notes.iter().any(|note| note.contains("already complete")));
    }

    #[test]
    fn never_writes_the_secret() {
        let (edited, _) = edit_config(
            "",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: false,
                auth_command: false,
            },
        );

        assert!(edited.contains("env_key = \"AROUTE_API_KEY\""));
        assert!(
            !edited.to_lowercase().contains("sk-"),
            "no secret may be written, got: {edited}"
        );
    }

    #[test]
    fn writes_the_profile_with_the_lane_window_and_no_hosted_search() {
        let body = profile_body(&Lane {
            id: "large-stack".to_owned(),
            context_length: Some(1_048_576),
        });

        assert!(body.contains("model                          = \"large-stack\""));
        assert!(body.contains("model_context_window           = 1048576"));
        assert!(body.contains("model_provider                 = \"aroute\""));
        assert!(body.contains("web_search                     = \"disabled\""));
    }

    #[test]
    fn compacts_below_the_window_so_the_answer_still_fits() {
        let body = profile_body(&Lane {
            id: "large-stack".to_owned(),
            context_length: Some(1_048_576),
        });

        let compact: u64 = body
            .lines()
            .find_map(|line| line.trim().strip_prefix("model_auto_compact_token_limit = "))
            .and_then(|value| value.trim().parse().ok())
            .expect("a compact limit");
        assert!(compact < 1_048_576, "compaction at the ceiling leaves no room");
        assert_eq!(
            compact, 891_289,
            "the limit is 85% of the window, the share the profile corpus uses"
        );
    }

    #[test]
    fn keeps_a_foreign_catalog_path_and_says_what_it_costs() {
        // The path is the operator's own choice. Overwriting it would point
        // Codex at a catalog it did not ask for, and the cost of preserving
        // it — the lanes may not appear — has to be said out loud.
        let (edited, notes) = edit_config(
            "model_catalog_json = \"/home/x/.codex/other-models.json\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: false,
                auth_command: false,
            },
        );

        assert!(
            edited.contains("model_catalog_json = \"/home/x/.codex/other-models.json\""),
            "the operator's path must survive, got: {edited}"
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("still points at") && note.contains("may not appear")),
            "the consequence must be reported, got: {notes:?}"
        );
    }

    #[test]
    fn keeps_a_crlf_config_crlf() {
        // A CRLF config is a whole-file rewrite if the editor normalises the
        // endings, and the operator's file is theirs to keep byte for byte
        // apart from the keys this command owns.
        let before = "model_reasoning_effort = \"medium\"\r\n\r\n[tui]\r\nscreen_reader_detection_done = true\r\n";
        let (edited, _) = edit_config(
            before,
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: true,
                auth_command: false,
            },
        );

        assert!(
            !edited.contains('\n') || edited.matches('\n').count() == edited.matches("\r\n").count(),
            "every LF must still be part of a CRLF, got: {edited:?}"
        );
        assert!(
            edited.contains("screen_reader_detection_done = true"),
            "the rest of the file must survive, got: {edited:?}"
        );
    }

    #[test]
    fn does_not_add_a_trailing_newline_that_was_not_there() {
        let before = "model_reasoning_effort = \"medium\"";
        let (edited, _) = edit_config(
            before,
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: true,
                auth_command: false,
            },
        );

        assert!(
            !edited.ends_with('\n'),
            "a file that did not end with a newline must not gain one, got: {edited:?}"
        );
    }

    #[test]
    fn keeps_an_operators_own_hosted_search_off_the_base_config_path() {
        // The base-config path is the one that works without `--profile`, and
        // the catalog's `supports_search_tool: false` is not enough on its
        // own: measured against the live router, a request with this key
        // absent is refused with `unsupported tool type "web_search"`, and the
        // same request with it present is answered.
        let (edited, notes) = edit_config(
            "model_reasoning_effort = \"medium\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: true,
                auth_command: false,
            },
        );

        assert!(
            edited.contains("web_search = \"disabled\""),
            "the base config must disable the hosted search tool, got: {edited}"
        );
        assert!(notes.iter().any(|note| note.contains("web_search")));
    }

    #[test]
    fn turns_off_an_operators_own_hosted_search() {
        // The router refuses the hosted tool either way, so a request with
        // hosted search enabled fails for the whole request and takes the MCP
        // tools in the same request down with it. A value that cannot work
        // here is not a preference to preserve.
        let (edited, notes) = edit_config(
            "web_search = \"cached\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: true,
                auth_command: false,
            },
        );

        assert!(
            edited.contains("web_search = \"disabled\""),
            "got: {edited}"
        );
        assert!(!edited.contains("cached"), "got: {edited}");
        assert!(notes.iter().any(|note| note.contains("web_search")));
    }

    #[test]
    fn leaves_the_web_search_key_out_when_the_provider_is_not_defaulted() {
        // With `--no-default-provider` the router is reachable only through
        // the profile, and the profile carries the switch itself. Writing it
        // into the base config would then be a change the operator did not
        // ask for.
        let (edited, _) = edit_config(
            "model_reasoning_effort = \"medium\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: false,
                auth_command: false,
            },
        );

        assert!(
            !edited.contains("web_search"),
            "the key must not be written, got: {edited}"
        );
    }

    #[test]
    fn falls_back_to_the_largest_window_when_the_named_lane_is_absent() {
        // A config that has renamed `large-stack` still has to yield a
        // working profile, and the operator has to be told which lane was
        // chosen.
        let lanes = vec![
            Lane {
                id: "small".to_owned(),
                context_length: Some(262_144),
            },
            Lane {
                id: "wide".to_owned(),
                context_length: Some(1_048_576),
            },
        ];

        let (lane, fallback) = pick_lane(&lanes, "large-stack").expect("a lane");
        assert_eq!(lane.id, "wide");
        assert!(fallback, "the substitution must be reported");
    }

    #[test]
    fn uses_the_named_lane_when_the_config_has_it() {
        let lanes = vec![
            Lane {
                id: "small".to_owned(),
                context_length: Some(262_144),
            },
            Lane {
                id: "wide".to_owned(),
                context_length: Some(1_048_576),
            },
        ];

        let (lane, fallback) = pick_lane(&lanes, "small").expect("a lane");
        assert_eq!(lane.id, "small");
        assert!(!fallback, "no substitution happened");
    }

    #[test]
    fn fails_when_the_config_has_no_lane_at_all() {
        let lanes: Vec<Lane> = Vec::new();
        assert!(pick_lane(&lanes, "large-stack").is_err());
    }

    #[test]
    fn selects_the_router_as_the_active_provider() {
        // Without this, Codex sends the request to OpenAI's own backend,
        // which answers every router model name with "not supported when
        // using Codex with a ChatGPT account" — an error that reads like a
        // plan problem and is not one.
        let (edited, notes) = edit_config(
            "",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: true,
                auth_command: false,
            },
        );

        assert!(
            edited.contains("model_provider = \"aroute\""),
            "got: {edited}"
        );
        assert!(notes.iter().any(|note| note.contains("model_provider")));
    }

    #[test]
    fn leaves_a_lane_without_a_window_out_of_the_catalog() {
        // A combo with no context window is not a chat lane. It cannot be
        // offered in a model picker, because a client that picks it gets a
        // request the endpoint refuses.
        let doc = "combos:\n  - id: large-stack\n    strategy: priority\n    context_length: 1048576\n    targets:\n      - nvidia/minimaxai/minimax-m3.1-flash\n  - id: span-01-lite\n    strategy: priority\n    targets:\n      - decisions/span-01-lite\n";
        let cfg = ar_config::Config::parse(doc, |_| Ok(None)).expect("a loadable config");

        let lanes = lanes(&cfg);
        assert_eq!(lanes.len(), 1, "only the chat lane belongs in the catalog");
        assert_eq!(lanes[0].id, "large-stack");
    }

    #[test]
    fn keeps_the_backup_when_one_is_already_there() {
        let dir = std::env::temp_dir().join(format!("aroute-codex-bak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.toml");
        std::fs::write(&config, "original\n").expect("wrote");

        backup_once(&config, "original\n").expect("first backup");
        let backup = dir.join("config.toml.aroute.bak");
        assert_eq!(
            std::fs::read_to_string(&backup).expect("backup"),
            "original\n"
        );

        // The second run must not overwrite the first backup: the first one is
        // the state before aroute ever touched the file.
        backup_once(&config, "edited\n").expect("second backup");
        assert_eq!(
            std::fs::read_to_string(&backup).expect("backup"),
            "original\n",
            "the backup must keep the pre-edit state"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn offers_only_the_efforts_the_router_runs() {
        // The bundled template is a GPT model's, and it offers `xhigh`,
        // `max` and `ultra`. The router refuses every effort above `high`
        // with a 400 that fails the whole turn, so the catalog must not
        // promise one. A level the picker offers and the router refuses is
        // a request that cannot work.
        let catalog = build(&bundled(), &[lane("large-stack")]).expect("built");
        let rows = catalog["models"].as_array().expect("an array");

        for row in rows {
            let levels: Vec<String> = row
                .get("supported_reasoning_levels")
                .and_then(serde_json::Value::as_array)
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|level| {
                            level.get("effort").map(serde_json::Value::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            for level in &levels {
                assert!(
                    SUPPORTED_REASONING_EFFORTS.contains(&level.trim_matches('"')),
                    "{slug} offers {level}, which the router refuses",
                    slug = row["slug"]
                );
            }
        }
    }

    #[test]
    fn drops_a_default_effort_the_router_refuses() {
        // The template's default is `xhigh`. Left as it is, Codex would send
        // it on every turn that names no effort, and the router would refuse
        // each one.
        let catalog = build(&bundled(), &[lane("large-stack")]).expect("built");
        let rows = catalog["models"].as_array().expect("an array");

        for row in rows {
            let default = row
                .get("default_reasoning_level")
                .and_then(serde_json::Value::as_str)
                .expect("a default effort");
            assert!(
                SUPPORTED_REASONING_EFFORTS.contains(&default),
                "{slug} defaults to {default}, which the router refuses",
                slug = row["slug"]
            );
        }
    }

    #[test]
    fn replaces_a_reasoning_effort_the_router_refuses() {
        // The operator's own base config may name `xhigh`, and every plain
        // `codex` turn would fail on it. An effort that works is left alone:
        // that is a preference worth keeping.
        let (edited, notes) = edit_config(
            "model_reasoning_effort = \"xhigh\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: true,
                auth_command: false,
            },
        );

        assert!(
            edited.contains("model_reasoning_effort = \"medium\""),
            "an effort the router refuses must be replaced, got: {edited}"
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("model_reasoning_effort")),
            "the replacement must be reported, got: {notes:?}"
        );

        let (kept, _) = edit_config(
            "model_reasoning_effort = \"high\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                base_url: "http://127.0.0.1:20128/v1".to_owned(),
                explicit_base_url: false,
                default_provider: true,
                auth_command: false,
            },
        );
        assert!(
            kept.contains("model_reasoning_effort = \"high\""),
            "an effort the router runs must be left alone, got: {kept}"
        );
    }

    #[test]
    fn writes_the_command_backed_auth_when_asked() {
        // Codex reads `env_key` from the process environment, so a key the
        // shell sources is in one shell's environment only, and a session
        // started any other way answers `Missing environment variable:
        // AROUTE_API_KEY`. The command-backed auth is the fix: Codex runs the
        // command itself, so the key reaches every session.
        let (edited, notes) = edit_config(
            "[model_providers.aroute]\nbase_url = \"https://x/v1\"\nenv_key = \"AROUTE_API_KEY\"\nwire_api = \"responses\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                auth_command: true,
                ..options()
            },
        );

        assert!(
            edited.contains("[model_providers.aroute.auth]"),
            "the auth table must be written, got: {edited}"
        );

        // `command` is a program path and the arguments go beside it. A single
        // string with a space names a program with a space in its name, and
        // Codex answers `failed to start: No such file or directory`.
        assert!(
            edited.contains(&format!("command = \"{AUTH_COMMAND}\"")),
            "the auth program must be named, got: {edited}"
        );
        assert!(
            edited.contains("args = [\"token\"]"),
            "the token subcommand must be an argument, got: {edited}"
        );
        assert!(
            !edited.contains("aroute token"),
            "the program and its argument must not be one string, got: {edited}"
        );

        // The environment-variable form must *go*. Codex's own validator says
        // `provider auth cannot be combined with env_key`, and a provider it
        // refuses makes the whole config fail to load — so keeping both is
        // worse than either alone. Measured against the installed CLI: a
        // config with both answers
        // `Error loading config.toml: model_providers.aroute: provider auth
        // cannot be combined with env_key`.
        assert!(
            !edited.contains("env_key"),
            "env_key must not survive beside the auth table, got: {edited}"
        );

        assert!(
            notes.iter().any(|note| note.contains("removed env_key")),
            "the removal must be reported, got: {notes:?}"
        );
        assert!(
            notes.iter().any(|note| note.contains("auth")),
            "the addition must be reported, got: {notes:?}"
        );

        // The block's other keys are the operator's, and they stay.
        assert!(edited.contains("wire_api = \"responses\""), "got: {edited}");
        assert!(edited.contains("base_url = \"https://x/v1\""), "got: {edited}");
    }

    #[test]
    fn removes_only_the_env_key_and_not_the_rest_of_the_block() {
        // The removal is surgical: a config with several providers loses the
        // one key the auth table replaces, and nothing else.
        let (edited, _) = edit_config(
            "[model_providers.aroute]\nbase_url = \"https://x/v1\"\nenv_key = \"AROUTE_API_KEY\"\nwire_api = \"responses\"\n\n[model_providers.other]\nenv_key = \"OTHER_KEY\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                auth_command: true,
                ..options()
            },
        );

        assert!(
            edited.contains("env_key = \"OTHER_KEY\""),
            "the other provider's key is not ours to remove, got: {edited}"
        );
        assert!(
            edited.contains("args = [\"token\"]"),
            "the auth arguments must be written, got: {edited}"
        );
    }

    #[test]
    fn replaces_a_stale_auth_command_rather_than_keeping_it() {
        // A config written by an earlier shape of this command carries
        // `command = "aroute token"`, which names a program with a space in
        // its name. Codex answers `provider auth command \`aroute token\`
        // failed to start: No such file or directory`. The command owns this
        // key, so it replaces the stale value instead of leaving a config
        // Codex cannot authenticate against.
        let (edited, notes) = edit_config(
            "[model_providers.aroute]\nbase_url = \"https://x/v1\"\nwire_api = \"responses\"\n\n[model_providers.aroute.auth]\ncommand = \"aroute token\"\ntimeout_ms = 10000\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                auth_command: true,
                ..options()
            },
        );

        assert!(
            edited.contains("command = \"aroute\""),
            "the stale command must be replaced, got: {edited}"
        );
        assert!(
            !edited.contains("\"aroute token\""),
            "the stale command must not survive, got: {edited}"
        );
        assert!(
            edited.contains("args = [\"token\"]"),
            "the arguments must be written, got: {edited}"
        );
        assert!(
            notes.iter().any(|note| note.contains("command")),
            "the replacement must be reported, got: {notes:?}"
        );
    }

    #[test]
    fn keeps_the_auth_table_when_the_command_is_run_without_the_flag() {
        // The credential form is decided by the config, not only by the flag.
        // A run without `--auth-command` must not write `env_key` back beside
        // an existing auth table: Codex refuses the combination outright
        // (`provider auth cannot be combined with env_key`) and a refused
        // provider makes the whole config fail to load. Measured against the
        // installed CLI: the next session answers
        // `Error loading config.toml: model_providers.aroute: provider auth
        // cannot be combined with env_key`.
        let before = "[model_providers.aroute]\nbase_url = \"https://x/v1\"\nwire_api = \"responses\"\n\n[model_providers.aroute.auth]\ncommand = \"aroute\"\nargs = [\"token\"]\ntimeout_ms = 10000\n";
        let (edited, _notes) = edit_config(
            before,
            Path::new("/home/x/.codex/aroute-models.json"),
            &options(),
        );

        assert!(
            !edited.contains("env_key"),
            "env_key must not be written back beside the auth table, got: {edited}"
        );
        assert!(
            edited.contains("args = [\"token\"]"),
            "the auth table must survive, got: {edited}"
        );
    }

    #[test]
    fn removes_an_env_key_that_sits_beside_an_existing_auth_table() {
        // The other direction: a config that somehow carries both is broken,
        // and the command has to repair it. Codex refuses the combination, so
        // leaving it is a config no session can load.
        let (edited, notes) = edit_config(
            "[model_providers.aroute]\nbase_url = \"https://x/v1\"\nenv_key = \"AROUTE_API_KEY\"\nwire_api = \"responses\"\n\n[model_providers.aroute.auth]\ncommand = \"aroute\"\nargs = [\"token\"]\ntimeout_ms = 10000\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &options(),
        );

        assert!(
            !edited.contains("env_key"),
            "env_key must go, got: {edited}"
        );
        assert!(
            notes.iter().any(|note| note.contains("removed env_key")),
            "the removal must be reported, got: {notes:?}"
        );
    }

    #[test]
    fn writes_a_key_inside_the_block_and_not_after_its_blank_line() {
        // A block that ends with a blank line still ends at the next header
        // in TOML, so a key written after the blank line belongs to the block
        // — but a reader that stops at the blank line does not see it, and
        // the two readings disagree about what the provider carries. The key
        // therefore goes after the block's last *key* line.
        let (edited, _) = edit_config(
            "[model_providers.aroute]\nbase_url = \"https://x/v1\"\n\n[model_providers.other]\nname = \"other\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &EditOptions {
                auth_command: false,
                ..options()
            },
        );

        let block = edited
            .split("[model_providers.aroute]")
            .nth(1)
            .and_then(|rest| rest.split("[model_providers.other]").next())
            .unwrap_or_default();
        assert!(
            block.contains("env_key = \"AROUTE_API_KEY\""),
            "the key must be inside the block, got: {block}"
        );
        assert!(
            !block.trim_start().starts_with('\n'),
            "the key must not be after a blank line, got: {block}"
        );
    }

    #[test]
    fn leaves_the_command_backed_auth_alone_when_not_asked() {
        // The mechanism is opt-in: a config the operator built by hand keeps
        // its own provider block.
        let (edited, _) = edit_config(
            "[model_providers.aroute]\nbase_url = \"https://x/v1\"\n",
            Path::new("/home/x/.codex/aroute-models.json"),
            &options(),
        );

        assert!(
            !edited.contains(AUTH_TABLE),
            "the auth table must not appear unasked, got: {edited}"
        );
    }

    #[test]
    fn pins_the_effort_in_the_profile_so_the_base_config_cannot_leak_through() {
        // The profile layers over the base config. Without the pin, the base
        // config's own `xhigh` would reach the router on the `--profile`
        // path too.
        let body = profile_body(&lane("large-stack"));

        assert!(
            body.contains("model_reasoning_effort         = \"medium\""),
            "the profile must pin an effort the router runs, got: {body}"
        );
    }
}
