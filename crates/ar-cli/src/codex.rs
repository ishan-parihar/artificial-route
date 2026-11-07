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
}

/// The provider block's own table name in `config.toml`.
const PROVIDER_TABLE: &str = "model_providers.aroute";

/// The default provider base URL: the local router.
///
/// A default, not a policy: an existing `base_url` in the operator's
/// `config.toml` wins, because that is where they pointed Codex on purpose.
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:20128/v1";

/// The combo a profile uses when the operator names none.
const DEFAULT_MODEL: &str = "large-stack";

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
    let lane = lanes
        .iter()
        .find(|lane| lane.id == model)
        .ok_or_else(|| {
            fail(
                format!("the config has no combo named {model:?}"),
                "run `aroute combo` to see the routable ids, or pass --model",
            )
        })?;
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
    let (edited, notes) = edit_config(&config, &catalog_path, &base_url, args.base_url.is_some());
    if edited != config {
        backup_once(&config_path, &config)?;
        write_text_atomic(&config_path, &edited)?;
        changed.push(config_path.display().to_string());
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
                "context_window": window,
                "changed": changed,
                "notes": notes,
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
        println!("next: codex --profile aroute");
    }
    Ok(())
}

/// The profile layer's body.
///
/// The window and the compact limit come from the lane, not from a constant,
/// so a lane with a smaller window compacts at its own scale instead of at a
/// figure borrowed from another lane.
fn profile_body(lane: &Lane) -> String {
    let window = lane.context_length.unwrap_or(0);
    let compact = u64::from(window) * 8100 / 10000;
    format!(
        "# codex --profile aroute\n\
         # aroute (Artificial Route)\n\
         model                          = \"{id}\"\n\
         model_provider                 = \"aroute\"\n\
         model_context_window           = {window}\n\
         model_auto_compact_token_limit = {compact}\n\
         tool_output_token_limit        = 32768\n\
         \n\
         # aroute has no web access and refuses the hosted tool, so Codex must not\n\
         # ask for it. The switch is client-side: it keeps the `web_search` tool out\n\
         # of the request aroute sees.\n\
         web_search                     = \"disabled\"\n",
        id = lane.id,
        window = window,
        compact = compact,
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
        listed.push(entry_from(&template, lane)?);
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
fn entry_from(
    template: &serde_json::Value,
    lane: &Lane,
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

/// Edits `config.toml`: adds the catalog key at the top level and makes sure
/// the provider block exists, without disturbing what is already there.
///
/// The editor is line-oriented rather than a TOML parser, because the repo
/// does not depend on a TOML crate and this command must not add one. It only
/// ever needs to recognise three things: a top-level key, a table header, and
/// the keys of one table. Everything else it passes through byte for byte.
///
/// `explicit_base_url` says the operator named the URL on the command line,
/// which is the only case where an existing `base_url` is replaced.
fn edit_config(
    body: &str,
    catalog: &Path,
    base_url: &str,
    explicit_base_url: bool,
) -> (String, Vec<String>) {
    let mut notes = Vec::new();
    let mut out = body.to_owned();
    let catalog_value = catalog.display().to_string();

    // The catalog key. It is top-level, not under a table: the catalog is
    // global, and a `model_catalog_json` inside `[model_providers.aroute]`
    // would be read as a provider key and ignored.
    if let Some(existing) = top_level_value(&out, "model_catalog_json") {
        if existing == catalog_value {
            notes.push("model_catalog_json already points at the catalog".to_owned());
        } else {
            out = replace_top_level(&out, "model_catalog_json", &catalog_value);
            notes.push(format!(
                "model_catalog_json now points at {catalog_value} (was {existing})"
            ));
        }
    } else {
        out = insert_top_level(&out, "model_catalog_json", &catalog_value);
        notes.push(format!("added model_catalog_json = {catalog_value}"));
    }

    // The provider block.
    edit_provider(&mut out, base_url, explicit_base_url, &mut notes);

    (out, notes)
}

/// The provider block's own keys, in the order Codex documents them.
///
/// The secret is never written: `env_key` names the environment variable, so
/// the key itself stays in the operator's shell and never lands in a file.
fn provider_keys(base_url: &str) -> Vec<(&'static str, String)> {
    vec![
        ("name", "aroute".to_owned()),
        ("base_url", base_url.to_owned()),
        ("env_key", "AROUTE_API_KEY".to_owned()),
        ("wire_api", "responses".to_owned()),
    ]
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
    notes: &mut Vec<String>,
) {
    let existing = table_keys(body, PROVIDER_TABLE);
    if existing.is_empty() {
        notes.push("added [model_providers.aroute]".to_owned());
        body.push_str("\n[model_providers.aroute]\n");
        for (key, value) in provider_keys(base_url) {
            body.push_str(&format!("{key} = {}\n", toml_string(&value)));
        }
        return;
    }

    // The block is there. Add the keys it lacks, and leave the rest.
    let missing: Vec<(&str, String)> = provider_keys(base_url)
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
}

/// Inserts `key = value` at the end of `table`'s block.
///
/// The end of the block is the line before the next table header, or the end
/// of the file when the table is the last one.
fn insert_into_table(body: &mut String, table: &str, key: &str, value: &str) {
    let header = format!("[{table}]");
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    let mut inserted = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // The next table starts: the key goes before it.
            if inside && !inserted {
                out.push(format!("{key} = {}", toml_string(value)));
                inserted = true;
            }
            inside = trimmed == header;
        }
        out.push(line.to_owned());
    }
    if inside && !inserted {
        out.push(format!("{key} = {}", toml_string(value)));
    }
    *body = out.join("\n");
    if !body.ends_with('\n') {
        body.push('\n');
    }
}

/// Replaces the value of `key` inside `table`.
fn replace_in_table(body: &mut String, table: &str, key: &str, value: &str) {
    let header = format!("[{table}]");
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == header;
        }
        if inside
            && let Some((name, _)) = split_key(trimmed)
            && name == key
        {
            out.push(format!("{key} = {}", toml_string(value)));
            continue;
        }
        out.push(line.to_owned());
    }
    *body = out.join("\n");
    if !body.ends_with('\n') {
        body.push('\n');
    }
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
fn replace_top_level(body: &str, key: &str, value: &str) -> String {
    let mut out = String::new();
    let mut replaced = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if !replaced
            && let Some((name, _)) = split_key(trimmed)
            && name == key
        {
            out.push_str(&format!("{key} = {}\n", toml_string(value)));
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Inserts a top-level key at the very top of the file.
///
/// At the top, not at the end: a key appended after the last table header
/// belongs to that table, and the catalog key must be global.
fn insert_top_level(body: &str, key: &str, value: &str) -> String {
    let mut out = format!("{key} = {}\n", toml_string(value));
    out.push_str(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            "http://127.0.0.1:20128/v1",
            false,
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
            "http://127.0.0.1:20128/v1",
            false,
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
            "https://new/v1",
            true,
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
            "http://127.0.0.1:20128/v1",
            false,
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
            "https://x/v1",
            false,
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
            "http://127.0.0.1:20128/v1",
            false,
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
}
