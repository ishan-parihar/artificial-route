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
use std::path::{Path, PathBuf};

use ar_config::{Combo, Config};
use ar_keys::{CredentialStore, KeyError};
use ar_registry::{Registry, global as registry};
use ar_server::config::split_target_known;
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

/// Default credential-store filename, next to the config file.
const DEFAULT_STORE_FILE: &str = "credentials.db";

/// Environment variable overriding the credential-store path.
pub const STORE_PATH_VAR: &str = "AR_CRED_STORE";

/// Where the local encrypted credential store lives: `$AR_CRED_STORE`, else
/// `credentials.db` beside the config file.
///
/// Beside the config rather than in the process's working directory, because the
/// config is what an operator thinks of as "this install"; a store resolved from
/// the CWD is how two checkouts in different directories end up sharing
/// credentials without either saying so.
pub fn store_path(config_path: &Path) -> PathBuf {
    if let Some(p) = std::env::var(STORE_PATH_VAR).ok().filter(|v| !v.trim().is_empty()) {
        return PathBuf::from(p);
    }
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(DEFAULT_STORE_FILE),
        _ => PathBuf::from(DEFAULT_STORE_FILE),
    }
}

/// Opens the credential store at `path`, creating it if absent.
///
/// The error is `ar-keys`' own, which is redacted by construction: it names
/// `$AR_MASTER_KEY` and sqlite's complaint, never a stored value.
pub fn open_store(path: &Path) -> Result<CredentialStore, KeyError> {
    CredentialStore::open_with_env_key(path)
}

/// The local encrypted credential store, or `None` when this host has none.
///
/// A missing file and an unset `AR_MASTER_KEY` are the pre-store configuration
/// and fall back to `keys:` in silence. A store that *is* on disk and still will
/// not open is an operator problem, so it says so once on stderr -- stdout is the
/// data channel -- rather than dispatching as if it had never been populated.
///
/// Shared by `ar serve` and `ar mcp`: both resolve credentials the same way, and
/// two copies of this fallback is two chances for them to disagree.
pub fn credential_store(cli: &Cli) -> Option<CredentialStore> {
    let path = store_path(&cli.config);
    match open_store(&path) {
        Ok(store) => Some(store),
        Err(e) if path.exists() => {
            eprintln!("ar: credential store {} is unusable ({e}); falling back to $VAR", path.display());
            None
        }
        Err(_) => None,
    }
}

/// What `ar` found at the credential-store path.
///
/// Public because `ar auth` resolves the same probe `ar doctor` prints, and two
/// implementations of "which credentials can this config see" is two chances for
/// a login to write a row a dispatch will not read.
///
/// Four outcomes because the four mean different things to an operator: a store
/// that is not there is a supported install, a store that is there and reads is
/// the feature working, a store that is there and does not is a silent loss of
/// every credential it holds, and a store that cannot even be opened is a
/// different failure from one that opens and refuses to read.
#[derive(Debug)]
pub enum StoreProbe<'a> {
    /// No file at the store path. `$VAR` only.
    Absent(PathBuf),
    /// Opened and listed. Names only — the names are the `keys:` labels already
    /// in `config.yaml`, so listing them decrypts nothing.
    Open {
        /// Where it is.
        path: PathBuf,
        /// Every credential name in it.
        names: Vec<String>,
    },
    /// A file is there and could not be read.
    Unreadable {
        /// Where it is.
        path: PathBuf,
        /// `ar-keys`' own reason, redacted by construction.
        reason: String,
    },
    /// Already-open store, for a caller that holds one.
    ///
    /// A login and a logout both *have* a store open — they are about to write
    /// through it — so re-probing the path would resolve a second connection to
    /// a file this process is mid-write on and report on the wrong half.
    Live {
        /// The store itself.
        store: &'a CredentialStore,
        /// Where it is, for the `store` row.
        path: PathBuf,
    },
}

impl StoreProbe<'_> {
    /// Resolves the store once, so `doctor` and `serve` cannot disagree about
    /// whether it is usable.
    pub fn resolve(config_path: &Path) -> Self {
        let path = store_path(config_path);
        // Not opened when absent: `open` would *create* the file, and a
        // read-only check that leaves a database behind is a check that lies
        // about what it found.
        if !path.exists() {
            return Self::Absent(path);
        }
        match open_store(&path).and_then(|store| store.list_names()) {
            Ok(names) => Self::Open { path, names },
            Err(e) => Self::Unreadable { path, reason: e.to_string() },
        }
    }

    /// The `(status, detail)` cell for the `store` row.
    ///
    /// Absent is `skip`, not `fail`: env-only is the configuration every install
    /// before this feature had, and failing it would make `ar doctor` exit 1 on
    /// every host that has not opted in.
    fn row(&self) -> (String, String) {
        match self {
            Self::Absent(path) => (
                "skip".to_owned(),
                format!("no store at {}; $VAR only", path.display()),
            ),
            Self::Open { path, names } => (
                "ok".to_owned(),
                format!("{} readable with {} credential(s)", path.display(), names.len()),
            ),
            Self::Live { path, store } => match store.list_names() {
                Ok(names) => (
                    "ok".to_owned(),
                    format!("{} readable with {} credential(s)", path.display(), names.len()),
                ),
                Err(e) => ("fail".to_owned(), format!("{} unreadable: {e}", path.display())),
            },
            Self::Unreadable { path, reason } => (
                "fail".to_owned(),
                format!("{} unreadable: {reason}", path.display()),
            ),
        }
    }

    /// Whether `name` is stored here, consulting nothing else.
    pub fn holds(&self, name: &str) -> bool {
        match self {
            Self::Open { names, .. } => names.iter().any(|n| n == name),
            Self::Live { store, .. } => store.list_names().is_ok_and(|names| names.iter().any(|n| n == name)),
            _ => false,
        }
    }
}

/// The catalog this config routes against: the compiled-in providers plus its
/// `custom_providers:` nodes.
///
/// A collision is fatal rather than skipped — shadowing a compiled-in id would
/// make the catalog stop describing the world, and `ar doctor` says which one.
fn catalog(cfg: &Config) -> anyhow::Result<Registry> {
    registry()
        .merge(&cfg.custom_providers)
        .map_err(|e| fail(e, "rename the `custom_providers` entry so its `id:` is not a compiled-in provider"))
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

/// The `ar auth status` probe for a caller that already holds a store.
///
/// Names over values, so a status listing never decrypts a row it does not need
/// — the same discipline [`StoreProbe::resolve`] keeps for `ar doctor`.
pub fn store_probe(config_path: &Path) -> StoreProbe<'static> {
    StoreProbe::resolve(config_path)
}

/// A probe over an already-open store, for a caller that is about to write
/// through it.
///
/// Borrowed rather than owned because a login both reads the client-secret row
/// and writes the token rows through the same handle, and a second connection to
/// a file this process is mid-write on would report on the wrong half.
pub fn live_store_probe<'a>(path: &Path, store: &'a CredentialStore) -> StoreProbe<'a> {
    StoreProbe::Live { path: path.to_path_buf(), store }
}

/// The `ar auth status` spelling of a doctor row's status, as a borrowed
/// `&'static str`.
///
/// The doctor vocabulary is `ok`/`fail` because a `fail` there exits 1; a status
/// listing has no exit to justify, so it names the state the operator is asking
/// about. Derived from the row's own status rather than re-decided, which is what
/// keeps the two surfaces from drifting.
pub fn armed_spelling(status: &str) -> &'static str {
    if status == "ok" { "armed" } else { "unarmed" }
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
        Some(Command::Auth(a)) => crate::auth::run(cli, a),
        Some(Command::Run(a)) => block_on(serve::run(cli, a)),
        Some(Command::Configure(a)) => configure(cli, a),
        Some(Command::Import(a)) => import_config(a),
        #[cfg(feature = "mcp")]
        Some(Command::Mcp(a)) => block_on(crate::mcp::run(cli, a)),
    }
}

/// Bare `ar`: content-first home view, not a manual (docs/06).
///
/// Deliberately not the combo table's full width: the point of a home view is
/// the three facts a caller cannot guess — where the binary is, what it is, and
/// whether it is configured at all.
fn home(cli: &Cli) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let catalog = catalog(&cfg)?;
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
        toon::list("combos", "combos", &COMBO_COLUMNS, &toon::fields_or_default(None), &combo_rows(&cfg.combos, &catalog), true)
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
    let catalog = catalog(&cfg)?;
    let fields = columns(&toon::fields_or_default(args.fields.as_deref()), &MODEL_COLUMNS, "models")?;
    print!(
        "{}",
        toon::list("models", "models", &MODEL_COLUMNS, &fields, &model_rows(&cfg, &catalog), args.full)
    );
    Ok(())
}

/// Routable models: every combo target, plus anything the catalog declares.
///
/// Combo targets come first because those are what this config can actually
/// route; the catalog's own model list is appended so `ar models` also answers
/// "what else could I route to". A target's status names the half that failed.
fn model_rows(cfg: &Config, catalog: &Registry) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for c in &cfg.combos {
        for t in &c.targets {
            let (provider, model) = split_target_known(t, |id| catalog.get(id).is_some());
            let status = match catalog.get(provider) {
                None => "unknown-provider",
                Some(_) if !model_known_in(catalog, provider, model) => "unknown-model",
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
    for (provider_id, def) in catalog.iter() {
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

/// The provider half of a `provider/model` target, under the same grammar
/// `ar serve` routes with (see `ar_server::config::split_target_known`).
///
/// Compiled-in catalog only: `ar import` builds a combo list from a registry it
/// just generated, so there is no config to widen it with.
pub fn target_provider(target: &str) -> &str {
    split_target_known(target, |id| registry().get(id).is_some()).0
}

/// [`target_provider`] against a catalog that also carries the file-declared
/// nodes, so a combo row names a custom provider as itself rather than as the
/// `default` the compiled-in-only grammar falls back to.
fn target_provider_in<'a>(target: &'a str, catalog: &Registry) -> &'a str {
    split_target_known(target, |id| catalog.get(id).is_some()).0
}

fn providers(cli: &Cli, args: &ListArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let catalog = catalog(&cfg)?;
    let fields = columns(&toon::fields_or_default(args.fields.as_deref()), &PROVIDER_COLUMNS, "providers")?;
    print!(
        "{}",
        toon::list("providers", "providers", &PROVIDER_COLUMNS, &fields, &provider_rows(&cfg, &catalog), args.full)
    );
    Ok(())
}

/// One provider row: `id`, the wire dialect, and whether the config resolves to
/// a known provider.
///
/// Both provider lists are listed: a `custom_providers:` node is a provider the
/// config declares, and a provider this cannot see is a provider an operator
/// cannot debug.
fn provider_rows(cfg: &Config, catalog: &Registry) -> Vec<Row> {
    let mut rows: Vec<Row> = cfg
        .providers
        .iter()
        .map(|p| {
            let def = catalog.get(&p.id);
            let dialect = def.map_or("unknown", |d| d.wire_format.as_str());
            let base_url = def.map_or("-", |d| d.base_url.as_str());
            let status = if def.is_some() { "ok" } else { "not-in-registry" };
            vec![p.id.clone(), dialect.to_owned(), status.to_owned(), base_url.to_owned(), p.key.clone()]
        })
        .chain(cfg.custom_providers.iter().map(|c| {
            vec![
                c.id.clone(),
                c.protocol.wire_format().as_str().to_owned(),
                "custom".to_owned(),
                c.base_url.clone(),
                c.key_ref.clone(),
            ]
        }))
        .collect();
    rows.sort();
    rows
}

fn combo(cli: &Cli, args: &ListArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let catalog = catalog(&cfg)?;
    let fields = columns(&toon::fields_or_default(args.fields.as_deref()), &COMBO_COLUMNS, "combo")?;
    print!(
        "{}",
        toon::list("combo", "combos", &COMBO_COLUMNS, &fields, &combo_rows(&cfg.combos, &catalog), args.full)
    );
    Ok(())
}

/// One combo row: distinct providers, strategy, and the full target chain.
///
/// `+` not `,`: rows are comma-delimited, so a comma-separated set would make
/// this row one cell wider than the header claims.
fn combo_rows(combos: &[Combo], catalog: &Registry) -> Vec<Row> {
    let mut rows: Vec<Row> = combos
        .iter()
        .map(|c| {
            let providers: BTreeSet<&str> = c.targets.iter().map(|t| target_provider_in(t, catalog)).collect();
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
    let store = StoreProbe::resolve(&cli.config);
    let checks = findings(&cfg, &cli.config.display().to_string(), &store);
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
/// two can never disagree about whether a config is valid. `store` is the same
/// [`StoreProbe`] `ar serve` resolves, so a credential the server would take
/// from the store and one this calls `$VAR` cannot be two different claims.
fn findings(cfg: &Config, path: &str, store: &StoreProbe<'_>) -> Vec<Row> {
    // A collision makes every merged lookup ambiguous, so it becomes one `fail`
    // row rather than a per-node complaint: no node is wrong on its own, the id
    // is. The rows below then read the compiled-in set alone — the collision has
    // its own row, and those still catch the typos that are not about it.
    let merged = registry().merge(&cfg.custom_providers);
    let collision = merged.as_ref().err().map(ToString::to_string);
    let catalog = merged.unwrap_or_else(|_| registry().clone());

    // Counted before the table is built because the `registry` row reports it:
    // the same `model_known_in` the per-target `fail` rows use, so the count and
    // the rows below can never disagree about which models are missing.
    let unroutable = cfg
        .combos
        .iter()
        .flat_map(|c| c.targets.iter().chain(c.pool.iter()))
        .filter(|t| {
            let (provider, model) = split_target_known(t, |id| catalog.get(id).is_some());
            catalog.get(provider).is_some() && !model.is_empty() && !model_known_in(&catalog, provider, model)
        })
        .count();

    let mut rows: Vec<Row> = vec![
        vec!["config".to_owned(), "ok".to_owned(), format!("{path} ({} providers + {} custom + {} combos)", cfg.providers.len(), cfg.custom_providers.len(), cfg.combos.len())],
        registry_row(unroutable),
    ];
    let (status, detail) = store.row();
    rows.push(vec!["store".to_owned(), status, detail]);

        // F-MED-2: the terminal-status list, reported from the executor's own copy
        // so an operator sees the same rows the classifier uses and the store's
        // CHECK is generated from. A literal here would be a fourth copy to drift.
        let executors: Vec<&str> = ar_server::OAuthKind::ALL.iter().map(|k| k.as_str()).collect();
    rows.push(vec![
        "terminal-status".to_owned(),
        "ok".to_owned(),
        format!(
            "{} terminal row(s); store CHECK from ar_exec::oauth::terminal_check_constraint(); executors: {}",
            ar_server::TERMINAL_REFRESH_STATUS.len(),
            executors.join("+"),
        ),
    ]);

    if let Some(reason) = collision {
        rows.push(vec!["custom".to_owned(), "fail".to_owned(), reason]);
    }

    if cfg.combos.is_empty() {
        rows.push(vec![
            "combos".to_owned(),
            "fail".to_owned(),
            "none configured; no model is routable".to_owned(),
        ]);
    }

    for p in &cfg.providers {
        let Some(def) = catalog.get(&p.id) else {
            rows.push(vec![
                format!("provider/{}", p.id),
                "fail".to_owned(),
                "not in registry".to_owned(),
            ]);
            continue;
        };
        rows.push(vec![format!("provider/{}", p.id), "ok".to_owned(), "in registry".to_owned()]);

        // F-CRIT-1: a provider the catalog labels `oauth` is not "known" to this
        // build just because it is in the registry. Without this row `doctor`
        // would report `codex` as a healthy provider while `ar serve` cannot
        // dispatch to it — the silent known-listing the audit names.
        if def.auth_kind.as_ref() == "oauth" {
            let (status, detail) = oauth_row(&p.id, cfg, store);
            rows.push(vec![format!("oauth/{}", p.id), status, detail]);
            // Login readiness is its own row rather than part of the `oauth/` one:
            // the two answer different questions (does it dispatch vs can it be
            // re-authorised) and a session can be `ok` on the first and
            // `unavailable` on the second. Folding it in would make a working
            // session read as broken.
            let (login, why) = login_readiness(&p.id, cfg, store);
            rows.push(vec![format!("auth/{}", p.id), login, why]);
        }
    }

    for c in &cfg.custom_providers {
        // Only the base URL is this row's own business: whether the credential
        // resolves is the `key/<name>` row's, and load already refuses an
        // undeclared `key_ref`.
        let (status, detail) = if c.base_url_ok() {
            let refused = if c.protocol.wire_format() == ar_registry::WireFormat::Openai {
                String::new()
            } else {
                format!("; {:?} is not dispatchable in this build", c.protocol.wire_format())
            };
            ("ok", format!("{} at {}{refused}", c.protocol.wire_format().as_str(), c.base_url))
        } else {
            ("fail", format!("base_url {:?} is not an http(s) URL with an authority", c.base_url))
        };
        rows.push(vec![format!("custom/{}", c.id), status.to_owned(), detail.to_owned()]);
    }

    let mut names: Vec<&str> = cfg.keys.keys().map(String::as_str).collect();
    names.sort_unstable();
    for name in names {
        rows.push(vec![format!("key/{name}"), key_status(name, cfg, store), key_detail(name, cfg, store)]);
    }

    // A bench entry gets its own `pool/<id>` prefix rather than being folded into
    // the `target/` rows: the audit's whole finding is that free-stack declares 2
    // targets and 7 candidates, and a doctor that printed one undifferentiated
    // list kept the two indistinguishable — which is how the bench came to be
    // invisible in the first place. Same verdict for both, because
    // `ar_server::config::resolve_target` resolves them on the same terms.
    for (prefix, entries) in [("target", target_rows(cfg)), ("pool", pool_rows(cfg))] {
        for t in entries {
            let (status, detail) = target_verdict(t, cfg, &catalog);
            rows.push(vec![format!("{prefix}/{t}"), status.to_owned(), detail.to_owned()]);
        }
    }

    rows
}

/// Every `targets:` entry, deduplicated across combos.
fn target_rows(cfg: &Config) -> BTreeSet<&str> {
    cfg.combos.iter().flat_map(|c| c.targets.iter().map(String::as_str)).collect()
}

/// Every `pool:` entry, deduplicated across combos.
fn pool_rows(cfg: &Config) -> BTreeSet<&str> {
    cfg.combos.iter().flat_map(|c| c.pool.iter().map(String::as_str)).collect()
}

/// Whether one `provider/model` string routes, and what to say when it does not.
///
/// Shared by the `target/` and `pool/` rows: an operator who mistypes a model in
/// the bench must be told the same thing `ar serve` will do about it.
fn target_verdict(target: &str, cfg: &Config, catalog: &Registry) -> (&'static str, &'static str) {
    let (provider, model) = split_target_known(target, |id| catalog.get(id).is_some());
    if catalog.get(provider).is_none() {
        ("fail", "provider not in registry")
    } else if !cfg.declares(provider) {
        ("fail", "provider not configured")
    } else if model.is_empty() {
        ("fail", "target names no model; write it as provider/model")
    } else if !model_known_in(catalog, provider, model) {
        // The provider half passing is not enough: a typo in the model half
        // survives every provider check and only surfaces as a 404 from the
        // upstream, which reads as "the proxy is broken".
        ("fail", "model not in registry; run `ar import --from omniroute --path <OmniRoute/open-sse/config/providers>` or `ar models`")
    } else {
        ("ok", "routable")
    }
}

/// The `registry` row: the compiled-in catalog's age, and `warn` once it is past
/// [`ar_registry::SNAPSHOT_TTL_DAYS`].
///
/// F-HIGH-3. The registry is a snapshot plus `ar import`, so nothing about a
/// stale one is visible until a request 404s against a model a provider has since
/// rotated. This is the signal that costs nothing: the build's own date, which
/// `ar-registry` stamps, is a floor on the snapshot's age, and the count of
/// configured targets this build cannot route is the drift it can *prove*. The
/// fix is named here rather than left for the reader to remember.
///
/// `warn`, not `fail`: a stale snapshot degrades a routing decision, it does not
/// make the config invalid, and `doctor` exiting 1 on a week-old build would
/// train operators to ignore it. The models themselves are named by the
/// `target/<id>` `fail` rows below, which already carry this same `ar import`
/// fix -- this row counts them and points at them rather than listing twice.
///
/// The detail is joined with `+` and never `,`: a TOON row is comma-delimited, so
/// a comma in a cell makes the row one column wider than the header claims. There
/// is a test for exactly that.
fn registry_row(unroutable: usize) -> Row {
    registry_row_at(unroutable, ar_registry::discovery::unix_now())
}

/// [`registry_row`] at an explicit `now`, so the ttl boundary is testable without
/// a seven-day-old build.
fn registry_row_at(unroutable: usize, now: u64) -> Row {
    let compiled = format!("{} providers compiled in + snapshot {}d old", registry().len(), ar_registry::age_days(now));
    if ar_registry::is_stale(now) {
        return vec![
            "registry".to_owned(),
            "warn".to_owned(),
            format!(
                "{compiled} (ttl {}d); {unroutable} configured model(s) below are not in it; run `ar import --from omniroute --path <OmniRoute/open-sse/config/providers>`",
                ar_registry::SNAPSHOT_TTL_DAYS
            ),
        ];
    }
    vec!["registry".to_owned(), "ok".to_owned(), compiled]
}

/// The `(status, detail)` cell of an `oauth/<provider>` row, public for `ar auth`.
///
/// Five states, each a different operator action, so they cannot share a status:
///
/// | state | status | why |
/// |---|---|---|
/// | no executor for this provider | `fail` | nothing can authenticate it |
/// | no session declared | `fail` | an access token alone cannot renew |
/// | access row missing | `fail` | the session has no bearer at all |
/// | refresh row missing / no endpoint | `fail` | works until it does not |
/// | armed | `ok` | dispatches, and renews on 401 |
///
/// The "works until it does not" rows are `fail` and not `skip` on purpose: the
/// session *will* serve traffic and then die on a 401, and a caller reading `ok`
/// would learn that from nowhere. Audit R2 is exactly this case — `kimi-coding`
/// expired with no stored refresh, which has to be visible now rather than a bare
/// 502 later.
///
/// This row is about *dispatch*. Whether the session can be (re-)authorised is
/// [`login_readiness`]'s question, folded in as the `auth/<id>` row below: a
/// session can be armed here and unloggable there, and collapsing the two would
/// report a working session as broken.
pub fn oauth_row(provider: &str, cfg: &Config, store: &StoreProbe<'_>) -> (String, String) {
    // Mechanism first, executor second: an anonymous session is armed *by
    // design* — there is no account, no row and nothing to renew — so asking
    // whether its access token resolves would report the absence of a credential
    // as a failure to produce one. And it is the one shape that needs no
    // executor at all, which is what makes it the answer for a provider this
    // build otherwise cannot authenticate (red-team R1's `kilocode`).
    if let Some(declared) = cfg.oauth_for(provider)
        && declared.anonymous
    {
        let (status, reason) = anonymous_row(declared);
        return (status, reason);
    }
    let Some(kind) = ar_server::OAuthKind::parse(provider) else {
        return (
            "fail".to_owned(),
            format!(
                "catalog authType is oauth but this build has no executor for it, so it cannot authenticate and will not route (AUDIT-REPORT F-CRIT-1, red-team R1){}",
                no_executor_fix(cfg, provider),
            ),
        );
    };
    let Some(declared) = cfg.oauth_for(provider) else {
        return (
            "fail".to_owned(),
            format!("catalog authType is oauth; add an `oauth:` block naming a refresh_key and token_url{}", relogin(provider)),
        );
    };

    let access_name = cfg.key_name(provider).unwrap_or(provider);
    if !resolves(access_name, cfg, store) {
        return (
            "fail".to_owned(),
            format!("access token {access_name:?} is in neither the credential store nor keys:{}", relogin(provider)),
        );
    }

    // Spelled from the executor's own list, so this reason cannot drift from the
    // one a real refresh failure would record.
    let terminal = terminal_reason(400, "no_refresh_token").unwrap_or("no_refresh_token");
    match (declared.refresh_key.as_deref(), declared.token_url.as_deref()) {
        (None, _) => (
            "fail".to_owned(),
            format!("access token resolves; no refresh_key, so the first 401 is terminal ({terminal})"),
        ),
        (Some(_), None) => (
            "fail".to_owned(),
            format!("access token resolves; refresh row declared but no token_url, so the first 401 is terminal ({terminal})"),
        ),
        (Some(name), Some(_)) if !resolves(name, cfg, store) => (
            "fail".to_owned(),
            format!("refresh row {name:?} is in neither the credential store nor keys:"),
        ),
        (Some(name), Some(url)) => (
            "ok".to_owned(),
            format!(
                "{} executor; access {access_name:?} + refresh {name:?} resolve; renews at {url}",
                kind.as_str()
            ),
        ),
    }
}

/// The `oauth/<id>` row for an anonymous free-tier session.
///
/// `ok`, and armed rather than merely usable: there is no account, so there is
/// nothing to expire, nothing to refresh and nothing an operator could log in to
/// renew. Calling it `unarmed` would send someone looking for a token that the
/// design says must not exist.
///
/// The editor header is named by *field*, never by value. It is not a secret —
/// it is the product name the upstream logs — but a row that renders config
/// values is a row that will render a credential the moment someone copies the
/// pattern.
fn anonymous_row(declared: &ar_config::OAuthSession) -> (String, String) {
    let editor = if declared.anonymous_editor.as_deref().is_some_and(|e| !e.is_empty()) {
        "anonymous_editor set"
    } else {
        "anonymous_editor is empty"
    };
    (
        "ok".to_owned(),
        format!(
            "anonymous free tier; armed by design with no credential row; dispatches `Bearer anonymous` plus {editor}",
        ),
    )
}

/// The mechanism-aware fix clause for an `oauth` provider with no executor.
///
/// One string, because the whole point of the row is that the next command is on
/// it. A provider with *any* workable mechanism gets the YAML that turns the
/// mechanism on; one with neither gets the honest refusal. `kilocode` is the
/// reason this exists: R1 recorded it active with no stored refresh and no traced
/// mechanism, and the free tier is now a mechanism this build can drive without
/// an executor at all.
fn no_executor_fix(cfg: &Config, provider: &str) -> String {
    match cfg.oauth_for(provider) {
        None => "; it can still work without one — add an `oauth:` block with `anonymous: true` and an `anonymous_editor:` for the free tier".to_owned(),
        Some(declared) if declared.anonymous => String::new(),
        Some(declared) if declared.device_auth_url.is_some() => format!(
            "; the block declares a device login (device_auth_url + device_poll_url), which needs no executor, but this build has no oauth kind for {provider}"
        ),
        Some(_) => "; if it has a device flow or a free tier, declare `device_auth_url` + `device_poll_url`, or `anonymous: true` + `anonymous_editor:`, in its `oauth:` block".to_owned(),
    }
}

/// Login readiness for one declared session: whether `ar auth login` can run it.
///
/// Separate from [`oauth_row`] on purpose. `oauth_row` answers "does this session
/// dispatch", which is a question about what is *already stored*; this answers
/// "can it be (re-)authorised", which is a question about what the operator
/// *declared*. A session can be fully armed and unloggable — both tokens present,
/// no `authorization_url` — and reporting that as a dispatch `fail` would be a
/// lie in the direction that costs an operator the most: the session works, and
/// the only thing missing is the ability to renew it by hand.
///
/// The fix therefore names two commands: `ar auth login` for the endpoint the
/// operator has to add, and the login itself for the tokens a re-auth mints.
pub fn login_readiness(
    provider: &str,
    cfg: &Config,
    store: &StoreProbe<'_>,
) -> (String, String) {
    let Some(declared) = cfg.oauth_for(provider) else {
        return (
            "unavailable".to_owned(),
            "no `oauth:` block declares this provider".to_owned(),
        );
    };
    // A free-tier session has nothing to authorise and nothing to wait for, so
    // `needs-login` would be a lie about a credential that by design never
    // exists. The three states below are all about producing a token.
    if declared.anonymous {
        let (_, detail) = anonymous_row(declared);
        return ("armed-by-design".to_owned(), detail);
    }
    // A device session is authorised by typing a code somewhere else, so its
    // readiness question is "are both endpoints declared", not "is there a URL
    // to open here". Same three verdicts, different question — and the answer
    // has to differ, because a device provider with no `authorization_url` is a
    // working login and reporting it `unavailable` would be exactly backwards.
    if declared.device_auth_url.is_some() {
        return match (declared.device_auth_url.as_deref(), declared.device_poll_url.as_deref()) {
            (Some(auth), Some(_)) => {
                let armed = resolves(cfg.key_name(provider).unwrap_or(provider), cfg, store);
                let relogin = relogin(provider);
                (
                    if armed { "armed".to_owned() } else { "needs-login".to_owned() },
                    format!(
                        "device login at {auth}; no browser is opened here, a code is entered on any device; access {}{relogin}",
                        if armed { "resolves" } else { "does not resolve yet" },
                    ),
                )
            }
            _ => (
                "unavailable".to_owned(),
                "a device login needs both device_auth_url and device_poll_url in the `oauth:` block".to_owned(),
            ),
        };
    }
    let Some(endpoint) = declared.authorization_url.as_deref() else {
        return (
            "unavailable".to_owned(),
            format!(
                "no authorization_url in the `oauth:` block, so there is no url to open; add one, then run {}",
                relogin(provider)
            ),
        );
    };
    let access_name = cfg.key_name(provider).unwrap_or(provider);
    let armed = resolves(access_name, cfg, store);
    (
        if armed { "armed".to_owned() } else { "needs-login".to_owned() },
        format!(
            "authorize at {endpoint}; access {access_name:?} {}{}",
            if armed { "resolves" } else { "does not resolve yet" },
            relogin(provider)
        ),
    )
}

/// The fix clause every unarmed `oauth/<id>` row carries.
///
/// One function so the fix string is one string: an operator who reads it on
/// one row and a different one on the next has two commands to learn, and the
/// whole point of the row is that the next command is on it.
fn relogin(provider: &str) -> String {
    format!("; run `ar auth login --provider {provider}`")
}

/// The canonical spelling of a terminal reason, read from the one list the
/// executor classifies against.
///
/// `None` for a reason the list does not carry. Every caller passes a literal, so
/// a `None` is a drift between two copies rather than a runtime path — which is
/// why the fallback at the call site is the same string.
fn terminal_reason(status: u16, reason: &str) -> Option<&'static str> {
    ar_server::TERMINAL_REFRESH_STATUS
        .iter()
        .find(|(s, r)| *s == status && *r == reason)
        .map(|(_, r)| *r)
}

/// Whether a credential name resolves, in the order `ar serve` resolves it: the
/// store first, then `keys:`.
fn resolves(name: &str, cfg: &Config, store: &StoreProbe<'_>) -> bool {
    store.holds(name) || cfg.key(name).is_some_and(|s| !s.expose().trim().is_empty())
}

/// Whether the catalog lists `model` under `provider`.
///
/// A provider with no model list of its own is accepted: those are the
/// passthrough providers whose catalog upstream is empty and the real list comes
/// from live discovery, so failing them here would report an unreachable
/// provider as a typo. A custom node is always in that case — its model spelling
/// lives in the combo target, not in a catalog row.
fn model_known_in(catalog: &Registry, provider: &str, model: &str) -> bool {
    let Some(def) = catalog.get(provider) else { return false };
    def.models.is_empty() || def.models.iter().any(|m| m.as_ref() == model)
}

/// Whether a provider needs a credential at all, per the compiled-in registry.
///
/// `auth_kind` is carried verbatim rather than resolved into an enum
/// (`ar_registry`'s own note), and the only question here is "would this
/// provider be dispatched unauthenticated". `apikey` is the answer that means
/// yes; `optional` and `none` are keyless providers, for which
/// `keys: {ollama: ""}` is the documented way to write them and an empty
/// credential is correct rather than a hole.
fn needs_credential(provider: &str) -> bool {
    registry().get(provider).is_none_or(|d| d.auth_kind.as_ref() == "apikey")
}

/// The `status` cell of a `key/<name>` row.
///
/// Resolution order matches `ar_server::config::resolve_key`: the store first,
/// then the config's `$VAR`-expanded `keys:` map. A name that resolves in neither
/// place is a failure only when something bound to it actually needs a
/// credential.
fn key_status(name: &str, cfg: &Config, store: &StoreProbe<'_>) -> String {
    if store.holds(name) || cfg.key(name).is_some_and(|s| !s.expose().trim().is_empty()) {
        return "ok".to_owned();
    }
    // A custom node always needs a credential: there is no compiled-in
    // `auth_kind` to say it is keyless, and dispatching one unauthenticated is
    // the hole this reports.
    if cfg.custom_providers.iter().any(|c| c.key_ref == name)
        || cfg.providers.iter().any(|p| p.key == name && needs_credential(&p.id))
    {
        return "fail".to_owned();
    }
    "ok".to_owned()
}

/// The `detail` cell of a `key/<name>` row. Names the binding and the source, and
/// never a value — a `doctor` transcript is something people paste into issues.
fn key_detail(name: &str, cfg: &Config, store: &StoreProbe<'_>) -> String {
    let users: Vec<&str> = cfg
        .providers
        .iter()
        .filter(|p| p.key == name)
        .map(|p| p.id.as_str())
        .chain(cfg.custom_providers.iter().filter(|c| c.key_ref == name).map(|c| c.id.as_str()))
        .collect();
    let bound = if users.is_empty() { "unused".to_owned() } else { format!("for {}", users.join("+")) };
    if store.holds(name) {
        return format!("resolved from store ({bound})");
    }
    match cfg.key(name) {
        None => format!("not declared under keys ({bound})"),
        Some(secret) if secret.expose().trim().is_empty() => {
            format!("resolved to nothing - no store row and an empty $VAR ({bound})")
        }
        Some(_) => format!("resolved from keys: ({bound})"),
    }
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

    let checks = findings(&cfg, &cli.config.display().to_string(), &StoreProbe::resolve(&cli.config));
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
        vec!["custom_providers".to_owned(), cfg.custom_providers.len().to_string()],
        vec!["combos".to_owned(), cfg.combos.len().to_string()],
    ];
    for p in &cfg.providers {
        let base_url = registry().get(&p.id).map_or_else(|| "-".to_owned(), |d| d.base_url.clone());
        rows.push(vec![format!("provider.{}.key", p.id), p.key.clone()]);
        rows.push(vec![format!("provider.{}.base_url", p.id), base_url]);
    }
    for c in &cfg.custom_providers {
        rows.push(vec![format!("provider.{}.key", c.id), c.key_ref.clone()]);
        rows.push(vec![format!("provider.{}.base_url", c.id), c.base_url.clone()]);
        rows.push(vec![format!("provider.{}.protocol", c.id), c.protocol.wire_format().as_str().to_owned()]);
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
/// The subcommand is found by walking the command graph as deep as the arguments
/// go, by *name* rather than by position: `ar --config path auth login --nope`
/// has four tokens before the flag, and a positional read of "the first non-flag
/// token" would land on `path` and list the root's flags instead of the login
/// verb's. The deepest level reached wins, so a sub-subcommand's flags are on
/// the hint — which is the whole point of adding a subtree.
pub fn unknown_flag_hint(args: &[OsString], invalid: &str) -> String {
    let root = Cli::command();
    let mut deepest = None;
    let mut current = &root;
    // Two levels is the deepest this surface goes (`ar auth login`), so a fixed
    // walk beats a recursive descent that would have nothing left to descend into.
    for _ in 0..2 {
        let Some(found) = args
            .iter()
            .filter_map(|a| a.to_str())
            .find_map(|tok| current.find_subcommand(tok))
        else {
            break;
        };
        current = found;
        deepest = Some(found);
    }
    let sub = deepest;

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
    fn treats_bare_target_as_unroutable() {
        // No slash: the shared grammar yields the default-combo provider,
        // which names no registry entry — doctor reports it, serve refuses it.
        assert_eq!(target_provider("openai"), "default");
    }

    #[test]
    fn splits_nested_model_paths_at_the_registered_provider() {
        // Same grammar `ar serve` routes with: longest registered prefix wins.
        assert_eq!(target_provider("nvidia/moonshotai/kimi-k3"), "nvidia");
        assert_eq!(
            target_provider("aihorde/aphrodite/TheDrummer/Cydonia-24B-v4.3"),
            "aihorde"
        );
    }

    #[test]
    fn flags_unknown_field_with_valid_list() {
        let e = columns(&["nope".to_owned()], &PROVIDER_COLUMNS, "providers").unwrap_err().to_string();
        assert!(e.contains("unknown field \"nope\""), "{e}");
        assert!(e.contains("id, provider, status"), "{e}");
    }

    #[test]
    fn marks_combo_with_no_targets() {
        let rows = combo_rows(
            &[Combo {
                id: "empty".to_owned(),
                strategy: ar_config::Strategy::Priority,
                targets: vec![],
                pool: vec![],
                compression: None,
            }],
            &registry().clone(),
        );
        assert_eq!(rows[0][2], "no-targets");
    }

    #[test]
    fn fails_check_when_a_pool_entry_names_an_unroutable_model() {
        // The bench is validated on the same terms as a target, so a typo in it
        // is a `fail` row rather than a 404 at 3am (audit F-HIGH-2).
        let cfg = Config::parse(
            "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n    pool:\n      - openai/gpt-9.9-typo\n",
            |_| Ok(Some("v".to_owned())),
        )
        .unwrap();
        let checks = findings(&cfg, "config.yaml", &StoreProbe::Absent(PathBuf::from("credentials.db")));
        let failed: Vec<&Row> = checks.iter().filter(|r| r[1] == "fail").collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0][0], "pool/openai/gpt-9.9-typo");
    }

    #[test]
    fn reports_pool_targets_as_a_separate_bench() {
        // The audit's finding was that free-stack's 2 targets and 7 candidates
        // were indistinguishable; the row prefix is what keeps them apart.
        let cfg = Config::parse(
            "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n    pool:\n      - groq/llama-3.3-70b\n",
            |_| Ok(Some("v".to_owned())),
        )
        .unwrap();
        let checks = findings(&cfg, "config.yaml", &StoreProbe::Absent(PathBuf::from("credentials.db")));
        assert!(checks.iter().any(|r| r[0] == "target/openai/gpt-5.4"));
        assert!(checks.iter().any(|r| r[0] == "pool/groq/llama-3.3-70b"));
    }

    #[test]
    fn fails_check_when_combo_target_provider_absent() {
        let cfg = Config::parse(
            "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - groq/llama\n",
            |_| Ok(Some("v".to_owned())),
        )
        .unwrap();
        let checks = findings(&cfg, "config.yaml", &StoreProbe::Absent(PathBuf::from("credentials.db")));
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
        let checks = findings(&cfg, "config.yaml", &StoreProbe::Absent(PathBuf::from("credentials.db")));
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
        let checks = findings(&cfg, "config.yaml", &StoreProbe::Absent(PathBuf::from("credentials.db")));
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
        let rows = model_rows(&cfg, &registry().clone());
        let first = rows.iter().find(|r| r[0] == "openai/gpt-5.4").expect("the shared target");
        assert_eq!(first[3], "a+b", "one row, both owning combos in one cell");
    }

    /// A file-declared node routed through every `doctor` surface.
    fn custom_config(base_url: &str, id: &str) -> Config {
        Config::parse(
            &format!(
                "keys:\n  k: v\ncustom_providers:\n  - id: {id}\n    protocol: openai-compatible\n    base_url: {base_url}\n    key_ref: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - {id}/some-model\n"
            ),
            |_| Ok(Some("v".to_owned())),
        )
        .expect("the fixture parses")
    }

    fn no_store() -> StoreProbe<'static> {
        StoreProbe::Absent(PathBuf::from("credentials.db"))
    }

    #[test]
    fn passes_check_when_a_custom_provider_is_well_formed() {
        let cfg = custom_config("https://api.example.invalid/v1", "local-gateway");
        let checks = findings(&cfg, "config.yaml", &no_store());
        assert!(!checks.iter().any(|r| r[1] == "fail"), "{checks:?}");
    }

    #[test]
    fn routes_a_custom_provider_target_when_it_is_declared() {
        let cfg = custom_config("https://api.example.invalid/v1", "local-gateway");
        let checks = findings(&cfg, "config.yaml", &no_store());
        let row = checks.iter().find(|r| r[0] == "target/local-gateway/some-model").expect("the target row");
        assert_eq!(row[1], "ok", "{row:?}");
    }

    #[test]
    fn fails_check_when_a_custom_providers_base_url_has_no_scheme() {
        let cfg = custom_config("api.example.invalid/v1", "local-gateway");
        let checks = findings(&cfg, "config.yaml", &no_store());
        let row = checks.iter().find(|r| r[0] == "custom/local-gateway").expect("the custom row");
        assert_eq!(row[1], "fail", "{row:?}");
        assert!(row[2].contains("not an http(s) URL"), "{row:?}");
    }

    #[test]
    fn fails_check_when_a_custom_provider_id_collides_with_the_catalog() {
        let cfg = custom_config("https://api.example.invalid/v1", "openai");
        let checks = findings(&cfg, "config.yaml", &no_store());
        let row = checks.iter().find(|r| r[0] == "custom").expect("the collision row");
        assert_eq!(row[1], "fail", "{row:?}");
        assert!(row[2].contains("openai"), "the row names the colliding id: {row:?}");
    }

    #[test]
    fn lists_a_custom_provider_when_it_is_declared() {
        let cfg = custom_config("https://api.example.invalid/v1", "local-gateway");
        let rows = provider_rows(&cfg, &catalog(&cfg).expect("no collision"));
        assert_eq!(rows[0][0], "local-gateway");
        assert_eq!(rows[0][2], "custom");
    }

    #[test]
    fn names_a_custom_provider_in_its_combo_row() {
        // The compiled-in-only grammar would fall back to `default`, so the row
        // would name a provider the config never declares.
        let cfg = custom_config("https://api.example.invalid/v1", "local-gateway");
        let rows = combo_rows(&cfg.combos, &catalog(&cfg).expect("no collision"));
        assert_eq!(rows[0][1], "local-gateway", "{rows:?}");
    }

    #[test]
    fn lists_root_flags_when_no_subcommand_named() {
        let hint = unknown_flag_hint(&[OsString::from("--nope")], "--nope");
        assert!(hint.contains("--config"), "{hint}");
    }

    /// An `oauth:`-capable provider with no session block: the F-CRIT-1 shape.
    const OAUTH_NO_SESSION: &str = concat!(
        "keys:\n  codex: $CODEX_ACCESS\nproviders:\n  - id: codex\n    key: codex\n",
        "combos:\n  - id: c\n    strategy: priority\n    targets:\n      - codex/gpt-5.4-codex\n",
    );

    /// The same provider, fully armed against a store that holds both rows.
    const OAUTH_ARMED: &str = concat!(
        "keys:\n  codex: $CODEX_ACCESS\nproviders:\n  - id: codex\n    key: codex\n",
        "oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n",
        "    token_url: https://auth.example.invalid/token\n    client_id: synthetic-client\n",
        "combos:\n  - id: c\n    strategy: priority\n    targets:\n      - codex/gpt-5.4-codex\n",
    );

    /// A provider the catalog labels `oauth` and this build has no executor for.
    /// Red-team R1's `kilocode`: two live sessions whose mechanism is unknown.
    const OAUTH_NO_EXECUTOR: &str = concat!(
        "keys:\n  kilocode: $KILO_ACCESS\nproviders:\n  - id: kilocode\n    key: kilocode\n",
        "oauth:\n  - provider: kilocode\n    refresh_key: kilocode_refresh\n",
        "    token_url: https://auth.example.invalid/token\n",
        "    authorization_url: https://auth.example.invalid/authorize\n",
        "combos:\n  - id: c\n    strategy: priority\n    targets:\n      - kilocode/kilo-1\n",
    );

    /// The armed session plus the one endpoint a browser login starts at.
    ///
    /// `authorization_url` is separate from arming on purpose: a session can hold
    /// both tokens and still be un-*loggable*, and that is a different fix from a
    /// missing refresh row, so it gets its own row and its own test.
    const OAUTH_LOGINABLE: &str = concat!(
        "keys:\n  codex: $CODEX_ACCESS\nproviders:\n  - id: codex\n    key: codex\n",
        "oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n",
        "    token_url: https://auth.example.invalid/token\n",
        "    authorization_url: https://auth.example.invalid/authorize\n",
        "    client_id: synthetic-client\n",
        "combos:\n  - id: c\n    strategy: priority\n    targets:\n      - codex/gpt-5.4-codex\n",
    );

    #[test]
    fn names_the_login_command_as_the_fix_when_a_session_cannot_be_logged_into() {
        // A session that holds both tokens but declares no authorization_url is
        // armed for dispatch and unloggable forever. Reporting that as a dispatch
        // `fail` would be a lie that costs an operator the most: the session
        // works, and only re-auth is impossible.
        let rows = checks(OAUTH_ARMED, &probe(&["codex", "codex_refresh"]));
        assert_eq!(row(&rows, "oauth/codex")[1], "ok", "it still dispatches: {rows:?}");
        let detail = &row(&rows, "auth/codex")[2];
        assert!(detail.contains("ar auth login --provider codex"), "the row names the fix: {detail}");
    }

    #[test]
    fn reports_login_readiness_as_unavailable_when_no_authorize_endpoint_is_declared() {
        let rows = checks(OAUTH_ARMED, &probe(&["codex", "codex_refresh"]));
        assert_eq!(row(&rows, "auth/codex")[1], "unavailable", "{rows:?}");
    }

    #[test]
    fn reports_a_loggable_session_as_armed_once_it_declares_an_authorize_endpoint() {
        let rows = checks(OAUTH_LOGINABLE, &probe(&["codex", "codex_refresh"]));
        assert_eq!(row(&rows, "auth/codex")[1], "armed", "{rows:?}");
    }

    #[test]
    fn reports_login_readiness_as_needed_when_no_access_token_resolves_anywhere() {
        // The state a first-ever login is in: the endpoint is declared, and the
        // access row resolves in neither the store nor `keys:`. `needs-login` says
        // so without claiming anything is broken. The empty expansion is the whole
        // point — a `$VAR` that expands to a value would resolve, which is a
        // different and also correct answer.
        let cfg = Config::parse(OAUTH_LOGINABLE, |_| Ok(Some(String::new()))).expect("the fixture parses");
        let rows = findings(&cfg, "config.yaml", &probe(&[]));
        assert_eq!(row(&rows, "auth/codex")[1], "needs-login", "{rows:?}");
    }

    #[test]
    fn fails_an_oauth_target_that_declares_no_session() {
        // The silent known-listing F-CRIT-1 names: the provider is in the
        // registry, so the old code called it `ok` while nothing could
        // authenticate it.
        let rows = checks(OAUTH_NO_SESSION, &no_store());
        assert_eq!(row(&rows, "oauth/codex")[1], "fail", "{rows:?}");
    }

    #[test]
    fn names_the_missing_session_block_as_the_fix() {
        let rows = checks(OAUTH_NO_SESSION, &no_store());
        let detail = &row(&rows, "oauth/codex")[2];
        assert!(detail.contains("oauth:"), "the detail names the fix: {detail}");
    }

    #[test]
    fn passes_an_oauth_target_whose_session_is_fully_armed() {
        let rows = checks(OAUTH_ARMED, &probe(&["codex", "codex_refresh"]));
        assert_eq!(row(&rows, "oauth/codex")[1], "ok", "{rows:?}");
    }

    #[test]
    fn fails_an_oauth_target_whose_refresh_row_is_missing_from_the_store() {
        // Armed in the file, absent from the store: the session dispatches and
        // then dies on its first 401, which the operator has to be told now.
        let rows = checks(OAUTH_ARMED, &probe(&["codex"]));
        assert_eq!(row(&rows, "oauth/codex")[1], "fail", "{rows:?}");
    }

    #[test]
    fn fails_an_oauth_target_whose_refresh_row_is_declared_but_has_no_endpoint() {
        let yaml = OAUTH_ARMED.replace("    token_url: https://auth.example.invalid/token\n", "");
        let rows = checks(&yaml, &probe(&["codex", "codex_refresh"]));
        assert_eq!(row(&rows, "oauth/codex")[1], "fail", "{rows:?}");
    }

    #[test]
    fn spells_the_unarmed_reason_from_the_executors_terminal_list() {
        // F-MED-2: the string a real refresh failure would record, read from the
        // same list the classifier uses. A literal here would be a fourth copy.
        //
        // The refresh row has to *resolve* for this to reach the branch that
        // prints the terminal reason — with the row missing the detail names the
        // missing row instead, which is a different (and also correct) message.
        let yaml = OAUTH_ARMED.replace("    token_url: https://auth.example.invalid/token\n", "");
        let rows = checks(&yaml, &probe(&["codex", "codex_refresh"]));
        let detail = &row(&rows, "oauth/codex")[2];
        let listed = ar_server::TERMINAL_REFRESH_STATUS
            .iter()
            .find(|(_, reason)| *reason == "no_refresh_token")
            .expect("the list carries it")
            .1;
        assert!(detail.contains(listed), "the detail quotes the list: {detail}");
    }

    #[test]
    fn fails_an_oauth_provider_this_build_has_no_executor_for() {
        // R1: kilocode stays loudly unroutable until its mechanism is traced.
        let rows = checks(OAUTH_NO_EXECUTOR, &probe(&["kilocode", "kilocode_refresh"]));
        let found = row(&rows, "oauth/kilocode");
        assert_eq!(found[1], "fail", "{rows:?}");
        assert!(found[2].contains("no executor"), "{found:?}");
    }

    #[test]
    fn reports_the_terminal_status_list_the_executor_classifies_against() {
        let rows = checks(ONE_PROVIDER, &no_store());
        let detail = &row(&rows, "terminal-status")[2];
        assert!(detail.contains(&ar_server::TERMINAL_REFRESH_STATUS.len().to_string()), "{detail}");
    }

    fn probe(names: &[&str]) -> StoreProbe<'static> {
        StoreProbe::Open { path: PathBuf::from("credentials.db"), names: names.iter().map(|n| (*n).to_owned()).collect() }
    }

    fn checks(yaml: &str, store: &StoreProbe<'_>) -> Vec<Row> {
        let cfg = Config::parse(yaml, |name| Ok(Some(format!("secret-{name}")))).expect("the fixture parses");
        findings(&cfg, "config.yaml", store)
    }

    const ONE_PROVIDER: &str = "keys:\n  k: $AR_KEY\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n";

    fn row<'a>(rows: &'a [Row], check: &str) -> &'a Row {
        rows.iter().find(|r| r[0] == check).unwrap_or_else(|| panic!("no {check} row in {rows:?}"))
    }

    #[test]
    fn reports_no_store_when_the_file_is_absent() {
        let absent = StoreProbe::Absent(PathBuf::from("credentials.db"));
        let (status, detail) = absent.row();
        assert_eq!(status, "skip", "an env-only install is supported, not broken");
        assert!(detail.contains("credentials.db"), "{detail}");
    }

    #[test]
    fn reports_the_store_as_failed_when_it_cannot_be_read() {
        let (status, detail) = StoreProbe::Unreadable {
            path: PathBuf::from("credentials.db"),
            reason: "master key unavailable: AR_MASTER_KEY is unset".to_owned(),
        }
        .row();
        assert_eq!(status, "fail");
        assert!(detail.contains("AR_MASTER_KEY"), "the reason is the fix: {detail}");
    }

    #[test]
    fn counts_credentials_when_the_store_opens() {
        let (status, detail) = probe(&["openai", "anthropic"]).row();
        assert_eq!(status, "ok");
        assert!(detail.contains("2 credential(s)"), "{detail}");
    }

    #[test]
    fn names_the_store_as_the_source_when_it_holds_the_key() {
        let rows = checks(ONE_PROVIDER, &probe(&["k"]));
        assert_eq!(row(&rows, "key/k")[1], "ok");
        assert!(row(&rows, "key/k")[2].contains("from store"), "{:?}", row(&rows, "key/k"));
    }

    #[test]
    fn names_the_config_as_the_source_when_the_store_has_no_row() {
        let rows = checks(ONE_PROVIDER, &probe(&["other"]));
        assert!(row(&rows, "key/k")[2].contains("from keys:"), "{:?}", row(&rows, "key/k"));
    }

    #[test]
    fn fails_a_key_check_when_an_apikey_provider_resolves_to_nothing() {
        // The `store` first, `keys:` second order means an empty entry is a real
        // state: the store is shadowed by an empty variable, or neither exists.
        // `checks` expands to a non-empty value, so this one parses its own.
        let cfg = Config::parse(ONE_PROVIDER, |_| Ok(Some(String::new()))).expect("the fixture parses");
        let rows = findings(&cfg, "config.yaml", &probe(&[]));
        assert_eq!(row(&rows, "key/k")[1], "fail", "{rows:?}");
        assert!(row(&rows, "key/k")[2].contains("resolved to nothing"), "{:?}", row(&rows, "key/k"));
    }

    #[test]
    fn accepts_an_empty_key_for_a_keyless_provider() {
        // `keys: {p: ""}` is the documented way to write a provider that needs no
        // credential, so failing it would make a valid install look broken.
        let yaml = "keys:\n  k: \"\"\nproviders:\n  - id: pollinations\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - pollinations/mistral-large-2411\n";
        let rows = checks(yaml, &probe(&[]));
        assert_eq!(row(&rows, "key/k")[1], "ok", "{rows:?}");
    }

    #[test]
    fn never_renders_a_credential_value_in_a_check_row() {
        let rows = checks(ONE_PROVIDER, &probe(&["k"]));
        for r in &rows {
            assert!(!r[2].contains("secret-AR_KEY"), "a check row leaked a value: {r:?}");
        }
    }

    #[test]
    fn keeps_every_check_detail_commaless() {
        // A list row is comma-delimited, so a comma in `detail` makes the row one
        // column wider than the header claims.
        let rows = checks(ONE_PROVIDER, &probe(&["k"]));
        for r in &rows {
            assert!(!r[2].contains(','), "a comma in a check detail: {r:?}");
        }
    }

    #[test]
    fn reports_the_snapshot_age_in_the_registry_row() {
        // The freshness signal F-HIGH-3 asks for: without a date in the row, a
        // stale catalog is invisible until a request 404s.
        let rows = checks(ONE_PROVIDER, &probe(&["k"]));
        let detail = &row(&rows, "registry")[2];
        assert!(detail.contains("snapshot") && detail.contains("d old"), "{detail}");
    }

    #[test]
    fn reads_a_build_made_today_as_a_fresh_registry() {
        // `ar-registry` stamps the build, and a test build is stamped now, so the
        // row is `ok` here and `warn` only once a build is genuinely old.
        let rows = checks(ONE_PROVIDER, &probe(&["k"]));
        assert_eq!(row(&rows, "registry")[1], "ok", "{rows:?}");
    }

    #[test]
    fn a_fresh_registry_row_does_not_name_the_import_fix() {
        // The fix belongs on the row that has something to fix; printing it on
        // every run is noise an agent has to parse past.
        let rows = checks(ONE_PROVIDER, &probe(&["k"]));
        assert!(!row(&rows, "registry")[2].contains("ar import"), "{rows:?}");
    }

    #[test]
    fn a_stale_registry_row_warns_and_names_the_import_fix() {
        // Age is the build's, so the test drives `registry_row` at a `now` far
        // enough past the stamp rather than waiting seven days.
        let row = registry_row_at(3, ar_registry::discovery::unix_now() + (ar_registry::SNAPSHOT_TTL_DAYS + 1) * 86_400);
        assert_eq!(row[1], "warn", "{row:?}");
        assert!(row[2].contains("ar import --from omniroute"), "{row:?}");
        assert!(row[2].contains('3'), "it counts the models it cannot route: {row:?}");
    }

    #[test]
    fn puts_the_store_beside_the_config_file_by_default() {
        assert_eq!(store_path(Path::new("config.yaml")), PathBuf::from("credentials.db"));
        assert_eq!(store_path(Path::new("/etc/ar/config.yaml")), PathBuf::from("/etc/ar/credentials.db"));
    }
}
