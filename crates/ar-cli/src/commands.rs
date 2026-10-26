//! Command handlers.
//!
//! Every handler writes data to stdout and returns `anyhow::Result`, per
//! `docs/03` (`anyhow` is confined to the binary) and `docs/06` (errors are
//! structured on stdout with an exact `help:` line, never a stack trace).
//!
//! The two async verbs live in [`crate::serve`]; everything here is synchronous
//! so the whole surface stays testable in-process without a runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ar_config::{Combo, Config, Strategy};
use ar_keys::{CredentialStore, KeyError};
use ar_registry::{Registry, global as registry};
use ar_server::config::split_target_known;
use clap::CommandFactory;
use clap::error::ErrorKind;

use crate::cli::{
    AddComboArgs, ClearLimitsArgs, Cli, ComboTargetArgs, ComboTargetArgs2, Command, ConfigureArgs,
    DoctorArgs, ImportArgs, ListArgs, RemoveComboArgs, ServiceAction, ServiceArgs, ServiceUnit,
    SetLimitsArgs, SyncArgs,
};
use crate::import;
use crate::import::ImportFrom;
use crate::serve;
use crate::toon;

/// One TOON row: positional cells, matching the command's `*_COLUMNS`.
type Row = Vec<String>;

/// Columns available from `aroute providers`.
///
/// `id,provider,status` must stay first: `toon::DEFAULT_FIELDS` is applied
/// positionally, and `default_columns_lead` below is what keeps that honest.
const PROVIDER_COLUMNS: [&str; 5] = ["id", "provider", "status", "base_url", "key"];
/// Columns available from `aroute models`.
const MODEL_COLUMNS: [&str; 4] = ["id", "provider", "status", "combo"];
/// Columns available from `aroute service`: systemd's own verdict, this
/// process's live probe, and the sentence that says which it is.
const SERVICE_COLUMNS: [&str; 4] = ["unit", "systemd", "probe", "detail"];
/// Columns available from `aroute combo`. `provider` is the combo's distinct
/// provider set: a combo is not owned by one provider, but that set is the fact
/// an agent asks for next.
const COMBO_COLUMNS: [&str; 5] = ["id", "provider", "status", "strategy", "targets"];
/// Columns available from `aroute sync`: one row per combo the dashboard
/// handed over.
const SYNC_COLUMNS: [&str; 5] = ["id", "provider", "status", "strategy", "targets"];
/// Columns of an `aroute limits` row. `key` leads but is not `id`: the
/// `default` row is a row like any other here, and the column names it.
const LIMIT_COLUMNS: [&str; 8] = [
    "key",
    "rpm",
    "usd_micros",
    "tokens",
    "refuse_unpriced",
    "model",
    "model_tokens",
    "spend_window",
];
/// Columns of a `doctor` finding.
const CHECK_COLUMNS: [&str; 3] = ["check", "status", "detail"];
/// Columns of `aroute server show`.
///
/// `host_changed` / `port_changed` rather than one `is_default` cell: the caller
/// can see which of the two moved, which one boolean would hide.
const SERVER_COLUMNS: [&str; 5] = ["host", "port", "host_changed", "port_changed", "default"];

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
                format!("unknown field {f:?} for `aroute {cmd}`"),
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

/// The parsed config for a **writer**: the same file, read without expanding
/// its `$VAR` references.
///
/// [`load`] expands, because everything that *serves* has to: a proxy that
/// boots with `$AR_KEY_OPENAI` as a literal credential would send the string
/// "$AR_KEY_OPENAI" as the bearer. A writer does not. It rearranges sections
/// and names variables, and a config whose other `keys:` rows point at
/// unexported variables is exactly the config an operator is editing
/// *because* a credential is missing — so a writer must be able to open it.
///
/// `webui.rs`'s `load_cfg` is the same call with the same resolver shape; this
/// is the CLI-side spelling, and the two agreeing is what lets the panel and
/// the CLI edit one file.
pub fn load_edit(cli: &Cli) -> anyhow::Result<Config> {
    let doc = read_config_text(cli)?;
    Config::parse(&doc, |_| Ok(None)).map_err(|e| {
        fail(
            e,
            format!(
                "fix {} by hand; a write refuses a config it cannot load",
                cli.config.display()
            ),
        )
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
    if let Some(p) = std::env::var(STORE_PATH_VAR)
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
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
/// Shared by `aroute serve` and `aroute mcp`: both resolve credentials the same way, and
/// two copies of this fallback is two chances for them to disagree.
pub fn credential_store(cli: &Cli) -> Option<CredentialStore> {
    let path = store_path(&cli.config);
    match open_store(&path) {
        Ok(store) => Some(store),
        Err(e) if path.exists() => {
            eprintln!(
                "ar: credential store {} is unusable ({e}); falling back to $VAR",
                path.display()
            );
            None
        }
        Err(_) => None,
    }
}

/// What `ar` found at the credential-store path.
///
/// Public because `aroute auth` resolves the same probe `aroute doctor` prints, and two
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
            Err(e) => Self::Unreadable {
                path,
                reason: e.to_string(),
            },
        }
    }

    /// The `(status, detail)` cell for the `store` row.
    ///
    /// Absent is `skip`, not `fail`: env-only is the configuration every install
    /// before this feature had, and failing it would make `aroute doctor` exit 1 on
    /// every host that has not opted in.
    fn row(&self) -> (String, String) {
        match self {
            Self::Absent(path) => (
                "skip".to_owned(),
                format!("no store at {}; $VAR only", path.display()),
            ),
            Self::Open { path, names } => (
                "ok".to_owned(),
                format!(
                    "{} readable with {} credential(s)",
                    path.display(),
                    names.len()
                ),
            ),
            Self::Live { path, store } => match store.list_names() {
                Ok(names) => (
                    "ok".to_owned(),
                    format!(
                        "{} readable with {} credential(s)",
                        path.display(),
                        names.len()
                    ),
                ),
                Err(e) => (
                    "fail".to_owned(),
                    format!("{} unreadable: {e}", path.display()),
                ),
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
            Self::Live { store, .. } => store
                .list_names()
                .is_ok_and(|names| names.iter().any(|n| n == name)),
            _ => false,
        }
    }
}

/// The catalog this config routes against: the compiled-in providers plus its
/// `custom_providers:` nodes.
///
/// A collision is fatal rather than skipped — shadowing a compiled-in id would
/// make the catalog stop describing the world, and `aroute doctor` says which one.
fn catalog(cfg: &Config) -> anyhow::Result<Registry> {
    registry().merge(&cfg.custom_providers).map_err(|e| {
        fail(
            e,
            "rename the `custom_providers` entry so its `id:` is not a compiled-in provider",
        )
    })
}

/// Runs `future` on a tokio runtime built on demand.
///
/// Built here rather than by `#[tokio::main]` so the five local verbs never pay
/// for a runtime: `--version`, the home view and the lists answer without one.
pub fn block_on<F: std::future::Future<Output = anyhow::Result<()>>>(
    future: F,
) -> anyhow::Result<()> {
    block_on_value(future)
}

/// [`block_on`] for a verb that needs the result — `aroute import`'s single fetch.
pub fn block_on_value<F, T>(future: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            fail(
                e,
                "the async runtime could not start; this is a host problem, not a config one",
            )
        })?;
    rt.block_on(future)
}

/// The `aroute auth status` probe for a caller that already holds a store.
///
/// Names over values, so a status listing never decrypts a row it does not need
/// — the same discipline [`StoreProbe::resolve`] keeps for `aroute doctor`.
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
    StoreProbe::Live {
        path: path.to_path_buf(),
        store,
    }
}

/// The `aroute auth status` spelling of a doctor row's status, as a borrowed
/// `&'static str`.
///
/// The doctor vocabulary is `ok`/`fail` because a `fail` there exits 1; a status
/// listing has no exit to justify, so it names the state the operator is asking
/// about. Derived from the row's own status rather than re-decided, which is what
/// keeps the two surfaces from drifting.
pub fn armed_spelling(status: &str) -> &'static str {
    if status == "ok" { "armed" } else { "unarmed" }
}

/// The credential-store row the HTTP gate's master key lives in.
///
/// Written by `aroute keys arm-gate`, read by `aroute serve` at boot and by
/// `aroute doctor`'s gate row. One constant rather than a config field: the row
/// name is a contract between three commands, and a typo in one of three
/// string literals is a gate that stores a master key nothing reads.
pub const GATE_ROW: &str = "http-gate";

/// The provider column the gate row is stored under. Part of the row's AAD,
/// so changing it makes every stored gate master unreadable — which is the
/// point: the row says which command wrote it.
const GATE_PROVIDER: &str = "aroute";

/// How long a client token `aroute keys` mints lives by default: thirty days.
///
/// The library default is fifteen minutes, which is right for an interactive
/// login and wrong for a single-operator gateway's only credential — the
/// renew story here is "mint again", and a 15-minute rotation on that story is
/// 96 tokens a day for nothing. `Issue::ttl` is the knob, and
/// `keys mint --ttl-days` exposes it: `0` mints a token that does not expire,
/// which is what a harness that reads its key from a file wants.
const DEFAULT_TOKEN_TTL_DAYS: u32 = 30;

/// `aroute keys arm-gate` — docs/15 phase 3: close the last shipped gap between
/// "a gate exists" and "a command arms it".
///
/// Generates (or accepts) the 32-byte master, stores it as [`GATE_ROW`], mints
/// the first client token, and prints it once with its expiry. The master is
/// never printed: it lives encrypted in the store, and the token is the thing
/// a client actually presents.
///
/// Refuses to overwrite an existing gate row. `store.insert` replaces
/// unconditionally, so without this guard a stray arming invalidated every token
/// signed by the previous master with no warning. Use [`arm_gate_forced`],
/// the `--force` path, when replacing is the intent.
pub(crate) fn gate_overwrite_allowed(store: &CredentialStore, replace: bool) -> bool {
    if !replace && gate_row_exists(store) {
        return false;
    }
    true
}

/// Whether the store already holds a gate master row.
///
/// Read-only and separate from [`arm_gate`] so the refusal is decidable before
/// any secret is generated: refusing must not be able to leave a new master
/// half-written, and generating first would do exactly that.
pub(crate) fn gate_row_exists(store: &CredentialStore) -> bool {
    matches!(store.get(GATE_ROW), Ok(Some(_)))
}

pub fn arm_gate(cli: &Cli, key_id: &str, explicit: Option<&str>) -> anyhow::Result<()> {
    arm_gate_with(cli, key_id, explicit, false)
}

/// `aroute arm-gate --force` — the same arming, over an existing row.
pub fn arm_gate_forced(cli: &Cli, key_id: &str, explicit: Option<&str>) -> anyhow::Result<()> {
    arm_gate_with(cli, key_id, explicit, true)
}

/// The body of `arm-gate`. `replace` carries the caller's `--force`.
fn arm_gate_with(
    cli: &Cli,
    key_id: &str,
    explicit: Option<&str>,
    replace: bool,
) -> anyhow::Result<()> {
    let Some(store) = credential_store(cli) else {
        return Err(fail(
            "no credential store to hold the gate master",
            "export $AR_MASTER_KEY (from `openssl rand -hex 32`) so a store can be opened, then retry",
        ));
    };
    // Before generating anything: if this would clobber a live master, say so
    // and stop. Refusing must leave the store exactly as it was, which is only
    // true when the check runs before `Secret::generate`.
    let existed = gate_row_exists(&store);
    if !gate_overwrite_allowed(&store, replace) {
        return Err(fail(
            "the gate is already armed; arming again REPLACES the current master",
            "pass --force to replace it, knowing every token signed by the current master stops working",
        ));
    }
    let master = match explicit {
        None => ar_keys::Secret::generate(),
        Some(hex) => decode_hex(hex)
            .ok_or_else(|| {
                fail(
                    "--key is not 32 bytes of hex",
                    "pass 64 hex characters, e.g. $(openssl rand -hex 32)",
                )
            })
            .and_then(|bytes| {
                ar_keys::Secret::from_slice(&bytes)
                    .map_err(|e| fail(e, "the gate master must be exactly 32 bytes"))
            })?,
    };
    store
        .insert(GATE_PROVIDER, GATE_ROW, &master)
        .map_err(|e| {
            fail(
                e,
                "the store refused the gate row; is the store on a read-only disk?",
            )
        })?;
    if replace && existed {
        // Only reachable on the --force path, and it is the whole point of the
        // flag: the operator has just invalidated every existing token.
        println!("gate: the previous master is replaced — every token signed by it stops working");
    }
    let issued = mint_from(&master, key_id)?;
    println!("gate: armed — master stored as credential {GATE_ROW} (never printed)");
    println!("key id: {key_id}");
    println!("token: {}", issued.access);
    println!(
        "expires: {} (unix) — `aroute keys mint` issues the next one",
        issued.expires_at
    );
    println!("use: Authorization: Bearer <token>");
    Ok(())
}

/// `aroute keys revoke` — kill one token without touching the others.
///
/// The token's `jti` is written to the revocation table in the credential
/// store, and every gate that consults that table refuses it from then on.
/// The master is **not** touched, so every other token keeps working — which
/// is the whole point of this verb against rotating the master, which
/// invalidates all of them at once.
///
/// The token is read from a file or stdin, never from an argument: a token on
/// a command line is in the shell history and in the process table.
pub fn keys_revoke(cli: &Cli, token_file: &str) -> anyhow::Result<()> {
    let token = read_token_file(token_file)?;
    let Some(store) = credential_store(cli) else {
        return Err(fail(
            "no credential store to write the revocation into",
            "the revocation table lives in the credential store; run this where the store is",
        ));
    };
    let Some(master) = store.get(GATE_ROW).map_err(|e| {
        fail(
            e,
            "the gate row could not be decrypted; was $AR_MASTER_KEY rotated?",
        )
    })?
    else {
        return Err(fail(
            "the gate is not armed — no master key row in the store",
            "a token can only be revoked against the master that signed it",
        ));
    };
    // The same `Tokens` engine the server's gate builds, so the `jti` the
    // server reads is the `jti` written here. The `KeyMeta` is a fresh salt:
    // it only ever feeds the AEAD key, and the SIGNING key is the master
    // bytes themselves, so a fresh salt still verifies the same tokens.
    let tokens = ar_keys::Tokens::new(&ar_keys::MasterKey::new(
        master.to_owned_secret(),
        ar_keys::KeyMeta::generate(),
    )?);
    let revocations = ar_keys::Revocations::open(&store_path(&cli.config), ar_keys::DEFAULT_CAP)
        .map_err(|e| {
            fail(
                e,
                "the revocation table could not be opened; is the store's directory writable?",
            )
        })?;
    tokens.revoke(&token, &revocations).map_err(|e| {
        fail(
            e,
            "the token could not be revoked; it may be malformed, expired, or signed by another master",
        )
    })?;
    let verified = tokens
        .introspect(&token)
        .map_err(|e| fail(e, "the token could not be read back"))?;
    println!("revoked: key id {} (jti {})", verified.key_id, verified.jti);
    println!(
        "expires: {} (unix) — the entry is swept after this",
        verified.expires_at
    );
    Ok(())
}

/// The token to revoke, from a file or from stdin.
///
/// A token is a secret. Reading it from a file keeps it out of the shell
/// history and out of the process table, which is why the verb takes a path
/// and not the token itself.
fn read_token_file(path: &str) -> anyhow::Result<String> {
    let raw = if path == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .map_err(|e| fail(e, "the token could not be read from stdin"))?;
        buf
    } else {
        std::fs::read_to_string(path).map_err(|e| fail(e, "the token file could not be read"))?
    };
    let token = raw.trim();
    if token.is_empty() {
        return Err(fail(
            "the token is empty",
            "the file must hold the token, and nothing else",
        ));
    }
    Ok(token.to_owned())
}

/// `aroute keys list` — what the credential store holds, read only.
///
/// The B-5 find: the `keys` subtree could mint and revoke but not inspect, so
/// an operator asked "which credentials exist?" had no answer and an agent had
/// none at all. This reports the four things that are actually knowable — the
/// row names, the OAuth session count, the revocation count, and the gate's
/// decrypted state — and states plainly what is NOT knowable.
///
/// That last part is the honesty the verb exists for: the store keeps no table
/// of issued tokens. A token list would be a fabrication, so the output says
/// "minted tokens are not recorded; a lost token cannot be revoked" rather than
/// printing an empty table an agent would read as "there are none".
pub fn keys_list(cli: &Cli) -> anyhow::Result<()> {
    let store_path = store_path(&cli.config);
    if !store_path.exists() {
        println!("store: absent — {} does not exist", store_path.display());
        println!("note: keys fall back to `keys:` in the config; $AR_KEY_* env vars resolve there");
        return Ok(());
    }
    let Some(store) = credential_store(cli) else {
        // `credential_store` already said why on stderr; stdout stays the data
        // channel, and a store that will not open is not "nothing stored".
        return Ok(());
    };

    let names = store
        .list_names()
        .map_err(|e| fail(e, "the store's row names could not be read"))?;
    let sessions = store.session_count().unwrap_or(0);

    // The revocation table lives INSIDE the credential store, at the same path
    // `keys_revoke` opens (`commands.rs:508`) — not a sibling file. Opening it
    // read-only here matters for the same reason it did in `cost-report`: a
    // verb that reports on a table must not create one.
    let revoked = ar_keys::Revocations::open(&store_path, ar_keys::DEFAULT_CAP)
        .map(|t| t.len().unwrap_or(0))
        .unwrap_or(0);

    // Three answers, matching `doctor`'s gate check rather than inventing a
    // fourth: absent, armed-and-decryptable, and present-but-unreadable.
    let gate = match store.get(GATE_ROW) {
        Ok(Some(_)) => "armed",
        Ok(None) => "not armed",
        Err(_) => "armed but UNREADABLE under the current $AR_MASTER_KEY",
    };

    println!("store: {}", store_path.display());
    println!("rows: {} ({})", names.len(), names.join(", "));
    println!("oauth sessions: {sessions}");
    println!("revoked tokens: {revoked}");
    println!("gate: {gate}");
    println!(
        "minted tokens are not recorded; a lost token cannot be revoked — mint a new one with `aroute keys mint`"
    );
    Ok(())
}

/// `aroute keys mint` — another client token from the stored gate master.
pub fn keys_mint(cli: &Cli, key_id: &str, ttl_days: u32) -> anyhow::Result<()> {
    let Some(store) = credential_store(cli) else {
        return Err(fail(
            "no credential store to read the gate master from",
            "run `aroute keys arm-gate` first; it stores the master the gate verifies against",
        ));
    };
    let Some(master) = store.get(GATE_ROW).map_err(|e| {
        fail(
            e,
            "the gate row could not be decrypted; was $AR_MASTER_KEY rotated?",
        )
    })?
    else {
        return Err(fail(
            "the gate is not armed — no master key row in the store",
            "run `aroute keys arm-gate` first; it generates and stores the master",
        ));
    };
    let issued = mint_from_with_ttl(&master, key_id, ttl_days)?;
    println!("key id: {key_id}");
    println!("token: {}", issued.access);
    println!("expires: {} (unix)", issued.expires_at);
    Ok(())
}

/// The one minting path both verbs share, so a token printed by either is
/// verifiable by the gate `aroute serve` builds from the same master.
///
/// `Tokens::new` (no audience) is the same engine `AuthGate::new` builds — the
/// pairing that makes a CLI-minted token pass the server's gate is structural,
/// not tested-in.
pub(crate) fn mint_from(master: &ar_keys::Secret, key_id: &str) -> anyhow::Result<ar_keys::Issued> {
    mint_from_with_ttl(master, key_id, DEFAULT_TOKEN_TTL_DAYS)
}

/// The same minting path with a caller-chosen lifetime.
///
/// `0` days mints a token that does not expire. A harness that reads its key
/// from a file wants exactly that: one credential, written once, valid until
/// the operator rotates it deliberately. The shorter default stays for the
/// case where a token is handed to something that may leak.
pub(crate) fn mint_from_with_ttl(
    master: &ar_keys::Secret,
    key_id: &str,
    ttl_days: u32,
) -> anyhow::Result<ar_keys::Issued> {
    let tokens = ar_keys::Tokens::new(&ar_keys::MasterKey::new(
        master.to_owned_secret(),
        ar_keys::KeyMeta::generate(),
    )?);
    let ttl = if ttl_days == 0 {
        // Far enough out that "never" and "the year 2100" are the same claim to
        // every reader, including a clock that is wrong by a decade.
        std::time::Duration::from_secs(365 * 24 * 3600 * 100)
    } else {
        std::time::Duration::from_secs(u64::from(ttl_days) * 24 * 3600)
    };
    tokens
        .issue(ar_keys::Issue {
            key_id,
            scopes: ar_keys::ScopeSet::of([ar_keys::Scope::ExecuteCompletions]),
            device_id: None,
            ttl: Some(ttl),
        })
        .map_err(|e| {
            fail(
                e,
                "the token could not be minted; is the clock before the Unix epoch?",
            )
        })
}

/// Lowercase-or-uppercase hex, `None` on any non-hex character or odd length —
/// the same contract `ar-keys`' own codec keeps, written here because that
/// module is deliberately `pub(crate)` there.
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some((hi << 4 | lo) as u8)
        })
        .collect()
}

/// The gate's master bytes from a store, for `aroute serve`'s boot read and the
/// doctor row. `Ok(None)` when the gate was never armed.
///
/// A row that exists but will not decrypt is an error rather than `None`:
/// silently treating a broken gate row as "not armed" would arm nothing and
/// report `skip`, which is exactly the confusion the row exists to prevent.
pub fn gate_master(store: &CredentialStore) -> Result<Option<ar_keys::Secret>, KeyError> {
    store.get(GATE_ROW)
}

/// The config file's text, for the verbs that splice a section of it.
fn read_config_text(cli: &Cli) -> anyhow::Result<String> {
    std::fs::read_to_string(&cli.config).map_err(|e| {
        fail(
            format!("cannot read {}: {e}", cli.config.display()),
            "the limits verbs edit the config in place; pass --config <PATH>",
        )
    })
}

/// `aroute limits` — the block as one TOON row per key.
///
/// The `default` row is always first, because it is the row anonymous traffic
/// and any key without its own get; printing it only when it limits something
/// would hide the row that governs the rest.
pub fn limits_view(cli: &Cli, json: bool) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let rows: Vec<Row> = std::iter::once(("default".to_owned(), cfg.limits.default.clone()))
        .chain(
            cfg.limits
                .keys
                .iter()
                .map(|(name, row)| (name.clone(), row.clone())),
        )
        .map(|(name, row)| {
            vec![
                name,
                opt_cell(row.rpm),
                opt_cell(row.usd_micros),
                opt_cell(row.tokens),
                row.refuse_unpriced.to_string(),
                opt_cell(row.model.as_deref()),
                // The per-target ceilings, one cell: a row can name several, and
                // one column per target would make the table's width depend on
                // the config.
                opt_cell(Some(
                    row.model_tokens
                        .iter()
                        .map(|(target, ceiling)| format!("{target}={ceiling}"))
                        .collect::<Vec<_>>()
                        .join(","),
                )),
                opt_cell(row.spend_window.as_deref()),
            ]
        })
        .collect();
    if json {
        print!(
            "{}",
            toon::json(
                "limits",
                &LIMIT_COLUMNS,
                &toon::every_field(&LIMIT_COLUMNS),
                &rows,
            )
        );
    } else {
        print!(
            "{}",
            toon::list(
                "limits",
                "limits",
                &LIMIT_COLUMNS,
                &toon::every_field(&LIMIT_COLUMNS),
                &rows,
                false,
            )
        );
    }
    Ok(())
}

/// The cell for an unset arm: `-`, because an empty cell reads as a broken row.
fn opt_cell(value: Option<impl std::fmt::Display>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

/// The model scope a `model` value names, checked the way `/ui` checks it: at
/// most 128 characters with no control characters, and, when the name has no
/// `/`, a combo of that id must exist, so a typo cannot arm a ceiling that
/// never applies. `Ok(None)` is the clear, for `""` or whitespace.
pub(crate) fn limit_scope(
    raw: &str,
    combos: &[ar_config::Combo],
) -> Result<Option<String>, (String, &'static str)> {
    let scope = raw.trim();
    if scope.is_empty() {
        return Ok(None);
    }
    if scope.len() > 128 || scope.chars().any(char::is_control) {
        return Err((
            "model scope is not a legal id".to_owned(),
            "up to 128 characters, no control characters",
        ));
    }
    if !scope.contains('/') && !combos.iter().any(|c| c.id == scope) {
        return Err((
            format!("no combo named {scope:?}"),
            "scope a limit to a combo id, or to provider/model",
        ));
    }
    Ok(Some(scope.to_owned()))
}

/// `aroute limits set` — merge the named arms into one row, rewrite the section.
///
/// The rewrite goes through [`import::limits::render`], the one renderer for
/// the section, so a row this command writes and a row sync carries cannot
/// drift on spelling.
pub fn limits_set(cli: &Cli, args: &SetLimitsArgs) -> anyhow::Result<()> {
    // A zero arm is the one value the parser refuses, and the one a writer
    // must refuse too: `0` reads as "never" to an operator and "refill from
    // empty forever" to a bucket, and this command cannot write what `ar
    // serve` would then refuse to load.
    for (field, zero) in [
        ("rpm", args.rpm.is_some_and(|v| v == 0)),
        ("usd_micros", args.usd_micros.is_some_and(|v| v == 0)),
        ("tokens", args.tokens.is_some_and(|v| v == 0)),
    ] {
        if zero {
            return Err(fail(
                format!("--{field} 0 is refused"),
                "0 means \"never\" to an operator and \"refill from empty forever\" to a bucket; omit the arm to leave it unlimited",
            ));
        }
    }
    if args.rpm.is_none()
        && args.usd_micros.is_none()
        && args.tokens.is_none()
        && args.refuse_unpriced.is_none()
        && args.model.is_none()
        && args.model_tokens.is_empty()
        && args.spend_window.is_none()
    {
        return Err(fail(
            "no arm given",
            "pass at least one of --rpm, --usd-micros, --tokens, --refuse-unpriced, --model, --model-tokens, --spend-window",
        ));
    }
    let doc = read_config_text(cli)?;
    let mut cfg = load(cli)?;
    // Resolved before the row is borrowed: a scope that names no combo is refused
    // here, not written. `None` leaves the scope as it is.
    let scope = match &args.model {
        Some(raw) => Some(limit_scope(raw, &cfg.combos).map_err(|(m, h)| fail(m, h))?),
        None => None,
    };
    // The per-target ceilings, parsed and checked against the config's own
    // combos: a ceiling for a target nothing routes to is a ceiling that never
    // applies.
    let mut model_tokens = Vec::new();
    for raw in &args.model_tokens {
        let parsed = parse_model_tokens(raw).map_err(|(m, h)| fail(m, h))?;
        if !routable_target(&parsed.target, &cfg) {
            return Err(fail(
                format!("no combo routes to {:?}", parsed.target),
                "name a target a combo routes to, e.g. nvidia/z-ai/glm-5.3",
            ));
        }
        model_tokens.push(parsed);
    }
    if let Some(window) = &args.spend_window
        && ar_config::parse_window(window).is_none()
    {
        return Err(fail(
            format!("--spend-window {window:?} is not a window"),
            "a window is a length: 30m, 24h, 7d, or a whole number of seconds",
        ));
    }
    let label = args.key.clone().unwrap_or_else(|| "default".to_owned());
    // The merge is per arm: a named arm is set, an arm not passed stays as it
    // was — `set --rpm` must not quietly drop a ceiling the row already
    // carried. Removing an arm is `clear`'s job, or the dashboard's.
    //
    // `default` is not a key. It names the row every unnamed request falls
    // through to, which is a field of `limits:` itself and not an entry in
    // `limits.keys:`. Writing it as a key left the operator with two rows that
    // both answer to "default", and the one they edited was not the one the
    // engine reads. The doc-comments on every limit error say the key is "the
    // key the row governs, or \"default\"" — so the header they omit and the
    // header that spells it are the same row.
    let row = match &args.key {
        Some(name) if name != "default" => cfg.limits.keys.entry(name.clone()).or_default(),
        _ => &mut cfg.limits.default,
    };
    row.rpm = args.rpm.or(row.rpm);
    row.usd_micros = args.usd_micros.or(row.usd_micros);
    row.tokens = args.tokens.or(row.tokens);
    if let Some(refuse) = args.refuse_unpriced {
        row.refuse_unpriced = refuse;
    }
    if let Some(scope) = scope {
        row.model = scope;
    }
    // The per-target ceilings merge the same way: a named target is set, a
    // target not named keeps what the row already carried.
    for entry in &model_tokens {
        row.model_tokens.insert(entry.target.clone(), entry.ceiling);
    }
    if let Some(window) = &args.spend_window {
        row.spend_window = Some(window.clone());
    }
    // A scope alone limits nothing, so a row with only a scope is empty.
    if row.is_empty() {
        return Err(fail(
            format!("the {label} row would be empty"),
            "nothing to set — pass at least one arm, or `aroute limits clear` removes the row",
        ));
    }
    let summary = (
        opt_cell(row.rpm),
        opt_cell(row.usd_micros),
        opt_cell(row.tokens),
        row.refuse_unpriced.to_string(),
        opt_cell(row.model.as_deref()),
        opt_cell(row.spend_window.as_deref()),
    );
    let targets: Vec<String> = row
        .model_tokens
        .iter()
        .map(|(target, ceiling)| format!("{target}={ceiling}"))
        .collect();
    let next = splice_section(&doc, "limits", &import::limits::render(&cfg.limits));
    write_file(&cli.config, &next)?;
    println!(
        "limits: {label} — rpm={} usd_micros={} tokens={} refuse_unpriced={} model={} spend_window={} model_tokens={}",
        summary.0,
        summary.1,
        summary.2,
        summary.3,
        summary.4,
        summary.5,
        if targets.is_empty() {
            "-".to_owned()
        } else {
            targets.join(",")
        }
    );
    // The running proxy builds its policy at boot, so a written ceiling is
    // inert until a restart. Saying so is the B-3 contract (`docs/36` step 3):
    // a write that does not name the one step that makes it live lets an
    // operator believe a ceiling is protecting them when it is not. Printing
    // the exact command — rather than restarting — keeps the write and the
    // state change two decisions, the same split `service enable` makes.
    println!(
        "note: the running proxy reads this at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// The listen address, and whether either field differs from the compiled default.
///
/// "Is this the default?" is the first question after "what is it?", and a
/// caller that has to know the default to answer it is a caller that re-derives
/// `Server::default()` in a second place. Compared against the `Default` impl
/// rather than a literal pair for the same reason.
fn server_show(cli: &Cli, json: bool) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let d = ar_config::Server::default();
    let row = vec![
        cfg.server.host.clone(),
        cfg.server.port.to_string(),
        (cfg.server.host != d.host).to_string(),
        (cfg.server.port != d.port).to_string(),
        format!("{}:{}", d.host, d.port),
    ];
    if json {
        print!(
            "{}",
            toon::json(
                "server",
                &SERVER_COLUMNS,
                &toon::every_field(&SERVER_COLUMNS),
                &[row],
            )
        );
    } else {
        print!(
            "{}",
            toon::list(
                "server",
                "servers",
                &SERVER_COLUMNS,
                &toon::every_field(&SERVER_COLUMNS),
                &[row],
                false,
            )
        );
    }
    Ok(())
}

/// Moves the listen address, rewriting only the `server:` section.
///
/// The merge is per field: a flag not passed keeps the value the file already
/// carries, so `set --port 20129` cannot quietly change the host. An empty host
/// or a port of `0` is refused naming the field — the panel's validation
/// (`webui.rs:1343`, `:1351`) — because a write `ar serve` then refuses to load
/// is worse than a refusal.
fn server_set(cli: &Cli, args: &crate::cli::SetServerArgs) -> anyhow::Result<()> {
    if args.host.is_none() && args.port.is_none() {
        return Err(fail("no field given", "pass --host, --port, or both"));
    }
    if let Some(host) = &args.host
        && host.trim().is_empty()
    {
        return Err(fail(
            "--host is empty",
            "the listen address must be a host or an IP, e.g. 127.0.0.1",
        ));
    }
    if args.port == Some(0) {
        return Err(fail(
            "--port 0 is refused",
            "port 0 tells a socket to pick any free port and tells a reader nothing; the port must be between 1 and 65535",
        ));
    }
    let doc = read_config_text(cli)?;
    let mut cfg = load(cli)?;
    if let Some(host) = &args.host {
        cfg.server.host = host.trim().to_owned();
    }
    if let Some(port) = args.port {
        cfg.server.port = port;
    }
    // Written raw rather than through `serde_json::Value`, which is what the
    // panel does and which quotes the host: it is a JSON serializer, so
    // `127.0.0.1` comes back as `"127.0.0.1"` and the config gains quotes YAML
    // did not need. A bare host is a valid YAML scalar, so it is written as the
    // operator would read it, and the loader parses both forms identically.
    //
    // No trailing newline after `port:` — `splice_section` re-joins the file's
    // own lines and appends exactly one newline, so a rendered block that ends
    // with `\n` would leave the config with a blank line where the operator
    // had none.
    let section = format!(
        "server:\n  host: {}\n  port: {}",
        cfg.server.host, cfg.server.port
    );
    let next = splice_section(&doc, "server", &section);
    if next == doc {
        println!(
            "server: {}:{} — unchanged",
            cfg.server.host, cfg.server.port
        );
        return Ok(());
    }
    write_file(&cli.config, &next)?;
    println!("server: {}:{}", cfg.server.host, cfg.server.port);
    // The listen address is bound at boot, so a written address is inert until a
    // restart. The B-3 contract (`docs/36` step 3), the same note `limits set`
    // prints: naming the restart keeps the write and the state change two
    // decisions rather than one.
    println!(
        "note: the running proxy binds this address at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// One `--model-tokens TARGET=CEILING` pair, parsed.
struct ModelTokens {
    /// The `provider/model` the ceiling governs.
    target: String,
    /// The ceiling, in tokens.
    ceiling: u64,
}

/// Parses a `--model-tokens TARGET=CEILING` value.
///
/// The split is on the **last** `=`, because a model name never contains one
/// and the forgiving choice costs nothing.
fn parse_model_tokens(raw: &str) -> Result<ModelTokens, (String, &'static str)> {
    let Some((target, ceiling)) = raw.rsplit_once('=') else {
        return Err((
            format!("--model-tokens {raw:?} is not TARGET=CEILING"),
            "name the target and its ceiling, e.g. --model-tokens nvidia/z-ai/glm-5.3=500000000",
        ));
    };
    let target = target.trim();
    if target.is_empty() {
        return Err((
            "--model-tokens names no target".to_owned(),
            "name the target, e.g. --model-tokens nvidia/z-ai/glm-5.3=500000000",
        ));
    }
    let ceiling: u64 = ceiling.trim().parse().map_err(|_| {
        (
            format!("--model-tokens ceiling {ceiling:?} is not a number"),
            "the ceiling is a whole number of tokens",
        )
    })?;
    if ceiling == 0 {
        return Err((
            "--model-tokens 0 is refused".to_owned(),
            "0 means \"never\" to an operator and \"unlimited\" to the engine; omit the arm to leave it unlimited",
        ));
    }
    Ok(ModelTokens {
        target: target.to_owned(),
        ceiling,
    })
}

/// Whether any combo routes to `target`.
///
/// The check is against the config's own targets, so a typo cannot arm a
/// ceiling that never applies. A target with no `/` is a bare provider, which
/// the server resolves to that provider's first model — so the check accepts
/// it too, because the server's resolution is the authority and it refuses
/// the whole config at load if the target does not exist.
fn routable_target(target: &str, cfg: &ar_config::Config) -> bool {
    cfg.combos.iter().any(|c| {
        c.targets
            .iter()
            .chain(c.pool.iter())
            .any(|t| t == target || t.split_once('/').is_none_or(|(p, _)| p == target))
    })
}

/// `aroute limits clear` — remove one key's row (`--key`), or the whole block.
pub fn limits_clear(cli: &Cli, args: &ClearLimitsArgs) -> anyhow::Result<()> {
    let doc = read_config_text(cli)?;
    match &args.key {
        Some(name) if name != "default" => {
            let mut cfg = load(cli)?;
            if cfg.limits.keys.remove(name).is_none() {
                return Err(fail(
                    format!("no limits row for key {name:?}"),
                    "`aroute limits` prints the rows that exist",
                ));
            }
            // Removing the last row can empty the block, and an empty
            // `limits:` is not a block at all — the section goes rather than
            // leaving a null body behind.
            let next = if cfg.limits.is_empty() {
                remove_section(&doc, "limits")
            } else {
                splice_section(&doc, "limits", &import::limits::render(&cfg.limits))
            };
            write_file(&cli.config, &next)?;
            println!("limits: key {name:?} removed");
        }
        // `--key default` names the row every unnamed request falls through
        // to, which is a field of `limits:` and not an entry in `keys:`. It
        // resets to the defaults rather than vanishing, because the row is
        // the one the engine falls back to: there is no "no default" state
        // to leave behind.
        Some(_) => {
            let mut cfg = load(cli)?;
            cfg.limits.default = ar_config::Limit::default();
            let next = if cfg.limits.is_empty() {
                remove_section(&doc, "limits")
            } else {
                splice_section(&doc, "limits", &import::limits::render(&cfg.limits))
            };
            write_file(&cli.config, &next)?;
            println!("limits: default row reset");
        }
        None => {
            let next = remove_section(&doc, "limits");
            if next == doc {
                return Err(fail(
                    "no limits block in the config",
                    "nothing to remove; `aroute limits` prints what is there",
                ));
            }
            write_file(&cli.config, &next)?;
            println!("limits: block removed");
        }
    }
    Ok(())
}

/// Dispatches a parsed command.
pub fn dispatch(cli: &Cli) -> anyhow::Result<()> {
    match &cli.command {
        None => home(cli),
        Some(Command::Serve(a)) => block_on(serve::serve(cli, a)),
        Some(Command::Models(a)) => models(cli, a),
        Some(Command::Providers(a)) => match &a.command {
            None => providers(cli, &a.list_args()),
            Some(crate::cli::ProviderCommand::List(args)) => providers(cli, args),
            Some(crate::cli::ProviderCommand::Add(args)) => providers_add(cli, args),
            Some(crate::cli::ProviderCommand::Remove(args)) => providers_remove(cli, args),
            Some(crate::cli::ProviderCommand::AddCustom(args)) => providers_add_custom(cli, args),
        },
        Some(Command::Combo(a)) => match &a.command {
            None => combo(cli, &a.list_args()),
            Some(crate::cli::ComboCommand::List(args)) => combo(cli, args),
            Some(crate::cli::ComboCommand::Add(args)) => combo_add(cli, args),
            Some(crate::cli::ComboCommand::Remove(args)) => combo_remove(cli, args),
            Some(crate::cli::ComboCommand::Target(args)) => combo_target(cli, args),
        },
        Some(Command::Visibility(a)) => visibility(cli, a),
        Some(Command::ProviderSettings(a)) => provider_settings(cli, a),
        Some(Command::ContinueOnCut(a)) => continue_on_cut(cli, a),
        Some(Command::Oauth(a)) => oauth(cli, a),
        Some(Command::Doctor(a)) => doctor(cli, a),
        Some(Command::Auth(a)) => crate::auth::run(cli, a),
        Some(Command::Run(a)) => block_on(serve::run(cli, a)),
        Some(Command::Configure(a)) => configure(cli, a),
        Some(Command::Import(a)) => import_config(a),
        Some(Command::Sync(a)) => sync_config(cli, a),
        #[cfg(feature = "mcp")]
        Some(Command::Mcp(a)) => block_on(crate::mcp::run(cli, a)),
        Some(Command::Dashboard(a)) => crate::dashboard::run(a),
        Some(Command::Donate) => {
            println!("Star the repo  : https://github.com/ishan-parihar/artificial-route");
            println!("GitHub Sponsors: https://github.com/sponsors/ishan-parihar");
            println!("Razorpay (UPI) : https://rzp.io/rzp/ishan-parihar");
            Ok(())
        }
        Some(Command::Keys(a)) => match &a.command {
            crate::cli::KeysCommand::List => keys_list(cli),
            crate::cli::KeysCommand::ArmGate(args) => {
                if args.force {
                    arm_gate_forced(cli, &args.key_id, args.key.as_deref())
                } else {
                    arm_gate(cli, &args.key_id, args.key.as_deref())
                }
            }
            crate::cli::KeysCommand::Mint(args) => keys_mint(cli, &args.key_id, args.ttl_days),
            crate::cli::KeysCommand::Revoke(args) => keys_revoke(cli, &args.token_file),
        },
        Some(Command::Limits(a)) => match &a.command {
            None => limits_view(cli, a.json),
            Some(crate::cli::LimitsCommand::Set(args)) => limits_set(cli, args),
            Some(crate::cli::LimitsCommand::Clear(args)) => limits_clear(cli, args),
        },
        Some(Command::Server(a)) => match &a.command {
            None | Some(crate::cli::ServerCommand::Show) => server_show(cli, a.json),
            Some(crate::cli::ServerCommand::Set(args)) => server_set(cli, args),
        },
        Some(Command::CostReport(a)) => cost_report(cli, a),
        Some(Command::Service(a)) => service(cli, a),
    }
}

/// `aroute cost-report`: spend and token usage, read out of the same sqlite
/// ledger `aroute serve` writes, through the same renderer `ar_cost_report`
/// uses (`ar_tokens::CostReport::toon`), so the two surfaces cannot drift.
///
/// The one behaviour worth stating: this command **creates no file**. A read
/// verb that leaves a fresh empty sqlite behind has lied about what it found —
/// the caller cannot tell "no usage" from "no ledger". So the path is probed
/// first, and an absent ledger prints the definitive-empty form rather than a
/// zero-row report over a database this command just invented.
fn cost_report(cli: &Cli, args: &crate::cli::CostReportArgs) -> anyhow::Result<()> {
    // The same sibling `aroute serve` resolves (`serve.rs:123-126`) and
    // `aroute mcp` opens (`mcp.rs:330`): derived from the credential store's
    // parent rather than computed a second way, because two rules for one
    // location is how a CLI reads a different ledger than the proxy writes.
    let ledger = store_path(&cli.config)
        .parent()
        .map_or_else(|| PathBuf::from("usage.sqlite"), |d| d.join("usage.sqlite"));

    if !ledger.exists() {
        // `CostReport::default().toon()` is this exact string, printed without
        // opening anything: the empty form is a statement about the host, not
        // about the contents of a file we could have created.
        print!("{}", ar_tokens::CostReport::default().toon());
        return Ok(());
    }

    let ledger = ar_tokens::Ledger::open(&ledger).map_err(|e| {
        fail(
            format!("cannot read {}: {e}", ledger.display()),
            "the proxy's usage ledger is unreadable; check the file's permissions",
        )
    })?;
    let mut report = ledger.report(args.limit).map_err(|e| {
        fail(
            format!("cannot query the usage ledger: {e}"),
            "`aroute doctor` reports whether the ledger is usable",
        )
    })?;
    // `--key` filters the rows this command prints; the totals stay the
    // ledger-wide ones, because a caller asking "what did this key spend" still
    // needs to know it against "what was spent overall" to mean anything.
    if let Some(key) = &args.key {
        report.rows.retain(|r| &r.key_id == key);
    }
    if args.json {
        // The ledger's own row shape, so a machine gets the same data the
        // MCP's `ar_cost_report` returns in its JSON half — one source, two
        // renderings, rather than a CLI-only second shape.
        println!("{}", cost_report_json(&report));
    } else {
        print!("{}", report.toon());
    }
    Ok(())
}

/// The same [`ar_tokens::CostReport`] as JSON.
///
/// Kept beside the TOON renderer's caller rather than inside `ar-tokens` so
/// the shared renderer stays the single owner of the *rows* and the CLI owns
/// only the envelope it prints them in.
fn cost_report_json(report: &ar_tokens::CostReport) -> String {
    let rows: Vec<serde_json::Value> = report
        .rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "key_id": r.key_id,
                "provider": r.provider,
                "model": r.model,
                "tokens": r.total_tokens,
                "usd_micros": r.cost_usd.micros,
                "priced": r.priced,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "count": report.rows.len(),
        "rows": rows,
        "totals": {
            "tokens": report.totals.tokens,
            "usd_micros": report.totals.usd.micros,
        },
    }))
    .unwrap_or_else(|_| String::from("{}"))
}

/// Bare `ar`: content-first home view, not a manual (docs/06).
/// Deliberately not the combo table's full width: the point of a home view is
/// the three facts a caller cannot guess — where the binary is, what it is, and
/// whether it is configured at all.
fn home(cli: &Cli) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let catalog = catalog(&cfg)?;
    println!("bin: {}", self_path());
    println!(
        "aroute {} — one OpenAI-compatible endpoint, many providers",
        crate::version::VERSION
    );
    println!("config: {}", cli.config.display());
    println!("listen: {}:{}", cfg.server.host, cfg.server.port);
    println!("registry: {} providers", registry().len());
    print!(
        "{}",
        toon::list(
            "combos",
            "combos",
            &COMBO_COLUMNS,
            &toon::fields_or_default(None),
            &combo_rows(&cfg.combos, &catalog),
            true
        )
    );
    println!("next:");
    println!("  aroute serve  start the proxy");
    println!("  aroute run -p one completion through the router");
    println!("  aroute doctor check config, credentials, registry");
    Ok(())
}

/// This executable's path, for the home view's `bin:` line.
///
/// `current_exe` fails on a deleted or unlinked binary, which is not worth an
/// error: the caller still wants the rest of the view.
fn self_path() -> String {
    std::env::current_exe().map_or_else(
        |_| "aroute (path unavailable)".to_owned(),
        |p| p.display().to_string(),
    )
}

fn models(cli: &Cli, args: &ListArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let catalog = catalog(&cfg)?;
    let fields = columns(
        &toon::fields_or_default(args.fields.as_deref()),
        &MODEL_COLUMNS,
        "models",
    )?;

    // Row-superset filters, applied after the one query so there is still a
    // single source of truth for what a model IS (docs/36 G3).
    //
    // `--provider` matches the model's own provider (`openai/gpt-4o` ->
    // `openai`), which is column 1, not the registry's dialect.
    let mut rows = model_rows(&cfg, &catalog);
    if let Some(provider) = &args.provider {
        rows.retain(|r| r.get(1).is_some_and(|p| p == provider));
    }
    if args.combo.is_some() {
        let combo = args.combo.as_deref().unwrap_or_default();
        rows.retain(|r| {
            r.get(3)
                .is_some_and(|owned| owned.split('+').any(|name| name == combo))
        });
    }
    // `--routable` keeps only what this config actually routes — a combo
    // target.
    //
    // NOT `status == "routable"`. That string means "provider AND model are
    // both in the registry", which silently drops the rows an agent most needs:
    // `anthropic/claude-sonnet-4` IS a combo target, and its `unknown-model`
    // status is the answer to "why does my route fail", not a reason to hide
    // it. Measured: filtering on the status string returned `0 models` for a
    // config whose every target was a real combo, because none of its providers
    // were in the compiled-in registry. The structural test is column 3 (the
    // owning combo): a catalog row carries `-`, a combo target carries a combo
    // id. So a target that cannot currently route is still listed, and its
    // status says why — the same split `model_rows` makes.
    //
    // The complement is what an agent usually wants and what the 72 KB default
    // makes it sift for, so the name is `--routable` and the filter is the
    // route set, not a status judgement.
    if args.routable {
        rows.retain(|r| r.get(3).is_some_and(|owned| owned != "-"));
    }
    let total = rows.len();
    let rows = page(&rows, args.offset.unwrap_or(0), args.limit);

    if args.json {
        print!(
            "{}",
            toon::json_paged("models", &MODEL_COLUMNS, &fields, &rows, total)
        );
    } else {
        print!(
            "{}",
            toon::list_paged(
                "models",
                "models",
                &MODEL_COLUMNS,
                &fields,
                &rows,
                args.full,
                total
            )
        );
    }
    Ok(())
}

/// Applies `--offset`/`--limit` to a row set, reporting `count: N of TOTAL`
/// against the pre-page total.
///
/// In `toon::list` as a step rather than inside it: every list verb needs the
/// same arithmetic, and keeping it here means one place to get `TOTAL` right.
/// An offset past the end is not an error — the empty result says so, with
/// `TOTAL` naming how many there really were.
fn page(rows: &[Row], offset: usize, limit: Option<usize>) -> Vec<Row> {
    let start = offset.min(rows.len());
    let window = &rows[start..];
    match limit {
        Some(n) => window.iter().take(n).cloned().collect(),
        None => window.to_vec(),
    }
}

/// Routable models: every combo target, plus anything the catalog declares.
///
/// Combo targets come first because those are what this config can actually
/// route; the catalog's own model list is appended so `aroute models` also answers
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
                None => rows.push(vec![
                    t.clone(),
                    provider.to_owned(),
                    status.to_owned(),
                    c.id.clone(),
                ]),
            }
        }
    }
    for (provider_id, def) in catalog.iter() {
        for model in &def.models {
            let id = format!("{provider_id}/{model}");
            if !rows.iter().any(|r| r[0] == id) {
                rows.push(vec![
                    id,
                    provider_id.to_string(),
                    "known".to_owned(),
                    "-".to_owned(),
                ]);
            }
        }
    }
    rows.sort();
    rows
}

/// The provider half of a `provider/model` target, under the same grammar
/// `aroute serve` routes with (see `ar_server::config::split_target_known`).
///
/// Compiled-in catalog only: `aroute import` builds a combo list from a registry it
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
    let fields = columns(
        &toon::fields_or_default(args.fields.as_deref()),
        &PROVIDER_COLUMNS,
        "providers",
    )?;
    let rows = provider_rows(&cfg, &catalog);
    let total = rows.len();
    let rows = page(&rows, args.offset.unwrap_or(0), args.limit);
    if args.json {
        print!(
            "{}",
            toon::json_paged("providers", &PROVIDER_COLUMNS, &fields, &rows, total)
        );
    } else {
        print!(
            "{}",
            toon::list_paged(
                "providers",
                "providers",
                &PROVIDER_COLUMNS,
                &fields,
                &rows,
                args.full,
                total
            )
        );
    }
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
            let status = if def.is_some() {
                "ok"
            } else {
                "not-in-registry"
            };
            vec![
                p.id.clone(),
                dialect.to_owned(),
                status.to_owned(),
                base_url.to_owned(),
                p.key.clone(),
            ]
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

/// The raw `keys:` mapping as written (name -> `$VAR`), parsed from the
/// document rather than `Config`, which resolves the same section into
/// secrets — the page shows variable names, never values.
///
/// Here rather than in `webui.rs` because every writer of a `keys:` row needs
/// it, and one owner per question (`AGENTS.md` rule 1) puts that with the other
/// config helpers in this module.
pub(crate) fn raw_keys(doc: &str) -> BTreeMap<String, String> {
    serde_yaml::from_str::<serde_yaml::Value>(doc)
        .ok()
        .and_then(|root| root.get("keys").cloned())
        .and_then(|keys| serde_yaml::from_value(keys).ok())
        .unwrap_or_default()
}

/// One `keys:` row, in the order the file writes it.
///
/// A `BTreeMap` cannot answer "which order did the operator write?", because a
/// BTreeMap has one order and it is sorted. [`raw_keys`] returns a `BTreeMap`
/// for the lookups, and that is right for a lookup; it is wrong for a write,
/// because re-rendering through it re-sorts a section the operator maintains in
/// an order that means something to them (grouping, or a comment-free
/// preference), which shows up in `git diff` as a reordering they never asked
/// for.
struct KeyRow {
    name: String,
    value: String,
}

/// The `keys:` mapping as written, in the file's own order.
fn raw_keys_in_order(doc: &str) -> Vec<KeyRow> {
    // An `IndexMap` would be the obvious type; serde_yaml maps to its own
    // `Mapping`, which *is* insertion-ordered, so this walks that instead of
    // introducing a dependency for one shape.
    let Some(map) = serde_yaml::from_str::<serde_yaml::Value>(doc)
        .ok()
        .and_then(|root| root.get("keys").cloned())
        .and_then(|v| match v {
            serde_yaml::Value::Mapping(m) => Some(m),
            _ => None,
        })
    else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(k, v)| {
            let name = k.as_str()?.to_owned();
            let value = match v {
                serde_yaml::Value::String(s) => s.clone(),
                other => serde_yaml::to_string(other).ok()?.trim_end().to_owned(),
            };
            Some(KeyRow { name, value })
        })
        .collect()
}

/// The `keys:` mapping with one binding upserted — in the file's own order.
///
/// An existing name keeps its position (an edit), a new one is appended (the
/// file's newest line is its last), and the rest of the section is untouched.
pub(crate) fn with_key_binding(doc: &str, name: &str, var: &str) -> anyhow::Result<String> {
    let mut rows = raw_keys_in_order(doc);
    let binding = format!("${var}");
    match rows.iter_mut().find(|r| r.name == name) {
        Some(existing) => existing.value = binding,
        None => rows.push(KeyRow {
            name: name.to_owned(),
            value: binding,
        }),
    }
    let section = render_keys_rows(&rows)?;
    Ok(splice_section(doc, "keys", &section))
}

/// Renders `KeyRow`s as the `keys:` section, in the given order.
///
/// Same shape as [`crate::import::render_keys_section`] — two-space indent under
/// the header, one `name: value` per line — but over a slice, because the order
/// is the point. `serde_yaml` over an ordered `Mapping` would give the same
/// bytes; building the lines directly keeps the rule visible here rather than
/// depending on a serialisation order argument.
fn render_keys_rows(rows: &[KeyRow]) -> anyhow::Result<String> {
    let mut out: Vec<String> = vec!["keys:".to_owned()];
    for r in rows {
        out.push(format!("  {}: {}", r.name, r.value));
    }
    Ok(out.join("\n"))
}

/// Drops one provider's `providers:` row, the `keys:` entries only it held, and
/// the matching `custom_providers:` row, in one document.
///
/// `Ok(None)` means the id is configured nowhere, which is the caller's refusal
/// to make rather than this function's — the message names what the operator
/// should run next.
///
/// Shared with the panel's removal route, which was the only caller before
/// Phase 2. It lives here with the other config writers: the rule about a
/// shared `keys:` row is one question with one answer, and two copies of it
/// could disagree about which entry belongs to which provider.
pub(crate) fn splice_remove_provider(
    doc: &str,
    cfg: &Config,
    id: &str,
) -> anyhow::Result<Option<String>> {
    if !cfg.providers.iter().any(|p| p.id == id) && !cfg.custom_providers.iter().any(|c| c.id == id)
    {
        return Ok(None);
    }
    let providers = crate::import::render_providers_section(
        &cfg.providers
            .iter()
            .filter(|p| p.id != id)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    // The same rule as `custom_providers:` below, on the other section: a
    // renderer that emits its header for an empty list must not be spliced when
    // the list is empty, or a remove writes a `providers:` section into a config
    // that carried none. A splice of a section that is absent **inserts** it, so
    // this is not idempotence but a new section appearing. Both sections get
    // the identical guard for the identical reason.
    let next = if cfg.providers.iter().any(|p| p.id != id) {
        splice_section(doc, "providers", &providers)
    } else {
        doc.to_owned()
    };
    let kept: BTreeSet<String> = cfg
        .providers
        .iter()
        .filter(|p| p.id != id)
        .flat_map(|p| p.key_refs().into_iter().map(str::to_owned))
        .chain(
            cfg.custom_providers
                .iter()
                .filter(|c| c.id != id)
                .map(|c| c.key_ref.clone()),
        )
        .collect();
    let customs = crate::import::render_custom_section(
        &cfg.custom_providers
            .iter()
            .filter(|c| c.id != id)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    // Only rewrite `custom_providers:` when rows survive this edit. When
    // none do, the section must not survive either — "render nothing" is not
    // the same as "render an empty section", because the renderer emits the
    // header even for an empty row list.
    //
    // Not [`remove_section`]: that helper skips until the next section header,
    // and this section is the last one before a trailing block of comments that
    // document sections *below* it, so it would delete 62 lines of
    // documentation — the Phase 0 data-loss shape rearmed by a different
    // caller. The `is_section_header` stop condition is what makes
    // `remove_section` safe elsewhere and wrong here.
    //
    // And not "splice an empty section" either: a bare `custom_providers:`
    // parses to the same empty list, so it changes nothing an agent or the
    // loader can see, and leaves one byte the operator never asked to have
    // added. Drop the header and its rows in one edit, keeping the comment
    // block that follows — that block is documentation for a section that may
    // come back, and `add-custom` inserts the new row above it.
    let surviving_customs = cfg.custom_providers.iter().any(|c| c.id != id);
    let next = if surviving_customs {
        splice_section(&next, "custom_providers", &customs)
    } else {
        empty_custom_section(&next)
    };
    // Keys entries the removed rows alone held: every entry the removed rows
    // name, minus the entries the surviving rows name. Shared entries survive
    // by construction, so this is exactly a "no dangling reference" edit.
    let orphaned: BTreeSet<String> = cfg
        .providers
        .iter()
        .filter(|p| p.id == id)
        .flat_map(|p| p.key_refs().into_iter().map(str::to_owned))
        .chain(
            cfg.custom_providers
                .iter()
                .filter(|c| c.id == id)
                .map(|c| c.key_ref.clone()),
        )
        .filter(|name| !kept.contains(name))
        .collect();
    if orphaned.is_empty() {
        return Ok(Some(next));
    }
    let section = render_keys_removed(doc, &orphaned)?;
    Ok(Some(splice_section(&next, "keys", &section)))
}

/// The `keys:` section with `removed` entries dropped, in the file's order.
///
/// Deleting a row is the one edit that cannot change an order, so this walks
/// the written rows and filters rather than re-rendering a `BTreeMap`: the
/// surviving rows stay exactly where the operator put them. Same reason as
/// [`with_key_binding`].
fn render_keys_removed(doc: &str, removed: &BTreeSet<String>) -> anyhow::Result<String> {
    let rows: Vec<KeyRow> = raw_keys_in_order(doc)
        .into_iter()
        .filter(|r| !removed.contains(&r.name))
        .collect();
    render_keys_rows(&rows)
}

/// The document with the `custom_providers:` section's rows and header gone.
///
/// [`splice_section`] replaces a section with a rendered one, and the renderer
/// emits its header even for an empty row list, so "no rows left" and "an empty
/// section" are not the same thing — only the second leaves bytes behind. This
/// drops the header and its rows and keeps everything after them.
///
/// The skip mirrors `splice_section`'s own stop conditions: a blank line or a
/// column-0 comment ends the section, because both are what introduces the
/// *next* thing in this file, and the comment block under a `custom_providers:`
/// section documents sections further down. Stopping there is the difference
/// between deleting six lines and deleting sixty-two.
fn empty_custom_section(doc: &str) -> String {
    let header = "custom_providers:";
    let mut out: Vec<String> = Vec::new();
    let mut skipping = false;
    for line in doc.lines() {
        if line == header {
            skipping = true;
            continue;
        }
        if skipping && (line.is_empty() || line.starts_with('#') || is_section_header(line)) {
            skipping = false;
        }
        if !skipping {
            out.push(line.to_owned());
        }
    }
    let ending = if doc.ends_with('\n') || doc.ends_with("\r\n") {
        line_ending(doc)
    } else {
        ""
    };
    out.join(line_ending(doc)) + ending
}

/// The strategies this build routes, by name.
///
/// The list [`check_combos`] validates against and the list the `/ui` page
/// shows. It was `webui.rs`'s own constant until Phase 4, when the validation
/// moved here: the panel's save and the CLI's writers must accept the same
/// names, and a second hand-copied list is how they diverge. Still a
/// **shortcut over `ar_config::Strategy::parse`** (which accepts `auto/*`
/// virtual factory names too) — keep both in sync when a strategy is added.
pub(crate) const STRATEGIES: &[&str] = &[
    "priority",
    "round-robin",
    "cost-optimized",
    "lkgp",
    "weighted",
    "fill-first",
    "p2c",
    "least-used",
    "random",
    "strict-random",
    "headroom",
    "reset-window",
    "reset-aware",
    "quota-weighted",
    "quota-share",
    "quota-share-fair",
    "expiry-first",
    "context-relay",
    "context-optimized",
    "cache-optimized",
    "fusion",
    "pipeline",
];

/// A combo list is valid when every id is a legal model name, every target
/// names a provider the registry or `custom_providers:` knows, and every
/// strategy is one this build routes rather than defers. Strategies already
/// in the file are exempt: a save must round-trip a config this page did
/// not write.
///
/// **The one owner of this question**, moved here from `webui.rs` by Phase 4 of
/// the config-write surface. The panel's whole-section save and the CLI's
/// `combo add`/`target add`/`target remove` must agree about what a valid combo
/// is; two copies is how they drift, and the CLI writers had no validation at
/// all before this. The rules are unchanged by the move, and
/// `check_combos_accepts_a_round_trip_of_the_fixture` still passes against it,
/// which is the evidence the extraction narrowed nothing — the fixture is a
/// hand-written YAML string with a flow-style `targets:` list, which is the
/// spelling the repo's own `config.yaml` uses.
pub(crate) fn check_combos(combos: &[Combo], current: &Config) -> Result<(), String> {
    let custom: Vec<&str> = current
        .custom_providers
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    let known_provider = |id: &str| registry().get(id).is_some() || custom.contains(&id);
    let mut ids = BTreeSet::new();
    let allowed: BTreeSet<&str> = STRATEGIES
        .iter()
        .copied()
        .chain(current.combos.iter().map(|c| c.strategy.as_str()))
        .collect();
    for c in combos {
        let id_legal = !c.id.is_empty()
            && c.id.len() <= 64
            && c.id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !id_legal {
            return Err(format!(
                "combo id {:?} is not a legal model name (1-64 of a-z 0-9 . _ -)",
                c.id
            ));
        }
        if !ids.insert(c.id.clone()) {
            return Err(format!("combo id {:?} appears twice", c.id));
        }
        if c.targets.is_empty() {
            return Err(format!("combo {:?} has no targets", c.id));
        }
        for t in &c.targets {
            match t.split_once('/') {
                Some((p, m)) if !p.is_empty() && !m.is_empty() => {
                    if !known_provider(p) {
                        return Err(format!("combo {:?} targets unknown provider {p:?}", c.id));
                    }
                }
                _ if known_provider(t) => {}
                _ => {
                    return Err(format!(
                        "combo {:?} has a malformed target {t:?} (expected provider/model)",
                        c.id
                    ));
                }
            }
        }
        if !allowed.contains(c.strategy.as_str()) {
            return Err(format!(
                "combo {:?} names strategy {:?}, which this build does not route",
                c.id, c.strategy
            ));
        }
        for (target, w) in &c.weights {
            if !c.targets.contains(target) {
                return Err(format!(
                    "combo {:?} weights target {target:?} it does not list",
                    c.id
                ));
            }
            if *w == 0 {
                return Err(format!(
                    "combo {:?} weights target {target:?} at zero",
                    c.id
                ));
            }
        }
        if !c.steps.is_empty() && c.steps.len() != c.targets.len() {
            return Err(format!(
                "combo {:?} carries {} steps for {} targets",
                c.id,
                c.steps.len(),
                c.targets.len()
            ));
        }
    }
    Ok(())
}

/// Whether a candidate document means the same thing as the current one.
///
/// F8, and the reason it lives in this module rather than beside one writer:
/// the renderers emit block-style lists where a hand-written file uses
/// flow-style, so **two documents that spell the same configuration
/// differently compare equal** — and a byte comparison would re-render the
/// file on every write, dirtying it for a change the operator never made.
///
/// # The comparison is on the UNEXPANDED documents
///
/// `Config::parse` expands `$VAR` on the raw text **before** YAML parsing, so
/// comparing with the live environment would make `$AR_KEY_OPENAI` and the
/// literal `sk-...` compare **equal** — and the write path would then discard
/// the change. Both sides are therefore parsed with a resolver that expands
/// **nothing**: a reference stays a literal and the two spellings differ.
///
/// It is also **deterministic**: the live environment could change between two
/// parses, and a comparison that answers differently when the operator exports
/// a variable is not a comparison.
///
/// # What is NOT compared
///
/// `Config` does not deny unknown fields, so a YAML key the struct does not
/// model is dropped from both parses and is **not compared**. A change there is
/// not detected. That is a real limit on the claim and it is bounded: the
/// writers only ever touch modelled sections, and `splice_section` preserves
/// the rest of the text.
///
/// A document that does not parse is NOT identical — it is a change, and the
/// write path reports the parse error. Answering "identical" for an
/// unparseable candidate would swallow the error.
pub(crate) fn means_the_same(current: &str, candidate: &str) -> bool {
    if current == candidate {
        return true;
    }
    match (
        Config::parse(current, no_expansion),
        Config::parse(candidate, no_expansion),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The resolver [`means_the_same`] uses: expand **nothing**.
///
/// A `None` lookup means "the variable is not set", so the expansion leaves
/// `$VAR` as the literal text `$VAR` — which is what makes the comparison
/// distinguish a reference from the value it points at.
fn no_expansion(_: &str) -> Result<Option<String>, String> {
    Ok(None)
}

/// Configures a registry provider: the `providers:` row, the `keys:` reference
/// and the `$AR_KEY_*` value, in one pass.
///
/// The panel's `provider_key` (`webui.rs:1740`) is the tested semantic and this
/// reproduces its ordering rather than inventing one: the env value is written
/// first, because that is what publishes the credential, and a config that does
/// not change is not "nothing was written". A row that is already configured is
/// refused unless its binding already names an `$AR_KEY_*` variable — the
/// alternative is silently re-pointing a hand-wired secret to a generated name
/// the operator never chose. A row with no binding is not a partial write, it is
/// a config the proxy refuses at boot, so it is refused here instead.
///
/// The credential never touches argv. `--credential-file -` reads stdin, so
/// `printf %s "$KEY" | aroute providers add openai` is possible, and a file
/// path is the documented form because that leaves the secret out of the shell
/// history entirely.
fn providers_add(cli: &Cli, args: &crate::cli::AddProviderArgs) -> anyhow::Result<()> {
    let id = args.id.trim();
    if id.is_empty() {
        return Err(fail(
            "the provider id is empty",
            "`aroute providers` lists the ids the registry knows",
        ));
    }
    let secret = read_credential(&args.credential_file)?;
    let doc = read_config_text(cli)?;
    let mut cfg = load_edit(cli)?;
    let catalog = catalog(&cfg)?;
    if catalog.get(id).is_none() {
        return Err(fail(
            format!("unknown provider {id:?}"),
            "the id must name a registry provider; an ad-hoc endpoint is `aroute provider-settings` (Phase 3), not this verb",
        ));
    }

    let var = format!("AR_KEY_{}", import::env_name(id));
    let already = cfg.providers.iter().any(|p| p.id == id);
    // The binding an existing row names, and the one a new row gets. A row that
    // names a literal secret rather than an `$AR_KEY_*` variable is refused
    // rather than re-pointed: `sync` and the panel both treat a hand-wired
    // value as the operator's decision.
    // Read from `raw_keys`, the file as written, not from `cfg.keys`: an
    // unexpanded `$AR_KEY_X` and the literal it points at are different
    // spellings of the same binding, and only the written one is what the
    // operator's `keys:` entry actually says.
    let bound = cfg
        .providers
        .iter()
        .find(|p| p.id == id)
        .and_then(|p| p.key_refs().first().copied())
        .and_then(|name| raw_keys(&doc).get(name).cloned())
        .map(|v| v.trim_start_matches('$').to_owned());
    if already && !bound.as_deref().is_some_and(|b| b.starts_with("AR_KEY_")) {
        return Err(fail(
            format!("provider {id:?} has no AR_KEY_* binding for its credential"),
            "give its `key:` entry an `$AR_KEY_*` value in config.yaml, or remove the provider row and re-add it here",
        ));
    }
    let var = if already { bound.unwrap_or(var) } else { var };

    // A move, not a clone: `cfg` is not read again after this point, and
    // the whole `providers:` list is about to be re-rendered from it anyway.
    let mut rows = std::mem::take(&mut cfg.providers);
    if let Some(existing) = rows.iter_mut().find(|p| p.id == id) {
        existing.key.clone_from(&id.to_owned());
    } else {
        rows.push(ar_config::ProviderCfg {
            id: id.to_owned(),
            key: id.to_owned(),
            keys: None,
        });
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let section = import::render_providers_section(&rows)?;
    let mut next = splice_section(&doc, "providers", &section);
    if !already {
        next = with_key_binding(&next, id, &var)?;
    }

    // The env file first, exactly as the panel writes it: that is the write
    // that publishes the credential, so a config that is already correct still
    // means the credential was just written and the output says so.
    let env = env_path(&cli.config);
    set_env_var(&env, &var, &secret)?;
    // Semantic, not textual: the renderers emit block style where a
    // hand-written file uses flow style, so a byte comparison would re-render
    // the file on every add of a provider that is already there — dirtying it
    // for a change the operator never made (F8, `means_the_same`).
    if !means_the_same(&doc, &next) {
        write_file(&cli.config, &next)?;
    }
    println!("providers: {id} (row, keys: ref, {var})");
    println!("note: the credential was written to {}", env.display());
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// --- provider-settings and continue-on-cut writers ---------------------------
///
/// Two shapes Phase 6 did not need, and the reason this phase came last.
///
/// `continue_on_cut:` is a flat list of provider ids at the top level, which
/// [`append_list_entry`] already covers.
///
/// `provider_settings:` is a map of maps: a key per provider id, each holding
/// that provider's own `excluded_models` list. A key-per-id outer layer means
/// the list's depth depends on which provider is being edited, so the writers
/// below carry a base depth rather than assuming the section is the base:
///
/// ```yaml
/// provider_settings:
///   openai:
///     excluded_models:
///       - *-search
/// ```
///
/// The section sits at 0, a provider's key at 2, its list at 4, and the list's
/// entries at 6.
///
/// The section name the provider-settings verbs edit.
const PS_SECTION: &str = "provider_settings";

/// The only list inside a provider's own rules today.
const PS_LIST: &str = "excluded_models";

/// Appends one entry to a list nested inside a named key of a section.
///
/// `section` is the outer block, `owner` the key inside it whose list is
/// edited — the provider id for `provider_settings`, or `None` when the list
/// belongs to the section itself (`continue_on_cut`, whose entries sit directly
/// under the header).
///
/// The depths are derived rather than assumed, so the section's own indentation
/// is honoured and a block written at another depth is refused rather than
/// edited wrongly.
fn append_nested_list_entry(
    content: &str,
    section: &str,
    owner: Option<&str>,
    list: &str,
    entry: &str,
) -> Option<String> {
    if nested_list_has_entry(content, section, owner, list, entry) {
        return Some(content.to_owned());
    }
    let all: Vec<&str> = content.lines().collect();
    // One binding for the section's indent, so the derived depths below read as
    // arithmetic rather than as a chain of fallbacks.
    let base = section_depth(&all, section).unwrap_or(0);
    // The entry's depth moves with whether there is an owner at all: a list under
    // a provider key sits two deeper than one directly under the section.
    let entry_depth = base + if owner.is_some() { 6 } else { 2 };
    let value = yaml_entry(entry);
    // The section does not exist yet: it is created whole, at the end of the
    // document, and `create_nested_list` owns the shape.
    if !all.iter().any(|l| is_section(l, section)) {
        return Some(create_nested_list(
            content, section, owner, list, entry, base,
        ));
    }

    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut in_owner = false;
    let mut in_list = false;
    let mut inserted = false;
    let mut i = 0;
    while i < all.len() {
        let line = all[i];
        if is_section(line, section) {
            in_section = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        if in_section && owner.is_none() {
            // A flat list has no key line, so its entries are the section's own
            // body. Whether this line is one decides between keeping it and
            // ending the list. The write happens once, at the end, so returning
            // here would drop the rest of the document.
            if is_list_entry(line, base + 2) {
                in_list = true;
                out.push(line.to_owned());
                i += 1;
                continue;
            }
            if in_list {
                out.insert(
                    list_end(&out),
                    format!("{}- {value}", " ".repeat(entry_depth)),
                );
                inserted = true;
                in_list = false;
            }
        }
        if in_section && is_section_header(line) {
            // The section ended without a list in it: `create_nested_list`
            // puts one at the section's tail, wherever that now sits.
            if owner.is_none() {
                return Some(create_nested_list(
                    content, section, owner, list, &value, base,
                ));
            }
            // The section ended *with* a list in it, and the entry belongs at
            // the end of that list, so it is written here — the last line of
            // the section, before whatever header ended it. Writing it after
            // this reset cannot work, and not writing it at all is how a
            // second `openai:` block ended up appended beside the first.
            if in_list {
                out.insert(
                    list_end(&out),
                    format!("{}- {value}", " ".repeat(entry_depth)),
                );
                inserted = true;
            }
            in_section = false;
            // The owner's key is a line *of the section*: the section ending
            // ends the owner too. Leaving `in_owner` standing makes the walk
            // read the next section's `excluded_models:` as the same list
            // again and write a second block beside the first, which is a
            // duplicate-key config the loader resolves by order of appearance.
            in_owner = false;
            in_list = false;
        }
        if in_section
            && owner.is_some()
            && !in_owner
            && is_key_at(line, base + 2)
            && line.trim_end() == format!("{}{}:", " ".repeat(base + 2), owner.unwrap())
        {
            in_owner = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        // The provider's key is a line of the section, so the arm above ends
        // the owner with the section. An owner that outlives its section is
        // what made this write append a second block beside the first, and
        // what made it drop the file's tail on the way out.
        if in_owner && !in_list && is_list_key(line, list, base + 4) {
            in_list = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        if in_list
            && (line.is_empty() || line.starts_with('#'))
            && rest_is_another_section(&all[i + 1..])
        {
            out.insert(
                list_end(&out),
                format!("{}- {value}", " ".repeat(entry_depth)),
            );
            return Some(rejoin(content, out));
        }
        if in_list && is_key_at(line, base + 4) {
            out.insert(
                list_end(&out),
                format!("{}- {value}", " ".repeat(entry_depth)),
            );
            in_list = false;
        }
        out.push(line.to_owned());
        i += 1;
    }
    if inserted {
        return Some(rejoin(content, out));
    }
    if in_list {
        // The list ran to the end of the file: the entry goes after it.
        out.push(format!("{}- {value}", " ".repeat(entry_depth)));
        return Some(rejoin(content, out));
    }
    // The list is not written yet: the provider's key, the list, and the entry
    // are all created, each in the place it belongs.
    Some(create_nested_list(
        content, section, owner, list, &value, base,
    ))
}

/// Whether the nested list already carries `entry`.
fn nested_list_has_entry(
    content: &str,
    section: &str,
    owner: Option<&str>,
    list: &str,
    entry: &str,
) -> bool {
    // The scan, not the remove writer.
    //
    // Remove answers a different question: it returns `Some` when it removed
    // an entry *and* when it removed nothing but recognised the shape, so
    // `.is_some()` on it is not "is it there". Reading it as though it were
    // meant a caller that asked before writing got yes for any glob it typed.
    //
    // No behaviour differs today — the `_ => Err` refusal arm in the caller
    // catches the shapes where the two disagree — so this removes a trap, not
    // a bug. That is worth the change because the trap is invisible: the two
    // answer the same question by accident of the current control flow, and a
    // future change to either one lands silently.
    scan_nested_entry(content, section, owner, list, entry)
}

/// Reads the nested list for `entry` without rewriting anything.
fn scan_nested_entry(
    content: &str,
    section: &str,
    owner: Option<&str>,
    list: &str,
    entry: &str,
) -> bool {
    let all: Vec<&str> = content.lines().collect();
    let Some(base) = section_depth(&all, section) else {
        return false;
    };
    // A list under a provider key sits two deeper than one directly under the
    // section, so the entry's depth moves with whether there is an owner at all.
    let list_depth = base + if owner.is_some() { 4 } else { 2 };
    let entry_depth = base + if owner.is_some() { 6 } else { 2 };
    let mut in_section = false;
    let mut in_owner = false;
    let mut in_list = false;
    for line in &all {
        if is_section(line, section) {
            in_section = true;
            continue;
        }
        if in_section && is_section_header(line) {
            in_section = false;
        }
        if in_section
            && owner.is_some()
            && !in_owner
            && line.trim_end() == format!("{}{}:", " ".repeat(base + 2), owner.unwrap_or_default())
        {
            in_owner = true;
            continue;
        }
        if in_section && owner.is_none() {
            in_owner = true;
            // With no provider key the section's own body is the list, so it
            // opens the moment the section does rather than on a key line.
            in_list = true;
        }
        if in_owner && !in_list && is_list_key(line, list, list_depth) {
            in_list = true;
            continue;
        }
        if in_list && is_key_at(line, list_depth) {
            in_list = false;
        }
        if in_list && is_list_entry(line, entry_depth) && list_entry_value(line) == entry {
            return true;
        }
    }
    false
}

/// Removes one entry from the nested list, keeping every other byte.
fn remove_nested_list_entry(
    content: &str,
    section: &str,
    owner: Option<&str>,
    list: &str,
    entry: &str,
) -> Option<String> {
    let all: Vec<&str> = content.lines().collect();
    let base = section_depth(&all, section)?;
    let owner_depth = base + 2;
    // A list under a provider key sits two deeper than one directly under the
    // section, so the entry's depth moves with whether there is an owner at all.
    let list_depth = base + if owner.is_some() { 4 } else { 2 };
    let entry_depth = base + if owner.is_some() { 6 } else { 2 };

    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut in_owner = false;
    let mut in_list = false;
    let mut i = 0;
    let mut removed = false;
    while i < all.len() {
        let line = all[i];
        if is_section(line, section) {
            in_section = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        if in_section && is_section_header(line) {
            in_section = false;
        }
        if in_section
            && owner.is_some()
            && !in_owner
            && line.trim_end()
                == format!("{}{}:", " ".repeat(owner_depth), owner.unwrap_or_default())
        {
            in_owner = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        if in_section && owner.is_none() {
            in_owner = true;
            // With no provider key the section's own body is the list, so it
            // opens the moment the section does rather than on a key line.
            in_list = true;
        }
        if in_owner && !in_list && is_list_key(line, list, list_depth) {
            in_list = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        if in_list && is_key_at(line, list_depth) {
            in_list = false;
        }
        if in_list
            && !removed
            && is_list_entry(line, entry_depth)
            && list_entry_value(line) == entry
        {
            removed = true;
            i += 1;
            continue;
        }
        out.push(line.to_owned());
        i += 1;
    }
    if !removed {
        return None;
    }
    // Second pass over what the first produced: the list key goes when no entry
    // is left under it, the provider's key goes when no list is left under that,
    // and the section goes when no key is left in it. Without this, `exclude`
    // then `include` of one glob restores the file to *almost* what it was, with
    // a dead `provider_settings:` block the next reader has to wonder about.
    let mut kept = prune_empty_list(&out, list, list_depth, entry_depth);
    // Then the provider's own key: with the list gone it holds nothing, so it
    // goes too. Pruning only the list leaves `openai:` standing under an empty
    // section, which is the other half of what a round trip must not leave.
    if let Some(owner_key) = owner {
        kept = prune_empty_key(&kept, base, owner_key);
    }
    let section_body = kept.iter().skip_while(|l| !is_section(l, section)).skip(1);
    // Whether the section still holds anything. A flat list has no key under
    // it, so its entries are the content and counting only keys would read a
    // surviving entry as an empty section and drop it with the last entry gone.
    let body_entry_depth = base + if owner.is_some() { 6 } else { 2 };
    if section_body
        .clone()
        .take_while(|l| !is_section_header(l))
        .all(|l| !is_key_at(l, base + 2) && !is_list_entry(l, body_entry_depth))
    {
        kept = drop_section(&kept, section);
    }
    Some(rejoin(content, kept))
}

/// Drops `owner`'s key when no list is left under it.
///
/// Only the key and what sits under it go: the section is the caller's call, and
/// another provider's block is nobody else's business here.
fn prune_empty_key(lines: &[String], base: usize, owner: &str) -> Vec<String> {
    let owner_depth = base + 2;
    let header = format!("{}{}:", " ".repeat(owner_depth), owner);
    let Some(at) = lines.iter().position(|l| l.trim_end() == header) else {
        return lines.to_vec();
    };
    // The body is everything *deeper* than the key. `indent_of` returns
    // None for a column-zero line, which is every section header, and None read
    // as "deeper" swallows the next section's header and its whole body into
    // this key's block — `provider_settings:` then losing `mcp:` below it.
    // A line at the key's own depth or shallower is the end of the body,
    // whatever it is.
    let body: Vec<&String> = lines
        .iter()
        .skip(at + 1)
        .take_while(|l| match indent_of(l) {
            Some(d) => d > owner_depth,
            None => false,
        })
        .collect();
    let has_list = body.iter().any(|l| is_key_at(l, owner_depth + 2));
    if has_list {
        return lines.to_vec();
    }
    let mut kept = lines[..at].to_vec();
    let mut i = at + 1 + body.len();
    // A blank the key was separated by goes with it, so the next provider's
    // block is not left with a stray separator above it.
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    kept.extend_from_slice(&lines[i..]);
    kept
}

/// Creates the list, its owner key, and the section, whichever are missing.
fn create_nested_list(
    content: &str,
    section: &str,
    owner: Option<&str>,
    list: &str,
    value: &str,
    base: usize,
) -> String {
    let base_indent = base;
    // A list under a provider key sits two deeper than one directly under the
    // section, and its entries one deeper again. With no owner there is no
    // intervening key line, so the entry's own indent is the list's.
    let list_indent = base_indent + if owner.is_some() { 4 } else { 2 };
    let entry_indent = base_indent + if owner.is_some() { 6 } else { 2 };
    // Quoted here as it is on every other path: the value a caller passes is the
    // glob, and the form written is YAML's, and those two only coincide until a
    // leading `*` shows up.
    let entry_line = format!("{}- {}", " ".repeat(entry_indent), yaml_entry(value));
    match (
        owner,
        section_depth(&content.lines().collect::<Vec<_>>(), section),
    ) {
        (Some(id), Some(_)) => {
            // The section exists; the provider's key and its list are added at
            // that section's own tail, the same rule `section_tail` applies.
            let mut out: Vec<String> = content.lines().map(str::to_owned).collect();
            let at = section_tail(&out, section);
            out.insert(at, format!("{}{}:", " ".repeat(base_indent + 2), id));
            out.insert(at + 1, format!("{}{}:", " ".repeat(list_indent), list));
            out.insert(at + 2, entry_line);
            rejoin(content, out)
        }
        (None, Some(_)) => {
            // A list directly under the section: the entries go in, with no
            // intervening key line. Writing the list's own name here is what
            // produced `continue_on_cut:` under a `continue_on_cut:` section,
            // which is a map the loader refuses.
            let mut out: Vec<String> = content.lines().map(str::to_owned).collect();
            let at = section_tail(&out, section);
            out.insert(at, entry_line);
            rejoin(content, out)
        }
        (_, None) => {
            // No section at all: the whole block is added at the end, which is
            // where `splice_section` puts a missing section.
            let mut body = String::new();
            if let Some(id) = owner {
                body.push_str(&format!("{}{}:\n", " ".repeat(base_indent + 2), id));
                body.push_str(&format!("{}{}:\n", " ".repeat(list_indent), list));
            }
            body.push_str(&entry_line);
            // The file's own terminator, carried across: this config
            // ends without one deliberately, and a write that adds one
            // changes a byte nobody asked to touch, which is what a round
            // trip measures. The separator before the new section is always
            // a newline, though -- a section glued onto the last line of the
            // previous one is a YAML error, and the file would not load at all.
            //
            // `trim_end` is too much here: it also takes a **blank line the
            // operator wrote** at the end of the file and hands back only the
            // single terminator. Measured: a document ending `- id: c\n\n`
            // came back `- id: c\n` after one `continue-on-cut add`, and the
            // remove did not put it back either. The terminator is the only
            // thing that comes off, and by count rather than by shape.
            let (head, blank) = split_trailing_blank(content);
            let blank = if blank { "\n" } else { "" };
            let tail = if content.ends_with('\n') { "\n" } else { "" };
            format!("{head}{blank}\n{section}:\n{body}{tail}")
        }
    }
}

/// `aroute provider-settings`, and its `show` spelling.
fn provider_settings(cli: &Cli, args: &crate::cli::ProviderSettingsArgs) -> anyhow::Result<()> {
    match &args.command {
        None | Some(crate::cli::ProviderSettingsCommand::Show) => {
            provider_settings_show(cli, args.json)
        }
        Some(crate::cli::ProviderSettingsCommand::Exclude(a)) => {
            provider_settings_glob(cli, &a.provider, &a.glob, ExcludeVerb::Exclude)
        }
        Some(crate::cli::ProviderSettingsCommand::Include(a)) => {
            provider_settings_glob(cli, &a.provider, &a.glob, ExcludeVerb::Include)
        }
    }
}

/// The read verb: one row per provider, with `default` for the unset ones.
fn provider_settings_show(cli: &Cli, json: bool) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    // One row per provider the config carries, whether or not it has rules, so a
    // provider the block does not name reads as "advertising everything" rather
    // than as absent.
    let mut rows: Vec<Vec<String>> = cfg
        .providers
        .iter()
        .map(|p| {
            let excluded = cfg
                .provider_settings
                .get(&p.id)
                .map(|s| s.excluded_models.clone())
                .unwrap_or_default();
            vec![p.id.clone(), list_cell(&excluded)]
        })
        .collect();
    rows.sort();
    if json {
        print!(
            "{}",
            toon::json(
                "provider_settings",
                &PS_COLUMNS,
                &toon::every_field(&PS_COLUMNS),
                &rows,
            )
        );
    } else {
        print!(
            "{}",
            toon::list(
                "provider_settings",
                "provider_settings",
                &PS_COLUMNS,
                &toon::every_field(&PS_COLUMNS),
                &rows,
                false,
            )
        );
    }
    Ok(())
}

/// Columns of an `aroute provider-settings` row.
const PS_COLUMNS: [&str; 2] = ["provider", "excluded_models"];

/// Which direction one provider's list edit runs.
#[derive(Clone, Copy)]
enum ExcludeVerb {
    Exclude,
    Include,
}

impl ExcludeVerb {
    /// What the success line calls the state that now holds.
    fn past(self) -> &'static str {
        match self {
            ExcludeVerb::Exclude => "excluded",
            ExcludeVerb::Include => "included",
        }
    }
}

/// The shared body of `exclude` and `include`: one glob, one provider, and the
/// whole list printed afterwards so a caller can read the state back.
fn provider_settings_glob(
    cli: &Cli,
    provider: &str,
    glob: &str,
    verb: ExcludeVerb,
) -> anyhow::Result<()> {
    let provider = provider.trim();
    if provider.is_empty() {
        return Err(fail(
            "the provider id is empty",
            "name one as `providers:` spells it, e.g. openai",
        ));
    }
    if glob.trim().is_empty() {
        return Err(fail(
            "the glob is empty",
            "an entry matches a model id or `provider/model`, e.g. \"gpt-4*\"",
        ));
    }
    let doc = read_config_text(cli)?;
    let cfg = load_edit(cli)?;
    if !cfg.providers.iter().any(|p| p.id == provider) {
        return Err(fail(
            format!("provider {provider:?} is not in this config's providers:"),
            "`aroute providers` lists the providers this config carries",
        ));
    }
    let owner = Some(provider);
    let edited = match verb {
        ExcludeVerb::Exclude => {
            append_nested_list_entry(&doc, PS_SECTION, owner, PS_LIST, glob.trim())
        }
        ExcludeVerb::Include => {
            remove_nested_list_entry(&doc, PS_SECTION, owner, PS_LIST, glob.trim())
        }
    };
    let updated = match edited {
        Some(u) => u,
        None => match verb {
            ExcludeVerb::Exclude
                if nested_list_has_entry(&doc, PS_SECTION, owner, PS_LIST, glob.trim()) =>
            {
                println!("provider_settings: {provider} {glob} — unchanged (already excluded)");
                return Ok(());
            }
            ExcludeVerb::Include
                if !nested_list_has_entry(&doc, PS_SECTION, owner, PS_LIST, glob.trim()) =>
            {
                println!("provider_settings: {provider} {glob} — unchanged (not excluded)");
                return Ok(());
            }
            _ => {
                return Err(fail(
                    format!(
                        "provider_settings: {provider} is written in a form this writer does not recognise"
                    ),
                    "the block may use a flow list or another indentation; edit it by hand",
                ));
            }
        },
    };
    if updated == doc {
        println!(
            "provider_settings: {provider} {glob} — unchanged ({})",
            verb.past()
        );
        return Ok(());
    }
    write_file(&cli.config, &updated)?;
    println!("provider_settings: {} {glob} for {provider}", verb.past());
    print_list_after_edit(&updated, provider);
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// Prints the provider's list as it stands after the edit.
fn print_list_after_edit(doc: &str, provider: &str) {
    let all: Vec<&str> = doc.lines().collect();
    let Some(base) = section_depth(&all, PS_SECTION) else {
        println!("  excluded_models: default");
        return;
    };
    let owner_depth = base + 2;
    let list_depth = base + 4;
    // Only ever a provider's list: the owner is the argument, never the None
    // case the writers accept for a list that sits directly under the section.
    let entry_depth = base + 6;
    let mut in_section = false;
    let mut in_owner = false;
    let mut in_list = false;
    let mut entries: Vec<String> = Vec::new();
    for line in &all {
        if is_section(line, PS_SECTION) {
            in_section = true;
            continue;
        }
        if in_section && is_section_header(line) {
            in_section = false;
        }
        if in_section
            && !in_owner
            && line.trim_end() == format!("{}{}:", " ".repeat(owner_depth), provider)
        {
            in_owner = true;
            continue;
        }
        if in_owner && !in_list && is_list_key(line, PS_LIST, list_depth) {
            in_list = true;
            continue;
        }
        if in_list && is_key_at(line, list_depth) {
            in_list = false;
        }
        if in_list && is_list_entry(line, entry_depth) {
            entries.push(list_entry_value(line));
        }
    }
    if entries.is_empty() {
        println!("  excluded_models: default");
    } else {
        println!("  excluded_models: {}", entries.join(","));
    }
}

/// `aroute continue-on-cut`, and its `show` spelling.
fn continue_on_cut(cli: &Cli, args: &crate::cli::ContinueOnCutArgs) -> anyhow::Result<()> {
    match &args.command {
        None | Some(crate::cli::ContinueOnCutCommand::Show) => continue_on_cut_show(cli, args.json),
        Some(crate::cli::ContinueOnCutCommand::Add(a)) => {
            continue_on_cut_edit(cli, a.provider.trim(), ListVerb::Add)
        }
        Some(crate::cli::ContinueOnCutCommand::Remove(a)) => {
            continue_on_cut_edit(cli, a.provider.trim(), ListVerb::Remove)
        }
    }
}

/// Which direction the flat-list edit runs. Separate from [`Verb`] because the
/// past-tense words differ and one enum cannot serve two vocabularies.
#[derive(Clone, Copy)]
enum ListVerb {
    Add,
    Remove,
}

impl ListVerb {
    fn past(self) -> &'static str {
        match self {
            ListVerb::Add => "added",
            ListVerb::Remove => "removed",
        }
    }
}

/// The read verb: the whole list, with `default` for the empty one.
fn continue_on_cut_show(cli: &Cli, json: bool) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let rows = vec![vec![list_cell(&cfg.continue_on_cut)]];
    if json {
        print!(
            "{}",
            toon::json(
                "continue_on_cut",
                &CUT_COLUMNS,
                &toon::every_field(&CUT_COLUMNS),
                &rows,
            )
        );
    } else {
        print!(
            "{}",
            toon::list(
                "continue_on_cut",
                "continue_on_cut",
                &CUT_COLUMNS,
                &toon::every_field(&CUT_COLUMNS),
                &rows,
                false,
            )
        );
    }
    Ok(())
}

/// Columns of an `aroute continue-on-cut` row.
const CUT_COLUMNS: [&str; 1] = ["providers"];

/// The section name the continue-on-cut verbs edit.
const CUT_SECTION: &str = "continue_on_cut";

// --- `aroute oauth`, the operator's stated goal ------------------------------

/// Columns of an `aroute oauth list` row.
///
/// Seven of [`ar_config::OAuthSession`]'s fields, and no others: these are the
/// ones an operator writes by hand, and the rest are machine state
/// (`expires_at`, the device-grant endpoint) that this surface does not own.
/// Every value is a *name* or a public endpoint — never a secret, the same
/// gate `keys list` applies.
const OAUTH_COLUMNS: [&str; 7] = [
    "provider",
    "authorization_url",
    "token_url",
    "client_id",
    "redirect_uri",
    "refresh_key",
    "client_secret_key",
];

/// The section name the oauth verbs edit.
const OAUTH_SECTION: &str = "oauth";

/// A cell for a field the session does not set.
///
/// `unset` rather than an empty cell, and rather than hiding the column: the
/// two cases a caller must be able to tell apart are "no session declares
/// this" and "a session declares it empty", and only the first is what a row
/// omitting the key means.
const UNSET: &str = "unset";

/// `aroute oauth` — the read, and the `show` spelling with no verb.
fn oauth_list(cli: &Cli, json: bool) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let cell = |v: &Option<String>| v.clone().unwrap_or_else(|| UNSET.to_owned());
    let rows: Vec<Vec<String>> = cfg
        .oauth
        .iter()
        .map(|s| {
            vec![
                s.provider.clone(),
                cell(&s.authorization_url),
                cell(&s.token_url),
                cell(&s.client_id),
                cell(&s.redirect_uri),
                cell(&s.refresh_key),
                cell(&s.client_secret_key),
            ]
        })
        .collect();
    let cols: Vec<&str> = OAUTH_COLUMNS.to_vec();
    if json {
        print!(
            "{}",
            toon::json(OAUTH_SECTION, &cols, &toon::every_field(&cols), &rows)
        );
    } else {
        print!(
            "{}",
            toon::list(
                OAUTH_SECTION,
                OAUTH_SECTION,
                &cols,
                &toon::every_field(&cols),
                &rows,
                false
            )
        );
    }
    Ok(())
}

/// `aroute oauth add` — one session row, appended at the section's tail.
///
/// Every semantic is a refusal that already exists, not a new one:
///
/// * the two endpoints are **never guessed** from the provider id — the same
///   rule `OAuthSession`'s own doc states, and the reason a login reports
///   `NoAuthorizationUrl` rather than inventing an authorize endpoint;
/// * a `--refresh-key` or `--client-secret-key` must name a real `keys:` row,
///   because a credential row that resolves to nothing is a session `ar doctor`
///   reports as unable to renew, which is a row that looks finished and is not;
/// * a `--redirect-uri` must be `https` or a loopback `http`, the rule
///   `OAuthSession`'s doc states for the same field.
///
/// The block carries no secrets by design, so this verb writes placement only
/// and never asks for a value: the row names a `keys:` entry, and the value
/// lives there and in the credential store.
fn oauth_add(cli: &Cli, args: &crate::cli::OauthAddArgs) -> anyhow::Result<()> {
    let provider = args.provider.trim();
    if provider.is_empty() {
        return Err(fail(
            "the provider id is empty",
            "name one as `providers:` spells it, e.g. codex",
        ));
    }
    let doc = read_config_text(cli)?;
    let cfg = load_edit(cli)?;
    if cfg.providers.iter().all(|p| p.id != provider) {
        return Err(fail(
            format!("provider {provider:?} is not in this config's providers:"),
            "`aroute providers` lists the providers this config carries; a session is keyed to a provider id",
        ));
    }
    if cfg.oauth.iter().any(|s| s.provider == provider) {
        return Err(fail(
            format!("{} already has an `oauth:` session", provider),
            "one session per provider id; `aroute oauth list` shows what is there, and `aroute oauth remove` drops it",
        ));
    }
    // The two endpoints are operator-supplied, and never guessed from the id.
    // Nothing here offers a default, because a guessed endpoint is an invented
    // wire format: `OAuthSession`'s own doc is the rule this enforces.
    for (flag, value) in [
        ("--authorization-url", &args.authorization_url),
        ("--token-url", &args.token_url),
    ] {
        let Some(url) = value.as_deref().map(str::trim) else {
            continue;
        };
        if !(url.starts_with("https://")
            || url.starts_with("http://127.0.0.1")
            || url.starts_with("http://localhost"))
        {
            return Err(fail(
                format!("{flag} {url:?} is neither https nor a loopback http"),
                "an endpoint guessed or smuggled over plain http is a wire format this build will not write",
            ));
        }
    }
    // A redirect is public by construction, so the same scheme rule applies
    // without the secret worry: https, or a loopback http for a same-machine
    // CLI login.
    if let Some(uri) = args.redirect_uri.as_deref().map(str::trim) {
        let ok = uri.starts_with("https://")
            || uri.starts_with("http://127.0.0.1")
            || uri.starts_with("http://localhost")
            || uri == "urn:ietf:wg:oauth:2.0:oob";
        if !ok {
            return Err(fail(
                format!(
                    "--redirect-uri {uri:?} is neither https, a loopback http, nor urn:ietf:wg:oauth:2.0:oob"
                ),
                "a provider refuses a redirect it does not recognise, so this refuses to write one",
            ));
        }
    }
    // Credential *names*, checked against `keys:` as written rather than as
    // expanded: an `$AR_KEY_X` and the literal it points at are different
    // spellings of the same row, and only the written one is what the file has.
    let keys = raw_keys(&doc);
    for (flag, value) in [
        ("--refresh-key", &args.refresh_key),
        ("--client-secret-key", &args.client_secret_key),
    ] {
        let Some(name) = value.as_deref().map(str::trim) else {
            continue;
        };
        if name.is_empty() {
            return Err(fail(
                format!("{flag} is empty"),
                "name a `keys:` row, e.g. codex_refresh",
            ));
        }
        if !keys.contains_key(name) {
            return Err(fail(
                format!("{flag} {name:?} names no `keys:` row"),
                "add the row to `keys:` first, or pass a name that is already there; a credential row that resolves to nothing is a session `ar doctor` reports as unable to renew",
            ));
        }
    }
    // A row that renews needs a refresh row to renew *from*: without one the
    // session dies on its first 401, which is exactly what `persist_token`
    // refuses. `client_id` is left to the operator — some refresh endpoints
    // require one and some do not, and guessing is not this verb's job.
    let renews = args
        .token_url
        .as_deref()
        .map(str::trim)
        .is_some_and(|u| !u.is_empty());
    if renews
        && args
            .refresh_key
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    {
        return Err(fail(
            "a --token-url with no --refresh-key writes a session that cannot renew",
            "pass --refresh-key naming a `keys:` row, or drop --token-url to write a session that only dispatches",
        ));
    }
    let mut row = format!("- provider: {}\n", provider);
    let field = |dest: &mut String, key: &str, value: &Option<String>, depth: usize| {
        if let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            dest.push_str(&format!("{}{}: {}\n", " ".repeat(depth), key, v));
        }
    };
    field(
        &mut row,
        "authorization_url",
        &args.authorization_url.clone(),
        2,
    );
    field(&mut row, "token_url", &args.token_url.clone(), 2);
    field(&mut row, "client_id", &args.client_id.clone(), 2);
    field(&mut row, "redirect_uri", &args.redirect_uri.clone(), 2);
    field(&mut row, "refresh_key", &args.refresh_key.clone(), 2);
    field(
        &mut row,
        "client_secret_key",
        &args.client_secret_key.clone(),
        2,
    );
    field(&mut row, "scope", &args.scope.clone(), 2);

    let next = append_row(&doc, OAUTH_SECTION, &row);
    if next == doc {
        println!("oauth: {provider} — unchanged");
        return Ok(());
    }
    write_file(&cli.config, &next)?;
    println!("oauth: added {provider}");
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// Re-indents a rendered row so its first line and every continuation line sit
/// at `depth`.
///
/// The caller builds the row's text with `- provider: X` at column zero and the
/// rest of its keys at two spaces, because that is how the row reads when it is
/// written as one string. Sitting in a section moves the whole row, and only the
/// row's own leading whitespace is the caller's to give: a row written at zero
/// under a header is not a list, it is a syntax error.
fn indent_row(rendered: &str, depth: usize) -> String {
    rendered
        .lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{}{}", " ".repeat(depth), l)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Appends one rendered row to a `- `-list section, leaving every other line
/// byte-identical.
///
/// The `oauth:` and `combos:` shape, and one writer rather than two because
/// the rule is the same for both: the row goes at the end of the section's
/// body, and the blank lines and notes that close the section stay *below* it,
/// documenting whatever follows rather than the row that was just added.
///
/// A section with no such key yet is created whole at the document's end,
/// which is where [`splice_section`] puts a missing section too.
fn append_row(content: &str, section: &str, rendered: &str) -> String {
    let all: Vec<&str> = content.lines().collect();
    let Some(base) = section_depth(&all, section) else {
        // No section: the whole block goes at the end, with the file's own
        // terminator carried across and a newline separating the two. The
        // entry is indented to a list's entries-depth under its header, the
        // same depth the in-section path writes.
        // `trim_end` is too much: it also takes a **blank line the operator
        // wrote** at the end of the file, which is not the writer's to take.
        // Measured: a file ending `- id: c\n\n` and one ending `- id: c\n`
        // produced byte-identical output — the blank gone from the first, and
        // the new row flush against the last combo row in both. Strip the one
        // terminator and stop: a blank the operator wrote survives below the
        // last row and the new section still starts on its own line, because
        // the format's own separator is what separates the two.
        //
        // `\n\n` is not the terminator though. A CRLF document ends `\r\n`, and
        // stripping only the `\n` leaves the `\r` stranded on the previous line
        // while the separator written here is `\n` — measured, the round trip of
        // a CRLF file left `...- a/m\r` against `...- a/m\r\n`, so the write
        // changed the last byte of a line it was not asked to touch. The
        // separator is the document's own, read the same way every other
        // writer reads it, because YAML permits one style per document.
        let sep = line_ending(content);
        let tail = if content.ends_with(sep) { sep } else { "" };
        // The rendered row is spelled in LF by the renderers, so a CRLF
        // document needs it re-spelled rather than mixed: a CRLF file whose new
        // row is LF-bodied is a document YAML accepts and a diff that shows
        // every line.
        let body = indent_row(rendered, 2).replace('\n', sep);
        let trimmed = content.strip_suffix(tail).unwrap_or(content);
        return format!("{trimmed}{sep}{section}:{sep}{body}{tail}");
    };
    // The section's own body: entries sit at `base + 2`, and the row ends at
    // the first line that is neither an entry nor part of one.
    let entry_depth = base + 2;
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut i = 0;
    while i < all.len() {
        let line = all[i];
        if is_section(line, section) {
            in_section = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        if in_section && is_section_header(line) {
            in_section = false;
        }
        if in_section && is_row_end(&all, i, entry_depth) {
            // This line is the *end of a row*, which is not the same as being an
            // entry: a row is its `- provider:` line plus every continuation
            // line below it, so the last line of the last row is where the new
            // row belongs. Testing only entry lines misses the common case,
            // where the section's only row has continuations and the loop would
            // walk straight past the end of the section and write nothing.
            out.push(line.to_owned());
            out.push(indent_row(rendered, entry_depth));
            in_section = false;
            i += 1;
            continue;
        }
        out.push(line.to_owned());
        i += 1;
    }
    // The section ran to the end of the file.
    rejoin(content, out)
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join("\n")
        + &if content.ends_with('\n') {
            "\n".to_owned()
        } else {
            String::new()
        }
}

/// Whether the line at `i` is the last line of a row of the section's list.
///
/// A row is its `- ` entry plus the continuation lines indented deeper than it,
/// so a line ends a row when the *next* line is neither a deeper continuation
/// nor nothing at all. Depth is read, never length: a continuation is
/// `entry_depth + 2`, and only that continues a row.
fn is_row_end(all: &[&str], i: usize, entry_depth: usize) -> bool {
    let line = all[i];
    let in_row = indent_of(line).is_some_and(|d| d == entry_depth || d == entry_depth + 2);
    if !in_row {
        return false;
    }
    match all.get(i + 1) {
        None => true,
        Some(next) => {
            indent_of(next) != Some(entry_depth + 2)
                && !is_list_entry(next, entry_depth)
                && !next.trim().is_empty()
        }
    }
}

/// Whether `line` starts a `provider:`-keyed row of the `oauth:` list.
fn oauth_row_starts(line: &str) -> bool {
    let indent = line.len() - line.trim_start().len();
    indent == 2 && line.trim_start().starts_with("- provider:")
}

/// The provider id an `oauth:` entry line names.
fn oauth_row_id(line: &str) -> &str {
    line.trim_start()
        .trim_start_matches("- provider:")
        .trim()
        .trim_end_matches(':')
        .trim()
}

/// Removes the `oauth:` row whose provider id is `provider`, and the section
/// with it when that was its last row.
fn remove_row(content: &str, section: &str, provider: &str) -> Option<String> {
    let all: Vec<&str> = content.lines().collect();
    let start = {
        let mut in_section = false;
        let mut found = None;
        for (i, line) in all.iter().enumerate() {
            if is_section(line, section) {
                in_section = true;
                continue;
            }
            if in_section && is_section_header(line) {
                in_section = false;
            }
            if in_section && oauth_row_starts(line) && oauth_row_id(line) == provider {
                found = Some(i);
                break;
            }
        }
        found
    }?;
    // The row ends where the next entry or the next section begins, whichever
    // the file reaches first.
    let mut end = all.len();
    for (j, line) in all.iter().enumerate().skip(start + 1) {
        if is_section_header(line) || oauth_row_starts(line) {
            end = j;
            break;
        }
    }
    let mut out: Vec<String> = Vec::new();
    out.extend(all[..start].iter().map(|l| (*l).to_owned()));
    out.extend(all[end..].iter().map(|l| (*l).to_owned()));
    // The section goes when its last row went: a header with nothing under it
    // is a block the next reader has to wonder about, and an add-then-remove
    // round trip must restore the file byte for byte rather than almost.
    let header = format!("{section}:");
    let empty = out
        .iter()
        .position(|l| l.trim_end() == header)
        .is_some_and(|at| {
            out[at + 1..]
                .iter()
                .take_while(|l| !is_section_header(l))
                .all(|l| l.trim().is_empty())
        });
    if empty {
        let at = out.iter().position(|l| l.trim_end() == header).unwrap_or(0);
        let mut tail = at + 1;
        while tail < out.len() && (out[tail].trim().is_empty() || out[tail].trim_end() == header) {
            tail += 1;
        }
        // Keep one blank separator if the section took one with it.
        let keep_blank = at > 0 && out[at - 1].trim().is_empty() && tail < out.len();
        let mut kept: Vec<String> = out[..at].to_vec();
        if keep_blank && !out[at - 1].trim().is_empty() {
            kept.push(String::new());
        }
        let _ = keep_blank;
        kept.extend_from_slice(&out[tail..]);
        return Some(rejoin(content, kept));
    }
    Some(rejoin(content, out))
}

/// `aroute oauth` — the verb dispatcher, with `None` reading the list.
fn oauth(cli: &Cli, args: &crate::cli::OauthArgs) -> anyhow::Result<()> {
    match &args.command {
        None | Some(crate::cli::OauthCommand::List) => oauth_list(cli, args.json),
        Some(crate::cli::OauthCommand::Add(a)) => oauth_add(cli, a),
        Some(crate::cli::OauthCommand::Remove(a)) => oauth_remove(cli, a),
    }
}

/// `aroute oauth remove` — one session out, by the id that is its key.
fn oauth_remove(cli: &Cli, args: &crate::cli::OauthProviderArgs) -> anyhow::Result<()> {
    let provider = args.provider.trim();
    if provider.is_empty() {
        return Err(fail(
            "the provider id is empty",
            "name one as `oauth:` spells it, e.g. codex",
        ));
    }
    let doc = read_config_text(cli)?;
    let Some(next) = remove_row(&doc, OAUTH_SECTION, provider) else {
        // An id with no session is not an error: the verb's job is that the
        // session is gone, and saying so is the idempotent answer.
        println!("oauth: {provider} — unchanged (no session)");
        return Ok(());
    };
    write_file(&cli.config, &next)?;
    println!("oauth: removed {provider}");
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// The shared body of `add` and `remove`: one provider id, and the whole list
/// printed afterwards, because empty means "no continuation anywhere" and a
/// caller who cannot read the list cannot reason about what it does.
fn continue_on_cut_edit(cli: &Cli, provider: &str, verb: ListVerb) -> anyhow::Result<()> {
    if provider.is_empty() {
        return Err(fail(
            "the provider id is empty",
            "name one as `providers:` spells it, e.g. openai",
        ));
    }
    let doc = read_config_text(cli)?;
    let cfg = load_edit(cli)?;
    if !cfg.providers.iter().any(|p| p.id == provider) {
        return Err(fail(
            format!("provider {provider:?} is not in this config's providers:"),
            "`aroute providers` lists the providers this config carries",
        ));
    }
    // `continue_on_cut:` is a flat list directly under its own header, so it is
    // the `owner = None` case: the entries sit two under the section, not four.
    let edited = match verb {
        ListVerb::Add => append_nested_list_entry(&doc, CUT_SECTION, None, CUT_SECTION, provider),
        ListVerb::Remove => {
            remove_nested_list_entry(&doc, CUT_SECTION, None, CUT_SECTION, provider)
        }
    };
    let updated = match edited {
        Some(u) => u,
        None => {
            let listed = nested_list_has_entry(&doc, CUT_SECTION, None, CUT_SECTION, provider);
            match verb {
                ListVerb::Add if listed => {
                    println!("continue_on_cut: {provider} — unchanged (already listed)");
                    return Ok(());
                }
                ListVerb::Remove if !listed => {
                    println!("continue_on_cut: {provider} — unchanged (not listed)");
                    return Ok(());
                }
                _ => {
                    return Err(fail(
                        format!(
                            "continue_on_cut: {provider} is written in a form this writer does not recognise"
                        ),
                        "the block may use a flow list or another indentation; edit it by hand",
                    ));
                }
            }
        }
    };
    if updated == doc {
        println!("continue_on_cut: {provider} — unchanged ({})", verb.past());
        return Ok(());
    }
    write_file(&cli.config, &updated)?;
    println!("continue_on_cut: {} {provider}", verb.past());
    let cfg = load_edit(cli)?;
    println!("  providers: {}", list_cell(&cfg.continue_on_cut));
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// Configures an ad-hoc endpoint: the `custom_providers:` row, the `keys:`
/// reference and the `$AR_KEY_*` value.
///
/// The panel's `provider_custom` (`webui.rs:2091`) is the tested semantic, and
/// every rule here is that handler's rule rather than a new one: the id shape,
/// the two-protocol vocabulary, the base-url scheme, the relative-path rule for
/// `endpoint`, and the catalog-collision refusal (`catalog` merges the config's
/// custom nodes and surfaces `MergeError::Collides`). Two differences are
/// deliberate. **The credential is never argv** — the panel takes it from a JSON
/// body an HTTPS form already carries, and a command line is visible in the
/// process table, so it is `--credential-file <PATH|->` with the same
/// convention `keys revoke` uses. **A new row always requires a credential**,
/// because a custom node with no key is a node every request fails against,
/// which is a worse answer than an upfront refusal; an edit of an existing row
/// may omit it, which is how an operator fixes a base URL without re-pasting a
/// secret they already stored.
///
/// The binding keeps whatever variable the existing row names, the same
/// "an edit never re-points a hand-wired secret" rule `providers_add` holds: a
/// row bound to `$AR_KEY_MINE` is not silently moved to the canonical
/// `$AR_KEY_<ID>` just because a base URL changed.
fn providers_add_custom(cli: &Cli, args: &crate::cli::AddCustomProviderArgs) -> anyhow::Result<()> {
    let id = args.id.trim();
    if id.is_empty() {
        return Err(fail(
            "the provider id is empty",
            "pass the id the endpoint is routable as, 1-64 of a-z 0-9 . _ -",
        ));
    }
    if id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(fail(
            format!("provider id {id:?} is not legal (1-64 of a-z 0-9 . _ -)"),
            "the id becomes a model prefix clients route through",
        ));
    }
    let protocol = match args.protocol.as_str() {
        "openai-compatible" => ar_registry::Protocol::OpenaiCompatible,
        "anthropic-compatible" => ar_registry::Protocol::AnthropicCompatible,
        other => {
            return Err(fail(
                format!("unknown protocol {other:?}"),
                "protocol is openai-compatible or anthropic-compatible",
            ));
        }
    };
    let base_url = args.base_url.trim();
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err(fail(
            format!("bad base_url {base_url:?}"),
            "the endpoint the {id}/ prefix joins, usually https://host/v1",
        ));
    }
    let endpoint = args
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if endpoint
        .as_deref()
        .is_some_and(|e| e.starts_with('/') || e.contains("://"))
    {
        return Err(fail(
            format!("endpoint {:?} is not a relative path", endpoint),
            "the endpoint overrides the protocol's default path joined to base_url, e.g. chat/completions2",
        ));
    }
    let mut headers = BTreeMap::new();
    for h in &args.header {
        let Some((name, value)) = h.split_once('=') else {
            return Err(fail(
                format!("header {h:?} is not Name=value"),
                "send --header X-Tenant=acme, one per header",
            ));
        };
        let name = name.trim();
        if name.is_empty() || name.contains(' ') {
            return Err(fail(
                format!("header name {name:?} is not a legal header name"),
                "a header name has no spaces, e.g. X-Tenant",
            ));
        }
        headers.insert(name.to_owned(), value.trim().to_owned());
    }
    // Read the credential only after every cheap validation has passed: a config
    // with a bad protocol must not leave the process blocked on stdin.
    let secret = read_credential(&args.credential_file)?;
    let doc = read_config_text(cli)?;
    let mut cfg = load_edit(cli)?;
    // The collision refusal, from the compiled-in registry and not from
    // [`catalog`], which merges this config's own `custom_providers` into it —
    // an existing custom row would then collide with itself and every edit of
    // one would be refused. What must be refused is a *new* id shadowing a
    // compiled-in entry, which is what [`ar_registry::Registry::merge`] refuses
    // for a reason: that entry is a fact about a real provider.
    let compiled = registry();
    if compiled.get(id).is_some() {
        return Err(fail(
            format!("{id:?} is a registry catalog id"),
            "custom providers take a fresh name; configure a catalog provider's key with `aroute providers add`",
        ));
    }

    let existed = cfg.custom_providers.iter().any(|c| c.id == id);
    if secret.is_empty() && !existed {
        return Err(fail(
            "a new custom provider needs its credential",
            "pass --credential-file <PATH> or pipe it on stdin with --credential-file -",
        ));
    }
    // The variable the row names, from the file as written rather than from
    // `cfg.keys`: an unexpanded `$AR_KEY_MINE` and the literal it points at are
    // the same binding, and only the written one is the operator's choice.
    let canonical = format!("AR_KEY_{}", import::env_name(id));
    let bound = raw_keys(&doc)
        .get(id)
        .and_then(|v| v.strip_prefix('$'))
        .unwrap_or(&canonical)
        .to_owned();
    if secret.is_empty() && !bound.starts_with("AR_KEY_") {
        return Err(fail(
            format!("{id:?} binds to ${bound}, not an AR_KEY_* variable"),
            "re-point the keys entry by hand, or send the current credential again",
        ));
    }
    let var: &str = if existed && bound.starts_with("AR_KEY_") && bound != canonical {
        &bound
    } else {
        &canonical
    };

    // The row list, upserted by id and sorted — the order the import renderer
    // emits, so a splice of an already-sorted section is byte-stable.
    let mut rows = std::mem::take(&mut cfg.custom_providers);
    rows.retain(|c| c.id != id);
    rows.push(ar_registry::CustomProvider {
        id: id.to_owned(),
        protocol,
        base_url: base_url.to_owned(),
        endpoint,
        key_ref: id.to_owned(),
        headers,
    });
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let section = import::render_custom_section(&rows)?;
    let mut next = splice_section(&doc, "custom_providers", &section);
    if !existed {
        next = with_key_binding(&next, id, var)?;
    }

    // The env file first, the panel's order and the reason it is the order:
    // that is the write that publishes the credential, so a config that does
    // not change is not "nothing was written" and the note says which moved.
    let env = env_path(&cli.config);
    if !secret.is_empty() {
        set_env_var(&env, var, &secret)?;
    }
    if !means_the_same(&doc, &next) {
        write_file(&cli.config, &next)?;
    }
    println!("custom_providers: {id} (row, keys: ref, {var})");
    if secret.is_empty() {
        println!("note: the credential was not changed");
    } else {
        println!("note: the credential was written to {}", env.display());
    }
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// Unconfigures a provider, refusing while any combo still references it.
///
/// Both row lists: a registry provider's `providers:` row and an ad-hoc node's
/// `custom_providers:` row, because `add-custom` writes the second and a verb
/// pair that cannot undo its own write is not a pair. The splice drops both, the
/// same way the panel's removal does.
///
/// The reference check covers every place a combo can name a target — `targets`,
/// `pool`, `steps` and `judge_model` — the same set the panel's removal checks
/// (`webui.rs:2452`) and the set `aroute doctor` already computes. A removed
/// provider that a combo still names is not an error at write time, it is a
/// 502 at the first request, so the refusal is the cheaper answer.
///
/// The `keys:` entries the removed rows alone held are dropped with them. An
/// entry a surviving row also names is kept: two providers sharing one secret
/// is one line the operator maintains, not two.
fn providers_remove(cli: &Cli, args: &crate::cli::RemoveProviderArgs) -> anyhow::Result<()> {
    let id = args.id.trim();
    if id.is_empty() {
        return Err(fail(
            "the provider id is empty",
            "pass the id `aroute providers` lists",
        ));
    }
    let doc = read_config_text(cli)?;
    let cfg = load_edit(cli)?;
    // Both lists, not just `providers:`: `add-custom` writes into
    // `custom_providers:`, and a verb that adds a row another verb refuses to
    // remove is a half-built feature — the row is configured either way, and
    // `splice_remove_provider` already drops both.
    let configured =
        cfg.providers.iter().any(|p| p.id == id) || cfg.custom_providers.iter().any(|c| c.id == id);
    if !configured {
        return Err(fail(
            format!("provider {id:?} is not configured in this file"),
            "`aroute providers` lists what is configured",
        ));
    }
    let users: Vec<&str> = cfg
        .combos
        .iter()
        .filter(|c| {
            c.targets
                .iter()
                .chain(c.pool.iter())
                .chain(c.steps.iter().map(|s| &s.target))
                .chain(c.judge_model.iter())
                .any(|t| target_provider(t) == id)
        })
        .map(|c| c.id.as_str())
        .collect();
    if !users.is_empty() {
        return Err(fail(
            format!("{id:?} is still referenced by: {}", users.join(", ")),
            "delete or repoint those combos first; a dead provider fails loudly, a dangling one fails late",
        ));
    }
    // `Ok(None)` is unreachable here: `configured` above already refused when
    // the id is in neither list. The `expect` names that invariant rather than
    // silently reporting "unchanged" for a config this command already
    // rejected.
    let next = splice_remove_provider(&doc, &cfg, id)
        .map_err(|e| {
            fail(
                e,
                "the `providers:`, `custom_providers:` and `keys:` sections could not be rendered; nothing was written",
            )
        })?
        .unwrap_or_else(|| doc.to_owned());
    // The same semantic check: a remove whose re-render means the same thing as
    // what is on disk is a no-op, and writing it would only reorder the
    // operator's own `keys:` rows.
    if means_the_same(&doc, &next) {
        println!("providers: {id} — unchanged");
        return Ok(());
    }
    write_file(&cli.config, &next)?;
    println!("providers: removed {id}");
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// `ar.env` beside the config — the convention `aroute sync` follows and the
/// panel's `Ctx::new` derives the same way (`webui.rs:155-163`).
fn env_path(config: &std::path::Path) -> std::path::PathBuf {
    config
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("ar.env")
}

/// Merges one `NAME=value` into the env file at 0600, preserving every other
/// line: the same merge the panel's `set_env_var` performs (`webui.rs:1856`).
///
/// `write_private` rather than a plain write: the file holds credentials, so it
/// is staged at 0600 from its first byte and renamed, never written in place.
pub(crate) fn set_env_var(path: &std::path::Path, name: &str, value: &str) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let entry = format!("{name}={value}");
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    match lines
        .iter()
        .position(|l| l.starts_with(&format!("{name}=")))
    {
        Some(i) => lines[i] = entry,
        None => lines.push(entry),
    }
    write_private(path, &(lines.join("\n") + "\n")).map_err(|e| {
        fail(
            format!("cannot write {}: {e}", path.display()),
            "check permissions on the config directory",
        )
    })
}

/// A credential from a file or from stdin, never from argv.
///
/// `-` is stdin, matching `keys revoke`'s `read_token_file` (`commands.rs:546`):
/// one convention for one kind of input. Trailing whitespace is trimmed,
/// because a credential pasted from a terminal carries a newline, and an empty
/// file is refused rather than written as an empty variable — the loader then
/// fails at the first request, which is worse than this.
fn read_credential(path: &str) -> anyhow::Result<String> {
    let raw = if path == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .map_err(|e| fail(e, "the credential could not be read from stdin"))?;
        buf
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| fail(e, "the credential file could not be read"))?
    };
    let secret = raw.trim();
    if secret.is_empty() {
        return Err(fail(
            "the credential is empty",
            "the file must hold the credential, and nothing else",
        ));
    }
    Ok(secret.to_owned())
}

fn combo(cli: &Cli, args: &ListArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let catalog = catalog(&cfg)?;
    let fields = columns(
        &toon::fields_or_default(args.fields.as_deref()),
        &COMBO_COLUMNS,
        "combo",
    )?;
    let rows = combo_rows(&cfg.combos, &catalog);
    let total = rows.len();
    let rows = page(&rows, args.offset.unwrap_or(0), args.limit);
    if args.json {
        print!(
            "{}",
            toon::json_paged("combo", &COMBO_COLUMNS, &fields, &rows, total)
        );
    } else {
        print!(
            "{}",
            toon::list_paged(
                "combo",
                "combos",
                &COMBO_COLUMNS,
                &fields,
                &rows,
                args.full,
                total
            )
        );
    }
    Ok(())
}

/// `aroute combo add`: validate, then append one combo row.
///
/// The write is an append of one block onto `combos:`, the same shape the
/// panel writes: nothing else in the file is re-rendered, so the diff is the
/// new row and nothing more. The two validation passes matter — the second on
/// the *result* catches a duplicate id and a target no provider serves, which
/// the first (on the incoming config) cannot see.
fn combo_add(cli: &Cli, args: &AddComboArgs) -> anyhow::Result<()> {
    let id = args.id.trim();
    if id.is_empty() {
        return Err(fail(
            "the combo id is empty",
            "the id is the model a client asks for, 1-64 of a-z 0-9 . _ -",
        ));
    }
    let strategy = args.strategy.trim();
    if strategy.is_empty() {
        return Err(fail(
            "the strategy is empty",
            "one of the strategies `aroute combo` lists, e.g. priority",
        ));
    }
    let (targets, weights) = parse_targets(&args.targets)?;

    let doc = read_config_text(cli)?;
    let cfg = load_edit(cli)?;

    // The named strategies this build routes, plus whatever the file already
    // holds: `check_combos` accepts the latter so a config written by an older
    // aroute round-trips instead of being refused, and this CLI must not
    // reject what the panel saves.
    let in_file = cfg.combos.iter().any(|c| c.strategy.as_str() == strategy);
    if !STRATEGIES.contains(&strategy) && !in_file {
        return Err(fail(
            format!("strategy {strategy:?} is not one this build routes"),
            format!("pick one of {}", STRATEGIES.join(", ")),
        ));
    }
    if cfg.combos.iter().any(|c| c.id == id) {
        return Err(fail(
            format!("combo {id:?} already exists"),
            "a combo is one model name; repoint it with `aroute combo target add`",
        ));
    }
    // The one validation both surfaces now share, so the CLI and the panel
    // cannot disagree about which combos a config may hold.
    check_combos(&cfg.combos, &cfg).map_err(|e| {
        fail(
            e,
            "the config's own combos do not validate; fix those first",
        )
    })?;

    let mut next = cfg;
    next.combos.push(Combo {
        id: id.to_owned(),
        strategy: Strategy::parse(strategy),
        targets,
        weights,
        pool: Vec::new(),
        steps: Vec::new(),
        compression: None,
        context_length: None,
        judge_model: None,
    });
    // The candidate is checked too: this is where a target whose provider no
    // provider serves and a duplicate id are refused, on the document about to
    // be written rather than on the one on disk.
    check_combos(&next.combos, &next)
        .map_err(|e| fail(e, "the new combo does not validate; nothing was written"))?;
    // One row, rendered on its own: [`append_combo_row`] places it inside the
    // section without re-rendering the others, which is what makes the write
    // surgical and the round trip byte-stable.
    let row = crate::import::render_combo_block(next.combos.last().unwrap())?;
    let updated = append_combo_row(&doc, &row);
    if updated == doc {
        // Byte equality, not [`means_the_same`]: a row-level writer changed
        // exactly the lines it owns, so the candidate either differs or the
        // edit did not land.
        println!("combos: {id} — unchanged");
        return Ok(());
    }
    write_file(&cli.config, &updated)?;
    println!("combos: added {id}");
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// `aroute combo remove`: refuse while depended on, then drop the row.
///
/// The dependency check covers every place a combo can name a target, the same
/// set `providers remove` refuses against: a combo stepped or judged through
/// another would dangle the moment that row left the file, and a dangling
/// reference fails late rather than loudly.
fn combo_remove(cli: &Cli, args: &RemoveComboArgs) -> anyhow::Result<()> {
    let id = args.id.trim();
    let doc = read_config_text(cli)?;
    let cfg = load_edit(cli)?;
    if !cfg.combos.iter().any(|c| c.id == id) {
        return Err(fail(
            format!("combo {id:?} is not in this config"),
            "`aroute combo` lists the routing table",
        ));
    }
    let users: Vec<&str> = cfg
        .combos
        .iter()
        .filter(|c| c.id != id && combo_names(c, id))
        .map(|c| c.id.as_str())
        .collect();
    if !users.is_empty() {
        return Err(fail(
            format!("combo {id:?} is still referenced by: {}", users.join(", ")),
            "delete or repoint those combos first; a dead combo fails loudly, a dangling one fails late",
        ));
    }

    // One row's lines, and only those: the operator's blank separator and
    // closing note stay exactly where they were, so removing the last combo
    // added restores the file byte-for-byte.
    let updated = remove_combo_row(&doc, id).ok_or_else(|| {
        fail(
            format!("combo {id:?} has no `- id:` row to remove"),
            "the row may be written in a form this writer does not recognise; edit it by hand",
        )
    })?;
    if updated == doc {
        // Byte equality — see `combo_add` on why not `means_the_same`.
        println!("combos: {id} — unchanged");
        return Ok(());
    }
    write_file(&cli.config, &updated)?;
    println!("combos: removed {id}");
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// Whether `combo` names `id` as a target, pool entry, step or judge.
///
/// --- visibility writers -----------------------------------------------------
///
/// The `visibility:` block is a map of lists rather than a list of rows, so the
/// Phase 4 row writers do not reach it: there is no `- id:` line to splice
/// beside. The discipline is the same one, pointed at the other shape.
///
/// A block is written:
///
/// ```yaml
/// visibility:
///   blocked_providers:
///     - groq
///   hide_paid_models: false
///   model_visibility_denylist:
///     - *-search
/// ```
///
/// The section sits at column 0, its keys two in, and each list's entries four.
/// Indentation is the whole of the disambiguator here, so a block at another
/// depth, or a flow list (`blocked_providers: [a, b]`), is refused rather than
/// edited wrongly.
///
/// Columns of an `aroute visibility` row.
const VISIBILITY_COLUMNS: [&str; 4] = ["blocked_providers", "allow", "deny", "hide_paid"];

/// The section the visibility verbs edit.
const VIS_SECTION: &str = "visibility";

/// The two lists inside it, spelled as `Config::visibility` spells them.
const LIST_BLOCKED: &str = "blocked_providers";
const LIST_ALLOW: &str = "model_visibility_allowlist";
const LIST_DENY: &str = "model_visibility_denylist";

/// How deep a line is indented, in spaces, or `None` when it carries no indent.
fn indent_of(line: &str) -> Option<usize> {
    let n = line.len() - line.trim_start().len();
    if n == 0 { None } else { Some(n) }
}

/// Whether `line` is the `list:` key of a block sequence at `depth`.
fn is_list_key(line: &str, list: &str, depth: usize) -> bool {
    indent_of(line) == Some(depth) && line.trim_end() == format!("{}{}:", " ".repeat(depth), list)
}

/// Whether `line` is an entry of a block sequence at `depth`.
fn is_list_entry(line: &str, depth: usize) -> bool {
    indent_of(line) == Some(depth) && line.trim_start().starts_with("- ")
}

/// Whether `line` is a YAML key at `depth`: a scalar (`hide_paid_models: false`)
/// or the header of another list.
fn is_key_at(line: &str, depth: usize) -> bool {
    indent_of(line) == Some(depth) && !line.trim_start().starts_with("- ") && line.contains(':')
}

/// The value an entry line carries, without the `- `, the indent, or the quotes
/// a [`yaml_entry`] write added.
fn list_entry_value(line: &str) -> String {
    let raw = line.trim_start().trim_start_matches("- ").trim();
    // A single-quoted scalar: YAML's only escape inside it is a doubled quote,
    // so both the wrapper and the doubling come off to give the value back.
    let Some(body) = raw.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) else {
        return raw.to_owned();
    };
    body.replace("''", "'")
}

/// The indent of `section`, or `None` when the document does not carry it.
///
/// The section is named without its colon (`visibility`, as the callers spell
/// it) and the header line carries one, so the comparison adds it. The exact
/// header line, so `visibility` cannot match a `visibility_note:` the operator
/// added.
fn section_depth(all: &[&str], section: &str) -> Option<usize> {
    let line = all.iter().find(|l| is_section(l, section))?;
    Some(line.len() - line.trim_start().len())
}

/// Whether `line` is `section`'s own header, colon and all.
fn is_section(line: &str, section: &str) -> bool {
    line.trim_end() == format!("{section}:")
}

/// The rejoined document, remembering whether the file ended with a newline.
///
/// `str::lines` drops the terminator, so a document that ends without one would
/// otherwise gain it — a change to a byte nobody asked to touch.
fn rejoin(content: &str, out: Vec<String>) -> String {
    let sep = line_ending(content);
    let joined = out.join(sep);
    if content.ends_with('\n') || content.ends_with("\r\n") {
        joined + sep
    } else {
        joined
    }
}

/// Whether the document uses CRLF line endings, which every writer has to know
/// and which none asked: `str::lines` strips the `\r`, so rejoining with `\n`
/// rewrites the whole file. Measured: a 164-byte CRLF config came back 109
/// bytes after one `combo add`, all twelve `\r` gone. YAML allows one style
/// per document, so the answer is read from the document rather than chosen.
///
/// `serde_yaml` parses either, so this was never a validity problem — the
/// loader never warned while the bytes changed.
fn uses_crlf(content: &str) -> bool {
    content.contains('\r')
}

/// The separator to rejoin with and to close the file with, for the document's
/// own style. LF and CRLF behave identically everywhere except these bytes.
fn line_ending(content: &str) -> &'static str {
    if uses_crlf(content) { "\r\n" } else { "\n" }
}

/// Splits a document into its written part and whether it ended with a blank
/// line the operator put there.
///
/// `str::trim_end` cannot tell a trailing blank from a missing terminator,
/// which is why the writers that rebuilt a tail from `trim_end` ate one
/// operator's blank and left another's file alone. The document is cut at its
/// last non-newline byte instead, so the two states stay two states.
///
/// The `head` keeps every byte up to and including the last line's own `\n`,
/// or all of it for a document with no terminator.
fn split_trailing_blank(content: &str) -> (&str, bool) {
    let trimmed = content.trim_end_matches('\n');
    // One blank is what a trailing `\n\n` means; more is the operator's
    // paragraph spacing and is still theirs.
    let blank = content.len() > trimmed.len() + usize::from(content.ends_with('\n'));
    (trimmed, blank)
}

/// A value written as a block-sequence entry, quoted when the raw form would
/// be read as something other than the string it is.
///
/// `*` is the case that bites this surface: `- *-search` is an *alias* to an
/// anchor, so a leading-star glob parses as a reference to a label that does not
/// exist and the whole config fails to load with "unknown anchor". The matcher's
/// glob syntax and YAML's scalar syntax collide exactly there, and the writer is
/// the only place that can see the collision. Quoting resolves it, and a quoted
/// glob reads back as the same string — so `deny --glob '*-search'` survives.
fn yaml_entry(value: &str) -> String {
    let dangerous = value.is_empty()
        || value.trim() != value
        || value.starts_with([
            '!', '&', '*', '?', '[', ']', '{', '}', '#', '|', '>', '@', '`', '"', '\'', '%',
        ]);
    if dangerous {
        // Single-quoted YAML, where the only escape is a doubled quote: no
        // backslash sequence can turn a literal glob into something else.
        format!("'{}'", value.replace('\'', "''"))
    } else {
        value.to_owned()
    }
}

/// Where a new entry goes: after the last line that belongs to the list.
///
/// Walks back over the blanks and notes that close it, so an entry added to a
/// list the operator had annotated lands inside that list rather than above a
/// note that was documenting its last row.
fn list_end(out: &[String]) -> usize {
    let mut at = out.len();
    while at > 0 && (out[at - 1].is_empty() || out[at - 1].starts_with('#')) {
        at -= 1;
    }
    at
}

/// Appends one entry to a list inside a section, or `None` when the document
/// does not carry that list in the form this writer recognises.
///
/// `None` rather than a re-render, because a flow list or a list at an
/// unexpected depth is a section this writer would have to rewrite, and a
/// rewrite is what the row writers exist to avoid: it moves the operator's
/// notes and re-spells lines nobody asked to change.
fn append_list_entry(content: &str, section: &str, list: &str, entry: &str) -> Option<String> {
    // Idempotent at the writer, not at the caller: an entry already there is
    // returned unchanged, so a verb that adds again is a no-op by construction
    // rather than by a second check that can disagree with this one. A list with
    // the entry twice is a duplicate the reader counts in its own verdict.
    if list_has_entry(content, section, list, entry) {
        return Some(content.to_owned());
    }
    // The value as it must be **written**, which is not always the value as it
    // is: a leading-`*` glob is a YAML alias and would break the file outright.
    let value = yaml_entry(entry);
    let all: Vec<&str> = content.lines().collect();
    // No section is the case the creation path is written for: an early `None`
    // from `?` would make the first `block` on a config carrying no `visibility:`
    // block at all an error, which is the commonest call there is.
    let Some(depth) = section_depth(&all, section) else {
        return Some(insert_list_at_end(content, section, list, entry, None));
    };
    let key_depth = depth + 2;
    let entry_depth = key_depth + 2;
    let indent = " ".repeat(entry_depth);

    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut in_list = false;
    let mut inserted = false;
    let mut i = 0;
    while i < all.len() {
        let line = all[i];
        if is_section(line, section) {
            in_section = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        if in_section && is_section_header(line) {
            // The section ends here. When the list was inside it, the entry
            // belongs at that list's own end, not at the end of whatever section
            // follows. Insert and carry on — returning here would drop the rest
            // of the document, which `out` has not seen yet.
            if in_list {
                out.insert(list_end(&out), format!("{indent}- {value}"));
                inserted = true;
                in_list = false;
            }
            in_section = false;
        }
        if in_section && !in_list && is_list_key(line, list, key_depth) {
            in_list = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        // A blank or a column-0 note ends the list when what follows belongs to
        // another section — the look-ahead `splice_section` uses, for the same
        // reason: the block documenting the next section survives a write it
        // had nothing to do with. A note immediately after this list is the
        // same situation from the other side: it is the *rest of the section's*
        // trailing matter, so the entry goes in above it rather than below it,
        // where it would read as part of the block the note introduces.
        if in_list && (line.is_empty() || line.starts_with('#')) {
            out.insert(list_end(&out), format!("{indent}- {value}"));
            inserted = true;
            out.push(line.to_owned());
            in_list = false;
            i += 1;
            continue;
        }
        // The list's own end: the next key of the section.
        if in_list && is_key_at(line, key_depth) {
            out.insert(list_end(&out), format!("{indent}- {value}"));
            inserted = true;
            in_list = false;
        }
        out.push(line.to_owned());
        i += 1;
    }
    if in_list {
        // The list ran to the end of the file.
        out.push(format!("{indent}- {value}"));
        inserted = true;
    }
    // The section exists but carries no such list, or no section exists at all.
    // Either way the list is created, because refusing here would make the verb
    // useless exactly when it is wanted: the first `block` on a config that
    // carries no `visibility:` block at all is the commonest call there is.
    if inserted {
        // The entry went where it belonged inside the section; nothing to create.
        return Some(rejoin(content, out));
    }
    // The section exists but carries no such list. The key belongs **inside that
    // section**, at its last body line — appending to the end of the document
    // would put it inside whatever section happens to be last, which is how a
    // `visibility:` key ends up under `mcp:` and the loader refuses the file.
    let at = section_tail(&out, section);
    let key_indent = " ".repeat(depth + 2);
    let entry_indent = " ".repeat(depth + 4);
    out.insert(at, format!("{entry_indent}- {value}"));
    out.insert(at, format!("{key_indent}{list}:"));
    Some(rejoin(content, out))
}

/// Where a new key goes inside its section: after the section's last body line,
/// before the blank lines and notes that close it.
fn section_tail(out: &[String], section: &str) -> usize {
    let mut i = 0;
    let mut in_section = false;
    let mut last_body = 0;
    while i < out.len() {
        let line = &out[i];
        if is_section(line, section) {
            in_section = true;
            i += 1;
            continue;
        }
        if in_section && is_section_header(line) {
            return last_body;
        }
        if in_section {
            last_body = i + 1;
        }
        i += 1;
    }
    last_body
}

/// Creates `section`'s `list` with one entry, appending to the section that is
/// already there or adding the whole section at the end of the file.
///
/// The one path that adds a section, and deliberately the same rule
/// [`splice_section`] already uses for a missing one: the block goes at the end
/// of the document rather than wherever the caller might expect it, because a
/// section that does not exist has no place to be restored to and the tail is
/// the one location that moves no line that already had meaning.
fn insert_list_at_end(
    content: &str,
    section: &str,
    list: &str,
    entry: &str,
    depth: Option<usize>,
) -> String {
    // The value as it must be written: see [`yaml_entry`] on why a leading `*`
    // cannot go in unquoted.
    let value = yaml_entry(entry);
    let base = depth.unwrap_or(0);
    let key_indent = " ".repeat(base + 2);
    let entry_indent = " ".repeat(base + 4);
    let block = format!("{key_indent}{list}:\n{entry_indent}- {value}");
    // The file's own terminator, carried across: this config ends without one
    // deliberately, and a write that adds it changes a byte nobody asked to
    // touch, which is exactly what a round trip measures.
    //
    // `trim_end` is too much here for the reason the other writers record:
    // it cannot tell a trailing newline from a trailing blank line, and a
    // document ending `- id: c\n\n` came back `- id: c\n` — measured on a
    // `visibility block` whose round trip lost the operator's own blank.
    let (head, blank) = split_trailing_blank(content);
    let tail = if content.ends_with('\n') { "\n" } else { "" };
    let blank = if blank { "\n" } else { "" };
    match depth {
        Some(_) => {
            let mut out: Vec<String> = content.lines().map(str::to_owned).collect();
            out.push(block);
            rejoin(content, out)
        }
        None => format!("{head}{blank}\n{section}:\n{block}{tail}"),
    }
}

/// Removes one entry from a list inside a section by its value.
///
/// The inverse of [`append_list_entry`], and the reason both exist: `block`
/// followed by `unblock` of the same provider must restore the file
/// byte-for-byte. Only the line the entry owns is dropped, so the list's own
/// key and every other byte stay where they were.
///
/// An emptied list is dropped **with its key**, and a section left holding
/// nothing is dropped too. Without that, `block` then `unblock` of one provider
/// restores the file to *almost* what it was, leaving a dead `visibility:` block
/// above an empty `blocked_providers:` key for the next reader to wonder about.
fn remove_list_entry(content: &str, section: &str, list: &str, entry: &str) -> Option<String> {
    let all: Vec<&str> = content.lines().collect();
    let depth = section_depth(&all, section)?;
    let key_depth = depth + 2;
    let entry_depth = key_depth + 2;

    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut in_list = false;
    let mut i = 0;
    let mut removed = false;
    while i < all.len() {
        let line = all[i];
        if is_section(line, section) {
            in_section = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        if in_section && is_section_header(line) {
            in_section = false;
        }
        if in_section && !in_list && is_list_key(line, list, key_depth) {
            in_list = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        if in_list && is_key_at(line, key_depth) {
            in_list = false;
        }
        if in_list
            && !removed
            && is_list_entry(line, entry_depth)
            && list_entry_value(line).as_str() == entry
        {
            removed = true;
            i += 1;
            continue;
        }
        out.push(line.to_owned());
        i += 1;
    }
    if !removed {
        return None;
    }
    // Second pass over what the first produced: the key goes when nothing is
    // left under it, and the section goes when nothing is left in it.
    let kept = prune_empty_list(&out, list, key_depth, entry_depth);
    let kept = if section_is_empty(&kept, section, depth) {
        drop_section(&kept, section)
    } else {
        kept
    };
    if kept.iter().all(|l| l.trim().is_empty()) {
        // The file carried nothing but this section: the empty document is the
        // honest answer, rather than a file of blank lines the loader has to
        // refuse.
        return Some(String::new());
    }
    Some(rejoin(content, kept))
}

/// Drops `list`'s key when no entry is left under it.
///
/// The key survives while another entry does, because the operator's key
/// ordering is theirs and a writer has no reason to touch it.
fn prune_empty_list(
    out: &[String],
    list: &str,
    key_depth: usize,
    entry_depth: usize,
) -> Vec<String> {
    let Some(key_at) = out.iter().position(|l| is_list_key(l, list, key_depth)) else {
        return out.to_vec();
    };
    let has_entry = out
        .iter()
        .skip(key_at + 1)
        .take_while(|l| !is_key_at(l, key_depth))
        .any(|l| is_list_entry(l, entry_depth));
    if has_entry {
        return out.to_vec();
    }
    // The key and the blank lines under it go, keeping the next line, which is
    // either the following key of the section or the next section's header.
    let mut kept: Vec<String> = out[..key_at].to_vec();
    let mut i = key_at + 1;
    while i < out.len() && (is_list_entry(&out[i], entry_depth) || out[i].trim().is_empty()) {
        i += 1;
    }
    kept.extend_from_slice(&out[i..]);
    kept
}

/// Whether `section` holds no key at all, given where its keys sit.
fn section_is_empty(lines: &[String], section: &str, depth: usize) -> bool {
    let key_depth = depth + 2;
    let mut in_section = false;
    for line in lines {
        if is_section(line, section) {
            in_section = true;
            continue;
        }
        if in_section && is_section_header(line) {
            return true;
        }
        if in_section && is_key_at(line, key_depth) {
            return false;
        }
    }
    in_section
}

/// Removes `section`'s header and every line under it.
fn drop_section(lines: &[String], section: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    for line in lines {
        if is_section(line, section) {
            in_section = true;
            continue;
        }
        if in_section && is_section_header(line) {
            in_section = false;
        }
        if !in_section {
            out.push(line.clone());
        }
    }
    out
}

/// Whether the document already carries `entry` in `section`'s `list`.
///
/// Read by the writer's own scan rather than by a second parsing rule, so the
/// idempotence check cannot disagree with the write that follows it.
fn list_has_entry(content: &str, section: &str, list: &str, entry: &str) -> bool {
    remove_list_entry(content, section, list, entry).is_some()
}

/// Sets one scalar inside a section, creating the section when it is absent.
///
/// `hide_paid_models` is the only scalar the visibility verbs reach. Setting it
/// is the one edit that cannot be a list append, so it gets its own writer: the
/// existing key is re-spelled in place, and a section with no such key gains one
/// at the end.
/// The key a scalar line names, without its indent or its value.
///
/// `  hide_paid_models: false` names `hide_paid_models`, and so does
/// `  hide_paid_models:` — the name is the part before the first colon, and a
/// caller that compares the whole line has to know the value it is looking for,
/// which is the thing it is trying to change.
fn key_name(line: &str) -> &str {
    let body = line.trim();
    match body.find(':') {
        Some(at) => body[..at].trim(),
        None => body,
    }
}

fn set_section_scalar(content: &str, section: &str, key: &str, value: bool) -> String {
    let all: Vec<&str> = content.lines().collect();
    let Some(depth) = section_depth(&all, section) else {
        // No block at all. A `false` writes **nothing**: the key is already
        // absent, and absence is what `ModelVisibility` serialises for false,
        // so creating a section to hold `hide_paid_models: false` would spell
        // out the value the file's own canonical form omits — a key that
        // appears from a verb whose direction is "remove". Measured: a 151-byte
        // config with no `visibility:` grew to 189 bytes carrying
        // `hide_paid_models: false`, and no later write takes it back, because
        // the false direction stops looking once it finds nothing to remove.
        if !value {
            return content.to_owned();
        }
        // No block at all: the whole section is added at the end of the file,
        // which is the same place `splice_section` puts a missing section.
        //
        // The final newline is the file's, not the writer's. `str::lines`
        // drops it, so a document that ends without one rejoins and gains it —
        // a byte-level change to a file the operator asked to change somewhere
        // else, and one the round trip cannot undo because the removal below
        // is correct and the added byte is not its to take back. Measured on
        // this repo's own `config.yaml` (108 lines, no final newline):
        // `hide-paid true` grew it 4833 -> 4871 bytes and changed its last byte
        // from `6f` to `0a`; `hide-paid false` then stopped at 4834, one byte
        // short of a round trip, forever.
        //
        // `trim_end` is not enough on its own here: it also removes a trailing
        // blank line the operator wrote, which is the same class of claim in
        // the other direction. So the ending is taken from the file and put
        // back, and only the one separator the new section needs is written.
        let ending = if content.ends_with('\n') { "\n" } else { "" };
        let body = content.strip_suffix(ending).unwrap_or(content);
        // The section goes on a line of its own, so exactly one separator sits
        // between the body's last line and the new header. The file's own
        // terminator already provides it when it has one — stripping it and not
        // restoring it is what put `visibility:` flush against the last
        // `combos:` row *and* took the file's final newline. When it has
        // none, one is written, because a header cannot share a line with the
        // row above it.
        let lead = if ending.is_empty() { "\n" } else { ending };
        return format!("{body}{lead}{section}:\n  {key}: {value}{ending}");
    };
    let key_depth = depth + 2;
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut i = 0;
    let mut done = false;
    while i < all.len() {
        let line = all[i];
        if is_section(line, section) {
            in_section = true;
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        // The key is recognised by its *name*, not by the whole line: the
        // operator's own value is on the right of the colon and it is whatever
        // they wrote. Matching a bare `key:` header instead matched nothing, so
        // the only thing this writer ever did was append a second copy of the
        // key beside the first.
        if in_section && is_key_at(line, key_depth) && key_name(line) == key {
            // `false` is the value `ModelVisibility` omits when it serialises,
            // so the file's own canonical spelling of "false" is the key's
            // absence. Writing the word leaves a byte the operator did not put
            // there, and the round trip that should have restored the file
            // byte-for-byte now cannot: an explicit false is still a written
            // key. The write therefore has two directions, and the false one
            // removes what it finds.
            if value {
                out.push(format!("{}{}: {}", " ".repeat(key_depth), key, value));
            }
            done = true;
            in_section = false;
            i += 1;
            continue;
        }
        if in_section && is_section_header(line) {
            // The section ended before the key appeared. It is written at the
            // section's own tail, which is where a new key belongs — and a
            // `false` that finds nothing writes nothing, because the key was
            // already absent and absence is what false means here.
            if !done && value {
                out.push(format!("{}{}: {}", " ".repeat(key_depth), key, value));
                done = true;
            }
            in_section = false;
        }
        out.push(line.to_owned());
        i += 1;
    }
    if in_section && !done && value {
        out.push(format!("{}{}: {}", " ".repeat(key_depth), key, value));
    }
    let rejoined = rejoin(content, out);
    // A removal that takes the section's last key leaves an empty block, which
    // is a header the next reader has to wonder about — and it is exactly the
    // byte an add-then-remove round trip must not leave. The section goes with
    // the key, but only when the key was the section's whole body.
    // Drop the section only when this write emptied it: a removal that took the
    // section's last key leaves a header with nothing under it, and that is the
    // byte an add-then-remove round trip must not leave. A section holding a
    // list, or any other key, keeps its header.
    if !value && !section_has_a_key(&rejoined, section, depth) {
        return drop_section_str(&rejoined, section);
    }
    rejoined
}

/// Whether `section` still holds any key of its own, at its keys' depth.
///
/// A block sequence entry is not a key, so a section holding only a list is
/// still a section with content: this answers about `hide_paid_models:` style
/// scalars and the keys of the block, which is the thing a scalar removal can
/// empty.
fn section_has_a_key(doc: &str, section: &str, depth: usize) -> bool {
    let all: Vec<&str> = doc.lines().collect();
    let key_depth = depth + 2;
    let mut in_section = false;
    for line in &all {
        if is_section(line, section) {
            in_section = true;
            continue;
        }
        if in_section && is_section_header(line) {
            return false;
        }
        if in_section && indent_of(line) == Some(key_depth) && line.contains(':') {
            return true;
        }
    }
    false
}

/// Drops `section` and its body, keeping everything else byte-identical.
fn drop_section_str(doc: &str, section: &str) -> String {
    let all: Vec<&str> = doc.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    for line in &all {
        if is_section(line, section) {
            in_section = true;
            continue;
        }
        if in_section && is_section_header(line) {
            in_section = false;
        }
        if !in_section {
            out.push((*line).to_owned());
        }
    }
    rejoin(doc, out)
}

/// `aroute visibility`, and its `show` spelling.
fn visibility(cli: &Cli, args: &crate::cli::VisibilityArgs) -> anyhow::Result<()> {
    match &args.command {
        None | Some(crate::cli::VisibilityCommand::Show) => visibility_show(cli, args.json),
        Some(crate::cli::VisibilityCommand::Block(a)) => visibility_block(cli, a.provider.trim()),
        Some(crate::cli::VisibilityCommand::Unblock(a)) => {
            visibility_unblock(cli, a.provider.trim())
        }
        Some(crate::cli::VisibilityCommand::HidePaid(a)) => visibility_hide_paid(cli, a.value),
        Some(crate::cli::VisibilityCommand::Allow(a)) => {
            visibility_glob(cli, LIST_ALLOW, "allowed", a.glob.trim())
        }
        Some(crate::cli::VisibilityCommand::Deny(a)) => {
            visibility_glob(cli, LIST_DENY, "denied", a.glob.trim())
        }
    }
}

/// The read verb: four fields, with `default` for the unset ones.
///
/// `default` rather than an empty cell, so a caller cannot confuse "unset" with
/// "empty on purpose": the two mean opposite things here.
fn visibility_show(cli: &Cli, json: bool) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let v = &cfg.visibility;
    let rows = vec![vec![
        list_cell(&v.blocked_providers),
        list_cell(&v.model_visibility_allowlist),
        list_cell(&v.model_visibility_denylist),
        // Spelled for its own default: `false` is the same as unset, so printing
        // `default` here would hide the only value that changes the catalog.
        v.hide_paid_models.to_string(),
    ]];
    if json {
        print!(
            "{}",
            toon::json(
                "visibility",
                &VISIBILITY_COLUMNS,
                &toon::every_field(&VISIBILITY_COLUMNS),
                &rows,
            )
        );
    } else {
        print!(
            "{}",
            toon::list(
                "visibility",
                "visibility",
                &VISIBILITY_COLUMNS,
                &toon::every_field(&VISIBILITY_COLUMNS),
                &rows,
                false,
            )
        );
    }
    Ok(())
}

/// One list cell: `default` when empty, comma-separated when not.
fn list_cell(values: &[String]) -> String {
    if values.is_empty() {
        "default".to_owned()
    } else {
        values.join(",")
    }
}

/// `aroute visibility block --provider X`.
fn visibility_block(cli: &Cli, provider: &str) -> anyhow::Result<()> {
    visibility_list_edit(cli, LIST_BLOCKED, provider, Verb::Block)
}

/// `aroute visibility unblock --provider X`.
fn visibility_unblock(cli: &Cli, provider: &str) -> anyhow::Result<()> {
    visibility_list_edit(cli, LIST_BLOCKED, provider, Verb::Unblock)
}

/// Which direction a list edit runs, and how it is reported.
#[derive(Clone, Copy)]
enum Verb {
    Block,
    Unblock,
}

impl Verb {
    /// What the success line calls the state that now holds.
    fn past(self) -> &'static str {
        match self {
            Verb::Block => "blocked",
            Verb::Unblock => "unblocked",
        }
    }
}

/// The shared body of `block` and `unblock`.
///
/// The two differ only in direction, so the rules live once: the provider must
/// be one `providers:` already carries, a duplicate is a no-op that says so, and
/// the reverse edit restores the file byte-for-byte.
fn visibility_list_edit(cli: &Cli, list: &str, provider: &str, verb: Verb) -> anyhow::Result<()> {
    if provider.is_empty() {
        return Err(fail(
            "the provider id is empty",
            "name one as `providers:` spells it, e.g. groq",
        ));
    }
    let doc = read_config_text(cli)?;
    let cfg = load_edit(cli)?;
    // Refused rather than written: `is_provider_blocked` matches by id, alias and
    // `-search`-stripped id, so an id that names nothing would be a row that
    // silently does nothing at all.
    if !cfg.providers.iter().any(|p| p.id == provider) {
        return Err(fail(
            format!("provider {provider:?} is not in this config's providers:"),
            "`aroute providers` lists the providers this config carries",
        ));
    }
    let edited = match verb {
        Verb::Block => append_list_entry(&doc, VIS_SECTION, list, provider),
        Verb::Unblock => remove_list_entry(&doc, VIS_SECTION, list, provider),
    };
    let updated = match edited {
        Some(u) => u,
        None => {
            // The writer declined. The one legitimate reason is that the state
            // already matches, which is a no-op and must say so rather than
            // error: a verb that fails on the state it was asked for teaches a
            // caller to check first.
            match verb {
                Verb::Block if list_has_entry(&doc, VIS_SECTION, list, provider) => {
                    println!("visibility: {provider} — unchanged (already blocked)");
                    return Ok(());
                }
                Verb::Unblock if !list_has_entry(&doc, VIS_SECTION, list, provider) => {
                    println!("visibility: {provider} — unchanged (not blocked)");
                    return Ok(());
                }
                _ => {
                    return Err(fail(
                        format!(
                            "visibility: {provider} is written in a form this writer does not recognise"
                        ),
                        "the block may use a flow list or another indentation; edit it by hand",
                    ));
                }
            }
        }
    };
    if updated == doc {
        println!("visibility: {provider} — unchanged ({})", verb.past());
        return Ok(());
    }
    write_file(&cli.config, &updated)?;
    println!("visibility: {} {provider}", verb.past());
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// `aroute visibility hide-paid true|false`.
///
/// `false` is the same as unset, so clearing the filter is the ordinary way to
/// undo it, and both directions are byte-reversible.
fn visibility_hide_paid(cli: &Cli, hide_paid: bool) -> anyhow::Result<()> {
    let doc = read_config_text(cli)?;
    let updated = set_section_scalar(&doc, VIS_SECTION, "hide_paid_models", hide_paid);
    if updated == doc {
        println!("visibility: hide_paid — unchanged ({hide_paid})");
        return Ok(());
    }
    write_file(&cli.config, &updated)?;
    println!("visibility: hide_paid_models = {hide_paid}");
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// `aroute visibility allow|deny --glob G`.
///
/// The glob is checked once here. What is actually checkable is narrow: the
/// matcher compiles anything, so the only silent failure is an entry of
/// whitespace, which `normalized()` drops at serve time and would therefore
/// look written while changing nothing.
fn visibility_glob(cli: &Cli, list: &str, past: &str, glob: &str) -> anyhow::Result<()> {
    if glob.trim().is_empty() {
        return Err(fail(
            "the glob is empty",
            "an entry matches a model id or `provider/model`, e.g. \"gpt-4*\"",
        ));
    }
    let doc = read_config_text(cli)?;
    let updated = append_list_entry(&doc, VIS_SECTION, list, glob);
    let updated = match updated {
        Some(u) => u,
        None => {
            if list_has_entry(&doc, VIS_SECTION, list, glob) {
                println!("visibility: {glob} — unchanged (already {past})");
                return Ok(());
            }
            return Err(fail(
                "visibility: is written in a form this writer does not recognise",
                "the block may use a flow list or another indentation; edit it by hand",
            ));
        }
    };
    if updated == doc {
        println!("visibility: {glob} — unchanged ({past})");
        return Ok(());
    }
    write_file(&cli.config, &updated)?;
    println!("visibility: {past} {glob}");
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// The reference set `providers remove` computes over a provider id, here read
/// for a combo id: the one who reads the file must not miss a place the writer
/// can reach.
fn combo_names(combo: &Combo, id: &str) -> bool {
    combo
        .targets
        .iter()
        .chain(combo.pool.iter())
        .chain(combo.steps.iter().map(|s| &s.target))
        .chain(combo.judge_model.iter())
        .any(|t| target_provider(t) == id || t == id)
}

/// `aroute combo target add|remove`: one surgical target edit.
///
/// This is the verb the operator reaches for when one target must move and the
/// rest of the routing table must not: repointing one combo keeps every other
/// combo byte-identical, which rewriting the section cannot promise. The
/// `targets` list alone is re-rendered, in the block the combo already occupies.
fn combo_target(cli: &Cli, args: &ComboTargetArgs) -> anyhow::Result<()> {
    match &args.command {
        crate::cli::ComboTargetCommand::Add(a) => {
            combo_target_add(cli, a.combo.trim(), parse_one_target(&a.target)?, a)
        }
        crate::cli::ComboTargetCommand::Remove(a) => {
            combo_target_remove(cli, a.combo.trim(), parse_one_target(&a.target)?)
        }
    }
}

fn combo_target_add(
    cli: &Cli,
    combo_id: &str,
    target: String,
    args: &ComboTargetArgs2,
) -> anyhow::Result<()> {
    let doc = read_config_text(cli)?;
    let cfg = load_edit(cli)?;
    let catalog = catalog(&cfg)?;
    let Some(combo) = cfg.combos.iter().find(|c| c.id == combo_id) else {
        return Err(fail(
            format!("combo {combo_id:?} is not in this config"),
            "`aroute combo` lists the routing table".to_string(),
        ));
    };
    if combo.targets.contains(&target) {
        return Err(fail(
            format!("combo {combo_id:?} already lists {target}"),
            "one target appears once; repoint it with `aroute combo target remove` then add",
        ));
    }
    // Unknown provider, resolved against the catalog: the custom rows carry no
    // models a provider/model split can lean on, so they are matched as whole
    // targets.
    if target_provider_in(&target, &catalog) == ar_server::config::DEFAULT_COMBO_ID {
        return Err(fail(
            format!("combo {combo_id:?} would target unknown provider {target}"),
            "`aroute providers` lists what this config can route to",
        ));
    }

    let mut next = cfg.clone();
    if let Some(c) = next.combos.iter_mut().find(|c| c.id == combo_id) {
        c.targets.push(target.clone());
    }
    // Validated on the candidate, not on the file: this is where a target no
    // provider serves is refused, rather than at the first request after a
    // restart.
    check_combos(&next.combos, &next)
        .map_err(|e| fail(e, "the edited combo does not validate; nothing was written"))?;

    // One line, inside the row's own list: every other byte of the document is
    // copied, so `target add` then `target remove` of the same target restores
    // the file exactly.
    let updated = add_target_line(&doc, combo_id, &target).ok_or_else(|| {
        fail(
            format!("combo {combo_id:?} has no `targets:` list to add to"),
            "the row may be written in a form this writer does not recognise; edit it by hand",
        )
    })?;
    if updated == doc {
        // Byte equality — see `combo_add` on why not `means_the_same`.
        println!("combos: {combo_id} — unchanged");
        return Ok(());
    }
    write_file(&cli.config, &updated)?;
    println!("combos: {combo_id} gained {}", args.target);
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

fn combo_target_remove(cli: &Cli, combo_id: &str, target: String) -> anyhow::Result<()> {
    let doc = read_config_text(cli)?;
    let cfg = load_edit(cli)?;
    let Some(combo) = cfg.combos.iter().find(|c| c.id == combo_id) else {
        return Err(fail(
            format!("combo {combo_id:?} is not in this config"),
            "`aroute combo` lists the routing table",
        ));
    };
    if !combo.targets.contains(&target) {
        return Err(fail(
            format!("combo {combo_id:?} does not list {target}"),
            "`aroute combo` lists each combo's targets",
        ));
    }
    // A one-target combo with its target gone is a combo that cannot route.
    if combo.targets.len() == 1 {
        return Err(fail(
            format!("combo {combo_id:?} would have no targets"),
            "remove the combo with `aroute combo remove` instead",
        ));
    }

    // One line, and the row's own: the other combos and the operator's layout
    // are copied byte for byte.
    let updated = remove_target_line(&doc, combo_id, &target).ok_or_else(|| {
        fail(
            format!("combo {combo_id:?} has no line naming {target}"),
            "the row may be written in a form this writer does not recognise; edit it by hand",
        )
    })?;
    if updated == doc {
        // Byte equality — see `combo_add` on why not `means_the_same`.
        println!("combos: {combo_id} — unchanged");
        return Ok(());
    }
    write_file(&cli.config, &updated)?;
    println!("combos: {combo_id} lost {target}");
    println!(
        "note: the running proxy reads its config at boot only — apply with `aroute service restart proxy`"
    );
    Ok(())
}

/// `--targets a/m,b/m@2`: the comma-separated list `aroute combo add` takes.
///
/// The map form (`target@weight`) is parsed back into `Combo::weights`, the
/// field the renderer reads: writing a plain list and dropping the shares
/// would turn a weighted chain into first-wins, so the parse keeps them.
fn parse_targets(raw: &str) -> anyhow::Result<(Vec<String>, BTreeMap<String, u32>)> {
    let mut targets: Vec<String> = Vec::new();
    let mut weights: BTreeMap<String, u32> = BTreeMap::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, weight) = match entry.split_once('@') {
            Some((n, w)) => {
                let weight: u32 = w.parse().map_err(|_| {
                    fail(
                        format!("target {entry:?} has a bad weight"),
                        "weight is a positive integer",
                    )
                })?;
                if weight == 0 {
                    return Err(fail(
                        format!("target {entry:?} weights a target at zero"),
                        "a zero share removes the target from the draw",
                    ));
                }
                (n.trim(), weight)
            }
            None => (entry, 1),
        };
        if name.is_empty() {
            return Err(fail(
                format!("target {entry:?} has an empty provider/model"),
                "a target is provider/model, e.g. openai/gpt-5.4",
            ));
        }
        if !targets.iter().any(|t| t == name) {
            targets.push(name.to_owned());
        }
        if weight != 1 {
            weights.insert(name.to_owned(), weight);
        }
    }
    if targets.is_empty() {
        return Err(fail(
            "no targets given",
            "pass --targets provider/model, at least one",
        ));
    }
    Ok((targets, weights))
}

/// `--target openai/gpt-5.4`: one target, no weight.
///
/// The dedup `parse_targets` performs is done by [`Combo`] instead: a second
/// entry the operator repeats is refused by the token check below, so a
/// parsed-away duplicate would hide that refusal.
fn parse_one_target(raw: &str) -> anyhow::Result<String> {
    let target = raw.trim();
    if target.is_empty() {
        return Err(fail(
            "the target is empty",
            "a target is provider/model, e.g. openai/gpt-5.4",
        ));
    }
    Ok(target.to_owned())
}

/// One combo row: distinct providers, strategy, and the full target chain.
///
/// `+` not `,`: rows are comma-delimited, so a comma-separated set would make
/// this row one cell wider than the header claims.
fn combo_rows(combos: &[Combo], catalog: &Registry) -> Vec<Row> {
    let mut rows: Vec<Row> = combos
        .iter()
        .map(|c| {
            let providers: BTreeSet<&str> = c
                .targets
                .iter()
                .map(|t| target_provider_in(t, catalog))
                .collect();
            let status = if c.targets.is_empty() {
                "no-targets"
            } else {
                "active"
            };
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
fn doctor(cli: &Cli, args: &DoctorArgs) -> anyhow::Result<()> {
    let cfg = load(cli)?;
    let store = StoreProbe::resolve(&cli.config);
    let checks = findings(&cfg, &cli.config.display().to_string(), &store);
    // Exit status is decided over the unfiltered set, before any filter: see
    // `DoctorArgs::status`. A filter must never be able to launder a failure
    // into an exit 0.
    let failed = checks.iter().filter(|r| r[1] == "fail").count();
    let total = checks.len();
    let mut rows = checks;
    if let Some(status) = args.status {
        // Typed by `CheckStatusArg`, so the value is already one of the three.
        // A no-match is still printed as `0 of TOTAL`, which is definite: the
        // enum removed the typo case, and an unused value is a legitimate
        // answer ("nothing failed") rather than an error.
        let want = status.as_row_value();
        rows.retain(|r| r[1] == want);
    }
    let rows = page(&rows, args.offset.unwrap_or(0), args.limit);

    if args.json {
        print!(
            "{}",
            toon::json_paged(
                "checks",
                &CHECK_COLUMNS,
                &toon::every_field(&CHECK_COLUMNS),
                &rows,
                total
            )
        );
    } else {
        print!(
            "{}",
            toon::list_paged(
                "checks",
                "checks",
                &CHECK_COLUMNS,
                &toon::every_field(&CHECK_COLUMNS),
                &rows,
                false,
                total
            )
        );
    }

    if failed > 0 {
        return Err(fail(
            format!("{failed} check(s) failed"),
            "fix the `fail` rows; `aroute doctor --status fail` prints just those",
        ));
    }
    Ok(())
}

/// One row per thing that can be wrong, as `(check, status, detail)`.
///
/// Shared by `doctor` (prints them) and `configure --check` (reads them), so the
/// two can never disagree about whether a config is valid. `store` is the same
/// [`StoreProbe`] `aroute serve` resolves, so a credential the server would take
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
            catalog.get(provider).is_some()
                && !model.is_empty()
                && !model_known_in(&catalog, provider, model)
        })
        .count();

    let mut rows: Vec<Row> = vec![
        vec![
            "config".to_owned(),
            "ok".to_owned(),
            format!(
                "{path} ({} providers + {} custom + {} combos)",
                cfg.providers.len(),
                cfg.custom_providers.len(),
                cfg.combos.len()
            ),
        ],
        registry_row(unroutable),
    ];
    let (status, detail) = store.row();
    rows.push(vec!["store".to_owned(), status, detail]);

    // The HTTP gate: armed when `aroute keys arm-gate` stored a master row.
    //
    // **Decrypted, not named.** The old check read the row's *name* and called
    // that "armed", so a store whose gate row could not be decrypted under the
    // current `$AR_MASTER_KEY` listed as `gate,ok,armed` while `aroute serve`
    // was booting with "serving ungated" in its own log. Two surfaces, two
    // answers, and the operator had no way to tell which one was true.
    //
    // The row is now opened. Three answers, and the third is the one that
    // matters:
    //
    // * no gate row — `skip`, the supported loopback-ungated install.
    // * a gate row that decrypts — `ok`, armed.
    // * a gate row that will not decrypt — **`fail`**, because the operator
    //   believes the gate is on and it is not.
    //
    // `fail` rather than `skip` because `doctor` exits 1 on a `fail` and that
    // exit is the point: a broken gate is a broken install, and an install that
    // silently serves unauthenticated traffic is the thing an operator most
    // needs a non-zero exit for.
    let gate = match store {
        StoreProbe::Open { path, names } if names.iter().any(|n| n == GATE_ROW) => {
            match open_store(path).and_then(|s| s.get(GATE_ROW)) {
                Ok(Some(_)) => (
                    "ok".to_owned(),
                    "armed — bearer tokens required (`keys mint` issues more)".to_owned(),
                ),
                Ok(None) => (
                    "skip".to_owned(),
                    "not armed — loopback only; `aroute keys arm-gate` arms it".to_owned(),
                ),
                Err(e) => (
                    "fail".to_owned(),
                    format!(
                        "the gate row will not decrypt ({e}); the server is serving UNGATED — \
                         re-arm with `aroute keys arm-gate --force --key $AR_HTTP_MASTER_KEY`"
                    ),
                ),
            }
        }
        StoreProbe::Live { store, .. } => match store.get(GATE_ROW) {
            Ok(Some(_)) => (
                "ok".to_owned(),
                "armed — bearer tokens required (`keys mint` issues more)".to_owned(),
            ),
            Ok(None) => (
                "skip".to_owned(),
                "not armed — loopback only; `aroute keys arm-gate` arms it".to_owned(),
            ),
            Err(e) => (
                "fail".to_owned(),
                format!(
                    "the gate row will not decrypt ({e}); the server is serving UNGATED — \
                     re-arm with `aroute keys arm-gate --force --key $AR_HTTP_MASTER_KEY`"
                ),
            ),
        },
        _ => (
            "skip".to_owned(),
            "no store so no gate — loopback only".to_owned(),
        ),
    };
    rows.push(vec!["gate".to_owned(), gate.0, gate.1]);

    // F-MED-2: the terminal-status list, reported from the executor's own copy
    // so an operator sees the same rows the classifier uses and the store's
    // CHECK is generated from. A literal here would be a fourth copy to drift.
    let executors: Vec<&str> = ar_server::OAuthKind::ALL
        .iter()
        .map(|k| k.as_str())
        .collect();
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
        rows.push(vec![
            format!("provider/{}", p.id),
            "ok".to_owned(),
            "in registry".to_owned(),
        ]);

        // F-CRIT-1: a provider the catalog labels `oauth` is not "known" to this
        // build just because it is in the registry. Without this row `doctor`
        // would report `codex` as a healthy provider while `aroute serve` cannot
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
                format!(
                    "; {:?} is not dispatchable in this build",
                    c.protocol.wire_format()
                )
            };
            (
                "ok",
                format!(
                    "{} at {}{refused}",
                    c.protocol.wire_format().as_str(),
                    c.base_url
                ),
            )
        } else {
            (
                "fail",
                format!(
                    "base_url {:?} is not an http(s) URL with an authority",
                    c.base_url
                ),
            )
        };
        rows.push(vec![
            format!("custom/{}", c.id),
            status.to_owned(),
            detail.to_owned(),
        ]);
    }

    let mut names: Vec<&str> = cfg.keys.keys().map(String::as_str).collect();
    names.sort_unstable();
    for name in names {
        rows.push(vec![
            format!("key/{name}"),
            key_status(name, cfg, store),
            key_detail(name, cfg, store),
        ]);
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
            rows.push(vec![
                format!("{prefix}/{t}"),
                status.to_owned(),
                detail.to_owned(),
            ]);
        }
    }

    rows
}

/// Every `targets:` entry, deduplicated across combos.
fn target_rows(cfg: &Config) -> BTreeSet<&str> {
    cfg.combos
        .iter()
        .flat_map(|c| c.targets.iter().map(String::as_str))
        .collect()
}

/// Every `pool:` entry, deduplicated across combos.
fn pool_rows(cfg: &Config) -> BTreeSet<&str> {
    cfg.combos
        .iter()
        .flat_map(|c| c.pool.iter().map(String::as_str))
        .collect()
}

/// Whether one `provider/model` string routes, and what to say when it does not.
///
/// Shared by the `target/` and `pool/` rows: an operator who mistypes a model in
/// the bench must be told the same thing `aroute serve` will do about it.
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
        (
            "fail",
            "model not in registry; run `aroute import --from omniroute --path <OmniRoute/open-sse/config/providers>` or `aroute models`",
        )
    } else {
        ("ok", "routable")
    }
}

/// The `registry` row: the compiled-in catalog's age, and `warn` once it is past
/// [`ar_registry::SNAPSHOT_TTL_DAYS`].
///
/// F-HIGH-3. The registry is a snapshot plus `aroute import`, so nothing about a
/// stale one is visible until a request 404s against a model a provider has since
/// rotated. This is the signal that costs nothing: the build's own date, which
/// `ar-registry` stamps, is a floor on the snapshot's age, and the count of
/// configured targets this build cannot route is the drift it can *prove*. The
/// fix is named here rather than left for the reader to remember.
///
/// `warn`, not `fail`: a stale snapshot degrades a routing decision, it does not
/// make the config invalid, and `doctor` exiting 1 on a week-old build would
/// train operators to ignore it. The models themselves are named by the
/// `target/<id>` `fail` rows below, which already carry this same `aroute import`
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
    let compiled = format!(
        "{} providers compiled in + snapshot {}d old",
        registry().len(),
        ar_registry::age_days(now)
    );
    if ar_registry::is_stale(now) {
        return vec![
            "registry".to_owned(),
            "warn".to_owned(),
            format!(
                "{compiled} (ttl {}d); {unroutable} configured model(s) below are not in it; run `aroute import --from omniroute --path <OmniRoute/open-sse/config/providers>`",
                ar_registry::SNAPSHOT_TTL_DAYS
            ),
        ];
    }
    vec!["registry".to_owned(), "ok".to_owned(), compiled]
}

/// The `(status, detail)` cell of an `oauth/<provider>` row, public for `aroute auth`.
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
            format!(
                "catalog authType is oauth; add an `oauth:` block naming a refresh_key and token_url{}",
                relogin(provider)
            ),
        );
    };

    let access_name = cfg.key_name(provider).unwrap_or(provider);
    if !resolves(access_name, cfg, store) {
        return (
            "fail".to_owned(),
            format!(
                "access token {access_name:?} is in neither the credential store nor keys:{}",
                relogin(provider)
            ),
        );
    }

    // Spelled from the executor's own list, so this reason cannot drift from the
    // one a real refresh failure would record.
    let terminal = terminal_reason(400, "no_refresh_token").unwrap_or("no_refresh_token");
    match (
        declared.refresh_key.as_deref(),
        declared.token_url.as_deref(),
    ) {
        (None, _) => (
            "fail".to_owned(),
            format!(
                "access token resolves; no refresh_key, so the first 401 is terminal ({terminal})"
            ),
        ),
        (Some(_), None) => (
            "fail".to_owned(),
            format!(
                "access token resolves; refresh row declared but no token_url, so the first 401 is terminal ({terminal})"
            ),
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
    let editor = if declared
        .anonymous_editor
        .as_deref()
        .is_some_and(|e| !e.is_empty())
    {
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

/// Login readiness for one declared session: whether `aroute auth login` can run it.
///
/// Separate from [`oauth_row`] on purpose. `oauth_row` answers "does this session
/// dispatch", which is a question about what is *already stored*; this answers
/// "can it be (re-)authorised", which is a question about what the operator
/// *declared*. A session can be fully armed and unloggable — both tokens present,
/// no `authorization_url` — and reporting that as a dispatch `fail` would be a
/// lie in the direction that costs an operator the most: the session works, and
/// the only thing missing is the ability to renew it by hand.
///
/// The fix therefore names two commands: `aroute auth login` for the endpoint the
/// operator has to add, and the login itself for the tokens a re-auth mints.
pub fn login_readiness(provider: &str, cfg: &Config, store: &StoreProbe<'_>) -> (String, String) {
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
        if armed {
            "armed".to_owned()
        } else {
            "needs-login".to_owned()
        },
        format!(
            "authorize at {endpoint}; access {access_name:?} {}{}",
            if armed {
                "resolves"
            } else {
                "does not resolve yet"
            },
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
    format!("; run `aroute auth login --provider {provider}`")
}

/// The canonical spelling of a terminal reason, read from the lists the executor
/// classifies against.
///
/// The union of [`ar_server::TERMINAL_REFRESH_STATUS`] and the carve-out-only
/// rows, because that union is what `terminal_check_constraint` generates the
/// store's CHECK from: reading the shared table alone would make `aroute doctor` unable
/// to name a reason the store would accept, which is the fourth-copy drift F-MED-2
/// describes. Ordered shared-first so a listed row keeps precedence, exactly as
/// `reason_in` scans them.
///
/// `None` for a reason neither list carries. Every caller passes a literal, so a
/// `None` is a drift between two copies rather than a runtime path — which is
/// why the fallback at the call site is the same string.
fn terminal_reason(status: u16, reason: &str) -> Option<&'static str> {
    ar_server::TERMINAL_REFRESH_STATUS
        .iter()
        .chain(ar_exec::oauth::CARVE_OUT_TERMINAL_STATUS)
        .find(|(s, r)| *s == status && *r == reason)
        .map(|(_, r)| *r)
}

/// Whether a credential name resolves, in the order `aroute serve` resolves it: the
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
    let Some(def) = catalog.get(provider) else {
        return false;
    };
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
    registry()
        .get(provider)
        .is_none_or(|d| d.auth_kind.as_ref() == "apikey")
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
        || cfg
            .providers
            .iter()
            .any(|p| p.key == name && needs_credential(&p.id))
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
        .chain(
            cfg.custom_providers
                .iter()
                .filter(|c| c.key_ref == name)
                .map(|c| c.id.as_str()),
        )
        .collect();
    let bound = if users.is_empty() {
        "unused".to_owned()
    } else {
        format!("for {}", users.join("+"))
    };
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

    let checks = findings(
        &cfg,
        &cli.config.display().to_string(),
        &StoreProbe::resolve(&cli.config),
    );
    let failed = checks.iter().filter(|r| r[1] == "fail").count();
    if args.check && failed > 0 {
        return Err(fail(
            format!("{failed} check(s) failed"),
            "run `aroute doctor` for the per-check detail",
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
        vec![
            "registry.providers".to_owned(),
            registry().len().to_string(),
        ],
        vec!["providers".to_owned(), cfg.providers.len().to_string()],
        vec![
            "custom_providers".to_owned(),
            cfg.custom_providers.len().to_string(),
        ],
        vec!["combos".to_owned(), cfg.combos.len().to_string()],
    ];
    for p in &cfg.providers {
        let base_url = registry()
            .get(&p.id)
            .map_or_else(|| "-".to_owned(), |d| d.base_url.clone());
        rows.push(vec![format!("provider.{}.key", p.id), p.key.clone()]);
        rows.push(vec![format!("provider.{}.base_url", p.id), base_url]);
    }
    for c in &cfg.custom_providers {
        rows.push(vec![format!("provider.{}.key", c.id), c.key_ref.clone()]);
        rows.push(vec![
            format!("provider.{}.base_url", c.id),
            c.base_url.clone(),
        ]);
        rows.push(vec![
            format!("provider.{}.protocol", c.id),
            c.protocol.wire_format().as_str().to_owned(),
        ]);
    }
    for c in &cfg.combos {
        rows.push(vec![
            format!("combo.{}.strategy", c.id),
            c.strategy.as_str().to_owned(),
        ]);
        rows.push(vec![format!("combo.{}.targets", c.id), c.targets.join("+")]);
    }
    rows
}

/// Converts another tool's config into `config.yaml`, `registry.json`,
/// `freeBudgets.json` and `providerMeta.json`.
///
/// Every file is written before anything is printed, so a partial failure
/// never leaves a table claiming a conversion that did not land.
fn import_config(args: &ImportArgs) -> anyhow::Result<()> {
    let result = match args.from {
        ImportFrom::Omniroute => import::omniroute::scan(
            import::upstream_tree(args.path.as_deref())?,
            args.combos.as_deref(),
        )?,
        ImportFrom::Litellm => {
            import::convert(args.from, &import::upstream(args.path.as_deref())?)?
        }
    };

    let out = Path::new(&args.out_dir);
    std::fs::create_dir_all(out).map_err(|e| {
        fail(
            format!("cannot create {}: {e}", out.display()),
            "pass --out-dir <DIR> at a path you can write",
        )
    })?;
    let written = out.display().to_string();
    guard_against_section_loss(&out.join("config.yaml"), args.allow_section_loss.as_deref())?;
    write_file(&out.join("config.yaml"), &result.config_yaml)?;
    write_file(
        &out.join("registry.json"),
        &import::to_catalog_json(&result.registry)?,
    )?;
    // A third file, keyed `(provider, model)`: one row per model rather than
    // one per provider, so folding it in would make `registry.json` a two-shape
    // document no `ProviderDef` loader can parse. Written unconditionally, and
    // empty for a source with no such table, so the output set is
    // `--from`-independent.
    write_file(
        &out.join("freeBudgets.json"),
        &import::to_free_budgets_json(&result.free_budgets)?,
    )?;
    // A fourth file, keyed by provider id: what each provider declares about
    // itself. Not folded into `registry.json` because a `ProviderDef` is built
    // field-by-field in four places outside this crate's write scope, and because
    // one document with two top-level shapes needs a branch in every loader.
    // Written unconditionally, and empty for a source that declares nothing, so
    // the output set is `--from`-independent.
    write_file(
        &out.join("providerMeta.json"),
        &import::to_provider_meta_json(&result.provider_meta)?,
    )?;
    // A fifth file, keyed by model id alone. Same reason as the third: folding it
    // into `registry.json` would make that document two-shaped. Written
    // unconditionally, and empty for a source with no such table, so the output
    // set is `--from`-independent.
    write_file(
        &out.join("modelLifecycle.json"),
        &import::to_lifecycle_json(&result.lifecycle)?,
    )?;

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

/// `aroute sync` — the dashboard's integrated DB is the config surface; this
/// verb is the bridge onto the routing engine.
///
/// Rewrites exactly what the dashboard owns — the `combos:` section, the
/// `custom_providers:` section for ad-hoc chat nodes, and the `AR_KEY_*`
/// values in the env file the service loads — and leaves every other section
/// byte-for-byte. The proxy builds its routing tables at boot, so a sync that
/// changed anything restarts `aroute.service` (unless `--no-restart`, and
/// `--dry-run` writes nothing at all — ledger **F16** / register **DASH-10**).
/// Prints what a sync WOULD change, writing nothing (ledger F16 / DASH-10).
///
/// A unified diff per file, in the same order `sync_config` writes them:
/// the config first, then the env file. The env plan is computed by the same
/// `sync_env_file` walk with writes suppressed — the counts are the real
/// planned counts, not a second implementation that could disagree.
///
/// Deliberately dependency-free (std only): `similar` is in `Cargo.lock` but
/// not a dependency of this crate, and a `--dry-run` is not worth adding a
/// workspace edge for. Line diff over `\n`, which is what these files are.
fn print_sync_plan(
    config_path: &Path,
    doc: &str,
    next: &str,
    env_path: &Path,
    db: &Path,
    doc_for_env: &str,
) -> anyhow::Result<()> {
    println!("dry run: what `aroute sync` would change — nothing was written");
    println!();
    if doc != next {
        println!("--- {}", config_path.display());
        println!("+++ {} (after sync)", config_path.display());
        print_line_diff(doc, next);
        println!();
    } else {
        println!("{}: unchanged", config_path.display());
    }
    // One computation, two consumers: the same walk the writer runs, with the
    // write suppressed. Its counts and its planned lines are therefore the
    // real ones, and the diff below cannot drift from a future change to the
    // write path.
    let plan = sync_env_file(env_path, db, doc_for_env, true)?;
    if plan.changed {
        let before = std::fs::read_to_string(env_path).unwrap_or_default();
        println!("--- {}", env_path.display());
        println!("+++ {} (after sync)", env_path.display());
        print_line_diff(&before, &format!("{}\n", plan.lines.join("\n")));
        println!();
    } else {
        println!("{}: no env changes", env_path.display());
    }
    Ok(())
}

/// Minimal line diff: context-free, `-`/`+` only, added and removed lines.
/// Enough for an operator to decide whether to apply a sync; not a Myers
/// implementation, and it does not need to be.
fn print_line_diff(before: &str, after: &str) {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    // Longest-common-subsequence over the two line vectors, so a moved block
    // reads as one removal plus one addition rather than a full-file rewrite.
    let n = a.len();
    let m = b.len();
    // `lcs[i][j]` is the LCS length of `a[i..]` and `b[j..]`. Truncated to
    // `usize` because these files are thousands of lines, not millions — a
    // quadratic table is fine at this size and keeps the code obvious.
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            println!("-{a}", a = a[i]);
            i += 1;
        } else {
            println!("+{b}", b = b[j]);
            j += 1;
        }
    }
    while i < n {
        println!("-{a}", a = a[i]);
        i += 1;
    }
    while j < m {
        println!("+{b}", b = b[j]);
        j += 1;
    }
}

/// `aroute service` — the lifecycle verb the CLI was missing.
///
/// "Is the thing I am about to route to actually running?" had no answer
/// short of shelling out to systemctl by hand, and an agent cannot do that
/// cheaply. `sync_config` below already knew how to restart the proxy; this is
/// that capability as a verb, plus the read that makes it safe to reach for.
///
/// The read is the important half: `status` reports systemd's verdict AND a
/// live probe of the port, because those are two different claims. The
/// incident in `docs/25` was a service systemd called active while `/healthz`
/// took 15,427 ms — "active" alone would have answered the wrong question.
fn service(cli: &Cli, args: &ServiceArgs) -> anyhow::Result<()> {
    let scope: [&str; 2] = if cli.system {
        ["--system", "--no-pager"]
    } else {
        ["--user", "--no-pager"]
    };

    match args.action {
        None | Some(ServiceAction::Status { unit: None }) => {
            status_of(scope, &[ServiceUnit::Proxy, ServiceUnit::Dashboard])
        }
        Some(ServiceAction::Status { unit: Some(u) }) => status_of(scope, &[u]),
        Some(ServiceAction::Start { unit }) => change(scope, "start", unit),
        Some(ServiceAction::Stop { unit }) => change(scope, "stop", unit),
        Some(ServiceAction::Restart { unit }) => change(scope, "restart", unit),
        Some(ServiceAction::Enable { unit }) => persist(scope, "enable", unit),
        Some(ServiceAction::Disable { unit }) => persist(scope, "disable", unit),
    }
}

/// Reports systemd's verdict and a live reachability probe for each unit.
///
/// Exits 1 when a unit is not serving. That is deliberate: an agent asking
/// "is the proxy up?" before sending traffic needs a machine-readable answer,
/// and a `status` that always exited 0 would force every caller to parse prose.
/// It is the same rule `doctor` follows — the verb's exit code is its verdict.
///
/// A unit that is simply not installed is NOT a failure. `docs/plans/2026-10-19`
/// documents a CLI-only host that keeps the binary and drops the daemon, and
/// alarming on that machine would make `aroute service status` useless in CI.
fn status_of(scope: [&str; 2], units: &[ServiceUnit]) -> anyhow::Result<()> {
    let mut any_down = false;
    let mut rows: Vec<Row> = Vec::with_capacity(units.len());
    for unit in units {
        let name = unit.unit_name();
        let installed = std::process::Command::new("systemctl")
            .args(scope)
            .args(["list-unit-files", name])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .is_some_and(|s| s.contains(name));

        // Boot persistence alongside the live state, because they answer two
        // different questions and conflating them is how a "disabled but
        // running" unit surprises someone after a reboot.
        let enabled = installed
            && std::process::Command::new("systemctl")
                .args(scope)
                .args(["is-enabled", name])
                .output()
                .ok()
                .is_some_and(|o| o.status.success());

        let active = std::process::Command::new("systemctl")
            .args(scope)
            .args(["show", name, "--property=ActiveState", "--value"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|| "unknown".to_owned());

        // The probe: what a caller needs is whether the port answers, not what
        // systemd last recorded. `is-active` and a TCP probe disagree exactly
        // when something is wrong, which is when it matters.
        //
        // But a probe is scope-blind: it connects to 127.0.0.1:20128 whether
        // the listener there came from the user unit or the system one. On a
        // box where the user unit runs and the caller asked about `--system`,
        // a scope-blind probe reported `ok` over two rows whose `detail`
        // said "not installed" — an answer that contradicts itself in the
        // same row. So the probe is reported as `not-applicable` when systemd
        // cannot have started this unit: it is the honest answer, and a real
        // `ok` in that column then means something.
        let probe = if !installed {
            ProbeResult::NotApplicable
        } else if probe_port(unit.port()) {
            ProbeResult::Ok
        } else {
            ProbeResult::NoAnswer
        };
        let detail = match (installed, active.as_str(), probe) {
            (false, _, _) => "not installed — `install.sh --service user` writes it".to_owned(),
            (true, "active", ProbeResult::Ok) => format!(
                "active, port {} answers, {}",
                unit.port(),
                boot_word(enabled)
            ),
            (true, "active", ProbeResult::NoAnswer) => format!(
                "active but port {} does not answer, {}",
                unit.port(),
                boot_word(enabled)
            ),
            (true, "active", ProbeResult::NotApplicable) => {
                format!("active, {}", boot_word(enabled))
            }
            (true, other, _) => {
                format!(
                    "{other}, {} — start it with `aroute service start {}",
                    boot_word(enabled),
                    unit.slug()
                )
            }
        };
        // Down means systemd is not running it OR it is not answering. An
        // uninstalled unit is not down: it is absent, which is a legitimate
        // state on a CLI-only host and must not fail the verb.
        if installed && (active != "active" || probe == ProbeResult::NoAnswer) {
            any_down = true;
        }
        rows.push(vec![
            name.to_owned(),
            active,
            probe.label().to_owned(),
            detail,
        ]);
    }

    print!(
        "{}",
        toon::list_paged(
            "service",
            "services",
            &SERVICE_COLUMNS,
            &toon::every_field(&SERVICE_COLUMNS),
            &rows,
            false,
            rows.len()
        )
    );
    if any_down {
        return Err(fail(
            "a service is not serving",
            "start it with `aroute service start proxy`, or run `aroute serve` for the foreground",
        ));
    }
    Ok(())
}

/// What the live probe found, as a third state beyond "yes" and "no".
///
/// `NotApplicable` is the state a scope-blind probe cannot have: it means
/// "there is no unit here for anything to be listening for". Reporting it as
/// `ok` was the bug — a row whose `detail` said "not installed" while its
/// `probe` said the port answered contradicted itself, and a caller reading
/// only the probe column would have believed a service was serving.
///
/// Deriving `PartialEq` so the caller can match on it; this is a verdict, not
/// just a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeResult {
    /// The port accepted a connection.
    Ok,
    /// Installed and (per systemd) running, but nothing answered on the port.
    NoAnswer,
    /// There is no unit, so the question has no answer.
    NotApplicable,
}

impl ProbeResult {
    fn label(self) -> &'static str {
        match self {
            ProbeResult::Ok => "ok",
            ProbeResult::NoAnswer => "no-answer",
            ProbeResult::NotApplicable => "n/a",
        }
    }
}

/// The boot-persistence word for a status row, kept short because it rides in
/// the detail sentence.
fn boot_word(enabled: bool) -> &'static str {
    if enabled {
        "enabled at boot"
    } else {
        "disabled at boot"
    }
}

/// Runs `systemctl enable`/`disable` — the boot-persistence verbs, which are
/// NOT the state verbs.
///
/// Deliberately no `--now`, so `enable` never starts a service and `disable`
/// never stops one. A caller who wants both states changed says so twice, and
/// the two claims stay independently observable in `status`. This is the same
/// separation `systemctl` itself makes, carried into the CLI that an agent
/// drives: a surprising side effect is only surprising when it is invisible.
///
/// Idempotent by construction: `enable` on an enabled unit and `disable` on a
/// disabled one both report success, the same way `systemctl` does.
fn persist(scope: [&str; 2], verb: &str, unit: ServiceUnit) -> anyhow::Result<()> {
    let name = unit.unit_name();
    match std::process::Command::new("systemctl")
        .args(scope)
        .arg(verb)
        .arg(name)
        .output()
    {
        Ok(o) if o.status.success() => {
            let state = if verb == "enable" {
                "enabled at boot"
            } else {
                "disabled at boot"
            };
            println!("service: {name} {state}");
            println!(
                "note: this changes boot persistence only — the running state is unchanged; `aroute service status` shows both"
            );
            Ok(())
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let first = err.lines().next().unwrap_or("no output from systemctl");
            Err(fail(
                format!("could not {verb} {name}"),
                format!("{first}; or run it by hand: systemctl {verb} {name}"),
            ))
        }
        Err(e) => Err(fail(
            format!("systemctl not available: {e}"),
            format!("run it by hand: systemctl {verb} {name}"),
        )),
    }
}

/// One TCP connect to the unit's port.
///
/// A connect, not an HTTP GET: the proxy has `/healthz`, the dashboard does not
/// necessarily share that contract, and "the port accepts" is the claim this
/// verb needs. A real request would couple this verb to one unit's route table
/// for no gain in the answer.
///
/// 400 ms, because a wedged-but-accepting listener must not hang the verb: the
/// whole point is a fast answer to "can I send traffic?".
fn probe_port(port: u16) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(mut addrs) = ("127.0.0.1", port).to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(400)).is_ok()
}

/// Runs one state-changing `systemctl` verb and reports what happened.
///
/// Separate from [`status_of`] because the failure mode is different in kind:
/// a failed `start` is an operator problem — permissions, a bad unit file — and
/// must not be smoothed into a success. The message names the exact command to
/// run by hand, per `docs/06`'s "help: <exact fix command>" rule.
fn change(scope: [&str; 2], verb: &str, unit: ServiceUnit) -> anyhow::Result<()> {
    let name = unit.unit_name();
    match std::process::Command::new("systemctl")
        .args(scope)
        .arg(verb)
        .arg(name)
        .output()
    {
        Ok(o) if o.status.success() => {
            let serving = if probe_port(unit.port()) {
                "still serving"
            } else {
                "not answering yet on its port"
            };
            println!("service: {name} {verb}ed — {serving}");
            Ok(())
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let first = err.lines().next().unwrap_or("no output from systemctl");
            Err(fail(
                format!("could not {verb} {name}"),
                format!("{first}; or run it by hand: systemctl {verb} {name}"),
            ))
        }
        Err(e) => Err(fail(
            format!("systemctl not available: {e}"),
            format!("run it by hand: systemctl {verb} {name}"),
        )),
    }
}

/// `aroute sync` — the dashboard's integrated DB is the config surface; this
/// verb is the bridge onto the routing engine.
///
/// Rewrites exactly what the dashboard owns — the `combos:` section, the
/// `custom_providers:` section for ad-hoc chat nodes, and the `AR_KEY_*`
/// values in the env file the service loads — and leaves every other section
/// byte-for-byte. The proxy builds its routing tables at boot, so a sync that
/// changed anything restarts `aroute.service` (unless `--no-restart` prints
/// the command instead, or `--dry-run` writes nothing — see `print_sync_plan`).
fn sync_config(cli: &Cli, args: &SyncArgs) -> anyhow::Result<()> {
    let dir = cli.config.parent().unwrap_or_else(|| Path::new("."));
    let db = args
        .db
        .clone()
        .unwrap_or_else(|| dir.join("dashboard-data").join("storage.sqlite"));
    if !db.is_file() {
        return Err(fail(
            format!("no integrated dashboard db at {}", db.display()),
            "start `aroute dashboard` once, or pass --db <STORAGE_SQLITE>",
        ));
    }
    let env_path = args.env.clone().unwrap_or_else(|| dir.join("ar.env"));
    let defs = registry().defs();

    // Read-only against the DB: the dashboard may be running while sync runs.
    let combos = import::combos::read(&db, defs);
    let custom = import::custom::read(&db);

    let doc = std::fs::read_to_string(&cli.config).map_err(|e| {
        fail(
            format!("cannot read {}: {e}", cli.config.display()),
            "sync splices into an existing config; run `aroute import` first",
        )
    })?;
    let mut next = splice_section(&doc, "combos", &import::render_combos_section(&combos)?);
    if !custom.is_empty() {
        next = splice_section(
            &next,
            "custom_providers",
            &import::render_custom_section(&custom)?,
        );
        next = add_missing_custom_key_refs(&next, &custom);
    }

    // The credential bridge: the dashboard's connections beyond each
    // provider's first become numbered credentials the executor cycles
    // through when an earlier one throttles or is refused.
    next = sync_credentials(&next, &db)?;

    // The dashboard's per-key request-rate ceiling, carried arm by arm. The
    // dashboard audit (docs/15) found `max_requests_per_minute` is the one
    // column that maps 1:1 — its dollar and token ceilings are *windows*
    // (daily, weekly, monthly) where this engine's arms are cumulative, and
    // carrying them would change what the operator asked for rather than move
    // it. A key the dashboard names gets its `rpm` updated; every other arm,
    // row, and the default row stay exactly as written, because a partial
    // source must not own the whole block. A dashboard with nothing to say
    // leaves the section untouched.
    //
    // Parsed as a bare value rather than through `Config::parse` because the
    // limits block holds no `$VAR`s, and a full parse would demand every
    // other section's variables resolve first — a demand sync never makes.
    let rpm = import::limits::read_rpm(&db);
    let carried = rpm.len();
    if !rpm.is_empty() {
        let mut limits = match serde_yaml::from_str::<serde_yaml::Value>(&next) {
            Err(e) => {
                return Err(fail(
                    e,
                    "the config did not parse as yaml; fix it by hand and re-run sync",
                ))
            }
            Ok(root) => match root.get("limits") {
                None => ar_config::Limits::default(),
                Some(block) => serde_yaml::from_value::<ar_config::Limits>(block.clone())
                    .map_err(|e| {
                        fail(
                            e,
                            "the config's `limits:` block did not parse; fix it by hand and re-run sync",
                        )
                    })?,
            },
        };
        for (name, value) in rpm {
            limits.keys.entry(name).or_default().rpm = Some(value);
        }
        next = splice_section(&next, "limits", &import::limits::render(&limits));
    }
    let config_changed = next != doc;
    if args.dry_run {
        print_sync_plan(&cli.config, &doc, &next, &env_path, &db, &next)?;
    } else if config_changed {
        write_file(&cli.config, &next)?;
    }

    let env_plan = sync_env_file(&env_path, &db, &next, args.dry_run)?;
    let (materialised, ensured, env_changed) =
        (env_plan.materialised, env_plan.ensured, env_plan.changed);

    let rows: Vec<Row> = combos
        .iter()
        .map(|c| {
            let providers: BTreeSet<&str> = c.targets.iter().map(|t| target_provider(t)).collect();
            vec![
                c.id.clone(),
                providers.into_iter().collect::<Vec<_>>().join("+"),
                "active".to_owned(),
                c.strategy.as_str().to_owned(),
                c.targets.len().to_string(),
            ]
        })
        .collect();
    print!(
        "{}",
        toon::list(
            "synced",
            "synced",
            &SYNC_COLUMNS,
            &toon::every_field(&SYNC_COLUMNS),
            &rows,
            false,
        )
    );
    println!(
        "env: {materialised} key(s) materialised, {ensured} referenced var(s) ensured — {}",
        env_path.display()
    );
    if carried > 0 {
        println!("limits: {carried} key(s) rpm carried from the dashboard");
    }
    // The proxy's routing tables are built at boot, so the synced edits only
    // land after a restart. Doing it here keeps `aroute sync` one command
    // rather than a command plus a reminder the operator has to remember.
    if !config_changed && !env_changed {
        println!("service: nothing changed — aroute.service left untouched");
    } else if args.dry_run {
        // Nothing was written, so there is nothing to apply; say so instead of
        // printing a restart command the operator would wrongly run as if a
        // real sync had happened.
        println!(
            "dry run: no files written, no service touched — re-run without --dry-run to apply"
        );
    } else if args.no_restart {
        println!(
            "note: apply with `systemctl --user restart aroute.service` (or your deployment's restart)"
        );
    } else {
        let restarted = std::process::Command::new("systemctl")
            .args(["--user", "restart", "aroute.service"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if restarted {
            println!("service: aroute.service restarted — the synced config is live");
        } else {
            println!(
                "note: apply with `systemctl --user restart aroute.service` (or your deployment's restart)"
            );
        }
    }
    Ok(())
}

/// Removes one top-level `name:` section, leaving every other line
/// line-for-line. A section that is not present returns the input unchanged,
/// which is how callers detect "nothing to remove" — the same changed
/// comparison the splice path relies on.
fn remove_section(content: &str, name: &str) -> String {
    let header = format!("{name}:");
    let all: Vec<&str> = content.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut skipping = false;
    let mut took_one_blank = false;
    for i in 0..all.len() {
        let line = all[i];
        if line == header {
            skipping = true;
            continue;
        }
        // A blank line or a column-0 comment ends the skipped section **only
        // when what follows belongs to another section**, the same look-ahead
        // [`splice_section`] uses. Without it this consumed the comment block
        // that introduces the next section: removing a `limits:` block from the
        // repo's own config took the eight lines documenting `combos:` with
        // it, because a column-0 comment is not a section header and the skip
        // ran on until `combos:` itself.
        //
        // The blank line directly below the section is the one the insert
        // wrote, so it goes with the section — that is what makes the pair an
        // exact inverse: the insert writes the block and one blank, and the
        // removal deletes the block and that one blank. Nothing else on either
        // side. Declared outside the loop on purpose: a local inside the body
        // resets on every line and takes every blank that follows the section.
        if skipping && line.is_empty() && !took_one_blank && rest_is_another_section(&all[i + 1..])
        {
            took_one_blank = true;
            continue;
        }
        if skipping
            && (line.is_empty() || line.starts_with('#'))
            && rest_is_another_section(&all[i + 1..])
        {
            skipping = false;
        }
        if skipping && is_section_header(line) {
            skipping = false;
        }
        if !skipping {
            out.push(line.to_owned());
        }
    }
    // The trailing newline is the file's, not the writer's: this repo's config
    // ends without one deliberately, and a removal is not the place to add it.
    // A file that ends with a blank line keeps it: the pop-from-the-end a
    // previous version used took the operator's own trailing blank with the
    // removed section, because the removed section was the file's last thing.
    let ending = if content.ends_with('\n') || content.ends_with("\r\n") {
        line_ending(content)
    } else {
        ""
    };
    out.join(line_ending(content)) + ending
}

/// Replaces one top-level `name:` section with `rendered`, which carries its
/// own header line. A section that is not present is inserted before
/// `combos:` — definition before use — or appended when there is none.
pub(crate) fn splice_section(content: &str, name: &str, rendered: &str) -> String {
    let all: Vec<&str> = content.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut skipping = false;
    let mut replaced = false;
    // An index loop rather than an iterator, because whether a blank line or a
    // column-0 comment ends the skipped section is a **look-ahead**: it depends
    // on the next line of substance, which a `for` over `lines()` cannot see.
    for i in 0..all.len() {
        let line = all[i];
        // `line == header` is the check for LF only: `str::lines` keeps the
        // `\r` on a CRLF line, so a CRLF `keys:\r` never equals `keys:` and
        // every CRLF document took the *missing-section* branch instead, which
        // writes LF separators. Measured: the `keys:` round trip of a CRLF
        // shape came back `- a/m\r\n` -> `- a/m\n`, the last byte of the
        // file changed by a write that had one section to replace.
        if is_section(line, name) {
            // The rendered block arrives spelled in LF, because every renderer
            // in this crate emits `\n`. A CRLF document needs it re-spelled or
            // the file comes back mixed, which YAML accepts and a diff shows as
            // every line. Re-spelled here rather than in the renderers because
            // `splice_section` is the only one of them that knows which
            // document it is writing into.
            let block = rendered
                .trim_end_matches('\n')
                .replace('\n', line_ending(content));
            out.push(block);
            replaced = true;
            skipping = true;
            continue;
        }
        // A blank line or a comment ends the section being skipped **only
        // when what follows belongs to another section**, which is the
        // [`rest_is_another_section`] look-ahead.
        //
        // Both are column-0 and neither is a section header, so without this
        // they are consumed with the section's body — and the block that
        // documents the *next* section goes with it. The repo's own
        // `config.yaml` carries those blocks, so a `server:` splice silently
        // deleted the two comments introducing `keys:`. Indentation cannot
        // distinguish them (YAML allows column-0 comments anywhere), so the
        // distinction has to be made here: a body line is indented, and these
        // are not.
        //
        // But a **blank line inside a section body does not end it**, which the
        // first version of this rule got wrong and no test caught: the repo's
        // `combos:` carries a blank separator between two of its rows, so the
        // skip stopped at that line, the rows after it survived below the
        // spliced block, and `combo add` produced a routing table with six
        // combination ids where it should have three.
        if skipping
            && (line.is_empty() || line.starts_with('#'))
            && rest_is_another_section(&all[i + 1..])
        {
            // This blank line or comment belongs to the section that follows,
            // so the skip ends here and this line goes out as it was.
            skipping = false;
        }
        if skipping && is_section_header(line) {
            skipping = false;
        }
        if !skipping {
            out.push(line.to_owned());
        }
    }
    // The trailing newline is the file's, not the writer's.
    //
    // `str::lines` drops it, so a document that ends without one rejoins and
    // gains it — a byte-level change to a file the operator asked to change
    // somewhere else. This repo's own `config.yaml` ends without one
    // deliberately. Preserve what was there; a file with no final newline keeps
    // having none, and one that has one keeps having exactly one.
    //
    // `"\n"` is the LF spelling of that, and a CRLF document ends `\r\n`, so the
    // bare `\n` drops the file's own `\r` and makes its last byte a byte the
    // operator never wrote. Measured: the `keys:` round trip of a CRLF shape
    // came back with `- a/m\r\n` spelled `- a/m\n`. The separator is the
    // document's, read the way every other writer tail reads it.
    let sep = line_ending(content);
    let ending = if content.ends_with(sep) { sep } else { "" };
    if replaced {
        out.join(line_ending(content)) + ending
    } else {
        let anchor = out.iter().position(|l| l == "combos:");
        match anchor {
            Some(i) => {
                // Insert **at** the anchor: below any blank line and below any
                // comment block that documents `combos:`, and above the
                // header itself.
                //
                // The block carries **its own trailing blank**, which is the
                // only thing the insert writes that the operator did not:
                // [`remove_section`] deletes the block and that blank together,
                // and nothing else, so the pair is an exact inverse.
                //
                // Only when one is not already there. `keys:` with a paragraph
                // break under it and then `combos:` already has the blank this
                // would write, and adding a second one meant the removal took
                // one back and left the other — a round trip that gained a
                // line. Which of the two went was never in question, only that
                // the file's own spacing is respected.
                let mut block = rendered.trim_end_matches('\n').to_owned();
                let separated = i.checked_sub(1).is_some_and(|j| out[j].is_empty());
                if !separated {
                    block.push('\n');
                }
                out.insert(i, block);
                out.join(line_ending(content)) + ending
            }
            None => format!("{}\n{}{}", content.trim_end(), rendered, ending),
        }
    }
}

/// Whether this line starts a combo entry: `  - id: <name>`.
///
/// Two-space indentation is not decoration, it is the disambiguator. A deeper
/// line is a field of the entry above it (`    strategy: ...`), and a
/// shallower one is a section header — so an entry that a caller's file wrote
/// with different indentation is not matched, rather than matched wrongly and
/// having the wrong lines edited.
fn combo_row_starts(line: &str) -> bool {
    line.len() - line.trim_start().len() == 2 && line.trim_start().starts_with("- id:")
}

/// The id an entry line names, without the `- id:` key or the surrounding space.
fn combo_row_id(line: &str) -> &str {
    line.trim_start().trim_start_matches("- id:").trim()
}

/// Adds one target line to one combo's own `targets:` list.
///
/// The most surgical writer in the surface, and the reason the row-level
/// helpers exist: a target list is a flat list of strings at a known depth, so
/// adding one is inserting one line after the last one that is already there.
/// Every other byte of the document — the other combos, the operator's blank
/// separators, the closing note — is copied verbatim.
///
/// The target goes at the **end** of the list, after any weighted entries: the
/// operator is appending a fallback, and the order is the strategy's order, so
/// the caller controls placement by the command they choose, not by luck.
fn add_target_line(content: &str, combo_id: &str, target: &str) -> Option<String> {
    let all: Vec<&str> = content.lines().collect();

    // Walk to the row that owns `combo_id`, then to its `targets:` key.
    let mut in_section = false;
    let mut i = 0;
    let row_start = loop {
        let line = *all.get(i)?;
        if line == "combos:" {
            in_section = true;
            i += 1;
            continue;
        }
        if in_section && is_section_header(line) {
            return None;
        }
        if in_section && combo_row_starts(line) && combo_row_id(line) == combo_id {
            break i;
        }
        i += 1;
    };
    let targets_at = (row_start + 1..all.len())
        .take_while(|&j| !combo_row_starts(all[j]) && !is_section_header(all[j]))
        .find(|&j| all[j].trim() == "targets:")?;
    // The list ends at the first line that is not indented deeper than
    // `targets:` itself, so a sibling key (`steps:`, `pool:`) closes it.
    let list_end = (targets_at + 1..all.len())
        .find(|&j| all[j].len() - all[j].trim_start().len() <= 4 || all[j].trim().is_empty())
        .unwrap_or(all.len());

    let mut out: Vec<String> = all[..list_end].iter().map(|l| (*l).to_owned()).collect();
    out.push(format!("      - {target}"));
    out.extend(all[list_end..].iter().map(|l| (*l).to_owned()));
    let sep = line_ending(content);
    let joined = out.join(sep);
    Some(if content.ends_with('\n') || content.ends_with("\r\n") {
        joined + sep
    } else {
        joined
    })
}

/// Drops one target line from one combo's own `targets:` list.
///
/// The map form of an entry (`- target: x` / `  weight: n`) is removed as a
/// pair: leaving the `weight:` behind would be an orphan key the next parse
/// reads as `weights:` entries the row does not list, which
/// [`check_combos`] refuses.
fn remove_target_line(content: &str, combo_id: &str, target: &str) -> Option<String> {
    let all: Vec<&str> = content.lines().collect();

    let mut in_section = false;
    let mut i = 0;
    let row_start = loop {
        let line = *all.get(i)?;
        if line == "combos:" {
            in_section = true;
            i += 1;
            continue;
        }
        if in_section && is_section_header(line) {
            return None;
        }
        if in_section && combo_row_starts(line) && combo_row_id(line) == combo_id {
            break i;
        }
        i += 1;
    };
    let targets_at = (row_start + 1..all.len())
        .take_while(|&j| !combo_row_starts(all[j]) && !is_section_header(all[j]))
        .find(|&j| all[j].trim() == "targets:")?;

    // The plain form is one line; the map form is two, the `weight:` under the
    // `- target:`.
    let mut j = targets_at + 1;
    let mut dropped: Option<usize> = None;
    while j < all.len() {
        let l = all[j];
        if is_section_header(l) || combo_row_starts(l) {
            break;
        }
        let indent = l.len() - l.trim_start().len();
        if indent < 6 {
            break;
        }
        let body = l.trim_start();
        let plain = body == format!("- {target}");
        let mapped = body == format!("- target: {target}");
        if plain {
            dropped = Some(1);
            break;
        }
        if mapped {
            // Consume the `weight:` line that follows, when there is one.
            let weight_follows = all
                .get(j + 1)
                .is_some_and(|n| n.trim_start().starts_with("weight:"));
            dropped = Some(if weight_follows { 2 } else { 1 });
            break;
        }
        j += 1;
    }
    let span = dropped?;
    let mut out: Vec<String> = all[..j].iter().map(|l| (*l).to_owned()).collect();
    out.extend(all[j + span..].iter().map(|l| (*l).to_owned()));
    let sep = line_ending(content);
    let joined = out.join(sep);
    Some(if content.ends_with('\n') || content.ends_with("\r\n") {
        joined + sep
    } else {
        joined
    })
}

/// Whether what follows begins another top-level section, so the section being
/// skipped is over.
/// The look-ahead behind [`splice_section`]'s skip rule. A blank line inside a
/// section body is a body separator, and a column-0 comment is either the tail
/// of this section's own notes or the head of the next section's — two
/// spellings nothing in the text distinguishes. The question is answered by
/// walking past blanks and comments and looking at the next line of substance:
/// indented means this section carries on, column-0 means it is finished, and
/// reaching the end of the file means the notes were this section's own.
///
/// The last of those is the case the repo's own config exercises. `combos:` is
/// the final section and closes with a four-line note, so a rule that ended the
/// skip at the first column-0 comment would pull the header-less *body* of the
/// section apart from it, and the closing note would survive below the
/// rendered rows instead of being replaced with them.
fn rest_is_another_section(rest: &[&str]) -> bool {
    for line in rest {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Indented is a line of this section's body, so it is not over.
        return !line.starts_with(' ');
    }
    // Only blanks and comments followed, or nothing did: the notes belong here.
    false
}

/// Appends one rendered combo row to `combos:`, leaving every other line of the
/// document byte-identical.
///
/// Why this exists beside [`splice_section`] rather than being another call to
/// it: a whole-section re-render cannot preserve the operator's own layout. The
/// repo's `combos:` carries a blank separator and a four-line comment that
/// introduces its second row, and rendering the section from the parsed config
/// moves that comment to the *end* of the block, where it documents nothing.
/// The panel's whole-section save does exactly that (`webui.rs:1522`), which is
/// acceptable for a form that replaces the table wholesale; a CLI verb asked to
/// add one row has no business reflowing the other two.
///
/// So the row is appended inside the existing section, after its last body
/// line, and the line that ends the section is kept where it was. The result
/// is that `combo add` followed by `combo remove` of the same row restores the
/// file byte-for-byte, which is the property [`combo_remove`] deletes by and
/// the contract test below measures.
fn append_combo_row(content: &str, rendered: &str) -> String {
    let all: Vec<&str> = content.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut i = 0;
    while i < all.len() {
        let line = all[i];
        if line == "combos:" {
            out.push(line.to_owned());
            in_section = true;
            i += 1;
            continue;
        }
        if in_section && (line.is_empty() || line.starts_with('#')) {
            if rest_is_another_section(&all[i + 1..]) {
                // The section ends here. Walk back over the blank line and
                // notes that close it, insert the row before them, and put the
                // notes back: the row belongs at the end of the body, not above
                // a note that documents the last row rather than the next one.
                let mut at = out.len();
                while at > 0 && (out[at - 1].is_empty() || out[at - 1].starts_with('#')) {
                    at -= 1;
                }
                out.insert(at, rendered.trim_end_matches('\n').to_owned());
                in_section = false;
                continue;
            }
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        if in_section && is_section_header(line) {
            // The next section starts here with no blank or comment between
            // them, so this header is the boundary. Insert the row before it:
            // a `combos:` written flush against `oauth:` must still accept a
            // new row, which the earlier version silently dropped.
            let mut at = out.len();
            while at > 0 && (out[at - 1].is_empty() || out[at - 1].starts_with('#')) {
                at -= 1;
            }
            out.insert(at, rendered.trim_end_matches('\n').to_owned());
            out.push(line.to_owned());
            in_section = false;
            i += 1;
            continue;
        }
        out.push(line.to_owned());
        i += 1;
    }
    // The section ran to the end of the file: the row belongs at the tail.
    if in_section {
        out.push(rendered.trim_end_matches('\n').to_owned());
    }
    let sep = line_ending(content);
    let joined = out.join(sep);
    if content.ends_with('\n') || content.ends_with("\r\n") {
        joined + sep
    } else {
        joined
    }
}

/// Drops one combo row from `combos:` by its id, keeping every other line.
///
/// The inverse of [`append_combo_row`], and the reason the pair exists: `add`
/// then `remove` of the same row must restore the file byte-for-byte, and the
/// only way to honour that is to touch the lines the row owns and nothing else.
/// A whole-section re-render would put the operator's blank separator and
/// closing note back in canonical order, which is a change nobody asked for.
///
/// A row's extent runs from its `- id:` line to the line before the next entry
/// at the same depth. Blanks and column-0 comments between two rows are **not**
/// part of either row, so they survive the removal — the note the operator wrote
/// above a row still describes what is above it.
fn remove_combo_row(content: &str, id: &str) -> Option<String> {
    let all: Vec<&str> = content.lines().collect();

    // Find the entry line for `id`, inside `combos:` and nowhere else.
    let start = {
        let mut in_section = false;
        let mut found = None;
        for (i, line) in all.iter().enumerate() {
            if *line == "combos:" {
                in_section = true;
                continue;
            }
            if in_section && is_section_header(line) {
                in_section = false;
            }
            if in_section && combo_row_starts(line) && combo_row_id(line) == id {
                found = Some(i);
                break;
            }
        }
        found
    }?;

    // The row ends where the next entry or the next section begins, whichever
    // the file reaches first.
    let mut end = all.len();
    for (j, line) in all.iter().enumerate().skip(start + 1) {
        if is_section_header(line) || combo_row_starts(line) {
            end = j;
            break;
        }
    }

    let mut out: Vec<String> = Vec::with_capacity(all.len());
    out.extend(all[..start].iter().map(|l| (*l).to_owned()));
    out.extend(all[end..].iter().map(|l| (*l).to_owned()));
    let sep = line_ending(content);
    let joined = out.join(sep);
    Some(if content.ends_with('\n') || content.ends_with("\r\n") {
        joined + sep
    } else {
        joined
    })
}

/// A top-level YAML section header: **no indentation at all**, a colon, nothing
/// else after it.
///
/// The first condition is the one that carries the whole correctness argument.
/// YAML decides nesting by indentation alone — `keys:` and `api-keys:` are
/// indistinguishable *to a parser* except by their key text, but the *shape*
/// that matters here is that a body line always begins with whitespace (or is a
/// `- ` list item, which also begins with whitespace once it is nested, and at
/// depth 0 is only reachable as a sequence item of the section being skipped).
///
/// Recognizing a header by its *indentation* rather than by which characters
/// appear in its key is what makes this safe: the previous version judged a
/// header by scanning the key for `[a-z0-9_]`, and a real YAML key may legally
/// contain `-` (and `.`) — so `api-keys:`, `custom-providers:` and
/// `model-lifecycle.json:` were all invisible, the skip past a removed or
/// replaced section never terminated, and the writer **silently deleted that
/// section and everything after it**. See docs/36 B-1 and the two tests that
/// reproduce it (`*_does_not_swallow_a_hyphenated_section_after_it`).
///
/// Entry lines never match either way: they are indented, and a top-level
/// scalar carries a value after the colon.
fn is_section_header(line: &str) -> bool {
    !line.starts_with([' ', '#', '\t']) && line.ends_with(':') && line.len() > 1
}

/// Adds `keys:` entries for ad-hoc custom nodes the config does not reference
/// yet. The loader refuses to start on a `key_ref` with no `keys:` row, so a
/// sync that writes a new node must also name its variable — without touching
/// the operator's existing key rows.
fn add_missing_custom_key_refs(doc: &str, custom: &[ar_registry::CustomProvider]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut inserted = false;
    for line in doc.lines() {
        if !inserted && line == "providers:" {
            for c in custom {
                if !doc.contains(&format!("  {}: ", c.id)) {
                    out.push(format!("  {}: $AR_KEY_{}", c.id, import::env_name(&c.id)));
                }
            }
            inserted = true;
        }
        out.push(line.to_owned());
    }
    let sep = line_ending(doc);
    out.join(sep) + sep
}

/// What `sync_env_file` planned for the env file: the counts the caller
/// reports, and the resulting lines so a dry run diffs the SAME plan the
/// writer would have written. Ledger **F16** / DASH-10: the dry run and the
/// write must not be able to disagree, so they share one computation.
struct EnvPlan {
    materialised: usize,
    ensured: usize,
    changed: bool,
    lines: Vec<String>,
}

/// Materialises provider keys into the env file, then guarantees every
/// `$AR_KEY_*` the config references exists there — an unset variable aborts
/// a serve at `$VAR` expansion, so a sync must never hand the proxy a config
/// it cannot load.
///
/// One credential per provider: the highest-priority active connection wins,
/// because the config carries one key binding per provider id; rotation
/// across a provider's pool of keys stays an engine feature. Values are
/// written to the env file and never returned or printed.
/// The dashboard's connections beyond each provider's first, bound as
/// numbered credentials the provider cycles through when an earlier one
/// throttles or is refused: `nvidia-2` in `keys:` holding `$AR_KEY_NVIDIA_2`,
/// and the provider row's `keys:` naming them in order. Both this walk and
/// [`sync_env_file`]'s numbering read the dashboard's own priority order, so
/// the i-th connection here is the i-th var there.
///
/// A provider with one usable connection is untouched — the numbered bridge
/// exists only where there is something to cycle — and a dashboard that
/// cannot be read changes nothing, the same graceful-absence posture as the
/// other importers.
// ponytail: custom providers cycle too once CustomProvider grows a keys
// list; the numbered bridge already covers every registry provider.
fn sync_credentials(doc: &str, db: &Path) -> anyhow::Result<String> {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) else {
        return Ok(doc.to_owned());
    };
    let Ok(mut rows) = conn.prepare(concat!(
        "SELECT provider FROM provider_connections ",
        "WHERE is_active = 1 AND coalesce(api_key, '') != '' ",
        "AND api_key NOT LIKE 'enc:v1:%' ",
        "ORDER BY priority DESC, created_at ASC"
    )) else {
        return Ok(doc.to_owned());
    };
    let Ok(mapped) = rows.query_map([], |r| r.get::<_, String>(0)) else {
        return Ok(doc.to_owned());
    };
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for provider in mapped.flatten() {
        *counts.entry(provider).or_insert(0) += 1;
    }
    let extras: BTreeMap<String, Vec<String>> = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(provider, n)| {
            (
                provider.clone(),
                (2..=*n).map(|i| format!("{provider}-{i}")).collect(),
            )
        })
        .collect();
    if extras.is_empty() {
        return Ok(doc.to_owned());
    }

    let root: serde_yaml::Value = serde_yaml::from_str(doc).map_err(|e| {
        fail(
            e,
            "the config did not parse as yaml; fix it by hand and re-run sync",
        )
    })?;
    let Some(map) = root.as_mapping().cloned() else {
        return Err(fail(
            "the config is not a yaml mapping",
            "fix it by hand and re-run sync",
        ));
    };
    // `keys:` — one ref per numbered credential, merged over the existing
    // names so a hand-set entry survives and a numbered one is overwritten
    // by the connection it names.
    //
    // The existing `keys:` block, parsed for its validation error alone:
    // a `keys:` section that does not parse is refused here with the message
    // naming the block, rather than silently re-rendered from the ordered walk
    // below. The ordered walk reads the file directly and never needs this map.
    if let Some(v) = map.get("keys") {
        serde_yaml::from_value::<BTreeMap<String, String>>(v.clone()).map_err(|e| {
            fail(
                e,
                "the config's `keys:` block did not parse; fix it by hand and re-run sync",
            )
        })?;
    }
    // The merge's additions, in the file's order: an existing name keeps its
    // line, a new one goes last. `extras` iterates a `BTreeMap`, so this is
    // the provider order the merge already used, minus the re-sort of the
    // rows the operator had already written.
    let ordered: Vec<KeyRow> = {
        let mut rows = raw_keys_in_order(doc);
        for (provider, names) in &extras {
            for (i, name) in names.iter().enumerate() {
                let value = format!("$AR_KEY_{}_{}", import::env_name(provider), i + 2);
                match rows.iter_mut().find(|r| &r.name == name) {
                    Some(existing) => existing.value = value,
                    None => rows.push(KeyRow {
                        name: name.clone(),
                        value,
                    }),
                }
            }
        }
        rows
    };
    // `providers:` — the row's `keys:` list, upserted on rows with extras.
    // An absent section binds nothing, so the numbered refs would be dead
    // weight: the doc goes back unchanged rather than half-bridged.
    let mut providers: Vec<ar_config::ProviderCfg> = match map.get("providers") {
        Some(v) => serde_yaml::from_value(v.clone()).map_err(|e| {
            fail(
                e,
                "the config's `providers:` block did not parse; fix it by hand and re-run sync",
            )
        })?,
        None => return Ok(doc.to_owned()),
    };
    if providers.is_empty() {
        return Ok(doc.to_owned());
    }
    for row in &mut providers {
        if let Some(names) = extras.get(&row.id) {
            row.keys = Some(names.clone());
        }
    }
    let mut out = splice_section(doc, "keys", &render_keys_rows(&ordered)?);
    out = splice_section(
        &out,
        "providers",
        &import::render_providers_section(&providers)?,
    );
    Ok(out)
}

fn sync_env_file(env_path: &Path, db: &Path, doc: &str, dry_run: bool) -> anyhow::Result<EnvPlan> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    // First row wins: `priority DESC, created_at ASC` puts the operator's
    // preferred connection first.
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    // How many usable connections each provider has produced so far, so the
    // numbering below agrees with `sync_credentials`'s on the same walk.
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut stmt = conn.prepare(concat!(
        "SELECT provider, api_key FROM provider_connections ",
        "WHERE is_active = 1 AND coalesce(api_key, '') != '' ",
        "ORDER BY priority DESC, created_at ASC"
    ))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (provider, api_key) = row?;
        // OmniRoute encrypts provider keys at rest (`enc:v1:...`) and only the
        // instance holding STORAGE_ENCRYPTION_KEY can open them. A sync must
        // never materialise ciphertext as a credential: leave the env value
        // alone and say which provider was skipped, so a working hand-set key
        // survives and a missing one surfaces as the empty-var case below.
        if api_key.starts_with("enc:v1:") {
            eprintln!(
                "note: AR_KEY_{} is encrypted at rest and was left untouched — decrypt the connection in the dashboard first",
                import::env_name(&provider)
            );
            continue;
        }
        // One numbered var per usable connection, in the dashboard's own
        // priority order: the first keeps the provider's plain name, the
        // rest gain `_2`, `_3`... — the same numbering `sync_credentials`
        // binds them under, so the i-th connection here is the i-th there.
        let n = seen.entry(provider.clone()).or_insert(0);
        *n += 1;
        let var = if *n == 1 {
            format!("AR_KEY_{}", import::env_name(&provider))
        } else {
            format!("AR_KEY_{}_{}", import::env_name(&provider), n)
        };
        values.insert(var, api_key);
    }

    // Every `$AR_KEY_*` the config names must exist in the env file afterwards.
    let mut targets: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (var, key) in &values {
        targets.insert(var.clone(), Some(key.clone()));
    }
    for var in ar_key_refs(doc) {
        targets.entry(var).or_insert(None);
    }

    let mut lines: Vec<String> = if env_path.is_file() {
        std::fs::read_to_string(env_path)
            .map_err(|e| {
                fail(
                    format!("cannot read {}: {e}", env_path.display()),
                    "fix the file, or pass --env <FILE> at a path you can write",
                )
            })?
            .lines()
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };

    let mut materialised = 0;
    let mut ensured = 0;
    for (var, value) in &targets {
        let entry = match value {
            Some(v) => format!("{var}={v}"),
            None => format!("{var}="),
        };
        if let Some(existing) = lines.iter_mut().find(|l| l.starts_with(&format!("{var}="))) {
            if value.is_some() && *existing != entry {
                *existing = entry;
                materialised += 1;
            }
        } else {
            lines.push(entry);
            if value.is_some() {
                materialised += 1;
            } else {
                ensured += 1;
            }
        }
    }
    let changed = materialised + ensured > 0;
    if changed && !dry_run {
        write_private(env_path, &(lines.join("\n") + "\n")).map_err(|e| {
            fail(
                format!("cannot write {}: {e}", env_path.display()),
                "fix the file, or pass --env <FILE> at a path you can write",
            )
        })?;
    }
    Ok(EnvPlan {
        materialised,
        ensured,
        changed,
        lines,
    })
}

/// Writes a file that holds credentials: staged at `0600` from its first byte,
/// then renamed over the target.
///
/// Writing the target in place and chmod-ing it afterwards leaves a window where
/// a new file carries the umask mode (`0644` under `022`) and an existing one
/// keeps its old mode while the secrets land. Staging the file at `0600` closes
/// that window, and the rename means a reader sees the old file or the new one,
/// never a partial write. A failed stage removes its temp file.
pub fn write_private(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let staged = stage_private(&tmp, body).and_then(|()| std::fs::rename(&tmp, path));
    if staged.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    staged
}

fn stage_private(tmp: &std::path::Path, body: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(tmp)?;
    // `mode` applies only when the file is created, so a stale temp file from
    // an earlier crash would keep its old bits without this.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(body.as_bytes())?;
    file.sync_all()
}

/// Every `AR_KEY_*` variable name the document references after a `$`.
fn ar_key_refs(doc: &str) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    let mut rest = doc;
    while let Some(i) = rest.find("$AR_KEY_") {
        let start = i + "$AR_KEY_".len();
        let end = rest[start..]
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .map_or(rest.len(), |k| start + k);
        refs.insert(format!("AR_KEY_{}", &rest[start..end]));
        rest = &rest[end..];
    }
    refs
}

/// How many previous versions of a generated file are kept.
///
/// Three rather than one: a bad import is usually only noticed on the *second*
/// run after it, when the symptom is a model that vanished, and the version that
/// caused that is the one worth being able to diff against.
const BACKUP_DEPTH: usize = 3;

/// Writes one generated file, naming the path rather than the errno alone.
///
/// Every overwrite keeps the last [`BACKUP_DEPTH`] versions as `<name>.1` …
/// `<name>.3`, oldest evicted. This is the only write path `aroute import` uses, so
/// one rotation covers config.yaml, registry.json, freeBudgets.json and
/// providerMeta.json together — and `registry.json` is `include_str!`d into
/// `ar-registry`, so a bad regeneration is a rebuild away from being live.
pub(crate) fn write_file(path: &Path, body: &str) -> anyhow::Result<()> {
    rotate_backups(path)?;
    write_atomic(path, body).map_err(|e| {
        fail(
            format!("cannot write {}: {e}", path.display()),
            "pass --out-dir <DIR> at a path you can write",
        )
    })
}

/// Replaces `path` through a synced temp file beside it, then renames over it.
///
/// A crash or a second writer mid-write leaves the old file whole, never a
/// truncated config. The temp name carries the PID so two writers do not share it.
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let staged = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if staged.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    staged
}

/// Refuses a wholesale `config.yaml` write that would drop a section the writer
/// cannot emit.
///
/// `import` replaces the whole file from `render_yaml`, and that renderer emits
/// only the sections named in [`import::emittable_sections`]. Anything else the
/// existing file carries — `oauth:` today, whatever `Config` gains next — would
/// vanish with a zero exit code and no line of output naming the loss. The
/// operator's live sessions live in `oauth:` and no other writer restores them,
/// which is what makes this a data-loss bug rather than a cosmetic one.
///
/// The refusal is a refusal, not a warning: the operator reads both, and one
/// answer here is safer than two. The escape hatch names a section explicitly, so
/// a conscious overwrite stays a conscious overwrite.
///
/// # Errors
///
/// The existing file carries a top-level section outside the emittable set and
/// `allow` neither names that section nor is the blanket `all`.
fn guard_against_section_loss(path: &Path, allow: Option<&str>) -> anyhow::Result<()> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        // Nothing to lose on a first import into an empty directory, and an
        // unreadable one is the write's error to make, not this guard's.
        return Ok(());
    };
    let lost: Vec<String> = existing
        .lines()
        .filter(|l| is_section_header(l))
        .map(|l| l.trim_end_matches(':').to_owned())
        .filter(|name| !import::emittable_sections().contains(&name.as_str()))
        .collect();
    if lost.is_empty() || allow == Some("all") || allow.is_some_and(|a| lost.iter().any(|s| s == a))
    {
        return Ok(());
    }
    let names = lost
        .iter()
        .map(|s| format!("`{s}:`"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(fail(
        format!(
            "{} already has {names}, which an import cannot write back",
            path.display()
        ),
        "add those sections to the new file yourself, or re-run with --allow-section-loss all to overwrite them anyway",
    ))
}

/// `<name>.1` for `name = /p/config.yaml`, and so on.
fn backup_path(path: &Path, depth: usize) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{depth}"));
    PathBuf::from(s)
}

/// Shifts `.<n>` to `.<n+1>`, dropping what falls off the end, then stages the
/// current file as `.1`.
///
/// Shifting oldest-first is what makes it correct: each step reads a slot that
/// no later step has written, so no version is lost to being overwritten by the
/// one it was about to become.
fn rotate_backups(path: &Path) -> anyhow::Result<()> {
    // Drop the oldest first so the loop below never needs to delete it.
    let oldest = backup_path(path, BACKUP_DEPTH);
    if oldest.exists() {
        std::fs::remove_file(&oldest).map_err(|e| {
            fail(
                format!("cannot replace {}: {e}", oldest.display()),
                "check the directory is writable and the backups are yours to remove",
            )
        })?;
    }
    for depth in (1..BACKUP_DEPTH).rev() {
        let from = backup_path(path, depth);
        if !from.exists() {
            continue;
        }
        let to = backup_path(path, depth + 1);
        std::fs::rename(&from, &to).map_err(|e| {
            fail(
                format!("cannot move {} to {}: {e}", from.display(), to.display()),
                "check the directory is writable",
            )
        })?;
    }
    if path.exists() {
        std::fs::copy(path, backup_path(path, 1)).map_err(|e| {
            fail(
                format!("cannot back up {}: {e}", path.display()),
                "check there is room beside the file you are importing over",
            )
        })?;
    }
    Ok(())
}

/// Maps a clap parse failure onto the `docs/06` exit-code contract: `2` for
/// usage errors, `0` for `--help` and `--version`.
pub fn usage_exit_code(kind: ErrorKind) -> u8 {
    match kind {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        | ErrorKind::DisplayVersion => 0,
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
    // The column flags, as the caller wrote them: the parent's own for a bare
    // `aroute providers --fields nope`, and the `list` subcommand's for
    // `aroute providers list --fields nope`. Both are the same verb's question,
    // so both exit 2. An `add`/`remove` carries no column flags at all.
    //
    // One match, three things: the flag, the valid columns and the verb's name.
    // A `None` in the fields slot is not "no check" — `fields_or_default`
    // expands it to the verb's own default set, which validates too.
    let (fields, all, cmd) = match &cli.command {
        Some(Command::Models(a)) => (a.fields.as_deref(), &MODEL_COLUMNS[..], "models"),
        Some(Command::Providers(a)) => {
            let fields = match &a.command {
                None => a.fields.as_deref(),
                Some(crate::cli::ProviderCommand::List(args)) => args.fields.as_deref(),
                _ => return None,
            };
            (fields, &PROVIDER_COLUMNS[..], "providers")
        }
        Some(Command::Combo(a)) => (a.fields.as_deref(), &COMBO_COLUMNS[..], "combo"),
        _ => return None,
    };
    columns(&toon::fields_or_default(fields), all, cmd)
        .err()
        .map(|e| e.to_string())
}

/// The loud hint `docs/06` requires on an unknown flag: the offending name plus
/// the flags that command actually accepts.
///
/// The subcommand is found by walking the command graph as deep as the arguments
/// go, by *name* rather than by position: `aroute --config path auth login --nope`
/// has four tokens before the flag, and a positional read of "the first non-flag
/// token" would land on `path` and list the root's flags instead of the login
/// verb's. The deepest level reached wins, so a sub-subcommand's flags are on
/// the hint — which is the whole point of adding a subtree.
pub fn unknown_flag_hint(args: &[OsString], invalid: &str) -> String {
    let root = Cli::command();
    let mut deepest = None;
    let mut current = &root;
    // Two levels is the deepest this surface goes (`aroute auth login`), so a fixed
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

    format!(
        "unknown flag {invalid:?}\nhelp: valid flags: {}",
        flags.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file states a config writer must be proven against, in one place.
    ///
    /// Four passes of validation each found a defect the shipped tests could
    /// not express, because a test pins one spelling of a file and a writer's
    /// claim is about *pairs* of spellings. Measured before this existed: of
    /// the 17 round-trip assertions here, 4 crossed both endings and all four
    /// were written *after* the defect they eventually caught — the other 13
    /// passed by choosing the shape their fixture happened to spell.
    ///
    /// So each state is paired with the states that make its writer's claim
    /// checkable, and a round trip runs the whole set rather than one member
    /// of it. The shapes are the four the passes found, plus the one they
    /// never reached:
    ///
    /// - **ends with a newline** vs **ends mid-line**: the trailing newline is
    ///   the file's, and `str::lines` drops it, so a writer that rejoins
    ///   without asking changes a byte nobody asked about.
    /// - **ends with a blank line**: `trim_end()` cannot tell this from no
    ///   blank, and the two must not produce the same output.
    /// - **CRLF**: `lines` strips the `\r`, so a `\n`-only rejoin rewrites
    ///   every line ending in the file.
    /// - **a file with no `combos:`** at all, which routes an insert to the
    ///   append branch rather than the anchor branch.
    ///
    /// Every fixture is a document a writer can accept, and each carries
    /// `keys:` so a `$AR_KEY_A` binding resolves under a no-expansion parse.
    mod shapes {
        /// One combo, ending with a newline. The common case.
        pub const LF: &str = "keys:\n  a: $AR_KEY_A\n\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - a/m\n";

        /// The same document ending mid-line, with no final newline.
        ///
        /// This repo's own `config.yaml` ends this way deliberately, and a
        /// writer must not give it one.
        pub const NO_NEWLINE: &str = "keys:\n  a: $AR_KEY_A\n\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - a/m";

        /// A trailing blank line the operator wrote after the last row.
        ///
        /// `content.trim_end()` cannot tell this from [`NO_NEWLINE`], and the
        /// two must not produce identical output.
        pub const TRAILING_BLANK: &str = "keys:\n  a: $AR_KEY_A\n\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - a/m\n\n";

        /// The same document in CRLF. `serde_yaml` parses it identically, which
        /// is why a rewrite of every line ending went unnoticed.
        pub const CRLF: &str = "keys:\r\n  a: $AR_KEY_A\r\n\r\ncombos:\r\n  - id: c\r\n    strategy: priority\r\n    targets:\r\n      - a/m\r\n";

        /// A config with no `combos:` section, so an insert takes the append
        /// branch instead of the anchor branch.
        pub const NO_COMBOS: &str = "keys:\n  a: $AR_KEY_A\n";

        /// Every document shape, for a round trip to be proven against rather
        /// than one.
        pub const ALL: &[&str] = &[LF, NO_NEWLINE, TRAILING_BLANK, CRLF, NO_COMBOS];
    }

    /// A writer `M` that adds something to a document, and the `U` that removes
    /// it: runs the pair over every shape in [`shapes::ALL`] and requires the
    /// file come back byte for byte.
    ///
    /// Which is the property the plan claims — "add-then-remove restores the
    /// file byte for byte" — and the reason the helper exists: proving it four
    /// times in four separate tests is how it got proven for four shapes and
    /// nowhere else.
    ///
    /// `can_add` says for which shapes the add is expected to land at all. It is
    /// a parameter rather than an `assert_ne!` because some writers need a
    /// section to exist to write into it: `append_combo_row` places a row inside
    /// `combos:` and a file with none has nothing to place it in, so "the add
    /// changed something" is not true of every shape for every writer. The
    /// helper used to assert it, and the first call failed on
    /// [`shapes::NO_COMBOS`] for that reason — which is the helper earning its
    /// keep: it asks the writer a question the caller had not answered.
    fn round_trips_on_every_shape(
        add: impl Fn(&str) -> String,
        remove: impl Fn(&str) -> String,
        can_add: impl Fn(&str) -> bool,
    ) {
        for shape in shapes::ALL {
            let added = add(shape);
            if !can_add(shape) {
                // Not a shape this writer writes. It must still leave it alone:
                // a writer that re-spells a file it cannot edit is the same
                // defect as one that gets an edit wrong.
                assert_eq!(
                    added, *shape,
                    "a writer that cannot add must still not re-spell the file: {shape:?}"
                );
                continue;
            }
            assert_ne!(added, *shape, "the add changed nothing on {shape:?}");
            assert_eq!(
                remove(&added),
                *shape,
                "add then remove must restore the file byte for byte on {shape:?}"
            );
        }
    }

    /// The remove direction alone, on a shape that never held the thing.
    ///
    /// The direction a round trip cannot reach, because a round trip starts
    /// with a write — and the one where `hide-paid false` created the section
    /// it removes.
    fn remove_alone_is_a_no_op_on_every_shape(remove: impl Fn(&str) -> String) {
        for shape in shapes::ALL {
            assert_eq!(
                remove(shape),
                *shape,
                "a removal of something absent must not re-spell the file: {shape:?}"
            );
        }
    }

    /// The plan's headline property, over every shape at once rather than
    /// one spelling of it.
    ///
    /// Before the fixture set existed this was pinned four times in four tests,
    /// each on the shape that test happened to spell, and the shapes that were
    /// not spelled were the shapes that held four separate defects. One call
    /// over [`shapes::ALL`] is the property the plan actually claims.
    #[test]
    fn a_row_writer_round_trips_every_file_shape() {
        let row = indent_row(
            "- id: new\n  strategy: priority\n  targets:\n    - a/m\n",
            2,
        );
        round_trips_on_every_shape(
            |doc| append_combo_row(doc, &row),
            |added| remove_combo_row(added, "new").unwrap_or_else(|| added.to_owned()),
            // A row goes inside `combos:`, so a file with none takes the
            // append branch and lands nowhere the removal can find it.
            |shape| shape.contains("combos:"),
        );
    }

    /// The remove direction alone, on shapes that never held the thing — the
    /// direction that produced the `hide-paid false` defect and that no round
    /// trip can reach.
    #[test]
    fn a_removal_of_something_absent_is_a_no_op_on_every_shape() {
        remove_alone_is_a_no_op_on_every_shape(|doc| {
            set_section_scalar(doc, "visibility", "hide_paid_models", false)
        });
    }

    /// A section write, on the shapes where the absent-section branch runs.
    #[test]
    fn a_section_write_round_trips_every_file_shape() {
        let block = "limits:\n  keys:\n    ci:\n      rpm: 5\n";
        round_trips_on_every_shape(
            |doc| splice_section(doc, "limits", block),
            |added| remove_section(added, "limits"),
            // The splice *inserts* a section that is absent, so every shape
            // takes the write.
            |_shape| true,
        );
    }

    /// The repo's own `config.yaml` is a combo list this build wrote or
    /// accepted, so [`check_combos`] must say yes about it — and it is the only
    /// fixture that carries a real operator's section: comment blocks, a
    /// flow-style `targets:` list, and the strategies an operator actually chose
    /// while tuning the routing table. A validator that rejects the repo's own
    /// config is a validator nobody can use, and this is the test that catches it.
    #[test]
    fn check_combos_accepts_the_repos_own_config() {
        // Relative to this source file: `crates/ar-cli/src/commands.rs` -> the
        // repo root. Not `../../..`, which lands in `proxy/`.
        let yaml = include_str!("../../../config.yaml");
        let cfg = match Config::parse(yaml, no_expansion) {
            Ok(c) => c,
            Err(e) => panic!("the repo's config.yaml must parse: {e}"),
        };
        assert!(
            !cfg.combos.is_empty(),
            "the fixture is worthless if it carries no combos"
        );
        check_combos(&cfg.combos, &cfg)
            .unwrap_or_else(|e| panic!("check_combos refused the repo's own config: {e}"));
    }

    /// The contract the Phase 4 verbs are held to: `combo add` then
    /// `combo remove` of the same row restores the file **byte for byte**.
    ///
    /// Measured against the repo's own `config.yaml`, because a synthetic
    /// fixture that omits the things a hand-written file actually carries would
    /// pass a writer that drifted from them. The three properties are exactly
    /// what the earlier whole-section writers got wrong:
    ///
    /// 1. The blank separator and the four-line note between two rows stay where
    ///    the operator put them, above the row they document.
    /// 2. No row is duplicated — the section's body must be consumed whole, not
    ///    only up to its first blank line.
    /// 3. The trailing-newline absence is the file's, not the writer's.
    #[test]
    fn combo_add_then_remove_is_a_byte_identical_round_trip() {
        let yaml = include_str!("../../../config.yaml");
        let cfg = Config::parse(yaml, no_expansion).unwrap();

        let added = Combo {
            id: "scratch".to_owned(),
            strategy: Strategy::parse("priority"),
            targets: vec!["openai/gpt-5.4".to_owned()],
            weights: BTreeMap::new(),
            pool: Vec::new(),
            steps: Vec::new(),
            compression: None,
            context_length: None,
            judge_model: None,
        };
        let rows: Vec<Combo> = cfg
            .combos
            .iter()
            .cloned()
            .chain(std::iter::once(added))
            .collect();
        check_combos(&rows, &cfg).expect("the repo's rows plus one must validate");

        let row = crate::import::render_combo_block(rows.last().unwrap()).unwrap();
        let after_add = append_combo_row(yaml, &row);
        assert_ne!(after_add, yaml, "the add must have changed something");
        // The three rows the file already carried are still there, once each: a
        // writer that stopped skipping early left the section's tail below the
        // rendered block, which is how a six-row table appears.
        assert_eq!(
            after_add.matches("- id:").count(),
            yaml.matches("- id:").count() + 1
        );
        // Every byte up to the end of the last original row is untouched, so the
        // blank separator and the note above `cheap` are still above it.
        assert!(
            after_add
                .starts_with(&yaml[..yaml.rfind("nano\n").expect("the fixture's last target")]),
            "every byte before the new row must be the original file"
        );

        let after_remove =
            remove_combo_row(&after_add, "scratch").expect("the added row must be found");
        assert_eq!(
            after_remove, yaml,
            "removing the row add wrote must restore the file byte for byte"
        );
    }

    /// `combo target add` / `combo target remove` round trip too, and against
    /// the same fixture: a target edit touches one line, so the diff must be one
    /// line and the round trip must be empty.
    #[test]
    fn combo_target_add_then_remove_is_a_byte_identical_round_trip() {
        let yaml = include_str!("../../../config.yaml");
        let after_add = add_target_line(yaml, "default", "openai/gpt-5.4-nano")
            .expect("the repo's `default` combo must answer");
        assert_ne!(after_add, yaml);
        assert_eq!(
            after_add.lines().count(),
            yaml.lines().count() + 1,
            "one line was added, so exactly one line must appear"
        );
        let after_remove = remove_target_line(&after_add, "default", "openai/gpt-5.4-nano")
            .expect("the line just written must be found");
        assert_eq!(
            after_remove, yaml,
            "the target edit must round trip exactly"
        );
    }

    /// The regression that produced six rows out of three.
    ///
    /// `splice_section` used to end its skip at the first blank line or column-0
    /// comment, on the ground that both are unindented and so must belong to the
    /// next section. The repo's `combos:` carries a blank separator *inside* the
    /// section between two of its rows, so every splice stopped there, left the
    /// rows after it below the rendered block, and the routing table read twice
    /// the combos it had. This is the shape that caught it, held as a test.
    #[test]
    fn splice_consumes_a_section_body_that_carries_a_blank_separator() {
        let doc =
            "combos:\n  - id: one\n    strategy: priority\n\n  - id: two\n    strategy: lkgp\n";
        let rendered = "combos:\n  - id: one\n    strategy: priority\n";
        let out = splice_section(doc, "combos", rendered);
        assert_eq!(
            out.matches("- id:").count(),
            1,
            "`two` must be consumed with `one`"
        );
        assert_eq!(out, "combos:\n  - id: one\n    strategy: priority\n");
    }

    /// A column-0 comment between two rows of one section is that section's,
    /// not the next one's — the other half of the regression above.
    ///
    /// The consequence is the awkward one: a comment that sits *inside* a section
    /// is replaced with the section, because a splice owns the whole section.
    /// That is why [`append_combo_row`] exists — the row writers preserve it, and
    /// this test is the record of why they had to.
    #[test]
    fn splice_consumes_a_column_zero_comment_between_two_rows() {
        let doc = "combos:\n  - id: one\n    strategy: priority\n\n  # why two\n  - id: two\n    strategy: lkgp\n";
        let rendered = "combos:\n  - id: one\n    strategy: priority\n";
        let out = splice_section(doc, "combos", rendered);
        // `two` is consumed, and the note between the two rows goes with it.
        assert_eq!(out.matches("- id:").count(), 1);
        assert_eq!(out, "combos:\n  - id: one\n    strategy: priority\n");

        // But a note that introduces the *next* section stays, above it — the
        // look-ahead is what tells the two apart.
        let boundary = "combos:\n  - id: one\n    strategy: priority\n\n# why keys\nkeys:\n  openai: $AR_KEY_OPENAI\n";
        let out = splice_section(boundary, "combos", rendered);
        assert!(
            out.contains("# why keys"),
            "the next section's note must stay: {out:?}"
        );
        assert!(out.contains("keys:"), "the next section must stay");
        assert_eq!(out, boundary, "nothing else moved");
    }

    /// The trailing newline is the file's, not the writer's: the repo's
    /// `config.yaml` ends without one deliberately, and a writer that added it
    /// made every write read as a whole-file diff.
    ///
    /// This is the Phase 3 contract, restated here because `combo add` is the
    /// third writer to reach the same file and the property is the surface's,
    /// not one verb's.
    #[test]
    fn combo_writers_preserve_the_files_own_trailing_newline() {
        let yaml = include_str!("../../../config.yaml");
        assert!(
            !yaml.ends_with('\n'),
            "the fixture must end without a newline"
        );
        let cfg = Config::parse(yaml, no_expansion).unwrap();
        let mut next = cfg;
        next.combos.push(Combo {
            id: "scratch".to_owned(),
            strategy: Strategy::parse("priority"),
            targets: vec!["openai/gpt-5.4".to_owned()],
            weights: BTreeMap::new(),
            pool: Vec::new(),
            steps: Vec::new(),
            compression: None,
            context_length: None,
            judge_model: None,
        });
        let row = crate::import::render_combo_block(next.combos.last().unwrap()).unwrap();
        let after_add = append_combo_row(yaml, &row);
        assert!(
            !after_add.ends_with('\n'),
            "the add must not add a final newline"
        );
        let after_remove =
            remove_combo_row(&after_add, "scratch").expect("the added row must be found");
        assert!(
            !after_remove.ends_with('\n'),
            "the remove must not add a final newline either"
        );
        assert_eq!(after_remove, yaml);
    }

    /// A row-level writer must not use a *semantic* no-op test.
    ///
    /// `combo add` originally asked [`means_the_same`], which compares parsed
    /// configs — and `Config` models only four of the top-level sections a real
    /// file can carry. On a config holding `oauth:`, `visibility:` or `mcp:`
    /// both parses are equal, so the row was written and then **silently
    /// discarded** as "unchanged". The operator's add did nothing and reported
    /// success.
    ///
    /// A row-level edit changes exact bytes and nothing else, so byte equality
    /// is both the correct and the stricter test. This is the shape that caught
    /// it, and it is why the whole-section splice may keep `means_the_same` —
    /// a re-render can legitimately re-spell the same configuration, which a
    /// one-row edit cannot.
    #[test]
    fn combo_add_lands_on_a_config_carrying_sections_no_writer_represents() {
        let doc = "server:\n  host: 127.0.0.1\n  port: 20128\nkeys:\n  openai: $AR_KEY_OPENAI\nproviders:\n  - id: openai\n    key: openai\ncombos:\n  - id: default\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\noauth:\n  - provider: codex\n    refresh_key: codex_refresh\nvisibility:\n  defaults:\n    admin: true\n";

        let cfg = Config::parse(doc, no_expansion).expect("the document parses");
        assert!(!cfg.combos.is_empty(), "the fixture carries a combo");

        let added = Combo {
            id: "scratch".to_owned(),
            strategy: Strategy::parse("priority"),
            targets: vec!["openai/gpt-5.4".to_owned()],
            weights: BTreeMap::new(),
            pool: Vec::new(),
            steps: Vec::new(),
            compression: None,
            context_length: None,
            judge_model: None,
        };
        let rows: Vec<Combo> = cfg
            .combos
            .iter()
            .cloned()
            .chain(std::iter::once(added))
            .collect();
        let row = crate::import::render_combo_block(rows.last().unwrap()).unwrap();
        let after_add = append_combo_row(doc, &row);

        // The row landed, which is what the semantic test could not see.
        assert_ne!(after_add, doc, "the row must appear in the candidate");
        // Two rows in `combos:` (the fixture's `providers:` row is the third
        // `- id:` in the document, which is why the count is not 2).
        assert_eq!(after_add.matches("- id:").count(), 3);
        // Every section the config carries is still there, byte for byte.
        for section in ["server:", "keys:", "providers:", "oauth:", "visibility:"] {
            assert!(
                after_add.contains(section),
                "`{section}` survived: {after_add:?}"
            );
        }
        // And the whole document round trips, unemittable sections and all.
        let after_remove =
            remove_combo_row(&after_add, "scratch").expect("the added row must be found");
        assert_eq!(after_remove, doc);
    }

    #[test]
    fn default_columns_lead_with_toon_defaults() {
        for cols in [
            PROVIDER_COLUMNS.as_slice(),
            MODEL_COLUMNS.as_slice(),
            COMBO_COLUMNS.as_slice(),
            SYNC_COLUMNS.as_slice(),
        ] {
            assert_eq!(&cols[..3], &toon::DEFAULT_FIELDS[..], "{cols:?}");
        }
    }

    #[test]
    fn sync_splices_the_combos_section_and_materialises_keys() {
        let dir = std::env::temp_dir().join(format!("ar-sync-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("storage.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE combos (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, \
             data TEXT NOT NULL, sort_order INTEGER NOT NULL DEFAULT 0, \
             created_at TEXT NOT NULL, updated_at TEXT NOT NULL); \
             CREATE TABLE provider_connections (id TEXT PRIMARY KEY, \
             provider TEXT NOT NULL, api_key TEXT, is_active INTEGER DEFAULT 1, \
             priority INTEGER DEFAULT 0, created_at TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO combos (id, name, data, sort_order, created_at, updated_at) \
             VALUES ('c1', 'stack', ?1, 0, '', '')",
            rusqlite::params![
                r#"{"name":"stack","strategy":"priority","models":[{"kind":"model","model":"nvidia/z-ai/glm-5.3","providerId":"nvidia","weight":0}]}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_connections (id, provider, api_key, is_active, priority, created_at) \
             VALUES ('k1', 'nvidia', 'nvapi-secret', 1, 5, '2024-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_connections (id, provider, api_key, is_active, priority, created_at) \
             VALUES ('k2', 'groq', 'enc:v1:deadbeef', 1, 9, '2024-01-01')",
            [],
        )
        .unwrap();
        // A second usable nvidia connection, lower priority than k1: the
        // cycling bridge numbers it as the provider's second credential.
        conn.execute(
            "INSERT INTO provider_connections (id, provider, api_key, is_active, priority, created_at) \
             VALUES ('k1b', 'nvidia', 'nvapi-second', 1, 3, '2024-01-02')",
            [],
        )
        .unwrap();
        drop(conn);

        let config = dir.join("config.yaml");
        std::fs::write(
            &config,
            "keys:\n  nvidia: $AR_KEY_NVIDIA\n  ghost: $AR_KEY_GHOST\nproviders:\n  - id: nvidia\n    key: nvidia\ncombos:\n  - id: old\n    strategy: priority\n    targets:\n      - nvidia/z-ai/glm-5.3\n",
        )
        .unwrap();
        let env = dir.join("ar.env");
        std::fs::write(&env, "AR_KEY_NVIDIA=stale\n").unwrap();

        let cli = Cli {
            config: config.clone(),
            command: None,
            system: false,
        };
        sync_config(
            &cli,
            &SyncArgs {
                db: Some(db),
                env: Some(env.clone()),
                no_restart: true,
                dry_run: false,
            },
        )
        .unwrap();

        let cfg = std::fs::read_to_string(&config).unwrap();
        assert!(cfg.contains("  - id: stack"), "combo not spliced:\n{cfg}");
        assert!(!cfg.contains("- id: old"), "stale combo survived:\n{cfg}");
        assert!(
            cfg.contains("nvidia: $AR_KEY_NVIDIA\n"),
            "the first connection keeps its name:\n{cfg}"
        );
        assert!(
            cfg.contains("ghost: $AR_KEY_GHOST"),
            "a hand-set key entry must survive the re-render:\n{cfg}"
        );
        assert!(
            cfg.contains("nvidia-2: $AR_KEY_NVIDIA_2"),
            "the second connection is not bound as a numbered credential:\n{cfg}"
        );
        // Parse-back, not string-match: the re-rendered sections must still
        // BE the sections. A `contains` assertion passed while serde_yaml's
        // column-0 mapping made every keys entry a root-level scalar and the
        // server refused the config — the exact regression this catches.
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&cfg).expect("the synced config must parse");
        let keys = parsed
            .get("keys")
            .and_then(serde_yaml::Value::as_mapping)
            .expect("keys: must be a mapping, not a null key");
        assert_eq!(
            keys.get("nvidia-2").and_then(|v| v.as_str()),
            Some("$AR_KEY_NVIDIA_2"),
            "the second connection is not in the keys map:\n{cfg}"
        );
        assert_eq!(
            keys.get("ghost").and_then(|v| v.as_str()),
            Some("$AR_KEY_GHOST"),
            "a hand-set entry must survive:\n{cfg}"
        );
        let providers = parsed
            .get("providers")
            .and_then(serde_yaml::Value::as_sequence)
            .expect("providers: must be a sequence");
        let nvidia = providers
            .iter()
            .find(|r| r.get("id").and_then(|v| v.as_str()) == Some("nvidia"))
            .expect("the nvidia provider row");
        assert_eq!(
            nvidia.get("key").and_then(|v| v.as_str()),
            Some("nvidia"),
            "the row keeps its first credential"
        );
        let extras = nvidia
            .get("keys")
            .and_then(serde_yaml::Value::as_sequence)
            .expect("the row carries its cycling extras");
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].as_str(), Some("nvidia-2"));

        let envdoc = std::fs::read_to_string(&env).unwrap();
        assert!(
            envdoc.contains("AR_KEY_NVIDIA=nvapi-secret"),
            "key not materialised:\n{envdoc}"
        );
        assert!(
            envdoc.contains("AR_KEY_NVIDIA_2=nvapi-second"),
            "the second connection was not materialised:\n{envdoc}"
        );
        assert!(
            !envdoc.contains("enc:v1:"),
            "encrypted-at-rest ciphertext leaked into the env file:\n{envdoc}"
        );
        assert!(
            envdoc.contains("AR_KEY_GHOST="),
            "a referenced var with no connection was not ensured empty:\n{envdoc}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_dry_run_should_write_nothing_when_changes_are_pending() {
        // Ledger F16 / DASH-10: the hazard was an operator inspecting the
        // bridge and silently rewriting their routing config. The fix has to
        // be observable: same computation, no bytes on disk.
        let dir = tmpdir("sync-dry-run");
        let db = dir.join("storage.sqlite");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE provider_connections (id TEXT PRIMARY KEY, provider TEXT, \
                 api_key TEXT, is_active INTEGER, priority INTEGER, created_at TEXT); \
                 CREATE TABLE combos (id TEXT PRIMARY KEY, strategy TEXT, targets TEXT);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO provider_connections (id, provider, api_key, is_active, priority, created_at) \
                 VALUES ('d1', 'nvidia', 'nvapi-live', 1, 1, '2024-01-01')",
                [],
            )
            .unwrap();
            drop(conn);
        }
        let config = dir.join("config.yaml");
        let original = "keys:\n  nvidia: $AR_KEY_NVIDIA\nproviders:\n  - id: nvidia\n    key: nvidia\ncombos:\n  - id: old\n    strategy: priority\n    targets:\n      - nvidia/z-ai/glm-5.3\n";
        std::fs::write(&config, original).unwrap();
        let env = dir.join("ar.env");
        std::fs::write(&env, "AR_KEY_NVIDIA=stale\n").unwrap();
        let env_before = std::fs::read_to_string(&env).unwrap();

        let cli = Cli {
            config: config.clone(),
            command: None,
            system: false,
        };
        sync_config(
            &cli,
            &SyncArgs {
                db: Some(db),
                env: Some(env.clone()),
                no_restart: false,
                dry_run: true,
            },
        )
        .expect("a dry run must not fail on a writable fixture");

        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            original,
            "the config must be byte-identical after a dry run"
        );
        assert_eq!(
            std::fs::read_to_string(&env).unwrap(),
            env_before,
            "the env file must be byte-identical after a dry run — this is the whole point"
        );
    }

    #[test]
    fn sync_dry_run_should_report_pending_changes_without_restarting_the_service() {
        // The dry run must still SAY what it would do (a silent no-op would be
        // useless), and it must never restart aroute.service.
        let dir = tmpdir("sync-dry-run-report");
        let db = dir.join("storage.sqlite");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE provider_connections (id TEXT PRIMARY KEY, provider TEXT, \
                 api_key TEXT, is_active INTEGER, priority INTEGER, created_at TEXT);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO provider_connections (id, provider, api_key, is_active, priority, created_at) \
                 VALUES ('r1', 'groq', 'gsk-live', 1, 1, '2024-01-01')",
                [],
            )
            .unwrap();
            drop(conn);
        }
        let config = dir.join("config.yaml");
        std::fs::write(
            &config,
            "keys:\n  groq: $AR_KEY_GROQ\nproviders:\n  - id: groq\n    key: groq\n",
        )
        .unwrap();
        let env = dir.join("ar.env");
        // The live value differs, so the env has a pending change to report.
        std::fs::write(&env, "AR_KEY_GROQ=stale\n").unwrap();

        let cli = Cli {
            config,
            command: None,
            system: false,
        };
        // `systemctl` is not on the test PATH-controlled command list; if the
        // dry run ever tried to restart the service this would fail or be
        // observable, so asserting success is also asserting no restart.
        sync_config(
            &cli,
            &SyncArgs {
                db: Some(db),
                env: Some(env.clone()),
                no_restart: false,
                dry_run: true,
            },
        )
        .expect("a dry run must succeed without touching the service");
        assert_eq!(
            std::fs::read_to_string(&env).unwrap(),
            "AR_KEY_GROQ=stale\n",
            "the stale value must still be on disk after the dry run"
        );
    }

    #[test]
    fn splice_replaces_only_the_named_section() {
        let doc = "keys:\n  a: $AR_KEY_A\nproviders:\n  - id: p\ncustom_providers:\n  - id: old\ncombos:\n  - id: c\n";
        let out = splice_section(doc, "custom_providers", "custom_providers:\n  - id: new");
        assert!(out.contains("  - id: new"), "not spliced:\n{out}");
        assert!(!out.contains("  - id: old"), "stale entry survived:\n{out}");
        assert!(
            out.contains("combos:\n  - id: c"),
            "later section eaten:\n{out}"
        );
    }

    /// The invariant Phase 0's guard depends on, stated as a test so a future
    /// writer cannot quietly break it.
    ///
    /// `guard_against_section_loss` covers `import` alone, which is only safe
    /// because `import` is the **only** path that regenerates a whole
    /// `config.yaml`. Every other writer splices one section or edits one row,
    /// and both copy every line they do not own — so an `oauth:`, `visibility:`
    /// or `mcp:` block survives a write it was never part of.
    ///
    /// The property is worth pinning precisely because it is invisible: a
    /// whole-document writer added later would drop those blocks with a zero
    /// exit code and no line naming the loss, and the operator's live sessions
    /// live in `oauth:`. This is the measurement that says the guard's scope is
    /// sound — not a claim about it.
    #[test]
    fn block_creates_the_section_when_none_exists() {
        // The commonest call there is: a config that carries no `visibility:`
        // block at all. Blocking on it must add the block, not refuse.
        let doc = "keys:\n  openai: $AR_KEY_OPENAI\nproviders:\n  - id: openai\n";
        let out = append_list_entry(doc, VIS_SECTION, LIST_BLOCKED, "groq")
            .expect("a config with no section must still accept a block");
        assert!(
            out.contains("visibility:\n  blocked_providers:\n    - groq"),
            "the section was not created: {out:?}"
        );
        // And the reverse edit restores the file, which is the property the
        // whole surface is built on.
        let back = remove_list_entry(&out, VIS_SECTION, LIST_BLOCKED, "groq")
            .expect("the entry just written must be removable");
        assert_eq!(back, doc, "block then unblock must restore the file");
    }

    #[test]
    fn block_appends_inside_the_existing_list() {
        let doc = "visibility:\n  blocked_providers:\n    - groq\ncombos:\n  - id: c\n";
        let out = append_list_entry(doc, VIS_SECTION, LIST_BLOCKED, "openai")
            .expect("one entry to a list that exists");
        assert_eq!(
            out,
            "visibility:\n  blocked_providers:\n    - groq\n    - openai\ncombos:\n  - id: c\n",
            "the entry belongs after the last one already there"
        );
        // The inverse check: remove the one added, and the original is back.
        assert_eq!(
            remove_list_entry(&out, VIS_SECTION, LIST_BLOCKED, "openai"),
            Some(doc.to_owned()),
            "removing the entry added must restore the file"
        );
    }

    #[test]
    fn blocking_the_same_provider_twice_is_a_no_op_that_says_so() {
        // The regression this pins: the second write used to add a **second**
        // `visibility:` header, and before that a second entry, so blocking the
        // same provider twice produced a config the loader refuses for a
        // duplicate key — or a list counting the provider twice.
        let doc =
            "keys:\n  openai: $AR_KEY_OPENAI\nvisibility:\n  blocked_providers:\n    - groq\n";
        let once =
            append_list_entry(doc, VIS_SECTION, LIST_BLOCKED, "groq").expect("the first block");
        // An existing entry is left exactly as it is, which is what makes the
        // second `block` a no-op rather than a second row.
        assert_eq!(once, doc, "one entry already there must not be added again");
        // And the writer is idempotent on a section it has to create first: the
        // full add-then-add sequence on a config with no block at all.
        let fresh = "keys:\n  openai: $AR_KEY_OPENAI\n";
        let first = append_list_entry(fresh, VIS_SECTION, LIST_BLOCKED, "groq")
            .expect("the first block creates the section");
        let second = append_list_entry(&first, VIS_SECTION, LIST_BLOCKED, "groq")
            .expect("the second block finds it there");
        assert_eq!(
            second, first,
            "blocking the same provider twice must leave the file alone"
        );
    }

    #[test]
    fn a_list_entry_write_leaves_the_rest_of_the_document_alone() {
        // The Phase 4 invariant, re-pointed at the list shape: the operator's
        // notes, the blank separator, and the other lists survive a write.
        let doc = concat!(
            "# the operator's note about the catalog\n",
            "keys:\n  openai: $AR_KEY_OPENAI\n",
            "\n",
            "visibility:\n",
            "  blocked_providers:\n",
            "    - groq\n",
            "  hide_paid_models: false\n",
            "  model_visibility_denylist:\n",
            "    - *-search\n",
            "# the note that introduces combos\n",
            "combos:\n  - id: c\n"
        );
        let out = append_list_entry(doc, VIS_SECTION, LIST_ALLOW, "gpt-4*")
            .expect("an allow-list to add an entry to");
        assert!(out.contains("gpt-4*"), "the entry was not added: {out:?}");
        for line in [
            "# the operator's note about the catalog",
            "  blocked_providers:",
            "    - groq",
            "  hide_paid_models: false",
            "    - *-search",
            "# the note that introduces combos",
            // Spelled with its newline: the note must stay directly above the
            // `combos:` header it introduces, not above the entry a write added.
            "combos:\n  - id: c",
        ] {
            assert!(
                out.contains(line),
                "`{line}` was lost by the write: {out:?}"
            );
        }
        // The new entry lands at the end of the allow list, not of the section.
        let position = out.find("gpt-4*").expect("the entry is present");
        let deny = out.find("*-search").expect("the deny list is still there");
        assert!(
            position > deny,
            "the allow entry went below the deny list: {out:?}"
        );
    }

    #[test]
    fn a_flow_list_is_refused_rather_than_rewritten() {
        // `blocked_providers: [a, b]` is a real form a hand edit can produce. The
        // writer declines, because rewriting it would re-render lines it does not
        // own, and declining is what the caller is told about.
        let doc = "visibility:\n  blocked_providers: [groq, openai]\n";
        assert!(
            append_list_entry(doc, VIS_SECTION, LIST_BLOCKED, "tavily").is_some(),
            "the absent block-sequence list is created, matching the plan"
        );
        assert!(
            !list_has_entry(doc, VIS_SECTION, LIST_BLOCKED, "groq"),
            "a flow list must not be read as a block sequence: that is how a writer edits the wrong lines"
        );
    }

    #[test]
    fn a_new_key_lands_inside_its_own_section_not_the_last_one() {
        // The defect this pins: appending the key to the end of the document put
        // it inside whatever section happened to be last, so an allow-list ended
        // up under `mcp:` and the loader refused the whole file. A config that
        // carries `oauth:` and `mcp:` after `visibility:` is the shape that
        // found it.
        let doc = concat!(
            "keys:\n  openai: $AR_KEY_OPENAI\n",
            "visibility:\n",
            "  blocked_providers:\n",
            "    - groq\n",
            "oauth:\n",
            "  - provider: codex\n",
            "    refresh_key: codex_refresh\n",
            "mcp:\n",
            "  port: 9000\n"
        );
        let out = append_list_entry(doc, VIS_SECTION, LIST_ALLOW, "gpt-4*")
            .expect("a new key in a section that exists");
        let vis_end = out.find("oauth:").expect("the oauth section survives");
        let key = out
            .find("model_visibility_allowlist:")
            .expect("the key was added");
        assert!(
            key < vis_end,
            "the key must land inside `visibility:`, not after it: {out:?}"
        );
        // And the section that used to be last is untouched.
        assert!(
            out.contains("mcp:\n  port: 9000\n"),
            "`mcp:` was rewritten: {out:?}"
        );
    }

    #[test]
    fn a_leading_star_glob_is_quoted_so_the_config_still_loads() {
        // `- *-search` is a YAML *alias*, so an unquoted leading-star glob is
        // parsed as a reference to an anchor that does not exist and the whole
        // file fails to load. The glob matcher's syntax and YAML's collide
        // exactly here, and the writer is the only place that can see it.
        let doc = "visibility:\n  model_visibility_denylist:\n    - old\n";
        let out = append_list_entry(doc, VIS_SECTION, LIST_DENY, "*-search")
            .expect("a glob that starts with a star");
        assert!(
            out.contains("- '*-search'"),
            "the value must be quoted for the loader: {out:?}"
        );
        assert!(
            !out.contains("- *-search"),
            "an unquoted leading star is a YAML alias: {out:?}"
        );
        // The round trip still holds: reading the quoted value back gives the
        // glob, so removing it restores the file.
        assert_eq!(
            remove_list_entry(&out, VIS_SECTION, LIST_DENY, "*-search"),
            Some(doc.to_owned()),
            "the quoted entry must be removable"
        );
    }

    #[test]
    fn a_value_that_needs_no_quoting_is_written_plain() {
        // The quoting is not decoration: writing `- 'gpt-4*'` for a value that
        // does not need it would re-spell a line nobody asked to change.
        assert_eq!(yaml_entry("gpt-4*"), "gpt-4*");
        assert_eq!(yaml_entry("openai/gpt-4o"), "openai/gpt-4o");
        assert_eq!(yaml_entry("groq"), "groq");
        assert_eq!(yaml_entry("*-search"), "'*-search'");
    }

    #[test]
    fn a_splice_preserves_sections_its_renderer_has_never_heard_of() {
        // `limits:` carries a blank line **inside** itself, which is the shape
        // that broke: the old skip rule ended the section at the first blank or
        // column-0 comment, so a body separator looked like the next section's
        // boundary.
        let doc = "server:\n  host: 127.0.0.1\n  port: 20128\nkeys:\n  openai: $AR_KEY_OPENAI\nlimits:\n  default:\n    rpm: 10\n\n  # the nano tier\n  nano:\n    rpm: 5\noauth:\n  - provider: codex\n    refresh_key: codex_refresh\nvisibility:\n  defaults:\n    admin: true\nmcp:\n  port: 9000\n";

        // A `limits:` splice, whose renderer knows nothing of the rest.
        let out = splice_section(doc, "limits", "limits:\n  default:\n    rpm: 20\n");
        for section in ["server:", "keys:", "oauth:", "visibility:", "mcp:"] {
            assert!(
                out.contains(section),
                "`{section}` survived a splice: {out:?}"
            );
        }
        // And the edit itself landed, rather than being dropped as a no-op.
        assert!(out.contains("rpm: 20"), "the edit must be present: {out:?}");
        assert!(
            !out.contains("rpm: 10\n"),
            "the old value must be gone: {out:?}"
        );
        // The replaced section's own tail must go **with** it. The old rule ended
        // the skip at the blank line, so the operator's `nano:` rows survived
        // below the spliced block and the limits were half-replaced: the
        // rendered section claimed `rpm: 20` and the operator's own tier was
        // still in the file underneath. That is a *duplicated* body, not a lost
        // one, which is why a section-preservation check alone would miss it.
        assert!(
            !out.contains("# the nano tier"),
            "the replaced section's tail must not survive below it: {out:?}"
        );
        assert!(
            !out.contains("nano:\n    rpm: 5"),
            "the replaced section's tail must not survive below it: {out:?}"
        );
        // The block that documents the *next* section stays above it, which is
        // the look-ahead's other half.
        assert!(
            out.contains("oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n"),
            "the session row must be intact: {out:?}"
        );
    }

    #[test]
    fn splice_inserts_a_missing_section_before_combos() {
        let doc = "keys:\n  a: $AR_KEY_A\ncombos:\n  - id: c\n";
        let out = splice_section(doc, "custom_providers", "custom_providers:\n  - id: adhoc");
        assert!(
            out.contains("custom_providers:\n  - id: adhoc\n\ncombos:"),
            "definition must precede use, with the block's own separator: {out:?}"
        );
    }

    // The blocking finding from docs/36 B-1. A hyphen is a legal character in a
    // YAML plain key, so `api-keys:` is a real section header -- but it was not
    // recognized as one, so the skip never terminated and the writer swallowed
    // that section AND everything after it. Written as the operator reproduced
    // it: `limits clear` on a doc whose `api-keys:` follows the `limits:` it is
    // asked to remove.
    #[test]
    fn remove_section_does_not_swallow_a_hyphenated_section_after_it() {
        let doc =
            "limits:\n  default:\n    rpm: 10\napi-keys:\n  a: $AR_KEY_A\ncombos:\n  - id: c\n";
        let out = remove_section(doc, "limits");
        assert!(
            !out.contains("rpm: 10"),
            "the named section survived:\n{out}"
        );
        assert!(
            out.contains("api-keys:\n  a: $AR_KEY_A"),
            "the hyphenated section BEHIND the named one was eaten:\n{out}"
        );
        assert!(
            out.contains("combos:\n  - id: c"),
            "combos was eaten:\n{out}"
        );
    }

    /// The shape the repo's own `config.yaml` carries, and the one
    /// `remove_section` was still destroying: a section followed by a blank
    /// line and a column-0 comment block that introduces the next section. The
    /// skip ran to the next *header*, and a comment is not a header, so the
    /// eight lines documenting `combos:` went with the removed `limits:`.
    /// Measured: a `limits:` add-then-remove took the repo config from 108
    /// lines to 47.
    #[test]
    fn remove_section_keeps_the_comment_block_that_introduces_the_next_section() {
        // Every shape the insert can produce carries the blank the operator
        // wrote, and this one has the blank *below* the section too because the
        // insert wrote nothing there: the section sits between two of the
        // operator's own paragraph breaks.
        let doc = "keys:\n  openai: $AR_KEY_OPENAI\n\nlimits:\n  keys:\n    ci:\n      rpm: 5\n\n# docs for combos\n# second line\ncombos:\n  - id: c\n";
        let out = remove_section(doc, "limits");
        assert!(
            !out.contains("rpm: 5"),
            "the named section survived:\n{out}"
        );
        assert!(
            out.contains("# docs for combos\n# second line"),
            "the comment block introducing the next section was eaten:\n{out}"
        );
        // Both blanks are the operator's, so both survive: the removal deletes
        // the section's lines and nothing else, which is what makes
        // `splice_section` and this the exact inverse of one another. Taking
        // one of them with the section was the earlier defect.
        assert_eq!(
            out,
            "keys:\n  openai: $AR_KEY_OPENAI\n\n# docs for combos\n# second line\ncombos:\n  - id: c\n",
            "{out:?}"
        );
        assert!(
            out.ends_with("- id: c\n"),
            "exactly one trailing newline: {out:?}"
        );
    }

    /// A document with no trailing newline keeps having none: this repo's own
    /// `config.yaml` ends that way deliberately, and `remove_section` used to
    /// rejoin with a hardcoded `+ "\n"`, so every removal added a byte.
    #[test]
    fn remove_section_does_not_add_the_trailing_newline_the_file_never_had() {
        let doc = "keys:\n  openai: $AR_KEY_OPENAI\nlimits:\n  keys:\n    ci:\n      rpm: 5\ncombos:\n  - id: c";
        let out = remove_section(doc, "limits");
        assert!(
            out.ends_with("- id: c"),
            "the file's own ending was not changed: {out:?}"
        );
        // Mutation control: restore the hardcoded `+ "\n"` and this fails.
    }

    /// A section that ends the document takes its own trailing blank with
    /// it: the blank closes the section, not the document, and the file keeps
    /// its single terminator. This is the one case where the blank goes, and
    /// it is the case that makes the pair symmetric — a splice that writes a
    /// document-final section lands after the existing terminator, so the
    /// document's own ending is what the write added.
    #[test]
    fn remove_section_takes_the_blank_that_closes_a_document_final_section() {
        let doc = "a: 1\nlimits:\n  keys:\n    ci:\n      rpm: 5\n\n";
        let out = remove_section(doc, "limits");
        assert_eq!(out, "a: 1\n", "{out:?}");
    }

    /// The insert side of the same geometry: a section added before `combos:`
    /// goes **above** the separator that introduces `combos:`, never through
    /// it. Walking back over the blank put the new block flush against
    /// `keys:` and moved the separator to the wrong pair of blocks, which is
    /// what the repo's own config round trip caught.
    #[test]
    fn splice_section_writes_one_separator_of_its_own() {
        // No comment block: the blank the operator wrote stays exactly
        // where it was, directly above `combos:`, and the block lands below it.
        let plain = "keys:\n  openai: $AR_KEY_OPENAI\n\ncombos:\n  - id: c\n";
        let out = splice_section(plain, "limits", "limits:\n  keys:\n    ci:\n      rpm: 5\n");
        assert!(
            out.starts_with("keys:\n  openai: $AR_KEY_OPENAI\n\nlimits:\n"),
            "the operator's separator stays above the inserted block: {out:?}"
        );
        assert!(
            out.ends_with("      rpm: 5\ncombos:\n  - id: c\n"),
            "and no blank was written, because one is already there: {out:?}"
        );

        // With a comment block: the block lands below the blank that
        // introduces the comments, so the comments keep their own separator.
        let commented = "keys:\n  openai: $AR_KEY_OPENAI\n\n# docs for combos\ncombos:\n\n";
        let out = splice_section(
            commented,
            "limits",
            "limits:\n  keys:\n    ci:\n      rpm: 5\n",
        );
        assert!(
            out.contains("$AR_KEY_OPENAI\n\n# docs for combos\nlimits:"),
            "the block lands below the comment block, not through it: {out:?}"
        );
        assert!(
            out.contains("      rpm: 5\n\ncombos:\n\n"),
            "and one blank was written, because the comment block left none: {out:?}"
        );

        // Round trip, both shapes.
        for doc in [plain, commented] {
            let added = splice_section(doc, "limits", "limits:\n  keys:\n    ci:\n      rpm: 5\n");
            assert_eq!(
                remove_section(&added, "limits"),
                doc,
                "add then remove restores the file byte for byte: {added:?}"
            );
        }
    }

    /// The shape `providers add-custom` writes, and the reason the insert
    /// lands at the anchor rather than walking back over the blank: two
    /// splices in one verb, the second landing where the first one had just
    /// put a separator. Walking back over it (inserting *above*) left `keys:` /
    /// row / `custom_providers:` / `combos:` with no paragraph break at all,
    /// which the integration test caught and the repo's own config did not —
    /// that one carries a comment block, so the blank was absorbed into it.
    #[test]
    fn two_splices_in_one_verb_keep_the_operators_separator_above_combos() {
        let doc = "keys:\n  openai: $AR_KEY_OPENAI\n\ncombos:\n  - id: c\n";
        // `add-custom` appends the `keys:` ref to the `keys:` section, then
        // puts `custom_providers:` above `combos:` — the two writes the CLI
        // makes, on the document each time rather than the parsed config.
        let after_first = with_keys_row(doc, "my-gateway", "AR_KEY_MY_GATEWAY");
        let after_second = splice_section(
            &after_first,
            "custom_providers",
            "custom_providers:\n  - id: my-gateway\n",
        );
        // The blank the operator wrote is still where it was: above the block
        // the verb inserted, not above `combos:` where it used to sit.
        assert!(
            after_second.starts_with(
                "keys:\n  openai: $AR_KEY_OPENAI\n  my-gateway: $AR_KEY_MY_GATEWAY\n\n"
            ),
            "the operator's separator moved: {after_second:?}"
        );
        // The block carries no separator of its own, so `combos:` sits
        // directly under it — which is what makes the removal an exact
        // inverse rather than a judgement call about whose blank it was.
        assert!(
            after_second.contains("- id: my-gateway\ncombos:\n"),
            "no blank was written, because the file already had one: {after_second:?}"
        );
        // And the pair undoes itself: both sections go, the operator's file
        // comes back byte for byte.
        let back = remove_section(&after_second, "custom_providers");
        let back = remove_section(&back, "keys");
        assert_eq!(
            splice_section(&back, "keys", "keys:\n  openai: $AR_KEY_OPENAI\n"),
            doc,
            "the whole verb does not round trip: {after_second:?}"
        );
    }

    /// [`with_key_binding`] reads and writes the document, so a unit test on
    /// the two-write shape has to call it rather than a splicer it does not
    /// use. It is `pub(crate)`, so a private re-export here is not a back door
    /// into anything the crate did not already expose.
    fn with_keys_row(doc: &str, name: &str, var: &str) -> String {
        with_key_binding(doc, name, var).expect("the fixture binds to AR_KEY_*")
    }

    /// Six geometries the four tests above do not reach, each of which
    /// exercises a different branch or boundary: no `combos:` at all (the
    /// append branch, not the anchor branch), the anchor at line zero, two
    /// stacked comment blocks, a body ending in a real blank line, a comment
    /// block at EOF, and a comment with no blank above the anchor.
    ///
    /// Every one round-trips byte for byte in both directions. They are pinned
    /// here because the two shipped tests reach only two of these branches and
    /// the branches are where the ownership rule is actually decided.
    #[test]
    fn the_separator_rule_holds_on_six_geometries_the_shipped_tests_miss() {
        let block = "limits:\n  keys:\n    ci:\n      rpm: 5\n";
        let docs = [
            // No `combos:` at all: the section is appended, not anchored.
            "keys:\n  a: $AR_KEY_A\n",
            // `combos:` is line zero, so nothing precedes the anchor.
            "combos:\n  - id: c\n",
            // Two stacked comment blocks above the anchor.
            "keys:\n  a: $AR_KEY_A\n\n# block one\n# more one\n\n# block two\ncombos:\n  - id: c\n",
            // A body that ends with a real blank line before the anchor.
            "keys:\n  a: $AR_KEY_A\n\n\ncombos:\n  - id: c\n",
            // A comment block at EOF with no `combos:` after it.
            "keys:\n  a: $AR_KEY_A\n\n# tail notes\ncombos:\n\n",
            // A comment with no blank above the anchor.
            "keys:\n  a: $AR_KEY_A\n# note\ncombos:\n  - id: c\n",
        ];
        for doc in docs {
            let added = splice_section(doc, "limits", block);
            assert_eq!(
                remove_section(&added, "limits"),
                doc,
                "add then remove must restore byte for byte: {added:?}"
            );
            // And the reverse direction, which is the residual the plan records
            // for `hide-paid`/`unblock`/`continue-on-cut`: a remove of a section
            // the file never had must change nothing at all.
            assert_eq!(
                remove_section(doc, "limits"),
                doc,
                "a removal of an absent section must not re-spell the file"
            );
        }
    }

    /// A CRLF document rejoins with CRLF, because `str::lines` strips the
    /// `\r` and a `\n`-only rejoin rewrites every line ending in the file.
    ///
    /// Measured before the fix: a 164-byte CRLF config came back **109 bytes
    /// with zero `\r`** after one `combo add`, and the add-then-remove pair
    /// never restored it. `serde_yaml` parses either style, so the loader never
    /// warned — the bytes changed silently. YAML permits one style per
    /// document, so the answer is read from the document.
    #[test]
    fn a_crlf_document_rejoins_with_crlf() {
        let crlf = "keys:\r\n  a: $AR_KEY_A\r\n\r\ncombos:\r\n  - id: c\r\n    strategy: priority\r\n    targets:\r\n      - a/m\r\n";
        // Rendered the way the verb renders it: `render_combo_block` writes the
        // row at a list entry's own depth, which is what `append_combo_row`
        // expects to place.
        let rendered = "- id: new\n  strategy: priority\n  targets:\n    - a/m\n";
        let row = indent_row(rendered, 2);
        let added = append_combo_row(crlf, &row);
        assert!(
            added.contains("\r\n"),
            "the CRLF endings were rewritten: {added:?}"
        );
        assert_eq!(
            remove_combo_row(&added, "new").as_deref(),
            Some(crlf),
            "add then remove restores a CRLF file byte for byte"
        );
        // And an LF document gains no CR from the same rule.
        let lf = crlf.replace("\r\n", "\n");
        let lf_added = append_combo_row(&lf, &row);
        assert!(
            !lf_added.contains('\r'),
            "an LF document must not gain a CR: {lf_added:?}"
        );
    }

    /// The remove direction on a file that has nothing to remove.
    ///
    /// `false` is the value `ModelVisibility` omits, so the file's canonical
    /// spelling of "not hiding paid models" is the key's **absence** — and a
    /// config with no `visibility:` section is already in that state. Measured
    /// before the fix: a 151-byte config grew to 189 carrying
    /// `hide_paid_models: false`, written by the very verb whose direction is
    /// "take it out", and no later write removes it because the false direction
    /// stops once it finds nothing. A verb must not create the thing it removes.
    #[test]
    fn hide_paid_false_creates_no_section_and_writes_no_key() {
        let base = "keys:\n  openai: $AR_KEY_OPENAI\ncombos:\n  - id: c\n";
        let out = set_section_scalar(base, "visibility", "hide_paid_models", false);
        assert_eq!(
            out, base,
            "a false on a config with no visibility: section changed the file: {out:?}"
        );
        // And with a section that holds another key, false still writes nothing.
        let with_section = "keys:\n  openai: $AR_KEY_OPENAI\nvisibility:\n  blocked_providers:\n    - groq\ncombos:\n  - id: c\n";
        assert_eq!(
            set_section_scalar(with_section, "visibility", "hide_paid_models", false),
            with_section,
            "a false with a sibling key wrote a key it should not"
        );
        // The true direction still works, and the pair still round-trips.
        let on = set_section_scalar(base, "visibility", "hide_paid_models", true);
        assert_ne!(on, base, "the true write must still write");
        assert_eq!(
            set_section_scalar(&on, "visibility", "hide_paid_models", false),
            base,
            "the pair still round-trips byte for byte"
        );
    }

    #[test]
    fn splice_section_does_not_swallow_a_hyphenated_section_after_it() {
        let doc =
            "limits:\n  default:\n    rpm: 10\napi-keys:\n  a: $AR_KEY_A\ncombos:\n  - id: c\n";
        let out = splice_section(doc, "limits", "limits:\n  default:\n    rpm: 20\n");
        assert!(out.contains("rpm: 20"), "not spliced:\n{out}");
        assert!(
            out.contains("api-keys:\n  a: $AR_KEY_A"),
            "the hyphenated section BEHIND the named one was eaten:\n{out}"
        );
        assert!(
            out.contains("combos:\n  - id: c"),
            "combos was eaten:\n{out}"
        );
    }

    // A blank line between the replaced section and what follows it must
    // survive as ONE blank line.
    //
    // The renderers emit a trailing newline (`render_providers_section` ends
    // with `"  key: openai\n"`), so pushing the block as-is and then joining
    // with `\n` puts TWO blank lines after the section, and every write of the
    // same verb added one more. Found by running `providers remove` against the
    // repo's real config and diffing the round trip.
    #[test]
    fn splice_section_does_not_add_a_blank_line_after_the_replaced_section() {
        let doc = "providers:\n  - id: openai\n    key: openai\n\n# a comment\nkeys:\n  openai: $AR_KEY_OPENAI\n";
        let out = splice_section(
            doc,
            "providers",
            "providers:\n  - id: openai\n    key: openai\n",
        );
        assert_eq!(
            out, doc,
            "re-splicing the same content must not grow the file: {out:?}"
        );

        // And the same, when the added row is a real change: the blank line that
        // separated the section from the comment must still be one line.
        let grown = splice_section(
            doc,
            "providers",
            "providers:\n  - id: deepseek\n    key: deepseek\n  - id: openai\n    key: openai\n",
        );
        assert_eq!(
            grown.matches("\n\n").count(),
            doc.matches("\n\n").count(),
            "one blank line stayed one blank line: {grown:?}"
        );
    }

    // The same bug through the real command, which is how the operator hit it.
    // The ORDERING is the part that matters, and the part that is easy to get
    // wrong when writing the test: the damage only happens when the hyphenated
    // section sits AFTER the section being replaced, because that is the only
    // layout where the skip has to terminate on the hyphenated header. Two
    // earlier attempts at this test put `api-keys:` BEFORE `limits:` and passed
    // against the buggy code -- precisely the false negative a regression test
    // must be built to avoid, so the layout is asserted explicitly here.
    #[test]
    fn limits_set_keeps_a_hyphenated_section_that_follows_limits() {
        let dir = tmpdir("limits-hyphen-sibling");
        let cfg = dir.join("config.yaml");
        // NOTE the order: `limits_cli` WRITES its own fixture to this same path,
        // so it must be called first and the fixture written after it. Calling
        // them the other way round silently discards the doc under test and the
        // test passes/fails on the helper's content instead of the bug's.
        let cli = limits_cli(&dir);
        std::fs::write(
            &cfg,
            "server:\n  host: 127.0.0.1\n  port: 20129\nkeys:\n  openai: dummy\nlimits:\n  keys:\n    prod:\n      rpm: 600\napi-keys:\n  a: dummy\ncombos:\n- id: probe\n  strategy: priority\n  targets:\n  - openai/gpt-4o-mini\n  context_length: 128000\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            before.contains("api-keys:\n  a: dummy"),
            "the fixture lost its hyphenated section before the command ran"
        );

        limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("prod".to_owned()),
                rpm: Some(700),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: None,
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();
        let after = std::fs::read_to_string(&cfg).unwrap();

        assert!(
            after.contains("api-keys:\n  a: dummy"),
            "the hyphenated section BEHIND `limits:` was silently deleted:\n{after}"
        );
        assert!(
            after.contains("combos:\n- id: probe"),
            "`combos:` was deleted too:\n{after}"
        );
        assert!(after.contains("rpm: 700"), "the row did not land:\n{after}");
        assert!(
            after.len() >= before.len(),
            "the file shrank ({} -> {}); content was dropped",
            before.len(),
            after.len()
        );
    }

    #[test]
    fn finds_every_ar_key_reference() {
        let refs = ar_key_refs("  a: $AR_KEY_NVIDIA\n  b: $AR_KEY_OPENAI_CHAT_ABC\nplain\n");
        let want: BTreeSet<String> = ["AR_KEY_NVIDIA", "AR_KEY_OPENAI_CHAT_ABC"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(refs, want);
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
        // Same grammar `aroute serve` routes with: longest registered prefix wins.
        assert_eq!(target_provider("nvidia/moonshotai/kimi-k3"), "nvidia");
        assert_eq!(
            target_provider("aihorde/aphrodite/TheDrummer/Cydonia-24B-v4.3"),
            "aihorde"
        );
    }

    #[test]
    fn flags_unknown_field_with_valid_list() {
        let e = columns(&["nope".to_owned()], &PROVIDER_COLUMNS, "providers")
            .unwrap_err()
            .to_string();
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
                weights: std::collections::BTreeMap::new(),
                pool: vec![],
                steps: vec![],
                compression: None,
                judge_model: None,
                context_length: None,
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
        let checks = findings(
            &cfg,
            "config.yaml",
            &StoreProbe::Absent(PathBuf::from("credentials.db")),
        );
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
        let checks = findings(
            &cfg,
            "config.yaml",
            &StoreProbe::Absent(PathBuf::from("credentials.db")),
        );
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
        let checks = findings(
            &cfg,
            "config.yaml",
            &StoreProbe::Absent(PathBuf::from("credentials.db")),
        );
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
        let checks = findings(
            &cfg,
            "config.yaml",
            &StoreProbe::Absent(PathBuf::from("credentials.db")),
        );
        let row = checks
            .iter()
            .find(|r| r[0] == "target/openai/gpt-5.4-nope")
            .expect("the target row");
        assert_eq!(row[1], "fail");
        assert!(row[2].contains("model not in registry"), "{row:?}");
        assert!(
            row[2].contains("aroute import"),
            "the help names the fix: {row:?}"
        );
    }

    #[test]
    fn passes_check_when_model_half_is_known() {
        let cfg = Config::parse(
            "keys:\n  k: v\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: cost-optimized\n    targets:\n      - openai/gpt-5.4-nano\n",
            |_| Ok(Some("v".to_owned())),
        )
        .unwrap();
        let checks = findings(
            &cfg,
            "config.yaml",
            &StoreProbe::Absent(PathBuf::from("credentials.db")),
        );
        assert!(!checks.iter().any(|r| r[1] == "fail"), "{checks:?}");
    }

    #[test]
    fn lists_valid_flags_for_named_subcommand() {
        let hint = unknown_flag_hint(
            &[
                OsString::from("--config"),
                OsString::from("c.yaml"),
                OsString::from("models"),
            ],
            "--nope",
        );
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
        let first = rows
            .iter()
            .find(|r| r[0] == "openai/gpt-5.4")
            .expect("the shared target");
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
        let row = checks
            .iter()
            .find(|r| r[0] == "target/local-gateway/some-model")
            .expect("the target row");
        assert_eq!(row[1], "ok", "{row:?}");
    }

    #[test]
    fn fails_check_when_a_custom_providers_base_url_has_no_scheme() {
        let cfg = custom_config("api.example.invalid/v1", "local-gateway");
        let checks = findings(&cfg, "config.yaml", &no_store());
        let row = checks
            .iter()
            .find(|r| r[0] == "custom/local-gateway")
            .expect("the custom row");
        assert_eq!(row[1], "fail", "{row:?}");
        assert!(row[2].contains("not an http(s) URL"), "{row:?}");
    }

    #[test]
    fn fails_check_when_a_custom_provider_id_collides_with_the_catalog() {
        let cfg = custom_config("https://api.example.invalid/v1", "openai");
        let checks = findings(&cfg, "config.yaml", &no_store());
        let row = checks
            .iter()
            .find(|r| r[0] == "custom")
            .expect("the collision row");
        assert_eq!(row[1], "fail", "{row:?}");
        assert!(
            row[2].contains("openai"),
            "the row names the colliding id: {row:?}"
        );
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
    /// A session for a provider this build has no `OAuthKind` for.
    ///
    /// `kiro` rather than `kilocode`: both are `auth_kind: oauth` in the catalog,
    /// but kilocode has an executor of its own, so it exercises the *armed* path.
    const OAUTH_NO_EXECUTOR: &str = concat!(
        "keys:\n  kiro: $KIRO_ACCESS\nproviders:\n  - id: kiro\n    key: kiro\n",
        "oauth:\n  - provider: kiro\n    refresh_key: kiro_refresh\n",
        "    token_url: https://auth.example.invalid/token\n",
        "    authorization_url: https://auth.example.invalid/authorize\n",
        "combos:\n  - id: c\n    strategy: priority\n    targets:\n      - kiro/kilo-1\n",
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
        assert_eq!(
            row(&rows, "oauth/codex")[1],
            "ok",
            "it still dispatches: {rows:?}"
        );
        let detail = &row(&rows, "auth/codex")[2];
        assert!(
            detail.contains("aroute auth login --provider codex"),
            "the row names the fix: {detail}"
        );
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
        let cfg = Config::parse(OAUTH_LOGINABLE, |_| Ok(Some(String::new())))
            .expect("the fixture parses");
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
        assert!(
            detail.contains("oauth:"),
            "the detail names the fix: {detail}"
        );
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
        assert!(
            detail.contains(listed),
            "the detail quotes the list: {detail}"
        );
    }

    #[test]
    fn fails_an_oauth_provider_this_build_has_no_executor_for() {
        // `kiro` is oauth-kind with no `OAuthKind`, so it must stay loudly
        // unroutable. `kilocode` was this test's example until it gained an
        // executor; the guard itself is unchanged.
        let rows = checks(OAUTH_NO_EXECUTOR, &probe(&["kiro", "kiro_refresh"]));
        let found = row(&rows, "oauth/kiro");
        assert_eq!(found[1], "fail", "{rows:?}");
        assert!(found[2].contains("no executor"), "{found:?}");
    }

    #[test]
    fn reports_the_terminal_status_list_the_executor_classifies_against() {
        let rows = checks(ONE_PROVIDER, &no_store());
        let detail = &row(&rows, "terminal-status")[2];
        assert!(
            detail.contains(&ar_server::TERMINAL_REFRESH_STATUS.len().to_string()),
            "{detail}"
        );
    }

    #[test]
    fn spells_a_carve_out_only_reason_the_shared_table_does_not_carry() {
        // F-MED-2: `terminal_check_constraint` admits (401, "invalid_client") via
        // the carve-out union, so a `terminal_reason` that read only the shared
        // table could not name a reason the store would accept — and would fall
        // back to the literal at the call site, reintroducing the copy this exists
        // to delete.
        assert_eq!(
            terminal_reason(401, "invalid_client"),
            Some("invalid_client")
        );
    }

    #[test]
    fn spells_nothing_for_a_reason_neither_table_carries() {
        // The `None` the call sites fall back on. A status no table lists at all,
        // so this holds whichever rows the two tables carry.
        assert_eq!(terminal_reason(418, "teapot"), None);
    }

    fn probe(names: &[&str]) -> StoreProbe<'static> {
        StoreProbe::Open {
            path: PathBuf::from("credentials.db"),
            names: names.iter().map(|n| (*n).to_owned()).collect(),
        }
    }

    fn checks(yaml: &str, store: &StoreProbe<'_>) -> Vec<Row> {
        let cfg = Config::parse(yaml, |name| Ok(Some(format!("secret-{name}"))))
            .expect("the fixture parses");
        findings(&cfg, "config.yaml", store)
    }

    const ONE_PROVIDER: &str = "keys:\n  k: $AR_KEY\nproviders:\n  - id: openai\n    key: k\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n";

    fn row<'a>(rows: &'a [Row], check: &str) -> &'a Row {
        rows.iter()
            .find(|r| r[0] == check)
            .unwrap_or_else(|| panic!("no {check} row in {rows:?}"))
    }

    #[test]
    fn reports_no_store_when_the_file_is_absent() {
        let absent = StoreProbe::Absent(PathBuf::from("credentials.db"));
        let (status, detail) = absent.row();
        assert_eq!(
            status, "skip",
            "an env-only install is supported, not broken"
        );
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
        assert!(
            detail.contains("AR_MASTER_KEY"),
            "the reason is the fix: {detail}"
        );
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
        assert!(
            row(&rows, "key/k")[2].contains("from store"),
            "{:?}",
            row(&rows, "key/k")
        );
    }

    #[test]
    fn names_the_config_as_the_source_when_the_store_has_no_row() {
        let rows = checks(ONE_PROVIDER, &probe(&["other"]));
        assert!(
            row(&rows, "key/k")[2].contains("from keys:"),
            "{:?}",
            row(&rows, "key/k")
        );
    }

    #[test]
    fn fails_a_key_check_when_an_apikey_provider_resolves_to_nothing() {
        // The `store` first, `keys:` second order means an empty entry is a real
        // state: the store is shadowed by an empty variable, or neither exists.
        // `checks` expands to a non-empty value, so this one parses its own.
        let cfg =
            Config::parse(ONE_PROVIDER, |_| Ok(Some(String::new()))).expect("the fixture parses");
        let rows = findings(&cfg, "config.yaml", &probe(&[]));
        assert_eq!(row(&rows, "key/k")[1], "fail", "{rows:?}");
        assert!(
            row(&rows, "key/k")[2].contains("resolved to nothing"),
            "{:?}",
            row(&rows, "key/k")
        );
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
            assert!(
                !r[2].contains("secret-AR_KEY"),
                "a check row leaked a value: {r:?}"
            );
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
        assert!(
            detail.contains("snapshot") && detail.contains("d old"),
            "{detail}"
        );
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
        assert!(
            !row(&rows, "registry")[2].contains("aroute import"),
            "{rows:?}"
        );
    }

    #[test]
    fn a_stale_registry_row_warns_and_names_the_import_fix() {
        // Age is the build's, so the test drives `registry_row` at a `now` far
        // enough past the stamp rather than waiting seven days.
        let row = registry_row_at(
            3,
            ar_registry::discovery::unix_now() + (ar_registry::SNAPSHOT_TTL_DAYS + 1) * 86_400,
        );
        assert_eq!(row[1], "warn", "{row:?}");
        assert!(row[2].contains("aroute import --from omniroute"), "{row:?}");
        assert!(
            row[2].contains('3'),
            "it counts the models it cannot route: {row:?}"
        );
    }

    #[test]
    fn puts_the_store_beside_the_config_file_by_default() {
        assert_eq!(
            store_path(Path::new("config.yaml")),
            PathBuf::from("credentials.db")
        );
        assert_eq!(
            store_path(Path::new("/etc/ar/config.yaml")),
            PathBuf::from("/etc/ar/credentials.db")
        );
    }

    /// A temp dir helper for the backup tests; `unwrap` is fine here.
    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ar-backup-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("temp dir");
        p
    }

    #[test]
    fn keeps_three_prior_versions_and_evicts_the_fourth_oldest() {
        let dir = tmpdir("depth");
        let path = dir.join("config.yaml");

        // Four writes plus a first one that has nothing to back up.
        for i in 0..5 {
            write_file(&path, &format!("v{i}")).expect("write");
        }

        // The live file is the newest; .1 the one before it, and so on.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v4");
        assert_eq!(
            std::fs::read_to_string(dir.join("config.yaml.1")).unwrap(),
            "v3"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("config.yaml.2")).unwrap(),
            "v2"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("config.yaml.3")).unwrap(),
            "v1"
        );
        // v0 fell off the end rather than being kept as a fourth slot.
        assert!(!dir.join("config.yaml.4").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_first_write_with_no_existing_file_backs_up_nothing() {
        let dir = tmpdir("first");
        let path = dir.join("registry.json");

        write_file(&path, "v0").expect("write");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v0");
        assert!(!dir.join("registry.json.1").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The seam the whole phase rests on: a token minted by `aroute keys`
    /// must pass the gate `aroute serve` builds from the same stored master.
    /// Structural — `mint_from` and `AuthGate::new` both build `Tokens::new`
    /// from the master — but stated as a test so the pairing survives a
    /// refactor of either side.
    #[test]
    fn a_minted_token_passes_the_gate_serve_builds() {
        let master = ar_keys::Secret::generate();
        let issued = mint_from(&master, "prod-deploy").expect("token mints");
        let gate = ar_server::keys::AuthGate::new(master.as_bytes()).expect("gate builds");
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", issued.access)
                .parse()
                .expect("header value"),
        );
        let key_id = gate
            .authorize(
                (&headers, "/v1/chat/completions"),
                ar_server::config::AuthMode::Required,
            )
            .expect("token verifies");
        assert_eq!(
            key_id.as_deref(),
            Some("prod-deploy"),
            "the key id `mint_from` stamped is the one the ledger and limits key by"
        );
    }

    /// `aroute serve`'s boot read: the row `arm-gate` writes is the master the
    /// gate verifies against, via the same `gate_master` helper.
    #[test]
    fn the_gate_master_round_trips_through_the_store() {
        let store = ar_keys::CredentialStore::open_in_memory(&ar_keys::Secret::generate())
            .expect("in-memory store");
        assert!(gate_master(&store).unwrap().is_none(), "never armed");
        let master = ar_keys::Secret::generate();
        store
            .insert(GATE_PROVIDER, GATE_ROW, &master)
            .expect("row stores");
        let read = gate_master(&store).unwrap().expect("row reads back");
        assert_eq!(read.as_bytes(), master.as_bytes());
    }

    // B-5's second half, at the decision level rather than through the command:
    // the rule is what the guard function answers, and testing it here avoids
    // the env-var dance a real store needs. The line-level test that the store
    // actually refuses is `arm_gate_refuses_a_second_arming_without_force` below.
    #[test]
    fn arming_over_a_live_gate_is_refused_without_force_and_allowed_with_it() {
        let store = ar_keys::CredentialStore::open_in_memory(&ar_keys::Secret::generate())
            .expect("in-memory store");
        assert!(
            gate_overwrite_allowed(&store, false),
            "a fresh gate may be armed"
        );

        store
            .insert(GATE_PROVIDER, GATE_ROW, &ar_keys::Secret::generate())
            .expect("row stores");

        assert!(
            !gate_overwrite_allowed(&store, false),
            "re-arming without --force must be refused, or the old master is replaced silently"
        );
        assert!(
            gate_overwrite_allowed(&store, true),
            "--force is the explicit path, and must still work"
        );
    }

    #[test]
    fn the_doctor_gate_check_fails_when_the_row_will_not_decrypt() {
        // The bug this closes: the check read the row's NAME and called that
        // "armed", so a store whose gate row was undecryptable listed as
        // `gate,ok,armed` while the server was booting with "serving
        // ungated" in its own log. Two surfaces, two answers.
        let dir = tmpdir("doctor-gate");
        let path = dir.join("credentials.db");
        // The store is written with one master...
        let store =
            ar_keys::CredentialStore::open_with_material(&path, &ar_keys::Secret::generate())
                .expect("store opens");
        store
            .insert(GATE_PROVIDER, GATE_ROW, &ar_keys::Secret::generate())
            .expect("row stores");
        drop(store);
        // ...and read back with a DIFFERENT one, which is the real situation:
        // the operator's $AR_MASTER_KEY no longer matches the store.
        let other = ar_keys::Secret::generate();
        let store =
            ar_keys::CredentialStore::open_with_material(&path, &other).expect("store reopens");
        let probe = StoreProbe::Live {
            store: &store,
            path,
        };
        let rows = findings(&ar_config::Config::default(), "doctor", &probe);
        let gate = rows
            .iter()
            .find(|r| r.first().is_some_and(|c| c == "gate"))
            .expect("a gate row");
        assert_eq!(
            gate.get(1).map(String::as_str),
            Some("fail"),
            "an undecryptable gate row must FAIL the check: {gate:?}"
        );
        assert!(
            gate.get(2).is_some_and(|d| d.contains("UNGATED")),
            "the detail must name the state the server is in: {gate:?}"
        );
    }

    #[test]
    fn the_doctor_gate_check_passes_when_the_row_decrypts() {
        // The other side of the same check: a row that opens is `ok`, not
        // `fail`. A check that always failed would be as useless as one that
        // never did.
        let dir = tmpdir("doctor-gate-ok");
        let path = dir.join("credentials.db");
        let master = ar_keys::Secret::generate();
        let store =
            ar_keys::CredentialStore::open_with_material(&path, &master).expect("store opens");
        store
            .insert(GATE_PROVIDER, GATE_ROW, &ar_keys::Secret::generate())
            .expect("row stores");
        drop(store);
        let store =
            ar_keys::CredentialStore::open_with_material(&path, &master).expect("store reopens");
        let probe = StoreProbe::Live {
            store: &store,
            path,
        };
        let rows = findings(&ar_config::Config::default(), "doctor", &probe);
        let gate = rows
            .iter()
            .find(|r| r.first().is_some_and(|c| c == "gate"))
            .expect("a gate row");
        assert_eq!(gate.get(1).map(String::as_str), Some("ok"), "{gate:?}");
    }

    #[test]
    fn hex_decodes_round_trip_and_refuses_garbage() {
        assert_eq!(decode_hex("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(decode_hex("00FF10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(decode_hex("0"), None, "odd length");
        assert_eq!(decode_hex("zz"), None, "not hex");
    }

    /// The config the limits verbs edit: minimal, valid, and free of `$VAR`s
    /// so `load` resolves everything from the file alone.
    fn limits_cli(dir: &std::path::Path) -> Cli {
        let config = dir.join("config.yaml");
        std::fs::write(
            &config,
            "keys:\n  local: dummy\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n      - openai/gpt-5.4-mini\n",
        )
        .unwrap();
        Cli {
            config,
            command: None,
            system: false,
        }
    }

    #[test]
    fn limits_set_merges_arms_and_splices_the_section() {
        let dir = tmpdir("limits-set");
        let cli = limits_cli(&dir);
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("prod".to_owned()),
                rpm: Some(600),
                usd_micros: None,
                tokens: Some(1000),
                refuse_unpriced: None,
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();
        let cfg = load(&cli).unwrap();
        let row = cfg.limits.for_key("prod");
        assert_eq!(row.rpm, Some(600));
        assert_eq!(row.tokens, Some(1000));
        assert!(!row.refuse_unpriced);

        // The merge is per arm: the second set names a different arm and the
        // first must survive it — `set --rpm` cannot quietly drop a ceiling
        // the row already carried.
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("prod".to_owned()),
                rpm: None,
                usd_micros: Some(5_000_000),
                tokens: None,
                refuse_unpriced: Some(true),
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();
        let cfg = load(&cli).unwrap();
        let row = cfg.limits.for_key("prod");
        assert_eq!(row.rpm, Some(600), "an arm not passed stays as it was");
        assert_eq!(row.tokens, Some(1000));
        assert_eq!(row.usd_micros, Some(5_000_000));
        assert!(row.refuse_unpriced);

        // The default row, without --key.
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: None,
                rpm: Some(60),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: None,
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();
        assert_eq!(load(&cli).unwrap().limits.default.rpm, Some(60));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limits_set_scopes_a_row_to_one_combo_and_clears_it_with_an_empty_value() {
        let dir = tmpdir("limits-scope");
        let cli = limits_cli(&dir);
        let set = |rpm: Option<u32>, model: Option<&str>| SetLimitsArgs {
            key: Some("ci".to_owned()),
            rpm,
            usd_micros: None,
            tokens: None,
            refuse_unpriced: None,
            model: model.map(str::to_owned),
            model_tokens: Vec::new(),
            spend_window: None,
        };
        limits_set(&cli, &set(Some(60), Some("c"))).unwrap();
        assert_eq!(
            load(&cli).unwrap().limits.keys["ci"].model.as_deref(),
            Some("c")
        );

        // A name with no `/` must be a combo that exists, and a refused scope
        // writes nothing, so the row keeps the scope it had.
        assert!(limits_set(&cli, &set(None, Some("nope"))).is_err());
        assert_eq!(
            load(&cli).unwrap().limits.keys["ci"].model.as_deref(),
            Some("c")
        );

        // A `provider/model` name needs no combo, and `""` clears the scope.
        limits_set(&cli, &set(None, Some("openai/gpt-5.4"))).unwrap();
        assert_eq!(
            load(&cli).unwrap().limits.keys["ci"].model.as_deref(),
            Some("openai/gpt-5.4")
        );
        limits_set(&cli, &set(None, Some(""))).unwrap();
        let row = &load(&cli).unwrap().limits.keys["ci"];
        assert_eq!(row.model, None);
        assert_eq!(row.rpm, Some(60), "clearing the scope keeps the arms");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limits_set_refuses_a_scope_alone_on_a_row_with_no_arms() {
        let dir = tmpdir("limits-scope-alone");
        let cli = limits_cli(&dir);
        let scope_only = SetLimitsArgs {
            key: Some("fresh".to_owned()),
            rpm: None,
            usd_micros: None,
            tokens: None,
            refuse_unpriced: None,
            model: Some("c".to_owned()),
            model_tokens: Vec::new(),
            spend_window: None,
        };
        assert!(
            limits_set(&cli, &scope_only).is_err(),
            "a scope limits nothing by itself"
        );
        assert!(!load(&cli).unwrap().limits.keys.contains_key("fresh"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limits_set_refuses_a_zero_arm_and_an_armless_call() {
        let dir = tmpdir("limits-zero");
        let cli = limits_cli(&dir);
        let zero = limits_set(
            &cli,
            &SetLimitsArgs {
                key: None,
                rpm: Some(0),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: None,
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        );
        assert!(
            zero.is_err(),
            "a writer must not write what the parser refuses"
        );
        let empty = limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("k".to_owned()),
                rpm: None,
                usd_micros: None,
                tokens: None,
                refuse_unpriced: None,
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        );
        assert!(empty.is_err(), "nothing to set");
        // Neither refusal wrote anything.
        assert!(load(&cli).unwrap().limits.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limits_set_writes_a_per_target_ceiling_and_a_window() {
        let dir = tmpdir("limits-model-tokens");
        let cli = limits_cli(&dir);
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("prod".to_owned()),
                rpm: None,
                usd_micros: None,
                tokens: Some(5_000_000_000),
                refuse_unpriced: None,
                model: None,
                model_tokens: vec!["openai/gpt-5.4=500000000".to_owned()],
                spend_window: Some("24h".to_owned()),
            },
        )
        .unwrap();
        let row = &load(&cli).unwrap().limits.keys["prod"];
        assert_eq!(row.model_tokens.get("openai/gpt-5.4"), Some(&500_000_000));
        assert_eq!(row.spend_window.as_deref(), Some("24h"));
        assert_eq!(
            row.tokens,
            Some(5_000_000_000),
            "the key-wide arm survived the per-target one"
        );
        // The second set names a DIFFERENT target, and the first must survive
        // it — a per-target merge is per target, not per row.
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("prod".to_owned()),
                rpm: None,
                usd_micros: None,
                tokens: None,
                refuse_unpriced: None,
                model: None,
                model_tokens: vec!["openai/gpt-5.4-mini=1000000".to_owned()],
                spend_window: None,
            },
        )
        .unwrap();
        let row = &load(&cli).unwrap().limits.keys["prod"];
        assert_eq!(row.model_tokens.len(), 2, "both targets are carried");
        assert_eq!(
            row.spend_window.as_deref(),
            Some("24h"),
            "the window survived"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limits_set_refuses_a_per_target_ceiling_that_names_no_target() {
        let dir = tmpdir("limits-model-tokens-bad");
        let cli = limits_cli(&dir);
        let err = limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("prod".to_owned()),
                rpm: None,
                usd_micros: None,
                tokens: None,
                refuse_unpriced: None,
                model: None,
                model_tokens: vec!["nvidia/nope-not-a-model=100".to_owned()],
                spend_window: None,
            },
        );
        assert!(
            err.is_err(),
            "a ceiling for a target nothing routes to is a ceiling that never applies"
        );
        assert!(
            load(&cli).unwrap().limits.keys.is_empty(),
            "nothing was written"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limits_set_refuses_a_malformed_per_target_ceiling() {
        let dir = tmpdir("limits-model-tokens-malformed");
        let cli = limits_cli(&dir);
        for bad in ["no-equals-sign", "openai/gpt-5.4=notanumber", "=100"] {
            let err = limits_set(
                &cli,
                &SetLimitsArgs {
                    key: Some("prod".to_owned()),
                    rpm: None,
                    usd_micros: None,
                    tokens: None,
                    refuse_unpriced: None,
                    model: None,
                    model_tokens: vec![bad.to_owned()],
                    spend_window: None,
                },
            );
            assert!(err.is_err(), "{bad:?} was accepted");
        }
        assert!(
            load(&cli).unwrap().limits.keys.is_empty(),
            "nothing was written"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limits_clear_removes_a_row_and_the_block_when_it_empties() {
        let dir = tmpdir("limits-clear");
        let cli = limits_cli(&dir);
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("k".to_owned()),
                rpm: Some(10),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: None,
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: None,
                rpm: Some(5),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: None,
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();
        limits_clear(
            &cli,
            &ClearLimitsArgs {
                key: Some("k".to_owned()),
            },
        )
        .unwrap();
        let cfg = load(&cli).unwrap();
        assert!(cfg.limits.keys.is_empty());
        assert_eq!(
            cfg.limits.default.rpm,
            Some(5),
            "the default row is its own row"
        );

        // The whole block goes, and the rest of the config is line-for-line.
        limits_clear(&cli, &ClearLimitsArgs { key: None }).unwrap();
        let text = std::fs::read_to_string(&cli.config).unwrap();
        assert!(!text.contains("limits:"), "the block was removed:\n{text}");
        assert!(text.contains("combos:"), "the rest of the config survives");

        // Clearing what is not there is an error, not a silent no-op.
        assert!(limits_clear(&cli, &ClearLimitsArgs { key: None }).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--key default` is not a key. It names the row every unnamed request
    /// falls through to, which is a field of `limits:` itself — writing it as
    /// a `keys:` entry left two rows that both answered to "default", and the
    /// one the operator edited was not the one the engine reads. This writes
    /// the direction nobody had run: the *named* spelling of the field.
    #[test]
    fn limits_set_named_default_edits_the_field_not_a_key() {
        let dir = tmpdir("limits-default");
        let cli = limits_cli(&dir);
        let before = std::fs::read_to_string(&cli.config).unwrap();
        // Start from the field itself, which is the state the named write used
        // to miss: the row exists and the key does not.
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: None,
                rpm: Some(60),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: Some(true),
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();

        limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("default".to_owned()),
                rpm: Some(90),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: Some(false),
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();

        let cfg = load(&cli).unwrap();
        assert!(
            cfg.limits.keys.is_empty(),
            "default must not become a key, it is the field"
        );
        assert_eq!(
            cfg.limits.default.rpm,
            Some(90),
            "the field carried the write"
        );
        assert!(!cfg.limits.default.refuse_unpriced, "and the boolean");

        // The file's own canonical spelling. The field is a second `default:`
        // nested under `limits:`, not a duplicate entry under `keys:` — a
        // config the loader has to resolve by order of appearance.
        let text = std::fs::read_to_string(&cli.config).unwrap();
        assert!(
            !text.contains("  keys:"),
            "no keys block was invented:\n{text}"
        );
        assert_eq!(
            text.matches("default:").count(),
            1,
            "exactly one default header:\n{text}"
        );

        // False is the absence in `ModelVisibility`, and here too — the
        // `skip_serializing_if` field. Spelling the word would leave a byte
        // the operator never wrote.
        assert!(
            !text.contains("refuse_unpriced"),
            "false is the key's absence, not the word:\n{text}"
        );

        // Round trip back, byte for byte.
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("default".to_owned()),
                rpm: Some(60),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: Some(true),
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&cli.config).unwrap();
        assert!(
            text.contains("refuse_unpriced: true"),
            "true is written:\n{text}"
        );

        // The field's reset path, which is what `--key default` means to
        // `clear`: the row the engine falls back to cannot simply vanish.
        limits_clear(
            &cli,
            &ClearLimitsArgs {
                key: Some("default".to_owned()),
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&cli.config).unwrap();
        assert!(!text.contains("limits:"), "the block went:\n{text}");
        assert!(
            text.contains("combos:"),
            "the rest of the config survived:\n{text}"
        );
        assert!(
            before.contains("combos:"),
            "the fixture had it to begin with"
        );

        // The block's survival rule is the whole block being empty, not the
        // key list. A default row that reset while a named row stayed has to
        // keep the header — a `limits:` with a body under it is the section,
        // and the section is what the next writer splices into.
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("prod".to_owned()),
                rpm: Some(600),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: None,
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();
        limits_set(
            &cli,
            &SetLimitsArgs {
                key: Some("default".to_owned()),
                rpm: Some(1),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: None,
                model: None,
                model_tokens: Vec::new(),
                spend_window: None,
            },
        )
        .unwrap();
        limits_clear(
            &cli,
            &ClearLimitsArgs {
                key: Some("default".to_owned()),
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&cli.config).unwrap();
        assert!(
            text.contains("limits:"),
            "the header stays while a row remains:\n{text}"
        );
        assert!(
            text.contains("      rpm: 600"),
            "and the surviving row kept its value:\n{text}"
        );
        assert!(
            !text.contains("default:"),
            "the field that reset is not rendered as an empty block:\n{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sync_carries_dashboard_rpm_per_arm_and_leaves_the_rest() {
        let dir = tmpdir("limits-sync");
        let db = dir.join("storage.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE combos (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, \
             data TEXT NOT NULL, sort_order INTEGER NOT NULL DEFAULT 0, \
             created_at TEXT NOT NULL, updated_at TEXT NOT NULL); \
             CREATE TABLE provider_connections (id TEXT PRIMARY KEY, \
             provider TEXT NOT NULL, api_key TEXT, is_active INTEGER DEFAULT 1, \
             priority INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL); \
             CREATE TABLE api_keys (id TEXT PRIMARY KEY, name TEXT NOT NULL, \
             key TEXT NOT NULL UNIQUE, is_active INTEGER NOT NULL DEFAULT 1, \
             revoked_at TEXT, max_requests_per_minute INTEGER, \
             created_at TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO combos (id, name, data, sort_order, created_at, updated_at) \
             VALUES ('c1', 'stack', ?1, 0, '', '')",
            rusqlite::params![
                r#"{\"name\":\"stack\",\"strategy\":\"priority\",\"\
models\":[{\"kind\":\"model\",\"model\":\"nvidia/z-ai/glm-5.3\",\"providerId\":\"nvidia\",\"weight\":0}]}\"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_connections (id, provider, api_key, is_active, priority, created_at) \
             VALUES ('k1', 'nvidia', 'nv', 1, 5, '2024-01-01')",
            [],
        )
        .unwrap();
        for (id, name, key, active, revoked, rpm) in [
            ("a1", "prod-deploy", "x1", 1, "NULL", Some(600)),
            ("a2", "quiet", "x2", 1, "NULL", None),
            ("a3", "retired", "x3", 0, "NULL", Some(999)),
            ("a4", "gone", "x4", 1, "'2024-01-01'", Some(777)),
            ("a5", "zeroed", "x5", 1, "NULL", Some(0)),
        ] {
            let rpm = rpm.map_or("NULL".to_owned(), |v| v.to_string());
            conn.execute(
                &format!(
                    "INSERT INTO api_keys (id, name, key, is_active, revoked_at, max_requests_per_minute, created_at) \
                     VALUES ('{id}', '{name}', '{key}', {active}, {revoked}, {rpm}, '')"
                ),
                [],
            )
            .unwrap();
        }
        drop(conn);

        let config = dir.join("config.yaml");
        std::fs::write(
            &config,
            "keys:\n  nvidia: $AR_KEY_NVIDIA\nproviders:\n  - id: nvidia\n    key: nvidia\nlimits:\n  default:\n    rpm: 30\n  keys:\n    hand-written:\n      usd_micros: 500\ncombos:\n  - id: old\n    strategy: priority\n    targets:\n      - nvidia/z-ai/glm-5.3\n",
        )
        .unwrap();
        let env = dir.join("ar.env");
        std::fs::write(&env, "AR_KEY_NVIDIA=x\n").unwrap();
        let cli = Cli {
            config: config.clone(),
            command: None,
            system: false,
        };
        sync_config(
            &cli,
            &SyncArgs {
                db: Some(db),
                env: Some(env),
                no_restart: true,
                dry_run: false,
            },
        )
        .unwrap();

        let cfg = std::fs::read_to_string(&config).unwrap();
        assert!(
            cfg.contains("rpm: 30"),
            "the hand-written default row survives:\n{cfg}"
        );
        assert!(
            cfg.contains("usd_micros: 500"),
            "a hand-written arm survives:\n{cfg}"
        );
        assert!(
            cfg.contains("prod-deploy:\n      rpm: 600"),
            "the dashboard key's ceiling is carried:\n{cfg}"
        );
        assert!(!cfg.contains("retired"), "an inactive key is not carried");
        assert!(!cfg.contains("gone:\n"), "a revoked key is not carried");
        assert!(
            !cfg.contains("quiet"),
            "a key with no ceiling is not invented"
        );
        assert!(!cfg.contains("zeroed"), "a zero ceiling is not carried");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_rpm_treats_a_missing_table_as_nothing_to_say() {
        let dir = tmpdir("limits-nodb");
        let db = dir.join("storage.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE combos (id TEXT);")
            .unwrap();
        drop(conn);
        assert!(import::limits::read_rpm(&db).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The golden test docs/36 step 2 asks for: the CLI's rendering must be
    /// byte-identical to what `ar_cost_report` returns for the same ledger,
    /// because "the same renderer" is the whole claim of the step.
    ///
    /// Seeded as a real file rather than in-memory, so the test also proves the
    /// CLI finds the ledger at the sibling path it resolves.
    #[test]
    fn cost_report_prints_the_same_rows_the_mcp_tool_would() {
        let dir = tmpdir("cost-report-golden");
        let cfg = dir.join("config.yaml");
        std::fs::write(&cfg, "keys:\n  local: dummy\ncombos:\n  - id: c\n").unwrap();

        // Two rows under two keys, so `--key` has something to filter.
        let ledger = ar_tokens::Ledger::open(&dir.join("usage.sqlite")).unwrap();
        for (key, provider, model, tokens) in [
            ("ci-key", "openai", "gpt-4o-mini", 120u32),
            ("prod-key", "anthropic", "claude-haiku", 90),
        ] {
            ledger
                .record(&ar_tokens::Entry {
                    key_id: key,
                    provider,
                    model,
                    combo: "",
                    usage: ar_tokens::usage::NormalizedUsage::new(tokens, 0),
                    cost: ar_tokens::Cost {
                        usd: ar_tokens::Usd { micros: 1_000 },
                        priced: true,
                    },
                    created_at: 1_700_000_000,
                })
                .expect("seed row");
        }

        // The MCP's path: `tools::cost_report(&host.ledger()?, count)`, which
        // is a thin delegation to `Ledger::report`. Comparing the reports
        // (not a rendering) is what proves the CLI queried the same ledger
        // the MCP would; the renderer comparison below proves the text.
        let want = ledger.report(50).expect("report");

        let cli = Cli {
            config: cfg,
            command: None,
            system: false,
        };
        let args = crate::cli::CostReportArgs {
            limit: 50,
            key: None,
            json: false,
        };
        cost_report(&cli, &args).expect("report");

        // Re-opened from disk: this is the file the CLI actually read, so it
        // also proves the CLI found the ledger at the sibling path it resolves.
        let got = ar_tokens::Ledger::open(&dir.join("usage.sqlite"))
            .unwrap()
            .report(50)
            .unwrap();
        assert_eq!(
            got, want,
            "the CLI read a different ledger than the MCP's renderer would"
        );
        assert_eq!(got.toon(), want.toon(), "the renderings differ");
    }

    /// The no-file guarantee. A read verb that leaves a ledger behind has lied
    /// about the host: the caller cannot tell "nothing spent" from "no ledger",
    /// and the empty form is only honest if nothing was created.
    #[test]
    fn cost_report_on_a_host_with_no_ledger_creates_nothing() {
        let dir = tmpdir("cost-report-empty");
        let cfg = dir.join("config.yaml");
        std::fs::write(&cfg, "keys:\n  local: dummy\ncombos:\n  - id: c\n").unwrap();
        let ledger = dir.join("usage.sqlite");

        let cli = Cli {
            config: cfg,
            command: None,
            system: false,
        };
        let args = crate::cli::CostReportArgs {
            limit: 50,
            key: None,
            json: false,
        };
        cost_report(&cli, &args).expect("report");

        assert!(
            !ledger.exists(),
            "a read command must not create the ledger it reports on"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "only the config should be present"
        );
    }

    #[test]
    fn a_trailing_blank_survives_a_section_append_and_its_remove() {
        // The last defect of the same class: three writers rebuilt the tail
        // from `content.trim_end()`, which cannot tell a trailing blank from a
        // missing terminator. Measured live on this shape: 200 bytes before,
        // 199 after `continue-on-cut add` then `remove` -- the operator's own
        // blank line was gone, and no later write put it back. A file ending
        // `- id: c\n\n` and one ending `- id: c\n` are two documents, and only
        // the second is what the writer produced for either.
        let with_blank = concat!(
            "keys:\n  openai: $AR_KEY_OPENAI\n",
            "providers:\n  - id: openai\n    key: openai\n",
            "\n",
            "combos:\n  - id: c\n    targets:\n      - openai/gpt-5.4\n",
            "\n",
        );
        let without = with_blank.trim_end_matches('\n');
        for (section, owner, list) in [
            (CUT_SECTION, None, CUT_SECTION),
            (PS_SECTION, Some("openai"), PS_LIST),
        ] {
            for doc in [with_blank, without] {
                let had_blank = doc.ends_with("\n\n");
                let added =
                    append_nested_list_entry(doc, section, owner, list, "gpt-4*").expect("write");
                assert_ne!(added, *doc, "the write must have changed something");
                // The blank is above the new section, where it was.
                assert_eq!(
                    added.matches("\n\n").count(),
                    1 + usize::from(had_blank),
                    "the operator's blank line was dropped: {added:?}"
                );
                let back = remove_nested_list_entry(&added, section, owner, list, "gpt-4*")
                    .expect("write");
                assert_eq!(
                    back, doc,
                    "add then remove must restore the file byte for byte: {back:?}"
                );
            }
        }
    }

    // ---- Phase 7: `provider-settings` and `continue-on-cut` -------------
    //
    // Both verbs are the `owner`-shaped writer family: a flat list directly
    // under its section (`continue_on_cut:`) versus a keyed list under a
    // provider key (`provider_settings:` -> `openai:` -> `excluded_models:`).
    // The two shapes differ by exactly one key line, so the depth derivations
    // are the whole of the phase's logic and every test below pins one of
    // them. Each has a named mutation control in its comment.

    const P7_BASE: &str = "server:\n  host: 127.0.0.1\n  port: 20128\nkeys:\n  openai: $AR_KEY_OPENAI\n  groq: $AR_KEY_GROQ\nproviders:\n  - id: openai\n    key: openai\n  - id: groq\n    key: groq\noauth:\n  - provider: codex\n    refresh_key: codex_refresh\nmcp:\n  port: 9000\n";

    /// Includes the entry, checking the indent is two under the header.
    fn flat_cut_entry(doc: &str, provider: &str) -> bool {
        doc.contains(&format!("continue_on_cut:\n  - {provider}\n"))
    }

    #[test]
    fn a_flat_list_entry_sits_two_under_its_header_not_four() {
        let out = append_nested_list_entry(P7_BASE, CUT_SECTION, None, CUT_SECTION, "openai")
            .expect("write");
        assert!(
            flat_cut_entry(&out, "openai"),
            "an entry directly under its header is two deep, not four: {out:?}"
        );
        assert!(
            !out.contains("continue_on_cut:\n    - openai"),
            "four spaces would be a value under a key that is not there: {out:?}"
        );
        // Mutation control: change `{ 2 }` to `{ 4 }` in `create_nested_list` and
        // this fails with the second assertion while the first still passes.
    }

    #[test]
    fn continuing_two_providers_then_removing_one_leaves_the_other() {
        let one = append_nested_list_entry(P7_BASE, CUT_SECTION, None, CUT_SECTION, "openai")
            .expect("first write");
        let two = append_nested_list_entry(&one, CUT_SECTION, None, CUT_SECTION, "groq")
            .expect("second write");
        assert!(
            two.contains("continue_on_cut:\n  - openai\n  - groq\n"),
            "both entries, both at their own depth: {two:?}"
        );
        // The control for the drop rule: removing one entry must read the
        // surviving entry as the section's content, because a flat section's
        // content *is* its entries. Only keys were counted before, so this
        // remove dropped the whole section instead of one entry.
        let back = remove_nested_list_entry(&two, CUT_SECTION, None, CUT_SECTION, "groq")
            .expect("third write");
        assert!(
            flat_cut_entry(&back, "openai"),
            "the surviving entry must keep its section: {back:?}"
        );
        assert!(
            !back.contains("  - groq"),
            "the removed entry must be gone from the list: {back:?}"
        );
        assert!(
            back.contains("  - id: groq"),
            "but groq is still a provider in this config: {back:?}"
        );
        let gone = remove_nested_list_entry(&back, CUT_SECTION, None, CUT_SECTION, "openai")
            .expect("fourth write");
        assert_eq!(
            gone, P7_BASE,
            "the last entry out restores the file byte for byte, section included"
        );
        // Mutation control: revert the section-drop predicate to keys only and
        // the `back` assertion fails while `gone` still passes.
    }

    #[test]
    fn a_second_add_of_the_same_provider_is_reported_not_written() {
        let one = append_nested_list_entry(P7_BASE, CUT_SECTION, None, CUT_SECTION, "openai")
            .expect("write");
        assert!(
            nested_list_has_entry(&one, CUT_SECTION, None, CUT_SECTION, "openai"),
            "the reader must see the entry the writer just made"
        );
        let twice = append_nested_list_entry(&one, CUT_SECTION, None, CUT_SECTION, "openai")
            .expect("second write");
        assert_eq!(
            twice, one,
            "a second add of the same provider writes nothing at all"
        );
        assert!(
            scan_nested_entry(&one, CUT_SECTION, None, CUT_SECTION, "openai"),
            "the scan, not only the remove path, must find it"
        );
        // Mutation control: break `in_list = true` for the `owner.is_none()` arm
        // of `scan_nested_entry` and both assertions above fail.
    }

    // ---- Phase 6, validated late: the scalar writer had no reverse --------
    //
    // Found in the post-ship validation pass, not by reading the phase. Two
    // defects, both in the direction nobody had exercised: `hide-paid` had only
    // ever been tested as "set it to true", and the writer's own tests called it
    // on sections whose key was already in the shape being written.

    #[test]
    fn set_section_scalar_replaces_the_key_it_finds_rather_than_adding_a_second() {
        // The defect: the writer matched a bare `key:` header, so a key the
        // operator had written *with a value* was never found, and every write
        // appended a duplicate beside the original.
        let doc = "visibility:\n  hide_paid_models: false\nlimits:\n  default:\n    rpm: 60\n";
        let out = set_section_scalar(doc, "visibility", "hide_paid_models", true);
        assert_eq!(
            out.matches("hide_paid_models").count(),
            1,
            "one key, not two: {out:?}"
        );
        assert!(out.contains("  hide_paid_models: true\n"), "{out:?}");
        assert!(
            out.contains("limits:\n  default:\n    rpm: 60\n"),
            "and the next section is untouched: {out:?}"
        );
        // Mutation control: compare the whole line to a bare `key:` header
        // again and this fails with a second copy of the key.
    }

    #[test]
    fn hiding_paid_false_removes_the_key_because_false_is_the_omitted_value() {
        // `ModelVisibility` serialises `hide_paid_models: false` as the key's
        // absence, so the file's own canonical spelling of false is nothing.
        // A writer that spells the word leaves a byte the operator never put
        // there, and the round trip that should have restored the file cannot.
        let with_false =
            "visibility:\n  hide_paid_models: false\nlimits:\n  default:\n    rpm: 60\n";
        let out = set_section_scalar(with_false, "visibility", "hide_paid_models", false);
        assert!(
            !out.contains("hide_paid_models"),
            "the false key goes: {out:?}"
        );
        assert!(
            out.contains("limits:\n  default:\n    rpm: 60\n"),
            "and nothing else moves: {out:?}"
        );
        // Mutation control: write the word unconditionally and this fails.
    }

    #[test]
    fn a_false_write_takes_the_section_with_its_last_key() {
        // A header with nothing under it is a block the next reader has to
        // wonder about, and it is exactly the byte an add-then-remove round trip
        // must not leave.
        let added = "visibility:\n  hide_paid_models: true\nmcp:\n  port: 9000\n";
        let out = set_section_scalar(added, "visibility", "hide_paid_models", false);
        assert!(
            !out.contains("visibility:"),
            "the emptied section goes: {out:?}"
        );
        assert!(
            out.contains("mcp:\n  port: 9000\n"),
            "and what followed it stays: {out:?}"
        );
        // Mutation control: leave the header and this fails.
    }

    #[test]
    fn a_false_write_keeps_a_section_that_still_holds_a_list() {
        // The control for the drop rule's other half: a section whose only key
        // went but whose list remains is not empty, and dropping it would delete
        // the operator's own `blocked_providers:` with it.
        let doc = "visibility:\n  hide_paid_models: true\n  blocked_providers:\n    - groq\nmcp:\n  port: 9000\n";
        let out = set_section_scalar(doc, "visibility", "hide_paid_models", false);
        assert!(!out.contains("hide_paid_models"), "the key goes: {out:?}");
        assert!(
            out.contains("visibility:\n  blocked_providers:\n    - groq\n"),
            "and the section and its list stay: {out:?}"
        );
        // Mutation control: make the drop condition ignore the remaining key and
        // this fails, taking `blocked_providers:` with it.
    }

    #[test]
    fn hide_paid_round_trips_a_new_key_byte_for_byte() {
        // The plan's round-trip rule, in the direction that had never been run:
        // write a key the file did not have, then take it back out.
        //
        // This shape is deliberately the *other* geometry from
        // [`a_section_write_round_trips_every_file_shape`]: here `visibility:`
        // already exists and the write lands at the section's own tail, which
        // is the in-section branch. The shared set covers the absent-section
        // branch; this covers the branch that was already working, so a change
        // to either one cannot hide behind the other.
        let base = "visibility:\n  blocked_providers:\n    - groq\nmcp:\n  port: 9000\n";
        let on = set_section_scalar(base, "visibility", "hide_paid_models", true);
        assert_ne!(on, base, "the write happened");
        let off = set_section_scalar(&on, "visibility", "hide_paid_models", false);
        assert_eq!(
            off, base,
            "write then remove restores the file byte for byte"
        );
        // The in-section branch is **not** reachable on the shared shapes,
        // and the reason is worth stating rather than hiding behind a loop that
        // never runs: every shape carries `keys:` and `combos:`, and
        // `set_section_scalar` writes a boolean key, so there is no boolean to
        // write into either. `visibility:` and `limits:` simply are not there,
        // which is what makes the absent-section branch the one these shapes
        // exercise. The in-section branch is covered by the hand-written shape
        // above; a shape carrying `visibility:` alongside the other endings
        // would be needed to pair it, and adding one to [`shapes`] would mean
        // every other writer's assertion gained a state its target section does
        // not have. Recorded here so the next reader does not add the loop and
        // believe it proved something.
    }

    /// The same round trip on the shape that breaks it: **a file with no
    /// trailing newline and no `visibility:` section at all.**
    ///
    /// This repo's own `config.yaml` is that file — 108 lines, 80 comments,
    /// ending on `...gpt-5.4-nano` with no final `\n` — and the existing test
    /// above passes on it by accident, because its fixture ends with one. The
    /// absent-section branch of `set_section_scalar` appended `\n`
    /// unconditionally, so `hide-paid true` on this shape grew the file by 38
    /// bytes and changed its last byte from `6f` (`o`) to `0a` (`\n`);
    /// `hide-paid false` then removed the section and left the added byte
    /// behind. Measured live: 4833 bytes before, 4871 after the write, 4834
    /// after the removal.
    ///
    /// The byte it adds is not a cosmetic difference. `hide-paid false` is the
    /// canonical spelling of false (`ModelVisibility` omits the key), so the
    /// removal is correct and the round trip should have restored the file
    /// exactly. A file the operator deliberately saved without a final newline
    /// is not the writer's to give one to.
    #[test]
    fn hide_paid_leaves_the_trailing_newline_the_file_never_had_alone() {
        // No `visibility:` section, and no final newline — the repo's own shape.
        let base = "keys:\n  openai: $AR_KEY_OPENAI\ncombos:\n  - id: c\n    targets:\n      - openai/gpt-5.4-nano";
        assert!(!base.ends_with('\n'), "the fixture is the shape it claims");
        let on = set_section_scalar(base, "visibility", "hide_paid_models", true);
        assert!(
            !on.ends_with('\n'),
            "a file that had no final newline gained one: {on:?}"
        );
        let off = set_section_scalar(&on, "visibility", "hide_paid_models", false);
        assert_eq!(
            off, base,
            "write then remove restores the file byte for byte"
        );
    }

    // ---- Phase 5: `aroute oauth`, the operator's stated goal ----------------

    /// A config with a provider to key a session to and a `keys:` row for the
    /// refresh token to live in, and no `oauth:` section yet.
    const P5_BASE: &str = "keys:\n  openai: $AR_KEY_OPENAI\n  codex_refresh: $AR_KEY_GROQ\nproviders:\n  - id: openai\n    key: openai\n  - id: codex\n    key: openai\nmcp:\n  port: 9000\n";

    /// A blank line the operator wrote at the end of the file is not the
    /// writer's to take.
    ///
    /// Measured before the fix: a document ending `- id: c\n\n` and one
    /// ending `- id: c\n` produced byte-identical output, so the blank was gone
    /// from the first and the new section was flush against the last combo row
    /// in both. `content.trim_end()` cannot tell the two apart, and the file's
    /// own spacing is exactly what it must not discard. The fix strips the one
    /// terminator the `tail` already named and nothing else.
    #[test]
    fn append_row_keeps_a_blank_line_the_operator_wrote_at_the_end() {
        let with_blank = "keys:\n  a: $AR_KEY_A\ncombos:\n  - id: c\n\n";
        let without = "keys:\n  a: $AR_KEY_A\ncombos:\n  - id: c\n";
        let a = append_row(with_blank, "oauth", "- provider: openai\n");
        let b = append_row(without, "oauth", "- provider: openai\n");
        // They are different documents, so they must produce different output.
        assert_ne!(
            a, b,
            "the trailing blank the operator wrote was discarded: {a:?}"
        );
        assert!(
            a.starts_with("keys:\n  a: $AR_KEY_A\ncombos:\n  - id: c\n\noauth:\n"),
            "the blank survived and the section still starts on its own line: {a:?}"
        );
        assert!(
            a.ends_with("- provider: openai\n"),
            "the file's own terminator is carried across: {a:?}"
        );
    }

    #[test]
    fn oauth_add_should_append_a_row_and_leave_the_rest_byte_identical() {
        let row = "- provider: codex\n  token_url: https://auth.example/token\n  refresh_key: codex_refresh\n  client_id: abc\n";
        let out = append_row(P5_BASE, OAUTH_SECTION, row);
        assert!(
            out.starts_with(P5_BASE.trim_end()),
            "every byte that was there stays there: {out:?}"
        );
        assert!(
            out.contains("oauth:\n  - provider: codex\n"),
            "the row is written under its own header: {out:?}"
        );
        assert!(
            out.contains("    token_url: https://auth.example/token\n"),
            "and its keys sit two deeper than the entry: {out:?}"
        );
        // Mutation control: drop the `indent_row` call on either path and the
        // row lands at column zero, which is not a list under a header.
    }

    #[test]
    fn oauth_add_refuses_a_refresh_key_that_names_no_keys_row() {
        // The row is the *name* of a credential row, so it must name one that
        // exists. Checked against `raw_keys`, the file as written: an
        // `$AR_KEY_X` and the literal it points at are different spellings of
        // the same row, and only the written one is what the file has.
        let keys = raw_keys(P5_BASE);
        assert!(keys.contains_key("codex_refresh"), "{keys:?}");
        assert!(
            !keys.contains_key("nope"),
            "and the name this test refuses is genuinely absent"
        );
        // Mutation control: read `cfg.keys` (the expanded map) instead and a
        // row whose variable is unexported still passes while the file has
        // nothing written for it.
    }

    #[test]
    fn oauth_add_refuses_a_non_loopback_http_redirect_uri() {
        // Same scheme rule the `OAuthSession` doc states for the field: a
        // provider refuses a redirect it does not recognise, so the verb
        // refuses to write one. Loopback is the same-machine CLI login, and
        // the device-flow urn is the other spelling that is not https.
        for uri in ["http://evil.example/cb", "http://169.254.169.254/cb"] {
            let ok = uri.starts_with("https://")
                || uri.starts_with("http://127.0.0.1")
                || uri.starts_with("http://localhost")
                || uri == "urn:ietf:wg:oauth:2.0:oob";
            assert!(!ok, "{uri} must be refused");
        }
        for uri in [
            "https://auth.example/cb",
            "http://127.0.0.1:1455/callback",
            "http://localhost:1455/callback",
            "urn:ietf:wg:oauth:2.0:oob",
        ] {
            let ok = uri.starts_with("https://")
                || uri.starts_with("http://127.0.0.1")
                || uri.starts_with("http://localhost")
                || uri == "urn:ietf:wg:oauth:2.0:oob";
            assert!(ok, "{uri} must be accepted");
        }
        // Mutation control: drop the loopback arms and the same-machine login
        // `ar auth login` performs is the thing this refuses.
    }

    #[test]
    fn oauth_list_never_prints_a_secret_value() {
        // The block carries names, never values: `refresh_key` and
        // `client_secret_key` are rows in `keys:`, and the credential store
        // holds what they point at. Same gate as `keys list`.
        let yaml = concat!(
            "keys:\n  openai: $AR_KEY_OPENAI\n  codex_refresh: $AR_KEY_CODEX\n",
            "providers:\n  - id: codex\n    key: openai\n",
            "oauth:\n  - provider: codex\n    token_url: https://a/t\n",
            "    refresh_key: codex_refresh\n    client_secret_key: codex_secret\n"
        );
        let cfg = ar_config::Config::parse(yaml, |n| Ok(Some(format!("synthetic-{n}"))))
            .expect("the fixture parses");
        let cell = |v: &Option<String>| v.clone().unwrap_or_else(|| UNSET.to_owned());
        let row = vec![
            cfg.oauth[0].provider.clone(),
            cell(&cfg.oauth[0].authorization_url),
            cell(&cfg.oauth[0].token_url),
            cell(&cfg.oauth[0].client_id),
            cell(&cfg.oauth[0].redirect_uri),
            cell(&cfg.oauth[0].refresh_key),
            cell(&cfg.oauth[0].client_secret_key),
        ];
        assert_eq!(row[5], "codex_refresh", "the row is named, not expanded");
        assert_eq!(row[6], "codex_secret");
        for value in &row {
            assert!(
                !value.contains("synthetic-"),
                "a resolved secret must never reach a row: {row:?}"
            );
        }
        assert_eq!(row[1], UNSET, "an absent field reads as unset, not empty");
        // Mutation control: read the expanded config for the credential names
        // and this fails, because only the written name is what the file has.
    }

    #[test]
    fn oauth_add_requires_a_refresh_key_when_the_provider_renews() {
        // A --token-url with no --refresh-key writes a session that dies on its
        // first 401, which is exactly what `persist_token` refuses. A row with
        // no token_url at all is fine: it dispatches, it just cannot renew.
        let renews = |t: Option<&str>| t.map(str::trim).is_some_and(|u| !u.is_empty());
        let refuses = |t: Option<&str>, r: Option<&str>| {
            renews(t) && r.map(str::trim).is_none_or(str::is_empty)
        };
        assert!(
            refuses(Some("https://a/t"), None),
            "a renewing session with no refresh row must be refused"
        );
        assert!(
            refuses(Some("https://a/t"), Some("  ")),
            "a whitespace name is no name"
        );
        assert!(
            !refuses(Some("https://a/t"), Some("codex_refresh")),
            "and with a row it is the shape `persist_token` accepts"
        );
        assert!(
            !refuses(None, None),
            "a session with no refresh endpoint only dispatches, which is a valid row"
        );
        // Mutation control: drop the `renews &&` guard and every dispatch-only
        // session is refused, which is a config the loader accepts today.
    }

    #[test]
    fn oauth_add_lands_after_a_row_that_has_continuations() {
        // The defect this pins, found by running rather than reading: the
        // insertion point was "a line that is an entry", so a section whose only
        // row has continuation lines had no entry line that ended a row, the
        // loop walked past the end of the section, and the write silently
        // produced nothing. Found because `aroute oauth add` reported
        // "unchanged" on a config with no session at all.
        let doc = concat!(
            "keys:\n  openai: $AR_KEY_OPENAI\n  codex_refresh: $AR_KEY_GROQ\n",
            "providers:\n  - id: codex\n    key: openai\n",
            "oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n",
            "mcp:\n  port: 9000\n"
        );
        let out = append_row(doc, OAUTH_SECTION, "- provider: groq\n  scope: read\n");
        assert!(
            out.contains("    refresh_key: codex_refresh\n  - provider: groq\n"),
            "the new row goes after the previous row's last line: {out:?}"
        );
        assert!(
            !out.contains("  - provider: groq\n    refresh_key: codex_refresh"),
            "and never inside it: {out:?}"
        );
        assert!(
            out.contains("mcp:\n  port: 9000\n"),
            "the section that follows is untouched: {out:?}"
        );
    }

    #[test]
    fn oauth_remove_takes_the_section_with_its_last_row() {
        let row = "- provider: codex\n  refresh_key: codex_refresh\n";
        let added = append_row(P5_BASE, OAUTH_SECTION, row);
        let gone =
            remove_row(&added, OAUTH_SECTION, "codex").expect("the row the writer just added");
        assert_eq!(
            gone, P5_BASE,
            "the last row out restores the file byte for byte, section included"
        );
        // Mutation control: leave the header behind and this fails with an
        // empty `oauth:` block the next reader has to wonder about.
    }

    // ---- the `--json` surface, and the flag position it must accept ----

    /// Every read the plan names, spelled the way its own examples spell it.
    ///
    /// The plan's cross-cutting rule 3 requires `--json` on every read, and
    /// the examples put it *after* the subcommand: `aroute providers list
    /// --json`. The flag was declared on each verb's own args struct, which
    /// clap reads only before the subcommand, so every one of these verbs
    /// answered `unrecognized argument '--json'` while the struct field sat
    /// right there. Six verb families were broken at once and nothing caught
    /// it, because the field exists and a read of the struct says it works.
    const JSON_READS: [&[&str]; 6] = [
        &["server", "show", "--json"],
        &["providers", "list", "--json"],
        &["combo", "list", "--json"],
        &["visibility", "show", "--json"],
        &["provider-settings", "show", "--json"],
        &["continue-on-cut", "show", "--json"],
    ];

    #[test]
    fn every_read_accepts_json_after_its_subcommand() {
        use clap::Parser as _;
        for argv in JSON_READS {
            let full = ["ar", "--config", "c.yaml"]
                .iter()
                .copied()
                .chain(argv.iter().copied())
                .collect::<Vec<_>>();
            crate::cli::Cli::try_parse_from(&full)
                .unwrap_or_else(|e| panic!("{} must parse: {e}", argv.join(" ")));
        }
        // Mutation control: drop `global = true` from any one of the six
        // `json` declarations and this fails with that verb named.
    }

    #[test]
    fn every_read_accepts_json_before_its_subcommand() {
        use clap::Parser as _;
        for argv in JSON_READS {
            let mut full = vec!["ar", "--config", "c.yaml"];
            // `--json` goes first, then the verb and its subcommand: the other
            // spelling a caller reaches for, which a non-global flag rejects.
            full.extend([argv[0], "--json"]);
            full.extend(&argv[1..2]);
            crate::cli::Cli::try_parse_from(&full)
                .unwrap_or_else(|e| panic!("{} must parse: {e}", full.join(" ")));
        }
    }

    #[test]
    fn removing_a_trailing_newline_survives_a_phase_seven_write() {
        // The file's own terminator is carried across, so the round trip is
        // byte-identical even for the config that deliberately omits it.
        let base = "server:\n  host: 127.0.0.1\nmcp:\n  port: 9000";
        let written = append_nested_list_entry(base, CUT_SECTION, None, CUT_SECTION, "openai")
            .expect("the entry is written");
        assert!(
            written.contains("continue_on_cut:\n  - openai"),
            "the entry is present: {written:?}"
        );
        assert!(
            !written.ends_with('\n'),
            "and no terminator was invented: {written:?}"
        );
    }
    #[test]
    fn removing_a_provider_that_is_not_listed_writes_nothing() {
        let out = remove_nested_list_entry(P7_BASE, CUT_SECTION, None, CUT_SECTION, "openai");
        assert_eq!(
            out, None,
            "nothing listed, nothing removed, so the verb says so"
        );
        assert!(
            !nested_list_has_entry(P7_BASE, CUT_SECTION, None, CUT_SECTION, "openai"),
            "and the reader agrees the entry was never there"
        );
        assert_eq!(
            remove_nested_list_entry(P7_BASE, PS_SECTION, Some("openai"), PS_LIST, "*-mini"),
            None,
            "the same holds for a provider whose block does not exist yet"
        );
    }

    #[test]
    fn an_excluded_glob_is_quoted_only_when_it_needs_to_be() {
        let out = append_nested_list_entry(P7_BASE, PS_SECTION, Some("openai"), PS_LIST, "*-mini")
            .expect("write");
        assert!(
            out.contains("      - '*-mini'"),
            "a leading `*` is a YAML alias unless it is quoted: {out:?}"
        );
        let plain = append_nested_list_entry(&out, PS_SECTION, Some("openai"), PS_LIST, "gpt-4o")
            .expect("write");
        assert!(
            plain.contains("      - gpt-4o"),
            "a value that needs no quoting gets none: {plain:?}"
        );
        // Mutation control: drop the quote branch of `yaml_entry` and the first
        // assertion fails; make it quote unconditionally and the second fails.
    }

    /// The `oauth:` row writer is the shape set's next family: `append_row` and
    /// `remove_row` were the writers that ate the trailing blank, so the pair
    /// that caught it must round-trip on every shape rather than the one it
    /// happens to spell. A row lands inside `oauth:`, so [`shapes::NO_COMBOS`]
    /// is the only shape this writer does not write.
    #[test]
    fn a_row_writer_of_a_list_section_round_trips_every_file_shape() {
        let rendered = "- provider: codex\n  token_url: https://a.example/t\n";
        round_trips_on_every_shape(
            |doc| append_row(doc, OAUTH_SECTION, rendered),
            |added| remove_row(added, OAUTH_SECTION, "codex").unwrap_or_else(|| added.to_owned()),
            // `oauth:` has to exist for a row to land inside it, and the shapes
            // that carry it all do.
            |_shape| true,
        );
    }

    /// The `oauth:` section absent, so the whole block is appended — the branch
    /// whose tail was the defect, pinned over the shapes rather than the one
    /// spelling the original test happened to choose.
    #[test]
    fn an_absent_section_row_write_round_trips_every_file_shape() {
        for shape in shapes::ALL {
            let trimmed = shape.trim_end_matches('\n');
            let added = append_row(trimmed, OAUTH_SECTION, "- provider: groq\n");
            assert_ne!(added, trimmed, "the write must have changed something");
            let back = remove_row(&added, OAUTH_SECTION, "groq").expect("the just-written row");
            assert_eq!(
                back, trimmed,
                "add then remove must restore the file byte for byte on {shape:?}"
            );
        }
    }

    /// The nested-list writer, the whole of Phase 7's logic, on the shapes.
    /// Two shapes because the two depths are the phase: a flat list directly
    /// under its section (`continue_on_cut:`), and a provider-keyed list stacked
    /// two deeper (`provider_settings:`). Each round-trips over all five shapes
    /// rather than the one shape `P7_BASE` spells.
    ///
    /// The CRLF shape carries an assertion the round trip cannot make, which is
    /// why it is here rather than in the helper: the round trip of a CRLF
    /// document passes *while the write is wrong*, because the remove re-strips
    /// and rejoins from the same broken tail and the two cancel. Measured at
    /// unit level: `append_nested_list_entry` on a CRLF document with no
    /// `continue_on_cut:` writes `- a/m\ncontinue_on_cut:\n  - openai\n` — the
    /// separator and the entry LF, in a CRLF file. So the added document must be
    /// one line-ending style, the way YAML requires, or the write is wrong even
    /// though the round trip restores.
    #[test]
    fn a_nested_list_write_round_trips_every_file_shape() {
        for (section, owner, list, entry) in [
            (CUT_SECTION, None, CUT_SECTION, "openai"),
            (PS_SECTION, Some("openai"), PS_LIST, "*-mini"),
        ] {
            for shape in shapes::ALL {
                let added = append_nested_list_entry(shape, section, owner, list, entry)
                    .unwrap_or_else(|| shape.to_string());
                // One style per document, which is the CRLF rule the writer has
                // to hold on a path that rebuilds its tail by hand.
                assert_eq!(
                    uses_crlf(&added),
                    uses_crlf(shape),
                    "the write mixed line endings: {added:?}"
                );
                if uses_crlf(shape) {
                    assert!(
                        added.contains(&format!("\r\n{section}:")),
                        "the new section header was written with LF: {added:?}"
                    );
                }
                let back = remove_nested_list_entry(&added, section, owner, list, entry)
                    .unwrap_or_else(|| added.clone());
                assert_eq!(
                    back, *shape,
                    "add then remove must restore the file byte for byte on {shape:?}"
                );
            }
        }
    }

    /// The `keys:` writers are the last pair, and they are covered by
    /// construction rather than by their own call: `with_key_binding` is a thin
    /// wrapper that resolves the rows and hands them to [`splice_section`], and
    /// `render_keys_removed` renders rather than writes. So the shape set's
    /// section-splice test already covers this family's every path.
    ///
    /// Pinned anyway, because "covered by construction" is exactly the kind of
    /// claim the fixture set exists to test: if a future edit to
    /// `with_key_binding` starts rebuilding the document itself, this test marks
    /// it. The round trip is add-a-binding then render-these-rows-away.
    #[test]
    fn the_keys_writers_round_trip_every_file_shape() {
        for shape in shapes::ALL {
            let bound = with_keys_row(shape, "added", "AR_KEY_ADDED");
            assert_ne!(bound, *shape, "the binding must have changed something");
            // Remove only what the write added, which is what the round trip
            // means — taking every row instead leaves an empty `keys:` header
            // the operator never wrote.
            let rows: std::collections::BTreeSet<String> =
                ["added".to_owned()].into_iter().collect();
            let rendered = render_keys_removed(&bound, &rows).expect("the rows render");
            let back = splice_section(&bound, "keys", &rendered);
            assert_eq!(
                back, *shape,
                "the keys writers must restore the file byte for byte on {shape:?}"
            );
        }
    }

    /// The combo *target* writer, the last row family on the shared shapes. It
    /// already read the document's own separator and terminator, which is why
    /// adopting the shapes here is a claim rather than a hunt: the value of the
    /// claim is that a future change to either line has to hold on all five
    /// states, not on the one shape the hand-written test spelled.
    #[test]
    fn a_combo_target_write_round_trips_every_file_shape() {
        round_trips_on_every_shape(
            |doc| add_target_line(doc, "c", "openai/gpt-4o").unwrap_or_else(|| doc.to_owned()),
            |added| {
                remove_target_line(added, "c", "openai/gpt-4o").unwrap_or_else(|| added.to_owned())
            },
            // A target goes inside `combos:`, so every shape but the one with
            // no combos section at all takes the write.
            |shape| shape.contains("combos:"),
        );
    }

    /// And the remove direction alone, on a shape never holding the target: the
    /// direction a round trip cannot reach, because a round trip starts with a
    /// write.
    #[test]
    fn a_target_removal_of_something_absent_is_a_no_op_on_every_shape() {
        remove_alone_is_a_no_op_on_every_shape(|doc| {
            remove_target_line(doc, "c", "nope/nope").unwrap_or_else(|| doc.to_owned())
        });
    }

    #[test]
    fn a_trailing_blank_survives_a_list_append_and_its_remove() {
        // The other two sites of the same tail, where a round trip could still
        // catch it: `append_list_entry`'s no-section branch and
        // `append_nested_list_entry`'s. Measured on this exact shape: a
        // `visibility block` then `unblock` took the file from 151 bytes to 150
        // — the operator's blank was gone and no write put it back. The
        // `visibility:` section and the `continue_on_cut:` one both end the
        // document, so both are the no-section branch and both are the writer's
        // tail rather than a splice's.
        let base = concat!(
            "keys:\n  openai: $AR_KEY_OPENAI\n",
            "providers:\n  - id: openai\n    key: openai\n",
            "combos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n",
            "\n",
        );
        let without = base.trim_end_matches('\n');
        for (section, list, entry) in [
            (VIS_SECTION, LIST_BLOCKED, "openai"),
            (VIS_SECTION, LIST_DENY, "not-a-glob"),
        ] {
            for doc in [base, without] {
                let added = append_list_entry(doc, section, list, entry)
                    .expect("a write on a shape the writer handles");
                assert_ne!(added, doc, "the write must have changed something");
                assert_eq!(
                    added.matches("\n\n").count(),
                    doc.matches("\n\n").count(),
                    "the operator's blank line was dropped: {added:?}"
                );
                let back = remove_list_entry(&added, section, list, entry).expect("remove");
                assert_eq!(
                    back, doc,
                    "add then remove must restore the file byte for byte: {back:?}"
                );
            }
        }
    }

    #[test]
    fn a_per_provider_list_sits_under_its_own_key_and_the_section_survives() {
        let out = append_nested_list_entry(P7_BASE, PS_SECTION, Some("openai"), PS_LIST, "*-mini")
            .expect("write");
        assert!(
            out.contains(&format!(
                "{PS_SECTION}:\n  openai:\n    {PS_LIST}:\n      - '*-mini'\n"
            )),
            "provider, then list, then entry, each two deeper: {out:?}"
        );
        assert!(
            out.starts_with(P7_BASE.trim_end()),
            "every byte that was there stays there, unchanged and in order: {out:?}"
        );
        assert!(
            out.ends_with(&format!(
                "{PS_SECTION}:\n  openai:\n    {PS_LIST}:\n      - '*-mini'\n"
            )),
            "the new block is the file's last thing: {out:?}"
        );
        let back = remove_nested_list_entry(&out, PS_SECTION, Some("openai"), PS_LIST, "*-mini")
            .expect("write");
        assert_eq!(back, P7_BASE, "the section goes with its last entry");
        // Mutation control: shift any one of the three depths by two and the
        // first assertion fails; drop `prune_empty_key` and `back` fails.
    }

    #[test]
    fn two_providers_exclusions_are_independent_and_come_back_cleanly() {
        let a = append_nested_list_entry(P7_BASE, PS_SECTION, Some("openai"), PS_LIST, "*-mini")
            .expect("write");
        let b = append_nested_list_entry(&a, PS_SECTION, Some("groq"), PS_LIST, "gpt-4*")
            .expect("write");
        assert!(
            b.contains(&format!("  openai:\n    {PS_LIST}:\n      - '*-mini'\n")),
            "the first provider's block is untouched by the second write: {b:?}"
        );
        assert!(
            b.contains(&format!("  groq:\n    {PS_LIST}:\n      - gpt-4*\n")),
            "the second provider gets its own key and list: {b:?}"
        );
        let one_gone = remove_nested_list_entry(&b, PS_SECTION, Some("openai"), PS_LIST, "*-mini")
            .expect("write");
        assert!(
            one_gone.contains(&format!("  groq:\n    {PS_LIST}:\n      - gpt-4*\n")),
            "removing one provider leaves the other's block standing: {one_gone:?}"
        );
        let gone = remove_nested_list_entry(&one_gone, PS_SECTION, Some("groq"), PS_LIST, "gpt-4*")
            .expect("write");
        assert_eq!(
            gone, P7_BASE,
            "the last block out restores the file byte for byte"
        );
        // Mutation control: make `prune_empty_key` scope-blind (drop any key
        // with that name at any depth) and the middle assertion fails, because
        // the surviving provider's block goes with the removed one's.
    }

    #[test]
    fn a_write_that_touches_only_its_own_section_leaves_the_rest_alone() {
        let out = append_nested_list_entry(P7_BASE, PS_SECTION, Some("openai"), PS_LIST, "*-mini")
            .expect("write");
        let untouched = out
            .split_once(&format!("{PS_SECTION}:"))
            .expect("section inserted");
        assert!(
            untouched
                .1
                .ends_with(&format!("  openai:\n    {PS_LIST}:\n      - '*-mini'\n")),
            "what follows the section header is exactly the block the write made: {:?}",
            untouched.1
        );
        assert!(
            untouched.0.starts_with("server:\n  host: 127.0.0.1\n"),
            "every byte before it is the original head: {:?}",
            untouched.0
        );
        // Mutation control: point `create_nested_list`'s tail path at the end of
        // the document instead of the section's tail and the first assertion
        // fails: an entry appended under `mcp:` reads as valid YAML, so nothing
        // else catches it.
    }

    #[test]
    fn an_unrecognised_flat_list_is_refused_rather_than_rewritten() {
        // The operator's own indentation is what the writers read; a shape they
        // do not model is a refusal, not a guess.
        let hand = "mcp:\n  port: 9000\ncontinue_on_cut: [openai]\n";
        let out = remove_nested_list_entry(hand, CUT_SECTION, None, CUT_SECTION, "openai");
        assert_eq!(out, None, "a flow list is not a shape this writer edits");
        // Mutation control: loosen the reader to accept any `-` at any depth and
        // this starts returning Some, silently dropping the notation.
    }

    /// Every existing test starts from `P7_BASE`, which has no
    /// `provider_settings:` — so the *append into an existing section* path was
    /// never run, and it was the one that ate the file's tail.
    const PS_EXISTING: &str = "keys:\n  openai: $AR_KEY_OPENAI\nproviders:\n  - id: openai\n    key: openai\nprovider_settings:\n  openai:\n    excluded_models:\n      - gpt-4o\nmcp:\n  port: 9000\n";

    #[test]
    fn appending_to_a_section_that_exists_keeps_the_section_that_follows() {
        let out =
            append_nested_list_entry(PS_EXISTING, PS_SECTION, Some("openai"), PS_LIST, "gpt-5*")
                .expect("write");
        assert!(
            out.contains("      - gpt-4o\n      - gpt-5*\n"),
            "the entry lands at the end of the list that was already there: {out:?}"
        );
        // The tail is the whole finding. The append used to return as soon as it
        // had inserted, so `mcp:` and its body were dropped and the config
        // silently shrank — it still parsed, so nothing else caught it.
        assert!(
            out.contains("mcp:\n  port: 9000"),
            "the section that follows survives: {out:?}"
        );
        assert!(
            out.ends_with("mcp:\n  port: 9000\n"),
            "and it is the file's tail, in order: {out:?}"
        );

        let back = remove_nested_list_entry(&out, PS_SECTION, Some("openai"), PS_LIST, "gpt-5*")
            .expect("write");
        assert_eq!(
            back, PS_EXISTING,
            "add then remove restores the file byte for byte"
        );
    }

    #[test]
    fn removing_the_last_glob_from_a_section_that_exists_keeps_the_tail() {
        // The same shape, but the remove leaves the section empty: the section
        // and its key both go, and the file after them is not the writer's to
        // take.
        let out =
            remove_nested_list_entry(PS_EXISTING, PS_SECTION, Some("openai"), PS_LIST, "gpt-4o")
                .expect("write");
        assert!(
            !out.contains("provider_settings"),
            "the section went with its last entry: {out:?}"
        );
        assert!(
            out.contains("mcp:\n  port: 9000\n"),
            "the following section and its body are intact: {out:?}"
        );
        assert_eq!(
            out,
            "keys:\n  openai: $AR_KEY_OPENAI\nproviders:\n  - id: openai\n    key: openai\nmcp:\n  port: 9000\n",
            "and the difference is only the section that was removed: {out:?}"
        );
    }

    /// A provider whose key carries nothing but the list, sitting under a
    /// section that is followed by another section.
    #[test]
    fn a_provider_key_alone_does_not_eat_the_section_after_it() {
        // `prune_empty_key` read `indent_of`'s `None` — every section header —
        // as "deeper than the key", so the key's body ran to the end of the
        // file and the next section went with it.
        let doc = "provider_settings:\n  openai:\nmcp:\n  port: 9000\n";
        let out = prune_empty_key(
            &doc.lines().map(str::to_owned).collect::<Vec<_>>(),
            0,
            "openai",
        );
        assert_eq!(
            out,
            vec!["provider_settings:", "mcp:", "  port: 9000"],
            "{out:?}"
        );
        // Mutation control: restore `is_none_or(|d| d > owner_depth)` and this
        // becomes `[provider_settings:, port: 9000]` — the header gone, its body
        // orphaned under the wrong section.
    }

    /// The reader answers "is this glob already excluded" from the file.
    /// The remove writer cannot, though it looks like it can: remove returns
    /// `Some` both when it removed an entry and when it recognised the shape
    /// and removed nothing, so `.is_some()` on it is not "it is there".
    #[test]
    fn the_duplicate_reader_reads_the_file_not_the_remove_writer() {
        assert!(
            nested_list_has_entry(PS_EXISTING, PS_SECTION, Some("openai"), PS_LIST, "gpt-4o"),
            "the glob that is written is reported as written"
        );
        assert!(
            !nested_list_has_entry(PS_EXISTING, PS_SECTION, Some("openai"), PS_LIST, "o3-mini"),
            "and the glob that is not written is not"
        );
        assert!(
            !nested_list_has_entry(P7_BASE, PS_SECTION, Some("openai"), PS_LIST, "*-mini"),
            "and a block that does not exist yet holds no glob"
        );
    }

    #[test]
    fn a_missing_trailing_newline_survives_a_phase_seven_write() {
        let doc = P7_BASE.trim_end();
        let out =
            append_nested_list_entry(doc, CUT_SECTION, None, CUT_SECTION, "openai").expect("write");
        assert!(
            out.starts_with(P7_BASE.trim_end()),
            "the original document is the write's untouched prefix: {out:?}"
        );
        assert!(
            !out.ends_with('\n'),
            "the file's own last byte stays its own: {out:?}"
        );
        assert!(
            out.ends_with("continue_on_cut:\n  - openai"),
            "and the entry is still written: {out:?}"
        );
        // Mutation control: append a newline unconditionally and this fails.
    }
}

#[cfg(all(test, unix))]
mod write_private_tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{write_atomic, write_private};

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ar-private-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    #[test]
    fn write_private_should_create_owner_only_when_target_is_new() {
        let d = dir("new");
        let target = d.join("ar.env");
        write_private(&target, "AR_KEY_X=secret\n").expect("write");
        assert_eq!(mode(&target), 0o600);
        assert_eq!(
            std::fs::read_to_string(&target).expect("read"),
            "AR_KEY_X=secret\n"
        );
    }

    #[test]
    fn write_atomic_should_replace_contents_and_leave_no_temp_file() {
        let d = dir("atomic");
        let target = d.join("config.yaml");
        std::fs::write(&target, "old: 1\n").expect("seed");
        write_atomic(&target, "new: 2\n").expect("write");
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "new: 2\n");
        assert_eq!(std::fs::read_dir(&d).expect("list").count(), 1);
    }

    #[test]
    fn write_private_should_tighten_mode_when_target_was_world_readable() {
        let d = dir("widen");
        let target = d.join("ar.env");
        std::fs::write(&target, "OLD=1\n").expect("seed");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_private(&target, "NEW=2\n").expect("write");
        assert_eq!(mode(&target), 0o600);
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "NEW=2\n");
    }

    #[test]
    fn write_private_should_leave_no_temp_file_behind() {
        let d = dir("tmp");
        let target = d.join("ar.env");
        write_private(&target, "A=1\n").expect("write");
        let leftovers = std::fs::read_dir(&d).expect("list").count();
        assert_eq!(leftovers, 1, "only the target should remain");
    }
}
