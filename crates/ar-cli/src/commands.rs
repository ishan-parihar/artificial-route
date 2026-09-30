//! Command handlers.
//!
//! Every handler writes data to stdout and returns `anyhow::Result`, per
//! `docs/03` (`anyhow` is confined to the binary) and `docs/06` (errors are
//! structured on stdout with an exact `help:` line, never a stack trace).
//!
//! The two async verbs live in [`crate::serve`]; everything here is synchronous
//! so the whole surface stays testable in-process without a runtime.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;

use ar_config::{Combo, Config, ProviderCfg};
use ar_registry::global as registry;
use clap::error::ErrorKind;
use clap::CommandFactory;

use crate::cli::{Cli, Command, ConfigureArgs, ImportArgs, ListArgs};
use crate::import::ImportFrom;
use crate::import;
use crate::serve;
use crate::toon;

/// One TOON row: positional cells, matching the command's `*_COLUMNS`.
type Row = Vec<String>;

/// Columns available from `ar providers`.
///
/// `id,provider,status` must stay first: `toon::DEFAULT_FIELDS` is applied
/// positionally, and `default_columns_lead` below is what keeps that honest.
const PROVIDER_COLUMNS: [&str; 5] = ["id", "provider", "status", "base_url", "key"];
/// Columns available from `ar models`.
const MODEL_COLUMNS: [&str; 4] = ["id", "provider", "status", "combo"];
/// Columns available from `ar combo`. `provider` is the combo's distinct
/// provider set: a combo is not owned by one provider, but that set is the fact
/// an agent asks for next.
const COMBO_COLUMNS: [&str; 5] = ["id", "provider", "status", "strategy", "targets"];
/// Columns of a `doctor` finding.
const CHECK_COLUMNS: [&str; 3] = ["check", "status", "detail"];
/// Columns of the `configure` dump.
const SETTING_COLUMNS: [&str; 2] = ["key", "value"];
/// Columns of an `import` row. `path` is the directory the pair was written to:
/// the two files are one outcome, so one column reports where to find it.
const IMPORT_COLUMNS: [&str; 4] = ["id", "provider", "status", "path"];

/// Builds an error carrying its own `help:` line, per `docs/06`.
pub fn fail(message: impl std::fmt::Display, help: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{message}\nhelp: {help}")
}

/// Rejects unknown `--fields` values instead of silently dropping columns.
fn columns(requested: &[String], all: &[&str], cmd: &str) -> anyhow::Result<Vec<String>> {
    for f in requested {
        anyhow::ensure!(
            all.contains(&f.as_str()),
            "{}",
            fail(
                format!("unknown field {f:?} for `ar {cmd}`"),
                format!("use --fields with any of: {}", all.join(", ")),
            )
        );
    }
    Ok(requested.to_vec())
}

/// Loads the config, translating a load failure into a `help:` line.
///
/// The help is the exact fix because the overwhelmingly common cause is an unset
/// `$VAR`; naming the env var saves a round trip.
pub fn load(cli: &Cli) -> anyhow::Result<Config> {
    ar_config::ConfigHandle::load(&cli.config)
        .map(|h| h.snapshot().as_ref().clone())
        .map_err(|e| {
            let help = format!(
                "check {} exists and every $VAR it references is exported, or pass --config <PATH>",
                cli.config.display()
            );
            fail(e, help)
        })
}

/// Runs `future` on a tokio runtime built on demand.
///
/// Built here rather than by `#[tokio::main]` so the five local verbs never pay
/// for a runtime: `--version`, the home view and the lists answer without one.
pub fn block_on<F: std::future::Future<Output = anyhow::Result<()>>>(future: F) -> anyhow::Result<()> {
    block_on_value(future)
}

/// [`block_on`] for a verb that needs the result — `ar import`'s single fetch.
pub fn block_on_value<F, T>(future: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| fail(e, "the async runtime could not start; this is a host problem, not a config one"))?;
    rt.block_on(future)
}

/// Dispatches a parsed command.
pub fn dispatch(cli: &Cli) -> anyhow::Result<()> {
    match &cli.command {
        None => home(cli),
        Some(Command::Serve(a)) => block_on(serve::serve(cli, a)),
        Some(Command::Models(a)) => models(cli, a),
        Some(Command::Providers(a)) => providers(cli, a),
        Some(Command::Combo(a)) => combo(cli, a),
        Some(Command::Doctor) => doctor(cli),
        Some(Command::Run(a)) => block_on(serve::run(cli, a)),
        Some(Command::Configure(a)) => configure(cli, a),
        Some(Command::Import(a)) => import_config(a),
    }
}

/// Bare `ar`: content-first home view, not a manual (docs/06).
///
/// Deliberately not the combo table's full width: the point of a home view is
/// the three facts a caller cannot guess — where the binary is, what it is, and
/// whether it is configured at all.
fn home(cli: &Cli) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    println!("bin: {}", self_path());
    println!(
        "ar {} — one OpenAI-compatible endpoint, many providers",
        crate::version::VERSION
    );
    println!("config: {}", cli.config.display());
    println!("listen: {}:{}", cfg.server.host, cfg.server.port);
    println!("registry: {} providers", registry().len());
    print!(
        "{}",
        toon::list("combos", "combos", &COMBO_COLUMNS, &toon::fields_or_default(None), &combo_rows(&cfg.combos), true)
    );
    println!("next:");
    println!("  ar serve      start the proxy");
    println!("  ar run -p     one completion through the router");
    println!("  ar doctor     check config, credentials, registry");
    Ok(())
}

/// This executable's path, for the home view's `bin:` line.
///
/// `current_exe` fails on a deleted or unlinked binary, which is not worth an
/// error: the caller still wants the rest of the view.
fn self_path() -> String {
    std::env::current_exe().map_or_else(|_| "ar (path unavailable)".to_owned(), |p| p.display().to_string())
}

fn models(cli: &Cli, args: &ListArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let fields = columns(&toon::fields_or_default(args.fields.as_deref()), &MODEL_COLUMNS, "models")?;
    print!(
        "{}",
        toon::list("models", "models", &MODEL_COLUMNS, &fields, &model_rows(&cfg), args.full)
    );
    Ok(())
}

/// Routable models: every combo target, plus anything the registry declares.
///
/// Combo targets come first because those are what this config can actually
/// route; the registry's own model list is appended so `ar models` also answers
/// "what else could I route to". A target's status names the half that failed.
fn model_rows(cfg: &Config) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for c in &cfg.combos {
        for t in &c.targets {
            let (provider, model) = split_target(t);
            let status = match registry().get(provider) {
                None => "unknown-provider",
                Some(_) if !model_known(provider, model) => "unknown-model",
                Some(_) => "routable",
            };
            match rows.iter_mut().find(|r| r[0] == *t) {
                // A model two combos share is one model, not two rows: `count`
                // is the number an agent branches on, and a duplicated row makes
                // it wrong. The owning combos join into one cell instead.
                Some(existing) => {
                    let owned = &mut existing[3];
                    if !owned.split('+').any(|name| name == c.id.as_str()) {
                        *owned = format!("{owned}+{}", c.id);
                    }
                }
                None => rows.push(vec![t.clone(), provider.to_owned(), status.to_owned(), c.id.clone()]),
            }
        }
    }
    for (provider_id, def) in registry().iter() {
        for model in &def.models {
            let id = format!("{provider_id}/{model}");
            if !rows.iter().any(|r| r[0] == id) {
                rows.push(vec![id, provider_id.to_string(), "known".to_owned(), "-".to_owned()]);
            }
        }
    }
    rows.sort();
    rows
}

/// Splits a `provider/model` target.
///
/// A target with no slash yields an empty model and the whole string as the
/// provider, which `doctor` then reports as unroutable rather than this function
/// silently accepting it.
pub fn split_target(target: &str) -> (&str, &str) {
    target.split_once('/').map_or((target, ""), |(p, m)| (p, m))
}

/// The provider half of a `provider/model` target.
fn target_provider(target: &str) -> &str {
    split_target(target).0
}

fn providers(cli: &Cli, args: &ListArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let fields = columns(&toon::fields_or_default(args.fields.as_deref()), &PROVIDER_COLUMNS, "providers")?;
    print!(
        "{}",
        toon::list("providers", "providers", &PROVIDER_COLUMNS, &fields, &provider_rows(&cfg.providers), args.full)
    );
    Ok(())
}

/// One provider row: `id`, the registry's wire dialect, and whether the config
/// resolves to a known provider.
///
/// `not-in-registry` is the useful case: it catches a typo in `config.yaml`
/// that would otherwise only surface as a failed upstream call.
fn provider_rows(providers: &[ProviderCfg]) -> Vec<Row> {
    let mut rows: Vec<Row> = providers
        .iter()
        .map(|p| {
            let def = registry().get(&p.id);
            let dialect = def.map_or("unknown", |d| d.wire_format.as_str());
            let base_url = def.map_or("-", |d| d.base_url.as_str());
            let status = if def.is_some() { "ok" } else { "not-in-registry" };
            vec![p.id.clone(), dialect.to_owned(), status.to_owned(), base_url.to_owned(), p.key.clone()]
        })
        .collect();
    rows.sort();
    rows
}

fn combo(cli: &Cli, args: &ListArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let fields = columns(&toon::fields_or_default(args.fields.as_deref()), &COMBO_COLUMNS, "combo")?;
    print!(
        "{}",
        toon::list("combo", "combos", &COMBO_COLUMNS, &fields, &combo_rows(&cfg.combos), args.full)
    );
    Ok(())
}

/// One combo row: distinct providers, strategy, and the full target chain.
///
/// `+` not `,`: rows are comma-delimited, so a comma-separated set would make
/// this row one cell wider than the header claims.
fn combo_rows(combos: &[Combo]) -> Vec<Row> {
    let mut rows: Vec<Row> = combos
        .iter()
        .map(|c| {
            let providers: BTreeSet<&str> = c.targets.iter().map(|t| target_provider(t)).collect();
            let status = if c.targets.is_empty() { "no-targets" } else { "active" };
            vec![
                c.id.clone(),
                providers.into_iter().collect::<Vec<_>>().join("+"),
                status.to_owned(),
                c.strategy.as_str().to_owned(),
                c.targets.join("+"),
            ]
        })
        .collect();
    rows.sort();
    rows
}

/// Reports config, credentials and registry state, and exits 1 on any failure.
///
/// Secret *values* are never printed; only whether each key name resolved, so
/// this is safe to paste into a bug report. Getting here already proves every
/// `$VAR` expanded, because `ar-config` refuses to load otherwise — so a "key"
/// row reports the binding, not a re-check that can never fail.
fn doctor(cli: &Cli) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let checks = findings(&cfg, &cli.config.display().to_string());
    print!(
        "{}",
        toon::list("checks", "checks", &CHECK_COLUMNS, &toon::every_field(&CHECK_COLUMNS), &checks, false)
    );
    let failed = checks.iter().filter(|r| r[1] == "fail").count();
    if failed > 0 {
        return Err(fail(
            format!("{failed} check(s) failed"),
            "fix the `fail` rows above; `ar configure --check` prints the resolved settings",
        ));
    }
    Ok(())
}

/// One row per thing that can be wrong, as `(check, status, detail)`.
///
/// Shared by `doctor` (prints them) and `configure --check` (reads them), so the
/// two can never disagree about whether a config is valid.
fn findings(cfg: &Config, path: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = vec![
        vec!["config".to_owned(), "ok".to_owned(), format!("{path} ({} providers, {} combos)", cfg.providers.len(), cfg.combos.len())],
        vec!["registry".to_owned(), "ok".to_owned(), format!("{} providers compiled in", registry().len())],
    ];

    if cfg.combos.is_empty() {
        rows.push(vec![
            "combos".to_owned(),
            "fail".to_owned(),
            "none configured; no model is routable".to_owned(),
        ]);
    }

    for p in &cfg.providers {
        let status = if registry().get(&p.id).is_some() { "ok" } else { "fail" };
        rows.push(vec![
            format!("provider/{}", p.id),
            status.to_owned(),
            if status == "ok" { "in registry".to_owned() } else { "not in registry".to_owned() },
        ]);
    }

    let mut names: Vec<&str> = cfg.keys.keys().map(String::as_str).collect();
    names.sort_unstable();
    for name in names {
        let users: Vec<&str> = cfg.providers.iter().filter(|p| p.key == name).map(|p| p.id.as_str()).collect();
        rows.push(vec![
            format!("key/{name}"),
            "ok".to_owned(),
            if users.is_empty() {
                "resolved but unused".to_owned()
            } else {
                format!("resolved for {}", users.join("+"))
            },
        ]);
    }

    let targets: BTreeSet<&str> = cfg.combos.iter().flat_map(|c| c.targets.iter().map(String::as_str)).collect();
    for t in targets {
        let (provider, model) = split_target(t);
        let (status, detail) = if registry().get(provider).is_none() {
            ("fail", "provider not in registry")
        } else if !cfg.providers.iter().any(|p| p.id == provider) {
            ("fail", "provider not configured")
        } else if model.is_empty() {
            ("fail", "target names no model; write it as provider/model")
        } else if !model_known(provider, model) {
            // The provider half passing is not enough: a typo in the model half
            // survives every provider check and only surfaces as a 404 from the
            // upstream, which reads as "the proxy is broken".
            ("fail", "model not in registry; run `ar import --from omniroute --path <OmniRoute/open-sse/config/providers>` or `ar models`")
        } else {
            ("ok", "routable")
        };
        rows.push(vec![format!("target/{t}"), status.to_owned(), detail.to_owned()]);
    }

    rows
}

/// Whether the catalog lists `model` under `provider`.
///
/// A provider with no model list of its own is accepted: those are the
/// passthrough providers whose catalog upstream is empty and the real list comes
/// from live discovery, so failing them here would report an unreachable
/// provider as a typo.
fn model_known(provider: &str, model: &str) -> bool {
    let Some(def) = registry().get(provider) else { return false };
    def.models.is_empty() || def.models.iter().any(|m| m.as_ref() == model)
}

/// Prints the effective configuration: defaults, file contents, and where each
/// credential came from — with no secret values.
///
/// `--check` turns a failing check into exit 1, which is what makes this usable
/// as a CI gate over a config file.
fn configure(cli: &Cli, args: &ConfigureArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    print!(
        "{}",
        toon::list(
            "settings",
            "settings",
            &SETTING_COLUMNS,
            &toon::every_field(&SETTING_COLUMNS),
            &setting_rows(&cfg, &cli.config.display().to_string()),
            true,
        )
    );

    let checks = findings(&cfg, &cli.config.display().to_string());
    let failed = checks.iter().filter(|r| r[1] == "fail").count();
    if args.check && failed > 0 {
        return Err(fail(
            format!("{failed} check(s) failed"),
            "run `ar doctor` for the per-check detail",
        ));
    }
    Ok(())
}

/// The config as the loader resolved it, one `key`/`value` pair per line.
///
/// Values are pre-`$VAR`-expansion, so this answers the question a reader of the
/// YAML cannot: what did the process actually end up with.
fn setting_rows(cfg: &Config, path: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = vec![
        vec!["config.path".to_owned(), path.to_owned()],
        vec!["server.host".to_owned(), cfg.server.host.clone()],
        vec!["server.port".to_owned(), cfg.server.port.to_string()],
        vec!["registry.providers".to_owned(), registry().len().to_string()],
        vec!["providers".to_owned(), cfg.providers.len().to_string()],
        vec!["combos".to_owned(), cfg.combos.len().to_string()],
    ];
    for p in &cfg.providers {
        let base_url = registry().get(&p.id).map_or_else(|| "-".to_owned(), |d| d.base_url.clone());
        rows.push(vec![format!("provider.{}.key", p.id), p.key.clone()]);
        rows.push(vec![format!("provider.{}.base_url", p.id), base_url]);
    }
    for c in &cfg.combos {
        rows.push(vec![format!("combo.{}.strategy", c.id), c.strategy.as_str().to_owned()]);
        rows.push(vec![format!("combo.{}.targets", c.id), c.targets.join("+")]);
    }
    rows
}

/// Converts another tool's config into `config.yaml` + `registry.json`.
///
/// Both files are written before anything is printed, so a partial failure
/// never leaves a table claiming a conversion that did not land.
fn import_config(args: &ImportArgs) -> anyhow::Result<()> {
    let result = match args.from {
        ImportFrom::Omniroute => import::omniroute::scan(import::upstream_tree(args.path.as_deref())?)?,
        ImportFrom::Litellm => import::convert(args.from, &import::upstream(args.path.as_deref())?)?,
    };

    let out = Path::new(&args.out_dir);
    std::fs::create_dir_all(out).map_err(|e| {
        fail(
            format!("cannot create {}: {e}", out.display()),
            "pass --out-dir <DIR> at a path you can write",
        )
    })?;
    let written = out.display().to_string();
    write_file(&out.join("config.yaml"), &result.config_yaml)?;
    write_file(&out.join("registry.json"), &import::to_catalog_json(&result.registry)?)?;

    print!(
        "{}",
        toon::list(
            "imports",
            "imports",
            &IMPORT_COLUMNS,
            &toon::every_field(&IMPORT_COLUMNS),
            &import::rows(&result, &written),
            false,
        )
    );
    Ok(())
}

/// Writes one generated file, naming the path rather than the errno alone.
fn write_file(path: &Path, body: &str) -> anyhow::Result<()> {
    std::fs::write(path, body).map_err(|e| {
        fail(
            format!("cannot write {}: {e}", path.display()),
            "pass --out-dir <DIR> at a path you can write",
        )
    })
}

/// Maps a clap parse failure onto the `docs/06` exit-code contract: `2` for
/// usage errors, `0` for `--help` and `--version`.
pub fn usage_exit_code(kind: ErrorKind) -> u8 {
    match kind {
        ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand | ErrorKind::DisplayVersion => 0,
        _ => 2,
    }
}

/// Validates a `--fields` selection for whichever command was parsed.
///
/// Returns the failure text, if any. `main` calls this before dispatch so a bad
/// column name exits 2 (misuse) rather than 1: clap cannot check it, because
/// the valid set depends on the command, and `docs/06` puts "invalid argument"
/// on the usage side of the line.
pub fn check_fields(cli: &Cli) -> Option<String> {
    let (args, all, cmd) = match &cli.command {
        Some(Command::Models(a)) => (a, &MODEL_COLUMNS[..], "models"),
        Some(Command::Providers(a)) => (a, &PROVIDER_COLUMNS[..], "providers"),
        Some(Command::Combo(a)) => (a, &COMBO_COLUMNS[..], "combo"),
        _ => return None,
    };
    columns(&toon::fields_or_default(args.fields.as_deref()), all, cmd)
        .err()
        .map(|e| e.to_string())
}

/// The loud hint `docs/06` requires on an unknown flag: the offending name plus
/// the flags that command actually accepts.
///
/// The subcommand is found by matching a known name, not by taking the first
/// non-flag token — `ar --config path models --nope` would read `path` as the
/// subcommand that way. Globals (`--config`) reach every subcommand, and clap
/// already folded them into each one, so the list below is complete.
pub fn unknown_flag_hint(args: &[OsString], invalid: &str) -> String {
    let root = Cli::command();
    let sub = args.iter().filter_map(|a| a.to_str()).find_map(|tok| root.find_subcommand(tok));

    // `Arg::get_long` is the bare name, without the dashes.
    let mut flags: Vec<String> = sub
        .map(|s| s.get_arguments())
        .into_iter()
        .flatten()
        .chain(root.get_arguments())
        .filter_map(clap::Arg::get_long)
        .map(|l| format!("--{l}"))
        .collect();
    flags.push("--help".to_owned());
    flags.sort_unstable();
    flags.dedup();

    format!("unknown flag {invalid:?}\nhelp: valid flags: {}", flags.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_columns_lead_with_toon_defaults() {
        for cols in [PROVIDER_COLUMNS.as_slice(), MODEL_COLUMNS.as_slice(), COMBO_COLUMNS.as_slice()] {
            assert_eq!(&cols[..3], &toon::DEFAULT_FIELDS[..], "{cols:?}");
        }
    }

    #[test]
    fn splits_provider_from_target() {
        assert_eq!(target_provider("openai/gpt-5.4"), "openai");
    }

    #[test]
    fn treats_bare_target_as_provider() {
        assert_eq!(target_provider("openai"), "openai");
    }

    #[test]
    fn flags_unknown_field_with_valid_list() {
        let e = columns(&["nope".to_owned()], &PROVIDER_COLUMNS, "providers").unwrap_err().to_string();
        assert!(e.contains("unknown field \"nope\""), "{e}");
        assert!(e.contains("id, provider, status"), "{e}");
    }

    #[test]
    fn marks_combo_with_no_targets() {
        let rows = combo_rows(&[Combo {
            id: "empty".to_owned(),
            strategy: ar_config::Strategy::Priority,
            targets: vec![],
        }]);
        assert_eq!(rows[0][2], "no-targets");
    }

    #[test]
    fn fails_check_when_combo_target_provider_absent() {
        let cfg = Config::parse(
            "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - groq/llama\n",
            |_| Ok(Some("v".to_owned())),
        )
        .unwrap();
        let checks = findings(&cfg, "config.yaml");
        let failed: Vec<&Row> = checks.iter().filter(|r| r[1] == "fail").collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0][0], "target/groq/llama");
    }

    #[test]
    fn fails_check_when_model_half_is_unknown() {
        // The provider half passing is not enough: a model typo otherwise only
        // surfaces as a 404 from the upstream.
        let cfg = Config::parse(
            "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4-nope\n",
            |_| Ok(Some("v".to_owned())),
        )
        .unwrap();
        let checks = findings(&cfg, "config.yaml");
        let row = checks.iter().find(|r| r[0] == "target/openai/gpt-5.4-nope").expect("the target row");
        assert_eq!(row[1], "fail");
        assert!(row[2].contains("model not in registry"), "{row:?}");
        assert!(row[2].contains("ar import"), "the help names the fix: {row:?}");
    }

    #[test]
    fn passes_check_when_model_half_is_known() {
        let cfg = Config::parse(
            "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: cost-optimized\n    targets:\n      - openai/gpt-5.4-nano\n",
            |_| Ok(Some("v".to_owned())),
        )
        .unwrap();
        let checks = findings(&cfg, "config.yaml");
        assert!(
            !checks.iter().any(|r| r[1] == "fail"),
            "{checks:?}"
        );
    }

    #[test]
    fn lists_valid_flags_for_named_subcommand() {
        let hint = unknown_flag_hint(&[OsString::from("--config"), OsString::from("c.yaml"), OsString::from("models")], "--nope");
        assert!(hint.contains("--fields"), "{hint}");
        assert!(!hint.contains("--prompt"), "{hint}");
    }

    #[test]
    fn dedupes_model_shared_by_two_combos() {
        let cfg = Config::parse(
            "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: a\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n  - id: b\n    strategy: lkgp\n    targets:\n      - openai/gpt-5.4\n",
            |_| Ok(Some("v".to_owned())),
        )
        .unwrap();
        let rows = model_rows(&cfg);
        let first = rows.iter().find(|r| r[0] == "openai/gpt-5.4").expect("the shared target");
        assert_eq!(first[3], "a+b", "one row, both owning combos in one cell");
    }

    #[test]
    fn lists_root_flags_when_no_subcommand_named() {
        let hint = unknown_flag_hint(&[OsString::from("--nope")], "--nope");
        assert!(hint.contains("--config"), "{hint}");
    }
}
