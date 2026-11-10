//! `aroute harness` — this router owns the harness model lists.
//!
//! Four harnesses keep a **static** copy of the catalog because they cannot
//! resolve it live: opencode (`provider.ar.models`), jcode
//! (`[[providers.aroute.models]]`), pi (`providers.aroute.models`) and omo
//! (`providers.ar.models`). A missing entry means the harness cannot ask for the
//! model at all, and a wrong window is worse: the harness sizes a request off it
//! and the upstream refuses one it would have accepted.
//!
//! Those lists used to be written by a vendored Python script that read the
//! *live* `GET /v1/models`, so the CLI published a fact and a script outside
//! this repository re-derived it over HTTP. This module makes the config the
//! single source: the catalog is the router's own combos, and its windows are
//! the combos' declared `context_length:`. Nothing is guessed and nothing needs
//! a gate token — the same principle `aroute codex` already follows.
//!
//! Three harnesses are *not* written here, and are reported so an operator can
//! see that they were considered:
//!
//! * `omp` and `claude` resolve the catalog live (`discovery:
//!   openai-models-list`, and `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`),
//!   so they need no static list and cannot drift.
//! * `codex` needs a generated `model_catalog_json` cloned from Codex's own
//!   bundled entries; `aroute codex` owns that file. `check` still reads it, so a
//!   lane added after the last `aroute codex` is reported here rather than
//!   discovered when Codex refuses the model.
//! * `dsh` keeps its list inside its own profile patch layer
//!   (`~/.dsh/profiles/web/cordis.patch.yml`), a hand-edited YAML document that
//!   also carries unrelated plugin entries, comments and `!!js` expressions.
//!   That list holds decision lanes (`jev`, `clef`, `span-01-lite`) which the
//!   chat catalog deliberately excludes, so it is **merged**, not replaced: the
//!   rows this router publishes are written in place, a declared decision combo
//!   is copied through untouched, any other unpublished row is dropped as a
//!   lane this router no longer serves, and only the `models:` sequence is
//!   edited — the document around it is never re-serialised.
//!
//! A combo with **no** declared window is excluded from the catalog and named
//! in `check`, rather than published at the server's 128K floor: a lane that
//! forgot its window is a config bug, and a harness that silently advertises the
//! floor hides it. This is the `mistral-1t` failure — the lane existed only as a
//! *target* inside `large-stack`, so no combo id published it and every harness
//! was correct to omit a model it could never request.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ar_config::Config;
use serde_json::{Map, Value, json};

use crate::cli::Cli;
use crate::commands::{fail, load_edit};

/// The provider block opencode writes under: `provider.ar.models`.
const OPENCODE_PROVIDER: &str = "ar";
/// The provider block pi writes under: `providers.aroute.models`.
const PI_PROVIDER: &str = "aroute";
/// The provider block omo writes under: `providers.ar.models`. omo spells the
/// provider `ar`, not `aroute`, and writes it under a different file.
const OMO_PROVIDER: &str = "ar";
/// The provider block jcode writes under: `[[providers.aroute.models]]`.
const JCODE_PROVIDER: &str = "aroute";

/// pi's output ceiling when the harness has none of its own.
///
/// pi requires `maxTokens` on every model and the router publishes none, so an
/// existing operator value is preserved and this is only the seed for a new row.
const PI_OUTPUT_DEFAULT: u64 = 32_000;
/// omo's output ceiling seed, same rule as [`PI_OUTPUT_DEFAULT`].
const OMO_OUTPUT_DEFAULT: u64 = 64_000;

/// The backup suffix every harness write leaves behind.
///
/// The same suffix the vendored script used, so an operator's existing
/// `.bak-model-sync` files keep meaning "the list before the last sync".
const BACKUP_SUFFIX: &str = ".bak-model-sync";

/// One model a harness should advertise: the id a client asks for, and the
/// window the router will actually honour.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Model {
    /// The combo id, which is also the `model` a request names.
    id: String,
    /// The combo's declared window, in tokens.
    context: u32,
}

/// How a harness learns the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Holds a static list this command owns and writes.
    Static,
    /// Resolves the catalog live, so there is nothing to write.
    Discovery,
    /// Has a catalog owned by another verb (`aroute codex`).
    Codex,
    /// Holds a list this command merges into: rows for the catalog's ids are
    /// written, rows for ids the catalog does not publish are preserved.
    Merged,
}

impl Kind {
    /// The word `list` prints for this kind.
    fn as_str(self) -> &'static str {
        match self {
            Kind::Static => "static",
            Kind::Discovery => "discovery",
            Kind::Codex => "codex",
            Kind::Merged => "merged",
        }
    }
}

/// One harness on this host, and where its catalog lives.
#[derive(Debug)]
struct Harness {
    /// The name an operator types in `--harness`.
    name: &'static str,
    /// How it learns the catalog.
    kind: Kind,
    /// The file that holds the list (absent for discovery harnesses).
    path: Option<PathBuf>,
}

/// Options for `aroute harness`.
#[derive(Debug, clap::Args)]
pub struct HarnessArgs {
    /// What to do: `list` (default), `check`, or `apply`.
    #[command(subcommand)]
    pub command: Option<HarnessCommand>,

    /// Only this harness. One of opencode, jcode, pi, omo, omp, claude, codex, dsh.
    #[arg(long, value_name = "NAME", global = true)]
    pub harness: Option<String>,

    /// Read harness files from this directory instead of `$HOME`.
    ///
    /// The harness paths are all under the home directory, so this is what makes
    /// an apply reviewable against a copy before it touches the real files.
    #[arg(long, value_name = "DIR", global = true)]
    pub home: Option<PathBuf>,
}

/// The three verbs `aroute harness` offers.
#[derive(Debug, clap::Subcommand)]
pub enum HarnessCommand {
    /// Every harness, its kind, its file, and whether it matches the router.
    #[command(verbatim_doc_comment)]
    List,

    /// Report drift against the router's own combos, and change nothing.
    ///
    /// Exits 1 when any static harness is missing a model, carries one the
    /// router does not publish, or has the wrong window.
    #[command(verbatim_doc_comment)]
    Check,

    /// Write every static harness list from the router's own combos.
    #[command(verbatim_doc_comment)]
    Apply {
        /// Print what would be written, and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Runs `aroute harness`.
pub fn run(cli: &Cli, args: &HarnessArgs) -> anyhow::Result<()> {
    // `load_edit` rather than `load`: rendering a model list must work on a
    // config whose credentials are not exported in this shell, which is the
    // ordinary state of a fresh terminal.
    let cfg = load_edit(cli)?;
    let models = catalog(&cfg);
    // A config with no chat lane would render an empty list into every harness
    // and call the result "in sync". `config.yaml` is relative to the working
    // directory, so running this from a checkout reads the repository's own
    // fixture — which is exactly how a whole fleet's lists get emptied.
    if models.is_empty() {
        return Err(fail(
            format!(
                "{} declares no chat combo with a context_length, so it cannot describe a harness catalog",
                cli.config.display()
            ),
            "pass --config ~/.config/ar/config.yaml (the path is relative to the working directory)",
        ));
    }
    let home = match &args.home {
        Some(dir) => dir.clone(),
        None => home_dir()?,
    };
    let harnesses = harnesses(&home);
    let selected = select(&harnesses, args.harness.as_deref())?;

    match args.command.as_ref() {
        None | Some(HarnessCommand::List) => list(&selected, &models, &home),
        Some(HarnessCommand::Check) => check(&selected, &models, &cfg, &home),
        Some(HarnessCommand::Apply { dry_run }) => {
            apply(&selected, &models, &decision_ids(&cfg), *dry_run)
        }
    }
}

/// The home directory the harness paths are relative to.
fn home_dir() -> anyhow::Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Err(fail(
            "HOME is not set, so no harness path can be resolved",
            "pass --home <DIR> to name the directory holding the harness configs",
        )),
    }
}

/// Every harness this command knows about, in a stable order.
fn harnesses(home: &Path) -> Vec<Harness> {
    vec![
        Harness {
            name: "opencode",
            kind: Kind::Static,
            path: Some(home.join(".config/opencode/opencode.json")),
        },
        Harness {
            name: "jcode",
            kind: Kind::Static,
            path: Some(home.join(".jcode/config.toml")),
        },
        Harness {
            name: "pi",
            kind: Kind::Static,
            path: Some(home.join(".pi/agent/models.json")),
        },
        Harness {
            name: "omo",
            kind: Kind::Static,
            path: Some(home.join(".omo/agent/models.json")),
        },
        Harness {
            name: "omp",
            kind: Kind::Discovery,
            path: None,
        },
        Harness {
            name: "claude",
            kind: Kind::Discovery,
            path: None,
        },
        Harness {
            name: "codex",
            kind: Kind::Codex,
            path: None,
        },
        Harness {
            name: "dsh",
            kind: Kind::Merged,
            path: Some(home.join(".dsh/profiles/web/cordis.patch.yml")),
        },
    ]
}

/// Applies `--harness`, refusing a name this host does not have.
fn select<'a>(harnesses: &'a [Harness], wanted: Option<&str>) -> anyhow::Result<Vec<&'a Harness>> {
    let Some(name) = wanted else {
        return Ok(harnesses.iter().collect());
    };
    let found: Vec<&Harness> = harnesses
        .iter()
        .filter(|harness| harness.name == name)
        .collect();
    if found.is_empty() {
        let known: Vec<&str> = harnesses.iter().map(|harness| harness.name).collect();
        return Err(fail(
            format!("no harness named {name:?}"),
            format!("use --harness with one of: {}", known.join(", ")),
        ));
    }
    Ok(found)
}

/// The catalog: every chat combo that declares its window.
///
/// The same structural rule `aroute codex` applies — a decision target makes the
/// combo unusable by a chat harness, and a combo with no window is not a chat
/// lane — so the CLI's two catalog surfaces cannot disagree.
fn catalog(cfg: &Config) -> Vec<Model> {
    let decision = decision_providers(cfg);
    let mut models: Vec<Model> = cfg
        .combos
        .iter()
        .filter(|combo| {
            combo.context_length.is_some()
                && !combo.targets.iter().any(|target| {
                    let provider = target.split('/').next().unwrap_or_default();
                    decision.contains(provider)
                })
        })
        .filter_map(|combo| {
            combo.context_length.map(|context| Model {
                id: combo.id.clone(),
                context,
            })
        })
        .collect();
    models.sort();
    models.dedup_by(|a, b| a.id == b.id);
    models
}

/// The combos that look like chat lanes but declared no window, so the catalog
/// had to leave them out. Named in `check` instead of published at the floor.
fn windows_missing(cfg: &Config) -> Vec<String> {
    let decision = decision_providers(cfg);
    cfg.combos
        .iter()
        .filter(|combo| combo.context_length.is_none())
        .filter(|combo| {
            !combo.targets.iter().any(|target| {
                let provider = target.split('/').next().unwrap_or_default();
                decision.contains(provider)
            })
        })
        .map(|combo| combo.id.clone())
        .collect()
}

/// The custom providers whose endpoint is a decisions/systemone path.
fn decision_providers(cfg: &Config) -> BTreeSet<&str> {
    cfg.custom_providers
        .iter()
        .filter(|provider| {
            provider.endpoint.as_deref().is_some_and(|endpoint| {
                ar_server::models::DECISION_ENDPOINTS
                    .iter()
                    .any(|decision| endpoint.contains(decision))
            })
        })
        .map(|provider| provider.id.as_str())
        .collect()
}

/// The combo ids a merged harness may keep even though the catalog omits them:
/// the decision lanes it lists beside the chat catalog. Anything else it holds
/// that the router does not publish is a lane this router no longer serves.
fn decision_ids(cfg: &Config) -> BTreeSet<String> {
    let decision = decision_providers(cfg);
    cfg.combos
        .iter()
        .filter(|combo| {
            combo.targets.iter().any(|target| {
                let provider = target.split('/').next().unwrap_or_default();
                decision.contains(provider)
            })
        })
        .map(|combo| combo.id.clone())
        .collect()
}

/// A display name for a model that has none: `deepseek-v4.1-flash` becomes
/// `Deepseek V4.1 Flash`, which is what every other harness shows anyway.
fn display_name(id: &str) -> String {
    id.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// `aroute harness list`.
fn list(selected: &[&Harness], models: &[Model], home: &Path) -> anyhow::Result<()> {
    let mut rows = Vec::with_capacity(selected.len());
    for harness in selected {
        let Some(path) = &harness.path else {
            let (count, state) = match harness.kind {
                Kind::Codex => match codex_ids(home) {
                    Some(ids) => {
                        let missing = models.iter().filter(|model| !ids.contains(&model.id)).count();
                        (
                            ids.len().to_string(),
                            if missing == 0 {
                                "in sync".to_owned()
                            } else {
                                format!("drifted: {missing} lane(s) missing")
                            },
                        )
                    }
                    None => ("-".to_owned(), "no model_catalog_json in ~/.codex/config.toml".to_owned()),
                },
                _ => ("-".to_owned(), "resolves the catalog live".to_owned()),
            };
            rows.push(vec![
                harness.name.to_owned(),
                harness.kind.as_str().to_owned(),
                "-".to_owned(),
                count,
                state,
            ]);
            continue;
        };
        let current = current_models(harness, path);
        let (count, state) = match current {
            Some(have) => (have.len().to_string(), state_of(&have, models, harness.kind)),
            None => ("-".to_owned(), "not installed on this host".to_owned()),
        };
        rows.push(vec![
            harness.name.to_owned(),
            harness.kind.as_str().to_owned(),
            path.display().to_string(),
            count,
            state,
        ]);
    }
    let all = ["harness", "kind", "path", "models", "state"];
    let fields: Vec<String> = all.iter().map(|f| (*f).to_owned()).collect();
    print!(
        "{}",
        crate::toon::list("harnesses", "harnesses", &all, &fields, &rows, false)
    );
    Ok(())
}

/// A one-word verdict for one harness's list.
///
/// A hand-kept list is judged only on what it is *missing*: dsh also carries
/// decision lanes the chat catalog excludes, and calling that drift would make
/// the row un-clearable without deleting working entries.
fn state_of(have: &BTreeMap<String, u32>, models: &[Model], kind: Kind) -> String {
    let synced = match kind {
        Kind::Merged => models
            .iter()
            .all(|model| have.get(&model.id) == Some(&model.context)),
        _ => {
            let wanted: BTreeMap<String, u32> = models
                .iter()
                .map(|model| (model.id.clone(), model.context))
                .collect();
            have == &wanted
        }
    };
    if synced {
        "in sync".to_owned()
    } else {
        "drifted".to_owned()
    }
}

/// `aroute harness check`: report drift, change nothing.
fn check(selected: &[&Harness], models: &[Model], cfg: &Config, home: &Path) -> anyhow::Result<()> {
    let keep = decision_ids(cfg);
    let wanted: BTreeMap<&str, u32> = models
        .iter()
        .map(|model| (model.id.as_str(), model.context))
        .collect();
    let mut drift = 0_usize;

    for harness in selected {
        match harness.kind {
            Kind::Discovery => continue,
            // Codex's catalog carries its own bundled entries beside this
            // router's lanes, so only a *missing* lane is drift: presence of the
            // lane is what makes `model` recognisable and gets the hosted
            // `web_search` tool stripped.
            Kind::Codex => {
                match codex_ids(home) {
                    Some(have) => {
                        let missing: Vec<&str> = wanted
                            .keys()
                            .filter(|id| !have.contains(**id))
                            .copied()
                            .collect();
                        if !missing.is_empty() {
                            drift += missing.len();
                            println!(
                                "codex: {} lane(s) missing from its model_catalog_json: {}",
                                missing.len(),
                                missing.join(", ")
                            );
                        }
                    }
                    None => println!("codex: no model_catalog_json in ~/.codex/config.toml, skipped"),
                }
                continue;
            }
            Kind::Static | Kind::Merged => {}
        }
        let Some(path) = &harness.path else {
            continue;
        };
        let Some(have) = current_models(harness, path) else {
            println!("{}: not installed on this host, skipped", harness.name);
            continue;
        };
        let missing: Vec<&str> = wanted
            .keys()
            .filter(|id| !have.contains_key(**id))
            .copied()
            .collect();
        let extra: Vec<&str> = have
            .keys()
            .filter(|id| !wanted.contains_key(id.as_str()))
            .map(String::as_str)
            .collect();
        let wrong: Vec<&str> = wanted
            .iter()
            .filter(|(id, context)| have.get(**id).is_some_and(|have| have != *context))
            .map(|(id, _)| *id)
            .collect();
        let mut findings: Vec<(&str, &Vec<&str>)> = vec![("missing", &missing)];
        // A merged list also carries lanes this catalog excludes on purpose
        // (dsh's decision models), so those ids are not drift there. Any other
        // unpublished id is a lane this router dropped, and `apply` removes it.
        let stale: Vec<&str> = match harness.kind {
            Kind::Static => extra.clone(),
            Kind::Merged => extra
                .iter()
                .copied()
                .filter(|id| !keep.contains(*id))
                .collect(),
            _ => Vec::new(),
        };
        if !stale.is_empty() {
            findings.push(("not on the router", &stale));
        }
        findings.push(("wrong window", &wrong));
        for (label, items) in findings {
            if !items.is_empty() {
                drift += items.len();
                println!("{}: {} {}: {}", harness.name, items.len(), label, items.join(", "));
            }
        }
        if harness.name == "jcode"
            && let Ok(text) = std::fs::read_to_string(path)
        {
            let duplicates = jcode_block_count(&text).saturating_sub(have.len());
            if duplicates > 0 {
                drift += duplicates;
                println!(
                    "jcode: {duplicates} duplicate model block(s) beside {} unique id(s)",
                    have.len()
                );
            }
        }
    }

    let no_window = windows_missing(cfg);
    if !no_window.is_empty() {
        drift += no_window.len();
        println!(
            "router: {} combo(s) declare no context_length, so no harness can list them: {}",
            no_window.len(),
            no_window.join(", ")
        );
    }

    if drift == 0 {
        println!("no drift: every harness list matches the router's combos");
        return Ok(());
    }
    Err(fail(
        format!("{drift} drift finding(s) across the harness lists"),
        "run `aroute harness apply` to write the router's combos into every harness list",
    ))
}

/// `aroute harness apply`: write every static list from the catalog.
fn apply(
    selected: &[&Harness],
    models: &[Model],
    keep: &BTreeSet<String>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mut changes = Vec::new();
    for harness in selected {
        if !matches!(harness.kind, Kind::Static | Kind::Merged) {
            continue;
        }
        let Some(path) = &harness.path else {
            continue;
        };
        if !path.exists() {
            println!("{}: not installed on this host, skipped", harness.name);
            continue;
        }
        let before = current_models(harness, path).unwrap_or_default();
        let (body, after) = rendered(harness, path, models, keep)?;
        // jcode is rewritten line by line, so the ids can match while the file
        // still carries duplicate blocks an earlier writer left behind. Comparing
        // the body is what makes those removable: a map comparison calls the file
        // clean and skips the write that would have fixed it.
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let body_differs = body.trim_end() != text.trim_end();
        if before == after && !body_differs {
            continue;
        }
        if before == after {
            // The ids can already agree while the file still needs the write:
            // jcode carries duplicate blocks a map cannot see, and dsh's rows
            // can be right in a document whose list this writer just normalised.
            changes.push(if harness.name == "jcode" {
                format!(
                    "{}: removed {} duplicate model block(s)",
                    harness.name,
                    jcode_block_count(&text).saturating_sub(after.len())
                )
            } else {
                format!("{}: normalized its list in place", harness.name)
            });
        }
        changes.extend(diff_lines(harness.name, &before, &after));
        if dry_run {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| fail(format!("could not create {}: {e}", parent.display()), "check the directory's permissions"))?;
        }
        backup(path)?;
        write_atomic(path, &body)?;
    }
    for line in &changes {
        println!("{line}");
    }
    if changes.is_empty() {
        println!("already in sync");
    } else if dry_run {
        println!("dry run: nothing was written");
    }
    Ok(())
}

/// The human lines describing what changed for one harness.
fn diff_lines(
    name: &str,
    before: &BTreeMap<String, u32>,
    after: &BTreeMap<String, u32>,
) -> Vec<String> {
    let mut out = Vec::new();
    let added: Vec<&str> = after
        .keys()
        .filter(|id| !before.contains_key(*id))
        .map(String::as_str)
        .collect();
    let removed: Vec<&str> = before
        .keys()
        .filter(|id| !after.contains_key(*id))
        .map(String::as_str)
        .collect();
    let changed: Vec<&str> = after
        .iter()
        .filter(|(id, window)| before.get(*id).is_some_and(|have| have != *window))
        .map(|(id, _)| id.as_str())
        .collect();
    if !added.is_empty() {
        out.push(format!("{name}: added {}", added.join(", ")));
    }
    if !removed.is_empty() {
        out.push(format!("{name}: removed {}", removed.join(", ")));
    }
    if !changed.is_empty() {
        out.push(format!("{name}: updated {}", changed.join(", ")));
    }
    out
}

/// The rewritten body and the ids/windows it now holds.
fn rendered(
    harness: &Harness,
    path: &Path,
    models: &[Model],
    keep: &BTreeSet<String>,
) -> anyhow::Result<(String, BTreeMap<String, u32>)> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| fail(format!("could not read {}: {e}", path.display()), "check the file's permissions"))?;
    match harness.name {
        "opencode" => {
            let mut doc: Value = serde_json::from_str(&text).map_err(|e| {
                fail(
                    format!("{} is not JSON: {e}", path.display()),
                    "fix the harness config by hand; a sync will not overwrite a file it cannot parse",
                )
            })?;
            let rows = render_opencode(&doc, models);
            let provider = doc
                .get_mut("provider")
                .and_then(Value::as_object_mut)
                .and_then(|providers| providers.get_mut(OPENCODE_PROVIDER))
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    fail(
                        format!("{} has no provider.{OPENCODE_PROVIDER} block", path.display()),
                        "point opencode at this router first, then re-run",
                    )
                })?;
            provider.insert("models".to_owned(), Value::Object(rows.clone()));
            let after = windows_from_object(&rows);
            let body = serde_json::to_string_pretty(&doc).map_err(|e| {
                fail(format!("the opencode config is not serialisable: {e}"), "report this")
            })?;
            Ok((body + "\n", after))
        }
        "jcode" => {
            let body = render_jcode(&text, models);
            Ok((body, windowed(models)))
        }
        "dsh" => {
            let body = render_dsh(&text, models, keep)?;
            let after = dsh_models(&body);
            Ok((body, after))
        }
        "pi" | "omo" => {
            let provider_name = if harness.name == "pi" { PI_PROVIDER } else { OMO_PROVIDER };
            let output_default = if harness.name == "pi" {
                PI_OUTPUT_DEFAULT
            } else {
                OMO_OUTPUT_DEFAULT
            };
            let mut doc: Value = serde_json::from_str(&text).map_err(|e| {
                fail(
                    format!("{} is not JSON: {e}", path.display()),
                    "fix the harness config by hand; a sync will not overwrite a file it cannot parse",
                )
            })?;
            let rows = {
                let provider = doc
                    .get_mut("providers")
                    .and_then(Value::as_object_mut)
                    .and_then(|providers| providers.get_mut(provider_name))
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| {
                        fail(
                            format!(
                                "{} has no providers.{provider_name} block",
                                path.display()
                            ),
                            "point the harness at this router first, then re-run",
                        )
                    })?;
                let current = provider
                    .get("models")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let rows = render_rows(&current, models, output_default);
                provider.insert("models".to_owned(), Value::Array(rows.clone()));
                rows
            };
            let after = windows_from_rows(&rows);
            let body = serde_json::to_string_pretty(&doc).map_err(|e| {
                fail(format!("the harness config is not serialisable: {e}"), "report this")
            })?;
            Ok((body + "\n", after))
        }
        other => Err(fail(
            format!("{other} has no renderer"),
            "this is a bug; report it",
        )),
    }
}

/// opencode's `models` map: one entry per model, preserving operator fields.
fn render_opencode(doc: &Value, models: &[Model]) -> Map<String, Value> {
    let current = doc
        .get("provider")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get(OPENCODE_PROVIDER))
        .and_then(Value::as_object)
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut out = Map::new();
    for model in models {
        let mut entry = current
            .get(&model.id)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut limit = entry
            .get("limit")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        limit.insert("context".to_owned(), json!(model.context));
        // The router publishes no output ceiling, so an operator's figure wins
        // and a new row is seeded with the window rather than a guess.
        if !limit.contains_key("output") {
            limit.insert("output".to_owned(), json!(model.context));
        }
        entry.insert("limit".to_owned(), Value::Object(limit));
        entry
            .entry("name".to_owned())
            .or_insert_with(|| json!(display_name(&model.id)));
        out.insert(model.id.clone(), Value::Object(entry));
    }
    out
}

/// pi/omo's `models` array: one object per model, preserving operator fields.
fn render_rows(current: &[Value], models: &[Model], output_default: u64) -> Vec<Value> {
    let existing: BTreeMap<&str, &Value> = current
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(|id| (id, row)))
        .collect();
    models
        .iter()
        .map(|model| {
            let mut row = existing
                .get(model.id.as_str())
                .and_then(|row| row.as_object())
                .cloned()
                .unwrap_or_default();
            row.insert("id".to_owned(), json!(model.id));
            row.insert("contextWindow".to_owned(), json!(model.context));
            row.entry("name".to_owned())
                .or_insert_with(|| json!(display_name(&model.id)));
            row.entry("maxTokens".to_owned())
                .or_insert_with(|| json!(output_default));
            Value::Object(row)
        })
        .collect()
}

/// jcode's `[[providers.aroute.models]]` blocks, with every other block kept.
///
/// The blocks are not contiguous — jcode interleaves other providers' blocks
/// between them — so replacing the span between the first and the last would
/// delete those. Each aroute block is removed where it stands and the new set is
/// appended after the file, which is where the previous writer put them.
fn render_jcode(text: &str, models: &[Model]) -> String {
    let header = format!("[[providers.{JCODE_PROVIDER}.models]]");
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() == header {
            // Drop the block's body, and leave whatever ended it — another
            // `[[…]]` header here — for the next iteration. Pushing that header
            // from inside this loop was the bug: the header and its body stayed,
            // so blocks written one after another (no blank line between them)
            // survived every apply and the appended set doubled them.
            index += 1;
            while index < lines.len() && !lines[index].trim_start().starts_with('[') {
                index += 1;
            }
            continue;
        }
        kept.push(lines[index]);
        index += 1;
    }

    let mut body = kept.join("\n");
    body = set_model_catalog_false(&body);
    while body.contains("\n\n\n") {
        body = body.replace("\n\n\n", "\n\n");
    }
    body = body.trim_end().to_owned();

    let blocks: Vec<String> = models
        .iter()
        .map(|model| {
            format!(
                "[[providers.{JCODE_PROVIDER}.models]]\nid = \"{}\"\ncontext_window = {}",
                model.id, model.context
            )
        })
        .collect();
    format!("{body}\n\n{}\n", blocks.join("\n\n"))
}

/// Forces `model_catalog = false` inside `[providers.aroute]`.
///
/// jcode otherwise refreshes the catalog from the router over HTTP and pulls
/// entries the static list deliberately excludes (decision lanes), which is the
/// one way a static list and a live one disagree in the same file.
fn set_model_catalog_false(text: &str) -> String {
    let header = format!("[providers.{JCODE_PROVIDER}]");
    let Some(start) = text.find(&header) else {
        return text.to_owned();
    };
    let after = start + header.len();
    let end = text[after..]
        .find("\n[")
        .map_or(text.len(), |offset| after + offset + 1);
    let section = &text[start..end];
    if !section.contains("model_catalog = true") {
        return text.to_owned();
    }
    let replaced = section.replace("model_catalog = true", "model_catalog = false");
    format!("{}{}{}", &text[..start], replaced, &text[end..])
}

/// dsh's provider list, merged row by row.
///
/// The patch layer is a hand-edited document — comments, `!!js` expressions and
/// unrelated plugin entries — so it is never parsed into a YAML value and
/// written back out. Only the `providers.<name>.models` sequence is touched:
/// a row the catalog publishes gets its `contextWindow` rewritten, a row it does
/// not publish (dsh's decision lanes) is copied through verbatim, and a catalog
/// id the list is missing is inserted before the first foreign row so the
/// router's own lanes stay contiguous.
fn render_dsh(
    text: &str,
    models: &[Model],
    keep: &BTreeSet<String>,
) -> anyhow::Result<String> {
    /// The id a list row starts with, if the chunk is a row at all.
    fn chunk_id(chunk: &[&str]) -> Option<String> {
        chunk.iter().find_map(|line| {
            line.trim()
                .strip_prefix("- id:")
                .map(|rest| rest.trim().trim_matches('"').to_owned())
        })
    }

    /// Appends a row for every catalog id the file does not hold at all.
    ///
    /// `emitted` alone is not enough: the rows after a foreign row are still
    /// coming, and inserting them here would duplicate every lane that a
    /// decision lane sits in front of. Only an id absent from the whole list is
    /// new.
    fn push_missing(
        out: &mut Vec<String>,
        wanted: &BTreeMap<&str, u32>,
        present: &BTreeSet<String>,
        emitted: &mut BTreeSet<String>,
        pad: &str,
        field_pad: &str,
    ) {
        for (id, window) in wanted {
            if present.contains(*id) || emitted.contains(*id) {
                continue;
            }
            emitted.insert((*id).to_owned());
            out.push(format!("{pad}- id: {id}"));
            out.push(format!("{field_pad}name: {id}"));
            out.push(format!("{field_pad}contextWindow: {window}"));
        }
    }

    /// The row with its window corrected, and its other fields left alone.
    fn with_window(chunk: &[&str], context: u32, field_pad: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(chunk.len() + 1);
        let mut wrote = false;
        for line in chunk {
            let trimmed = line.trim_start();
            if trimmed.starts_with("contextWindow:") {
                let indent = line.len() - trimmed.len();
                out.push(format!("{}contextWindow: {context}", " ".repeat(indent)));
                wrote = true;
            } else {
                out.push((*line).to_owned());
            }
        }
        if !wrote {
            let at = out
                .iter()
                .position(|line| line.trim_start().starts_with("- id:"))
                .map_or(out.len(), |index| index + 1);
            out.insert(at, format!("{field_pad}contextWindow: {context}"));
        }
        out
    }

    let wanted: BTreeMap<&str, u32> = models
        .iter()
        .map(|model| (model.id.as_str(), model.context))
        .collect();
    let lines: Vec<&str> = text.lines().collect();

    // Anchor on the `aroute:` provider and the `models:` under it. Patch entries
    // are inserted and reordered by hand, so no line number is stable.
    let mut anchor = None;
    let mut in_provider = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            in_provider = false;
            continue;
        }
        if trimmed == "aroute:" {
            in_provider = true;
            continue;
        }
        if in_provider && trimmed == "models:" {
            anchor = Some((index, indent));
            break;
        }
    }
    let Some((list_start, list_indent)) = anchor else {
        return Err(fail(
            "the dsh profile patch has no providers.aroute.models list",
            "point dsh at this router first, then re-run `aroute harness apply`",
        ));
    };

    // The list ends at the first line that dedents back to its own key's level.
    let mut list_end = lines.len();
    for (index, line) in lines.iter().enumerate().skip(list_start + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.len() - line.trim_start().len() <= list_indent {
            list_end = index;
            break;
        }
    }
    let region = &lines[list_start + 1..list_end];
    let entry_indent = region
        .iter()
        .find(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#') && trimmed.starts_with("- ")
        })
        .map_or(list_indent + 2, |line| line.len() - line.trim_start().len());
    let pad = " ".repeat(entry_indent);
    let field_pad = format!("{pad}  ");

    // Chunks rather than rows: comments and blank lines travel with the row they
    // precede, so an operator's note next to a decision lane survives too.
    let mut chunks: Vec<Vec<&str>> = Vec::new();
    for line in region {
        let trimmed = line.trim();
        let starts = !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && trimmed.starts_with("- ")
            && line.len() - line.trim_start().len() == entry_indent;
        if starts || chunks.is_empty() {
            chunks.push(vec![line]);
        } else if let Some(last) = chunks.last_mut() {
            last.push(line);
        }
    }

    let mut out: Vec<String> = lines[..=list_start]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
    let present: BTreeSet<String> = chunks.iter().filter_map(|chunk| chunk_id(chunk)).collect();
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut flushed = false;
    for chunk in &chunks {
        let id = chunk_id(chunk);
        let context = id.as_deref().and_then(|id| wanted.get(id).copied());
        match (id, context) {
            // A second row for an id already written is dropped: the list is a
            // set of lanes, and a duplicate only makes the picker ambiguous.
            (Some(id), Some(context)) => {
                if emitted.insert(id) {
                    out.extend(with_window(chunk, context, &field_pad));
                }
            }
            // A declared decision combo is preserved. Any other id the router
            // does not publish is a lane that was removed from `combos:` — the
            // same removal the other harnesses get, so dsh cannot keep
            // advertising a model that now 404s.
            (Some(id), None) => {
                if !keep.contains(&id) {
                    continue;
                }
                if !flushed {
                    push_missing(&mut out, &wanted, &present, &mut emitted, &pad, &field_pad);
                    flushed = true;
                }
                out.extend(chunk.iter().map(|line| (*line).to_owned()));
            }
            (None, _) => out.extend(chunk.iter().map(|line| (*line).to_owned())),
        }
    }
    if !flushed {
        push_missing(&mut out, &wanted, &present, &mut emitted, &pad, &field_pad);
    }
    out.extend(lines[list_end..].iter().map(|line| (*line).to_owned()));

    let mut body = out.join("\n");
    body.push('\n');
    Ok(body)
}

/// The ids/windows in a rendered opencode `models` map.
fn windows_from_object(rows: &Map<String, Value>) -> BTreeMap<String, u32> {
    rows.iter()
        .filter_map(|(id, entry)| {
            entry
                .get("limit")
                .and_then(|limit| limit.get("context"))
                .and_then(Value::as_u64)
                .and_then(|window| u32::try_from(window).ok())
                .map(|window| (id.clone(), window))
        })
        .collect()
}

/// The ids/windows in a rendered pi/omo `models` array.
fn windows_from_rows(rows: &[Value]) -> BTreeMap<String, u32> {
    rows.iter()
        .filter_map(|row| {
            let id = row.get("id")?.as_str()?.to_owned();
            let window = u32::try_from(row.get("contextWindow")?.as_u64()?).ok()?;
            Some((id, window))
        })
        .collect()
}

/// The catalog as a map, for the harnesses that render a whole list.
fn windowed(models: &[Model]) -> BTreeMap<String, u32> {
    models
        .iter()
        .map(|model| (model.id.clone(), model.context))
        .collect()
}

/// One harness's current ids/windows, or `None` when the file is absent.
fn current_models(harness: &Harness, path: &Path) -> Option<BTreeMap<String, u32>> {
    let text = std::fs::read_to_string(path).ok()?;
    match harness.name {
        "opencode" => {
            let doc: Value = serde_json::from_str(&text).ok()?;
            let rows = doc
                .get("provider")?
                .get(OPENCODE_PROVIDER)?
                .get("models")?
                .as_object()?;
            Some(windows_from_object(rows))
        }
        "pi" => {
            let doc: Value = serde_json::from_str(&text).ok()?;
            let rows = doc.get("providers")?.get(PI_PROVIDER)?.get("models")?.as_array()?;
            Some(windows_from_rows(rows))
        }
        "omo" => {
            let doc: Value = serde_json::from_str(&text).ok()?;
            let rows = doc.get("providers")?.get(OMO_PROVIDER)?.get("models")?.as_array()?;
            Some(windows_from_rows(rows))
        }
        "jcode" => Some(jcode_models(&text)),
        "dsh" => Some(dsh_models(&text)),
        _ => None,
    }
}

/// The `(id, context_window)` pairs inside jcode's aroute model blocks.
fn jcode_models(text: &str) -> BTreeMap<String, u32> {
    let header = format!("[[providers.{JCODE_PROVIDER}.models]]");
    let mut out = BTreeMap::new();
    let mut id: Option<String> = None;
    let mut in_block = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == header {
            id = None;
            in_block = true;
            continue;
        }
        if in_block && trimmed.starts_with('[') {
            in_block = false;
            continue;
        }
        if !in_block {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("id = ") {
            id = Some(rest.trim_matches('"').to_owned());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("context_window = ")
            && let (Some(id), Ok(window)) = (&id, rest.trim().parse::<u32>())
        {
            out.insert(id.clone(), window);
        }
    }
    out
}

/// The number of `[[providers.aroute.models]]` headers in a jcode config.
///
/// A file can hold more blocks than unique ids when an older writer appended a
/// second set; [`jcode_models`] returns a map, so the duplicates are invisible
/// without counting the headers as well.
fn jcode_block_count(text: &str) -> usize {
    let header = format!("[[providers.{JCODE_PROVIDER}.models]]");
    text.lines().filter(|line| line.trim() == header).count()
}

/// The `(id, contextWindow)` pairs in dsh's provider list.
///
/// dsh's profile patch layer is a YAML patch document, not a config this router
/// owns: it mixes unrelated plugin entries, comments and `!!js` expressions, so
/// it is merged in place rather than re-serialised. The list lives under
/// `providers.<name>.models` of the `llm-pi-ai` plugin entry — the same shape pi
/// uses — and is found by anchoring on the `aroute:` provider key rather than a
/// line number, because the patch is edited by hand between syncs.
fn dsh_models(text: &str) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    let mut in_provider = false;
    let mut list_indent = 0_usize;
    let mut in_list = false;
    let mut id: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "ar:" || trimmed == "aroute:" {
            in_provider = true;
            in_list = false;
            continue;
        }
        // A top-level list entry starts the next patch entry, which ends the
        // provider block this scan was inside.
        if indent == 0 {
            in_provider = false;
            in_list = false;
            continue;
        }
        if in_provider && trimmed == "models:" {
            in_list = true;
            list_indent = indent;
            id = None;
            continue;
        }
        if !in_list {
            continue;
        }
        if indent <= list_indent {
            in_list = false;
            id = None;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- id:") {
            id = Some(rest.trim().to_owned());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("contextWindow:")
            && let (Some(id), Ok(window)) = (&id, rest.trim().parse::<u32>())
        {
            out.insert(id.clone(), window);
        }
    }
    out
}

/// The lane ids in Codex's generated `model_catalog_json`.
///
/// `None` when Codex is not configured against this router, which is reported
/// rather than treated as drift: the file is written by `aroute codex`, and a
/// host without it has simply never run that verb.
fn codex_ids(home: &Path) -> Option<BTreeSet<String>> {
    let config = std::fs::read_to_string(home.join(".codex/config.toml")).ok()?;
    let path = config
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("model_catalog_json"))?;
    let path = path.trim_start().strip_prefix('=')?.trim().trim_matches('"');
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let models = doc.get("models")?;
    // Codex writes `models` as an array of entries carrying `slug`; older
    // catalogs keyed the same entries by id, so both shapes are read.
    let entries: Vec<&Value> = match models {
        Value::Array(entries) => entries.iter().collect(),
        Value::Object(entries) => entries.values().collect(),
        _ => return None,
    };
    Some(
        entries
            .iter()
            .filter_map(|entry| {
                entry
                    .get("slug")
                    .or_else(|| entry.get("id"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned)
            .collect(),
    )
}

/// Copies the file aside before a rewrite, over the previous sync's backup.
fn backup(path: &Path) -> anyhow::Result<()> {
    let target = PathBuf::from(format!("{}{BACKUP_SUFFIX}", path.display()));
    std::fs::copy(path, &target)
        .map(|_| ())
        .map_err(|e| fail(format!("could not back up {}: {e}", path.display()), "check the directory's permissions"))
}

/// Writes `body` to `path` through a temp file in the same directory, then a
/// rename, so a harness never reads a half-written list.
fn write_atomic(path: &Path, body: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| fail("the harness path has no parent directory", "report this"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("models");
    let temp = parent.join(format!(".{name}.tmp"));
    std::fs::write(&temp, body)
        .map_err(|e| fail(format!("could not write {}: {e}", temp.display()), "check the directory's permissions"))?;
    std::fs::rename(&temp, path)
        .map_err(|e| fail(format!("could not move {} into place: {e}", temp.display()), "check the directory's permissions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decision lanes a merged harness may keep.
    fn decisions() -> BTreeSet<String> {
        ["jev".to_owned()].into()
    }

    fn models() -> Vec<Model> {
        vec![
            Model {
                id: "large-stack".to_owned(),
                context: 1_048_576,
            },
            Model {
                id: "mistral-1t".to_owned(),
                context: 1_048_576,
            },
        ]
    }

    #[test]
    fn render_jcode_keeps_other_provider_blocks_when_rewriting() {
        let text = "[providers.aroute]\nmodel_catalog = true\n\n\
                    [[providers.pgsgrove.models]]\nid = \"keep-me\"\n\n\
                    [[providers.aroute.models]]\nid = \"stale\"\ncontext_window = 1\n";
        let out = render_jcode(text, &models());
        assert!(out.contains("[[providers.pgsgrove.models]]\nid = \"keep-me\""));
    }

    #[test]
    fn render_jcode_removes_blocks_that_touch_each_other() {
        let text = "[[providers.aroute.models]]\nid = \"stale\"\ncontext_window = 1\n\
                    [[providers.aroute.models]]\nid = \"also-stale\"\ncontext_window = 1\n";
        let out = render_jcode(text, &models());
        assert_eq!(jcode_block_count(&out), 2);
    }

    #[test]
    fn render_jcode_disables_live_catalog_when_it_was_enabled() {
        let text = "[providers.aroute]\nmodel_catalog = true\n";
        let out = render_jcode(text, &models());
        assert!(out.contains("model_catalog = false"));
    }

    #[test]
    fn render_opencode_preserves_operator_output_limit_when_window_changes() {
        let doc: Value = serde_json::from_str(
            "{\"provider\":{\"ar\":{\"models\":{\"large-stack\":{\"name\":\"Large\",\"limit\":{\"context\":1,\"output\":32768}}}}}}",
        )
        .expect("fixture is JSON");
        let rows = render_opencode(&doc, &models());
        assert_eq!(rows["large-stack"]["limit"]["output"], json!(32768));
    }

    #[test]
    fn render_rows_seeds_output_ceiling_when_the_harness_has_none() {
        let rows = render_rows(&[], &models(), OMO_OUTPUT_DEFAULT);
        assert_eq!(rows[1]["maxTokens"], json!(OMO_OUTPUT_DEFAULT));
    }

    #[test]
    fn dsh_models_reads_only_the_provider_list_when_the_patch_has_other_entries() {
        let text = "\
- id: agent-default-model
  config:
    provider: aroute
    model: step-5-preview
\
- id: llm-pi-ai
  config:
    providers:
      aroute:
        models:
          - id: large-stack
            contextWindow: 1048576
          - id: jev
            contextWindow: 128000
";
        let models = dsh_models(text);
        assert_eq!(models.get("jev"), Some(&128_000));
    }

    /// A patch layer shaped like the real one: comments, unrelated patch
    /// entries on both sides of the provider, and a decision lane in the list.
    fn dsh_patch() -> &'static str {
        "# patch layer comment\n- id: agent-default-model\n  config:\n    provider: aroute\n- id: llm-pi-ai\n  config:\n    providers:\n      aroute:\n        baseURL: https://example.invalid/v1\n        models:\n          - id: large-stack\n            name: large-stack\n            contextWindow: 1\n          - id: jev\n            name: jev\n            contextWindow: 128000\n- id: ui-settings-general\n  config:\n    welcomeNoticeVersion: 1\n"
    }

    #[test]
    fn render_dsh_corrects_a_window_without_touching_the_row() {
        let out = render_dsh(dsh_patch(), &models(), &decisions())
            .expect("fixture has a provider list");
        assert_eq!(dsh_models(&out).get("large-stack"), Some(&1_048_576));
    }

    #[test]
    fn render_dsh_keeps_a_decision_lane_the_catalog_excludes() {
        let out = render_dsh(dsh_patch(), &models(), &decisions())
            .expect("fixture has a provider list");
        assert_eq!(dsh_models(&out).get("jev"), Some(&128_000));
    }

    #[test]
    fn render_dsh_inserts_a_new_lane_before_the_foreign_rows() {
        let out = render_dsh(dsh_patch(), &models(), &decisions())
            .expect("fixture has a provider list");
        let added = out.find("- id: mistral-1t").expect("the missing lane is added");
        let foreign = out.find("- id: jev").expect("the decision lane is kept");
        assert!(added < foreign);
    }

    #[test]
    fn render_dsh_does_not_duplicate_a_lane_that_sits_after_a_foreign_row() {
        let text = "- id: llm-pi-ai\n  config:\n    providers:\n      aroute:\n        models:\n          - id: jev\n            contextWindow: 128000\n          - id: large-stack\n            contextWindow: 1\n";
        let out = render_dsh(text, &models(), &decisions())
            .expect("fixture has a provider list");
        assert_eq!(out.matches("- id: large-stack").count(), 1);
    }

    #[test]
    fn render_dsh_leaves_the_rest_of_the_patch_document_alone() {
        let out = render_dsh(dsh_patch(), &models(), &decisions())
            .expect("fixture has a provider list");
        assert!(out.starts_with("# patch layer comment\n- id: agent-default-model\n"));
    }

    #[test]
    fn render_dsh_is_idempotent_when_the_list_already_matches() {
        let once = render_dsh(dsh_patch(), &models(), &decisions())
            .expect("fixture has a provider list");
        let twice = render_dsh(&once, &models(), &decisions())
            .expect("rendered list still has the provider");
        assert_eq!(once, twice);
    }

    #[test]
    fn render_dsh_drops_a_row_the_router_no_longer_publishes() {
        let text = "- id: llm-pi-ai\n  config:\n    providers:\n      aroute:\n        models:\n          - id: large-stack\n            contextWindow: 1048576\n          - id: retired-lane\n            contextWindow: 1\n";
        let out = render_dsh(text, &models(), &decisions())
            .expect("fixture has a provider list");
        assert!(!out.contains("retired-lane"));
    }

    #[test]
    fn jcode_block_count_counts_duplicates_the_map_hides() {
        let text = "[[providers.aroute.models]]\nid = \"a\"\ncontext_window = 1\n\n\
                    [[providers.aroute.models]]\nid = \"a\"\ncontext_window = 1\n";
        assert_eq!(jcode_block_count(text), 2);
    }

    #[test]
    fn catalog_excludes_combo_when_it_declares_no_window() {
        let cfg = ar_config::Config::parse(
            "combos:\n  - id: with-window\n    strategy: priority\n    context_length: 1000\n    \
             targets:\n      - openai/gpt\n  - id: no-window\n    strategy: priority\n    \
             targets:\n      - openai/gpt\n",
            |_| Ok(None),
        )
        .expect("fixture config parses");
        assert_eq!(catalog(&cfg).len(), 1);
    }
}
