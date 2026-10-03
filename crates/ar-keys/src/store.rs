//! The local encrypted credential store: a gitignored sqlite table of `enc:`
//! envelopes, one row per credential name.
//!
//! This is the answer to F-CRIT-2 in `AUDIT-REPORT.md` — "no local credential
//! store" — and it is deliberately a *narrow* one. The store holds the same
//! `keys:` map `config.yaml` already declares, encrypted at rest, plus two
//! non-secret tables beside it. No token material outside `credentials`, no
//! rotation bookkeeping.
//!
//! # The second table: `oauth_sessions`
//!
//! F-MED-2 generated a terminal-status CHECK clause
//! ([`ar_exec::oauth::terminal_check_constraint`]) with nowhere to sit — this
//! is the table it goes in. A row answers three questions about one provider:
//! which `credentials` row its access token lives behind
//! ([`OAuthSession::access_key`], a *name*, never the secret), which one holds
//! its refresh token ([`OAuthSession::refresh_key`], the same), and whether a
//! refresh has retired it ([`OAuthSession::terminal_status`]). That is the whole
//! table: placement plus status. Nothing here decrypts anything, so a leaked
//! `credentials.db` copy leaks no more session material than a leaked
//! `config.yaml` does.
//!
//! The two key columns are also what makes a rotation writable back: the sink
//! below has to know which `credentials` rows to overwrite, and in what order.
//!
//! # The third table: `quota_snapshots`
//!
//! Ported from
//! `../OmniRoute/src/lib/db/migrations/013_quota_snapshots.sql`, which is where
//! this crate's [`ar_route::QuotaWindow`] values were going before anywhere
//! recorded them: [`QuotaWindow`] is config-supplied and carries no history, so
//! "this connection was at 4% an hour ago" had nowhere to live.
//!
//! Same columns as the reference migration — provider, window key, remaining
//! percentage, exhausted flag, reset time, recorded time, and the three indices
//! that make it a time series rather than a log. One deviation, deliberate: the
//! two timestamps are `INTEGER` unix seconds rather than the reference's `TEXT`
//! `datetime('now')`, matching [`crate::Revocation`] and this repository's other
//! tables. A column a time-series query sorts on wants an integer key.
//!
//! Append-only by the same argument as the ledger: a snapshot's value is its
//! timestamp, so rewriting one is destroying evidence rather than correcting a
//! record. [`CredentialStore::latest_quota_snapshot`] is the reader, and it
//! answers the question the routing layer actually asks — how old is the newest
//! reading — through [`QuotaSnapshot::is_stale`].
//!
//! # Being a rotation sink
//!
//! This store implements [`ar_exec::oauth::RotationSink`], which is how a token
//! rotation survives a restart. Without it a rotation lives only in the
//! connection's memory, and the next process reloads the refresh token the
//! refresh already spent — the `refresh_token_reused` cascade arriving by a
//! different road than the rotation pool prevents.
//!
//! The write is guarded compare-and-swap: the row's stored refresh token must
//! still be the one the caller exchanged. A sibling writer that rotated first
//! holds the live token, and overwriting it would revert their rotation.

//! The three session kinds ([`SessionKind`]) — refresh, device, anonymous — are
//! all accepted from day one, so the sibling device/anonymous streams do not need
//! a migration to add their rows.
//!
//! # Envelope discipline, and which half of v1 was copied
//!
//! A row is [`crate::hash::encrypt`]'s `enc:v2:` envelope — the same three-field
//! `enc:v<N>:<nonce>:<ct>:<tag>` grammar OmniRoute's `enc:v1:` uses
//! (`../OmniRoute/src/lib/db/encryption.ts`). What was mirrored is the *shape*:
//! a version tag in the string so a future format is refused by name, and a
//! nonce/ciphertext/tag triple that a reader can parse without the schema.
//!
//! What was **not** mirrored is the derivation. v1 ran
//! `scryptSync(master, "omniroute-field-encryption-v1", 32)` with a 28-byte salt
//! that is a string literal in a public repository, and audit red-team item R3
//! records what that costs: every install on earth derives the same key from the
//! same master, so a leaked master decrypts every install's rows. Here the key
//! comes from [`crate::MasterKey`], which runs argon2id over a per-install
//! [`crate::Salt`], and the row's AAD is `provider|name|v2` so a ciphertext
//! cannot be lifted from one row into another.
//!
//! A v1 string is refused by name as [`crate::KeyError::LegacyV1`], which is the
//! right answer: this build has never written one, so there is nothing to
//! migrate.
//!
//! # Why the salt lives in the database
//!
//! [`KeyMeta::generate`] draws a **fresh** salt every call, and
//! [`MasterKey::new`] derives from the salt it is handed. A store that derived
//! its key from a salt it did not persist would decrypt nothing on the next
//! boot — every row written before the restart is dead, and nothing says so
//! except a tag mismatch. So the metadata is row `id = 1` of `store_meta` and is
//! read back before any envelope is touched. This is what
//! [`KeyMeta::to_json`] exists for.
//!
//! # The file
//!
//! The caller picks the path. `ar` defaults to `credentials.db` beside the
//! config file; `.gitignore` carries `*.db` and the sqlite sidecars, so a
//! checkout cannot commit one.
//!
//! Journal mode is left at SQLite's default rollback journal rather than WAL.
//! WAL buys concurrent-reader throughput, and this store is written a handful of
//! times and read once at boot — while costing two extra sidecar files on disk
//! next to the ones an operator has to keep out of git.
//!
//! ```no_run
//! use ar_keys::{CredentialStore, Secret};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // `AR_MASTER_KEY` carries 32 bytes (hex, base64, or raw).
//! let store = CredentialStore::open_with_env_key(std::path::Path::new("credentials.db"))?;
//! store.insert("openai", "openai", &Secret::new(b"sk-provider".to_vec()))?;
//! assert_eq!(store.list_names()?, ["openai"]);
//! # Ok(())
//! # }
//! ```

use std::path::Path;

use ar_exec::oauth::{OAuthToken, RotationSink};
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::error::KeyError;
use crate::hash;
use crate::secret::{KeyMeta, MasterKey, Secret, decode_key_material};
/// Environment variable holding the store's master key material.
///
/// The value is 32 bytes as hex, base64, or raw — the three encodings
/// [`MasterKey::load`] already accepts from a key file, read by the same
/// [`decode_key_material`]. A `$VAR` rather than a file because `config.yaml`
/// already refuses to carry a secret, and the master key is the one secret that
/// has no config entry to live in; OmniRoute's desktop bootstrap writes the
/// equivalent to a plain dotenv file with default permissions, which is the
/// thing [`crate::MasterKeySource::File`] exists to refuse.
pub const MASTER_KEY_VAR: &str = "AR_MASTER_KEY";

/// The `CHECK (kind IN (...))` list, built from [`SessionKind::ALL`] rather
/// than written out, so a fourth session kind cannot be added to the enum and
/// forgotten here.
fn session_kind_check() -> String {
    format!(
        "CHECK (kind IN ({}))",
        SessionKind::ALL
            .map(|k| format!("'{}'", k.as_str()))
            .join(", ")
    )
}

/// The whole schema, with the terminal CHECK **generated**.
///
/// The one hand-written part is deliberate: `store_meta` holds exactly one row,
/// so the id is pinned to 1 by a CHECK rather than left to a convention nobody
/// would notice breaking.
///
/// The `oauth_sessions` clause is
/// [`ar_exec::oauth::terminal_check_constraint`]'s output verbatim, so the
/// database cannot hold a status the classifier would call retryable. Two
/// consequences of taking the generator's output as-is:
///
/// * it constrains `terminal_status` **and** `terminal_reason` together, because
///   the pair is what a provider sends — so the table carries both columns even
///   though the status is the interesting half;
/// * `terminal_status` is `INTEGER`, not the `TEXT` the clause's literal `400`
///   comparisons might suggest. The literals are integers, and a text column
///   would match them only through SQLite's affinity conversion.
///
/// `CREATE TABLE IF NOT EXISTS` throughout: opening a store written before this
/// table existed adds it, which is the whole upgrade path.
fn schema() -> String {
    format!(
        "
CREATE TABLE IF NOT EXISTS store_meta (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    meta TEXT    NOT NULL
);
CREATE TABLE IF NOT EXISTS credentials (
    name     TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    envelope TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS oauth_sessions (
    provider        TEXT PRIMARY KEY,
    kind            TEXT    NOT NULL {kind_check},
    access_key      TEXT    NULL,
    refresh_key     TEXT    NULL,
    terminal_status INTEGER NULL,
    terminal_reason TEXT    NULL,
    CHECK (kind <> 'anonymous' OR access_key IS NULL),
    {terminal}
);
CREATE TABLE IF NOT EXISTS quota_snapshots (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    provider             TEXT    NOT NULL,
    window_key           TEXT    NOT NULL,
    remaining_percentage REAL,
    exhausted            INTEGER NOT NULL DEFAULT 0,
    next_reset_at        INTEGER NULL,
    recorded_at          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS quota_snapshots_by_provider
    ON quota_snapshots (provider, recorded_at);
CREATE INDEX IF NOT EXISTS quota_snapshots_by_window
    ON quota_snapshots (provider, window_key, recorded_at);
CREATE INDEX IF NOT EXISTS quota_snapshots_by_recorded_at
    ON quota_snapshots (recorded_at);
",
        kind_check = session_kind_check(),
        terminal = ar_exec::oauth::terminal_check_constraint(),
    )
}

/// The `CREATE TABLE IF NOT EXISTS` batch above plus the columns a store written
/// by an earlier build is missing.
///
/// `CREATE TABLE IF NOT EXISTS` is the whole upgrade path for a *new* table, but
/// it does nothing for a column added to a table that already exists — the file
/// keeps the old shape and every query naming the new column fails. So the shape
/// is also asserted: read `PRAGMA table_info` and add what is absent.
///
/// ponytail: one column, so this is one statement and one pragma rather than a
/// migration framework. When a second column ever needs adding, replace the
/// `ALTER_COLUMNS` loop with a real migration table — the check below is what
/// makes each addition idempotent, and that part does not change.
const ADDED_COLUMNS: &[(&str, &str)] = &[("oauth_sessions", "refresh_key TEXT NULL")];

/// Adds any [`ADDED_COLUMNS`] entry the open store's schema does not have yet.
///
/// A file created by this build already has them, so the common path is one
/// pragma read and no writes.
///
/// # Errors
///
/// [`KeyError::Store`] if the pragma or the `ALTER` fails for a reason other than
/// the column already existing.
fn add_missing_columns(conn: &Connection) -> Result<(), KeyError> {
    for (table, column) in ADDED_COLUMNS {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(sql)?;
        let present = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(sql)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(sql)?;
        let name = column.split_whitespace().next().unwrap_or(column);
        if present.iter().any(|held| held == name) {
            continue;
        }
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column};"))
            .map_err(sql)?;
    }
    Ok(())
}

const SELECT_META: &str = "SELECT meta FROM store_meta WHERE id = 1";
const INSERT_META: &str = "INSERT OR REPLACE INTO store_meta (id, meta) VALUES (1, ?1)";

const UPSERT_CREDENTIAL: &str = "
INSERT INTO credentials (name, provider, envelope) VALUES (?1, ?2, ?3)
ON CONFLICT(name) DO UPDATE SET provider = ?2, envelope = ?3
";

const SELECT_CREDENTIAL: &str = "SELECT provider, envelope FROM credentials WHERE name = ?1";
const SELECT_NAMES: &str = "SELECT name FROM credentials ORDER BY name";

const DELETE_CREDENTIAL: &str = "DELETE FROM credentials WHERE name = ?1";

const UPSERT_SESSION: &str = "
INSERT INTO oauth_sessions (provider, kind, access_key, refresh_key, terminal_status, terminal_reason)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT(provider) DO UPDATE SET
    kind = ?2, access_key = ?3, refresh_key = ?4, terminal_status = ?5, terminal_reason = ?6
";

const SELECT_SESSION: &str = "
SELECT provider, kind, access_key, refresh_key, terminal_status, terminal_reason
FROM oauth_sessions WHERE provider = ?1
";

const DELETE_SESSION: &str = "DELETE FROM oauth_sessions WHERE provider = ?1";

const COUNT_SESSIONS: &str = "SELECT count(*) FROM oauth_sessions";

const INSERT_QUOTA_SNAPSHOT: &str = "
INSERT INTO quota_snapshots
    (provider, window_key, remaining_percentage, exhausted, next_reset_at, recorded_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
";

/// Newest first, so `LIMIT 1` *is* read-latest.
///
/// `recorded_at DESC, id DESC` rather than `id DESC` alone: two snapshots taken
/// in the same second are ordered by insertion order, and the row that landed
/// last is the reading that was actually observed last. A tie broken the other
/// way would let an older reading win a refresh.
const SELECT_LATEST_QUOTA_SNAPSHOT: &str = "
SELECT window_key, remaining_percentage, exhausted, next_reset_at, recorded_at
FROM quota_snapshots
WHERE provider = ?1 AND window_key = ?2
ORDER BY recorded_at DESC, id DESC
LIMIT 1
";

/// How long a stored snapshot still counts as a confident reading of a window.
///
/// Ported from
/// `../OmniRoute/open-sse/services/combo/quotaStrategies.ts::QUOTA_WEIGHTED_MAX_SNAPSHOT_AGE_MS`
/// (10 minutes there too). Past it the snapshot says **unknown**, not *empty*:
/// the routing layer drops that connection to its B pool and still sends to it
/// when nothing fresher has room. Collapsing the two — treating a stale row as
/// zero headroom — is what would take a provider offline on a timer rather than
/// on its own quota.
pub const MAX_QUOTA_SNAPSHOT_AGE_SECS: u64 = 10 * 60;

/// One connection's quota reading at one instant.
///
/// The `remaining_percentage` field is `Option<f64>` because "unmetered" and
/// "0% left" are different: [`ar_route::QuotaWindow`] reads a zero limit as
/// unlimited headroom, and a row that stored `0.0` for it would report an
/// unmetered window as drained.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaSnapshot {
    /// Provider the reading came from.
    pub provider: String,
    /// Which window of that provider's quota this describes — a session key, a
    /// model name, a daily bucket. Opaque here on purpose: the store records the
    /// label the caller already has and has no vocabulary for it.
    pub window_key: String,
    /// Fraction of the window still free, in `0.0..=1.0`. `None` for unmetered.
    pub remaining_percentage: Option<f64>,
    /// Whether the window is spent. Read independently of the percentage because
    /// a provider may report "exhausted" without publishing a percentage at all.
    pub exhausted: bool,
    /// Unix seconds at which the window rolls over. `None` when unpublished.
    pub next_reset_at: Option<u64>,
    /// Unix seconds this reading was taken.
    pub recorded_at: u64,
}

impl QuotaSnapshot {
    /// Builds a snapshot, taking the two timestamps as unix seconds.
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        window_key: impl Into<String>,
        remaining_percentage: Option<f64>,
        exhausted: bool,
        next_reset_at: Option<u64>,
        recorded_at: u64,
    ) -> Self {
        Self {
            provider: provider.into(),
            window_key: window_key.into(),
            remaining_percentage,
            exhausted,
            next_reset_at,
            recorded_at,
        }
    }

    /// Seconds since this reading was taken, saturating at zero for a clock that
    /// moved backwards.
    ///
    /// Saturating rather than wrapping: a `recorded_at` in the future reads as
    /// brand new, which routes it into the confident pool for a moment. A wrapped
    /// `u64` would read as ancient and drop the connection for a reason that does
    /// not exist.
    #[must_use]
    pub fn age_secs(&self, now_secs: u64) -> u64 {
        now_secs.saturating_sub(self.recorded_at)
    }

    /// Whether this reading has aged past [`MAX_QUOTA_SNAPSHOT_AGE_SECS`], i.e.
    /// whether it should now read as unknown headroom rather than as a number.
    #[must_use]
    pub fn is_stale(&self, now_secs: u64) -> bool {
        self.age_secs(now_secs) > MAX_QUOTA_SNAPSHOT_AGE_SECS
    }
}

/// The session row's placement, plus the envelope behind its refresh row.
///
/// The refresh envelope travels with the placement because the CAS guard needs
/// to compare the *stored* refresh token against the one a refresh presented, and
/// a separate read would leave a window between "compared" and "wrote" that a
/// sibling process could slip a rotation through. `NULL` for the envelope is a
/// row whose credential is not in this store — legal, and exactly the state
/// [`SessionKind::Refresh`] allows by leaving `access_key` unset.
const SELECT_SESSION_PLACEMENT: &str = "
SELECT access_key, refresh_key,
       (SELECT envelope FROM credentials WHERE name = oauth_sessions.refresh_key)
FROM oauth_sessions WHERE provider = ?1 AND kind = 'refresh'
";

/// Which of the three shapes an OAuth session takes.
///
/// # Why this is not `ar_exec::oauth::OAuthKind`
///
/// That enum is the *provider family* — codex / cline / claude / gemini-cli /
/// cursor — which drives the carve-out table. This is the *mechanism* axis, and
/// the two cross: a `cursor` session can be any of the three below. `ar-exec`
/// exposes no session-kind vocabulary, so these three spellings are this crate's
/// own; they are generated into the table's CHECK, which is what makes a fourth
/// kind a compile error rather than a silent hole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    /// Holds a refresh token, behind a credential row, and can renew itself.
    ///
    /// A missing `access_key` is legal and deliberate, not an oversight: audit
    /// red-team R1 records two live `kilocode` sessions with no stored refresh
    /// token that are nonetheless active, so the table has to be able to record
    /// "a refresh session whose credential is not in this store".
    Refresh,
    /// Authorization came from a device-code poll rather than a browser redirect.
    ///
    /// The poll state is **not** a column, and the reason is specific rather than
    /// cautious: RFC 8628 §3.5's four poll codes (`authorization_pending`,
    /// `slow_down`, `expired_token`, `access_denied`) are matched by
    /// `ar_exec::oauth`'s private `DeviceReply` enum, which exposes no accessor
    /// and no list — so there is no vocabulary here to constrain a column against,
    /// and writing one by hand is the exact drift
    /// [`ar_exec::oauth::terminal_check_constraint`] exists to prevent.
    ///
    /// TODO(#F-MED-2-device): add a `poll_state TEXT NULL` column once
    /// `DeviceReply` is public (or gains a generator of its own, as
    /// `terminal_check_constraint` does). Until then a device row records its
    /// outcome as a terminal status — `expired_token` and `access_denied` are
    /// terminal, and the two loop codes are not failures at all.
    Device,
    /// A browser session with no refresh token and no stored credential.
    Anonymous,
}

impl SessionKind {
    /// Every kind, so the CHECK and any caller enumerate one list.
    pub const ALL: [Self; 3] = [Self::Refresh, Self::Device, Self::Anonymous];

    /// The spelling stored in, and read from, the `kind` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refresh => "refresh",
            Self::Device => "device",
            Self::Anonymous => "anonymous",
        }
    }

    /// The kind for a stored spelling, or `None` for a column this build does
    /// not know.
    ///
    /// Over [`Self::ALL`] rather than a string `match`, so a new variant with no
    /// parse arm fails to compile instead of quietly answering `None`.
    #[must_use]
    pub fn parse(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == kind)
    }

    /// Whether a session of this kind may point at a credential row.
    ///
    /// False for [`Self::Anonymous`]: there is no stored secret to point at, so
    /// a placement claims a protection that does not exist.
    #[must_use]
    pub fn allows_credential(self) -> bool {
        !matches!(self, Self::Anonymous)
    }
}

/// One `oauth_sessions` row: placement plus status, never a secret.
///
/// Build it with [`Self::new`] and add what is known — a session exists from the
/// moment a login lands, and its terminal status arrives later, from a refresh
/// that failed unrecoverably. A half-written row is therefore normal state, not a
/// mistake, which is why every field past `kind` is optional.
///
/// `Debug` is derived and safe: every field is a provider id, a kind, a
/// `keys:` label, or a numeric status and reason string. None of them is
/// decryptable, which is the reason this type is not a [`crate::Secret`] — see
/// [`Self::access_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthSession {
    provider: String,
    kind: SessionKind,
    access_key: Option<String>,
    refresh_key: Option<String>,
    terminal_status: Option<u16>,
    terminal_reason: Option<String>,
}

impl OAuthSession {
    /// A session of `kind` for `provider`, with nothing recorded yet beyond that.
    #[must_use]
    pub fn new(provider: impl Into<String>, kind: SessionKind) -> Self {
        Self {
            provider: provider.into(),
            kind,
            access_key: None,
            refresh_key: None,
            terminal_status: None,
            terminal_reason: None,
        }
    }

    /// Records which `credentials` row holds the session's token.
    ///
    /// A *name*, not the secret: the session table stores where the token lives,
    /// and the token itself stays in `credentials` under the same master key and
    /// the same `enc:v2:` envelope as any other row. Rejected for
    /// [`SessionKind::Anonymous`] by [`crate::CredentialStore::insert_session`],
    /// as [`KeyError::AnonymousCredential`].
    #[must_use]
    pub fn with_access_key(mut self, name: impl Into<String>) -> Self {
        self.access_key = Some(name.into());
        self
    }

    /// Records which `credentials` row holds the session's *refresh* token.
    ///
    /// The second name a refresh session needs, and the reason a rotation can be
    /// written back: a renewed access token lands under [`Self::with_access_key`]
    /// and a renewed refresh token under this one, so the two halves are separate
    /// rows and a refresh that only rotates one of them has to know where.
    ///
    /// A *name*, like [`Self::with_access_key`] — never the secret. Absent means
    /// "this session has no refresh row in this store", which the store can hold
    /// and a rotation can still record: it writes the access row and leaves the
    /// refresh token where it was.
    #[must_use]
    pub fn with_refresh_key(mut self, name: impl Into<String>) -> Self {
        self.refresh_key = Some(name.into());
        self
    }

    /// Records that a refresh retired the session.
    ///
    /// Both halves together and always: the generated CHECK matches a
    /// `(status, reason)` pair, so a status with no reason — or a reason with no
    /// status — matches no row and is refused by the database. Keeping them in
    /// one builder means the public API cannot construct one.
    ///
    /// A `(status, reason)` the classifier would call *transient* is refused here
    /// too, by the same CHECK. Retiring a session on a transient is the failure
    /// F-HIGH-4 warns about, so the store is not willing to record it.
    #[must_use]
    pub fn terminal(mut self, status: u16, reason: impl Into<String>) -> Self {
        self.terminal_status = Some(status);
        self.terminal_reason = Some(reason.into());
        self
    }

    /// The registry provider id this session belongs to.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Which of the three session shapes this is.
    #[must_use]
    pub fn kind(&self) -> SessionKind {
        self.kind
    }

    /// The `credentials` name holding this session's token, when it has one.
    #[must_use]
    pub fn access_key(&self) -> Option<&str> {
        self.access_key.as_deref()
    }

    /// The `credentials` name holding this session's refresh token, when it has
    /// one.
    #[must_use]
    pub fn refresh_key(&self) -> Option<&str> {
        self.refresh_key.as_deref()
    }

    /// The refresh-endpoint status that retired the session, when it has one.
    #[must_use]
    pub fn terminal_status(&self) -> Option<u16> {
        self.terminal_status
    }

    /// The reason paired with [`Self::terminal_status`].
    #[must_use]
    pub fn terminal_reason(&self) -> Option<&str> {
        self.terminal_reason.as_deref()
    }
}

/// A sqlite-backed table of `enc:` credential envelopes.
///
/// A row is `(name, provider, envelope)`, and `name` is the key name
/// `config.yaml` declares under `keys:` — so this store *is* that map, encrypted,
/// with the same one-name-one-value semantics. A provider that shares a
/// credential between two entries shares the name, exactly as it does today.
///
/// Not `Debug`: [`MasterKey`] has no printable form and neither should anything
/// holding one.
///
/// Blocking: `rusqlite` is synchronous, like [`crate::Revocation`]. Nothing on
/// the request path calls it — the store is read once while a config is built.
pub struct CredentialStore {
    conn: Connection,
    master: MasterKey,
}

impl CredentialStore {
    /// Opens (or creates) the store at `path` under the key in `material`.
    ///
    /// Re-derives under the salt the store persisted, and persists a fresh one
    /// when the file is new. See the module docs for why that round trip is the
    /// difference between a store and a heap of ciphertext.
    ///
    /// # Errors
    ///
    /// [`KeyError::BadMasterKeyLen`] if `material` is not [`crate::KEY_LEN`],
    /// [`KeyError::Crypto`] if the persisted metadata names a different envelope
    /// version than this build, and [`KeyError::Store`] if the file or the
    /// schema cannot be opened.
    pub fn open_with_material(path: &Path, material: &Secret) -> Result<Self, KeyError> {
        let conn = Connection::open(path).map_err(sql)?;
        conn.execute_batch(&schema()).map_err(sql)?;
        add_missing_columns(&conn)?;
        let master = match read_meta(&conn)? {
            Some(meta) => MasterKey::new(material.to_owned_secret(), meta)?,
            None => {
                let master = MasterKey::new(material.to_owned_secret(), KeyMeta::generate())?;
                conn.execute(INSERT_META, [master.meta().to_json()?])
                    .map_err(sql)?;
                master
            }
        };
        Ok(Self { conn, master })
    }

    /// [`Self::open_with_material`] with the key material from
    /// [`MASTER_KEY_VAR`].
    ///
    /// # Errors
    ///
    /// As [`Self::open_with_material`], plus [`KeyError::MasterKey`] when the
    /// variable is unset or holds something that is not 32 bytes of hex, base64
    /// or raw key. The reason names the variable and never echoes its value.
    pub fn open_with_env_key(path: &Path) -> Result<Self, KeyError> {
        let raw = std::env::var(MASTER_KEY_VAR).map_err(|_| {
            KeyError::MasterKey(format!(
                "{MASTER_KEY_VAR} is unset; the store cannot be read without it"
            ))
        })?;
        let material = Secret::from_slice(&decode_key_material(raw.trim())?)?;
        Self::open_with_material(path, &material)
    }

    /// A private in-memory store, for a dry run that must not touch disk.
    ///
    /// # Errors
    ///
    /// As [`Self::open_with_material`], minus the file half.
    pub fn open_in_memory(material: &Secret) -> Result<Self, KeyError> {
        let conn = Connection::open_in_memory().map_err(sql)?;
        conn.execute_batch(&schema()).map_err(sql)?;
        add_missing_columns(&conn)?;
        let master = MasterKey::new(material.to_owned_secret(), KeyMeta::generate())?;
        Ok(Self { conn, master })
    }

    /// Encrypts `secret` under `name` and stores it, replacing any row of that
    /// name.
    ///
    /// `provider` is not metadata: it is half the AAD
    /// (`provider|name|v2`), so changing it on an existing name makes the old
    /// ciphertext undecryptable rather than silently reinterpretable.
    ///
    /// # Errors
    ///
    /// [`KeyError::Malformed`] if `provider` or `name` contains `|`,
    /// [`KeyError::Crypto`] if the cipher refuses, and [`KeyError::Store`] on a
    /// write failure.
    pub fn insert(&self, provider: &str, name: &str, secret: &Secret) -> Result<(), KeyError> {
        let envelope = hash::encrypt(&self.master, provider, name, secret)?;
        self.conn
            .execute(UPSERT_CREDENTIAL, params![name, provider, envelope])
            .map_err(sql)?;
        Ok(())
    }

    /// The plaintext behind `name`, or `None` when no row carries it.
    ///
    /// # Errors
    ///
    /// [`KeyError::TagMismatch`] when the row was written under a different
    /// master or a different `provider`, or was edited in place — one error for
    /// all three, so a reader cannot tell a wrong key from a tampered row.
    /// [`KeyError::Store`] on a read failure.
    pub fn get(&self, name: &str) -> Result<Option<Secret>, KeyError> {
        let row = self
            .conn
            .query_row(SELECT_CREDENTIAL, [name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .optional()
            .map_err(sql)?;
        let Some((provider, envelope)) = row else {
            return Ok(None);
        };
        // The AAD is rebuilt from the row's own `provider`, so an envelope lifted
        // into another row cannot authenticate. This is the v1 splice, closed.
        Ok(Some(hash::decrypt(
            &self.master,
            &provider,
            name,
            &envelope,
        )?))
    }

    /// [`Self::get`] as a header value.
    ///
    /// # Errors
    ///
    /// As [`Self::get`], plus [`KeyError::Store`] when the plaintext is not
    /// UTF-8. Refused rather than lossily replaced: this string becomes an
    /// `Authorization` header, and a U+FFFD in one is a 401 from the provider
    /// with nothing on this side to explain it.
    pub fn get_text(&self, name: &str) -> Result<Option<String>, KeyError> {
        match self.get(name)? {
            None => Ok(None),
            Some(secret) => String::from_utf8(secret.as_bytes().to_vec())
                .map(Some)
                .map_err(|_| KeyError::Store(format!("credential {name:?} is not valid UTF-8"))),
        }
    }

    /// Forgets the row named `name`, reporting whether one was there.
    ///
    /// A logout has to remove the row, not overwrite it with an empty value: an
    /// empty credential still resolves, still gets sent upstream, and turns a
    /// deliberate logout into a silent 401 that reads as a provider problem.
    ///
    /// # Errors
    ///
    /// [`KeyError::Store`] on a write failure.
    pub fn remove(&self, name: &str) -> Result<bool, KeyError> {
        self.conn
            .execute(DELETE_CREDENTIAL, [name])
            .map(|n| n > 0)
            .map_err(sql)
    }

    /// Every credential name in the store, sorted.
    ///
    /// Names are not secret — they are the `keys:` labels already in
    /// `config.yaml` — which is what lets `ar doctor` report what a store holds
    /// without decrypting a thing.
    ///
    /// # Errors
    ///
    /// [`KeyError::Store`] on a read failure.
    pub fn list_names(&self) -> Result<Vec<String>, KeyError> {
        let mut stmt = self.conn.prepare(SELECT_NAMES).map_err(sql)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql)?;
        rows.collect::<Result<Vec<String>, _>>().map_err(sql)
    }

    /// Records `session`, replacing any row of the same provider.
    ///
    /// Replace rather than merge, exactly as [`Self::insert`] does for a
    /// credential name: the row is the current state of one session, so a caller
    /// that knows less than the stored row writes what it knows. That is also why
    /// an upsert — the usual reason to reach for one is recording a terminal
    /// status some time after the login landed.
    ///
    /// Nothing is encrypted here and nothing needs to be: every field is a label
    /// or a status, so the session row is readable by anyone holding the file.
    ///
    /// # Errors
    ///
    /// [`KeyError::AnonymousCredential`] when an [`SessionKind::Anonymous`] row
    /// carries an access key — checked here so the caller learns which rule it
    /// broke, with the table's CHECK as the invariant behind it.
    /// [`KeyError::Store`] when the generated terminal CHECK refuses the
    /// `(status, reason)` pair, which is how a transient status is kept out of a
    /// durable retirement.
    pub fn insert_session(&self, session: &OAuthSession) -> Result<(), KeyError> {
        if let (SessionKind::Anonymous, Some(placement)) =
            (session.kind, session.access_key.as_deref())
        {
            return Err(KeyError::AnonymousCredential {
                placement: placement.to_owned(),
            });
        }
        self.conn
            .execute(
                UPSERT_SESSION,
                params![
                    session.provider,
                    session.kind.as_str(),
                    session.access_key,
                    session.refresh_key,
                    session.terminal_status,
                    session.terminal_reason,
                ],
            )
            .map_err(sql)?;
        Ok(())
    }

    /// The session recorded for `provider`, or `None` when the store holds none.
    ///
    /// # Errors
    ///
    /// [`KeyError::Store`] on a read failure, including a `kind` column this build
    /// does not recognise — which the CHECK makes unreachable through the API and
    /// only an out-of-band edit could produce.
    pub fn get_session(&self, provider: &str) -> Result<Option<OAuthSession>, KeyError> {
        let row = self
            .conn
            .query_row(SELECT_SESSION, [provider], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .optional()
            .map_err(sql)?;
        row.map(read_session).transpose()
    }

    /// Forgets the session row for `provider`, reporting whether one was there.
    ///
    /// [`Self::remove`]'s reasoning applies unchanged: a session that is logged
    /// out must leave no row, or the next login is refused by a leftover
    /// placement or a leftover terminal status.
    ///
    /// # Errors
    ///
    /// [`KeyError::Store`] on a write failure.
    pub fn remove_session(&self, provider: &str) -> Result<bool, KeyError> {
        self.conn
            .execute(DELETE_SESSION, [provider])
            .map(|n| n > 0)
            .map_err(sql)
    }

    /// How many session rows the store holds, for `ar doctor`'s `store` row.
    ///
    /// A count and not a list on purpose: doctor already reports the credential
    /// names it can act on, and the session rows are placement the operator
    /// cannot change from there — the useful question is "are there any, and how
    /// many are retired", not what each one says.
    ///
    /// # Errors
    ///
    /// [`KeyError::Store`] on a read failure.
    pub fn session_count(&self) -> Result<usize, KeyError> {
        self.conn
            .query_row(COUNT_SESSIONS, [], |row| row.get::<_, i64>(0))
            .map(|n| n.max(0) as usize)
            .map_err(sql)
    }

    /// Where one provider's renewal lands: the two `credentials` row names and
    /// the refresh token currently behind the second one.
    fn placement(&self, provider: &str) -> Result<Option<Placement>, KeyError> {
        let row = self
            .conn
            .query_row(SELECT_SESSION_PLACEMENT, [provider], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .optional()
            .map_err(sql)?;
        let Some((access_key, refresh_key, envelope)) = row else {
            return Ok(None);
        };
        // Decrypted here rather than compared as ciphertext: the AAD binds the
        // envelope to `provider|refresh_key`, so an envelope we cannot read is a
        // store problem, not a rotation. `None` for a row with no credential is
        // the normal case this table is allowed to hold.
        let stored = match (refresh_key.as_deref(), envelope) {
            (Some(name), Some(envelope)) => {
                Some(hash::decrypt(&self.master, provider, name, &envelope)?)
            }
            _ => None,
        };
        Ok(Some(Placement {
            access_key,
            refresh_key,
            stored_refresh: stored,
        }))
    }

    /// Appends one quota reading.
    ///
    /// Append and never replace: a snapshot's value is its timestamp, so a second
    /// reading of the same window is new evidence rather than a correction. The
    /// reader below is what answers "what is it now" — the writer has no opinion
    /// about which reading supersedes which, because only the clock does.
    ///
    /// # Errors
    ///
    /// [`KeyError::Store`] on a write failure.
    pub fn insert_quota_snapshot(&self, snapshot: &QuotaSnapshot) -> Result<(), KeyError> {
        self.conn
            .execute(
                INSERT_QUOTA_SNAPSHOT,
                params![
                    snapshot.provider,
                    snapshot.window_key,
                    snapshot.remaining_percentage,
                    i64::from(snapshot.exhausted),
                    snapshot.next_reset_at.map(to_i64),
                    to_i64(snapshot.recorded_at),
                ],
            )
            .map_err(sql)?;
        Ok(())
    }

    /// The newest reading for one provider/window pair, or `None` when none was
    /// ever recorded.
    ///
    /// "Newest" is [`SELECT_LATEST_QUOTA_SNAPSHOT`]'s order — `recorded_at`
    /// descending, insertion order breaking a tie — so the answer is the last
    /// observation rather than the last insert.
    ///
    /// Callers get [`QuotaSnapshot::is_stale`] to decide whether it is still
    /// confident headroom; a stale snapshot is returned, not hidden, because
    /// "unknown" and "absent" are different answers and the routing layer
    /// distinguishes them.
    ///
    /// # Errors
    ///
    /// [`KeyError::Store`] on a read failure.
    pub fn latest_quota_snapshot(
        &self,
        provider: &str,
        window_key: &str,
    ) -> Result<Option<QuotaSnapshot>, KeyError> {
        let row = self
            .conn
            .query_row(
                SELECT_LATEST_QUOTA_SNAPSHOT,
                params![provider, window_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<f64>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(sql)?;
        row.map(
            |(window_key, remaining, exhausted, next_reset_at, recorded_at)| {
                Ok(QuotaSnapshot {
                    provider: provider.to_owned(),
                    window_key,
                    remaining_percentage: remaining,
                    exhausted: exhausted != 0,
                    next_reset_at: next_reset_at.map(|n| u64::try_from(n).unwrap_or(0)),
                    recorded_at: u64::try_from(recorded_at).unwrap_or(0),
                })
            },
        )
        .transpose()
    }

    /// How many quota rows the store holds, for `ar doctor`.
    ///
    /// # Errors
    ///
    /// [`KeyError::Store`] on a read failure.
    pub fn quota_snapshot_count(&self) -> Result<usize, KeyError> {
        self.conn
            .query_row("SELECT count(*) FROM quota_snapshots", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|n| n.max(0) as usize)
            .map_err(sql)
    }
}

/// One provider's row names plus the refresh token currently stored behind them.
///
/// Resolved in a single read by [`SELECT_SESSION_PLACEMENT`] so the compare and
/// the write that follows it are as close together as sqlite allows.
struct Placement {
    access_key: Option<String>,
    refresh_key: Option<String>,
    stored_refresh: Option<Secret>,
}

/// Rebuilds a session from a row of [`SELECT_SESSION`].
///
/// A `u16` the schema cannot hold is a store error rather than a saturating cast:
/// the column only ever received a `u16` through this API, so a wider value means
/// the file was edited by something that is not this build.
fn read_session(
    row: (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
    ),
) -> Result<OAuthSession, KeyError> {
    let (provider, kind, access_key, refresh_key, terminal_status, terminal_reason) = row;
    let kind = SessionKind::parse(&kind).ok_or_else(|| {
        KeyError::Store(format!(
            "oauth session {provider:?} has unknown kind {kind:?}"
        ))
    })?;
    let terminal_status = terminal_status
        .map(|status| {
            u16::try_from(status).map_err(|_| {
                KeyError::Store(format!("oauth session {provider:?} has status {status}"))
            })
        })
        .transpose()?;
    Ok(OAuthSession {
        provider,
        kind,
        access_key,
        refresh_key,
        terminal_status,
        terminal_reason,
    })
}

impl std::fmt::Debug for CredentialStore {
    /// Names the type and nothing else. `MasterKey` has no printable form by
    /// design, and every field this holds is either that key or ciphertext; the
    /// one thing an operator diagnosing a store failure needs is that the value
    /// exists, which the type name already says.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialStore").finish_non_exhaustive()
    }
}

/// The credential store as `ar-exec`'s rotation sink.
///
/// A rotation that reaches the network but not the store leaves a spent refresh
/// token on disk, and the next process to load it spends a second refresh-token
/// use on a token the provider has already retired — the `refresh_token_reused`
/// cascade the rotation pool exists to prevent, arriving by a different road. So
/// the write is part of the rotation, not a follow-up to it.
///
/// # The compare-and-swap
///
/// `presented` is the refresh token the caller exchanged. If the store already
/// holds a *different* one, a sibling writer rotated past us and this write is
/// skipped: overwriting would revert their rotation, and they hold the live
/// token. The reference guard's `#4038` case, where a sibling process, a
/// concurrent health check or a replica lands a fresher rotation between the
/// caller's read of the row and its write.
///
/// Skipping is best-effort, never blocking: a row this store cannot read is
/// written anyway, because a failed read is a store problem and refusing to
/// persist would turn it into a session that dies on the next boot.
impl RotationSink for CredentialStore {
    fn persist_rotation(
        &self,
        provider: &str,
        presented: Option<&str>,
        renewed: &OAuthToken,
    ) -> bool {
        match self.write_rotation(provider, presented, renewed) {
            Ok(written) => written,
            Err(e) => {
                // The rotation itself succeeded upstream; failing to record it is
                // worth an operator's attention but is not this caller's problem,
                // and there is nothing it could do about it.
                tracing::warn!(provider, error = %e, "rotated tokens could not be persisted");
                false
            }
        }
    }
}

impl CredentialStore {
    fn write_rotation(
        &self,
        provider: &str,
        presented: Option<&str>,
        renewed: &OAuthToken,
    ) -> Result<bool, KeyError> {
        let Some(placement) = self.placement(provider)? else {
            // No session row: nothing declares where this provider's tokens live,
            // so there is no non-secret way to name a row to write. Refusing is
            // right — a guessed name would encrypt a token against an AAD no
            // reader could reproduce.
            tracing::debug!(provider, "no oauth_sessions row; rotation not persisted");
            return Ok(false);
        };

        // The CAS itself. `presented` is `None` only when the session had no
        // refresh half to exchange, in which case there is nothing to compare and
        // nothing a concurrent rotation could have invalidated.
        if let Some(presented) = presented
            && placement
                .stored_refresh
                .as_ref()
                .is_some_and(|stored| stored.as_bytes() != presented.as_bytes())
        {
            return Ok(false);
        }

        let mut written = false;
        if let Some(name) = placement.access_key.as_deref() {
            self.insert(provider, name, &row(renewed.access().expose().as_bytes()))?;
            written = true;
        }
        if let (Some(name), Some(refresh)) = (placement.refresh_key.as_deref(), renewed.refresh()) {
            self.insert(provider, name, &row(refresh.expose().as_bytes()))?;
            written = true;
        }
        Ok(written)
    }
}

/// Provider token bytes as a storable row.
///
/// Takes bytes rather than either `Secret` type on purpose: `ar-exec`'s is an
/// `ar-config` `String` and this crate's is zeroizing bytes, and naming either
/// would pin the copy to one of them. This is the crossing point, and it is the
/// same conversion `ar auth login` performs when it persists a fresh login.
fn row(bytes: &[u8]) -> Secret {
    Secret::new(bytes.to_vec())
}

/// The persisted argon2id metadata, or `None` for a store this build is creating.
fn read_meta(conn: &Connection) -> Result<Option<KeyMeta>, KeyError> {
    let json = conn
        .query_row(SELECT_META, [], |row| row.get::<_, String>(0))
        .optional()
        .map_err(sql)?;
    json.map(|raw| KeyMeta::from_json(&raw)).transpose()
}

/// `rusqlite` has no `Clone`/`PartialEq`, and `KeyError` promises both, so the
/// store's own failures are carried as text. The text is SQLite's, not a value
/// from the database.
fn sql(e: rusqlite::Error) -> KeyError {
    KeyError::Store(e.to_string())
}

/// Unix seconds to sqlite's signed integer, saturating rather than wrapping.
///
/// The only way a timestamp exceeds `i64::MAX` is a caller passing a nonsense
/// year, and a saturated `i64::MAX` sorts last — where an old reading belongs.
/// Wrapping would land it in 1966 and make a fresh snapshot read as ancient.
fn to_i64(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX)
}

// The store moves into a worker that builds a config; `rusqlite::Connection` is
// `Send` but not `Sync`, and `MasterKey` is both. Prove the one that matters.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<CredentialStore>();
};

#[cfg(test)]
mod tests {
    use super::{
        CredentialStore, MASTER_KEY_VAR, MAX_QUOTA_SNAPSHOT_AGE_SECS, OAuthSession, QuotaSnapshot,
        SessionKind, sql,
    };
    use crate::codec::hex;
    use crate::error::KeyError;
    use crate::secret::Secret;
    use ar_exec::oauth::{OAuthToken, ProviderSecret, RotationSink};

    fn store() -> CredentialStore {
        CredentialStore::open_in_memory(&Secret::generate()).expect("in-memory store")
    }

    #[test]
    fn round_trips_a_credential() {
        let s = store();
        s.insert("openai", "openai", &Secret::new(b"sk-provider".to_vec()))
            .expect("insert");
        assert_eq!(
            s.get("openai").expect("get").expect("a row").as_bytes(),
            b"sk-provider"
        );
    }

    #[test]
    fn reports_no_row_when_the_name_is_absent() {
        assert!(store().get("nope").expect("get").is_none());
    }

    #[test]
    fn lists_every_name_when_queried() {
        let s = store();
        s.insert("openai", "b", &Secret::generate())
            .expect("insert");
        s.insert("anthropic", "a", &Secret::generate())
            .expect("insert");
        assert_eq!(s.list_names().expect("names"), ["a", "b"]);
    }

    #[test]
    fn replaces_a_credential_when_inserted_again() {
        let s = store();
        s.insert("openai", "k", &Secret::new(b"first".to_vec()))
            .expect("insert");
        s.insert("openai", "k", &Secret::new(b"second".to_vec()))
            .expect("insert");
        assert_eq!(
            s.get("k").expect("get").expect("a row").as_bytes(),
            b"second"
        );
    }

    #[test]
    fn stores_the_envelope_and_never_the_plaintext() {
        let s = store();
        s.insert("openai", "k", &Secret::new(b"sk-plaintext".to_vec()))
            .expect("insert");
        let row: String = s
            .conn
            .query_row(
                "SELECT envelope FROM credentials WHERE name = 'k'",
                [],
                |r| r.get(0),
            )
            .expect("row");
        assert!(row.starts_with("enc:v2:"), "{row}");
        assert!(!row.contains("sk-plaintext"), "{row}");
    }

    #[test]
    fn refuses_a_credential_when_the_provider_does_not_match_the_aad() {
        let s = store();
        s.insert("openai", "k", &Secret::new(b"sk-x".to_vec()))
            .expect("insert");
        s.conn
            .execute(
                "UPDATE credentials SET provider = 'groq' WHERE name = 'k'",
                [],
            )
            .expect("tamper");
        assert!(matches!(s.get("k"), Err(KeyError::TagMismatch)));
    }

    #[test]
    fn reopens_under_the_salt_it_persisted() {
        // The regression this store exists to avoid: `KeyMeta::generate` draws a
        // fresh salt, so a store that derived from one it did not persist reads
        // back as a tag mismatch on every row after a restart.
        let path = std::env::temp_dir().join(format!("ar-keys-salt-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let material = Secret::generate();
        let first = CredentialStore::open_with_material(&path, &material).expect("first open");
        first
            .insert("openai", "openai", &Secret::new(b"sk-persisted".to_vec()))
            .expect("insert");
        drop(first);

        let second = CredentialStore::open_with_material(&path, &material).expect("reopen");
        assert_eq!(
            second
                .get("openai")
                .expect("get")
                .expect("a row")
                .as_bytes(),
            b"sk-persisted"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refuses_a_store_opened_under_a_foreign_master_key() {
        let path = std::env::temp_dir().join(format!("ar-keys-foreign-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mine = Secret::generate();
        let store = CredentialStore::open_with_material(&path, &mine).expect("open");
        store
            .insert("openai", "openai", &Secret::new(b"sk-x".to_vec()))
            .expect("insert");
        drop(store);

        let theirs =
            CredentialStore::open_with_material(&path, &Secret::generate()).expect("reopen");
        assert!(matches!(theirs.get("openai"), Err(KeyError::TagMismatch)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refuses_to_open_when_the_file_is_not_a_database() {
        let path = std::env::temp_dir().join(format!("ar-keys-garbage-{}.db", std::process::id()));
        std::fs::write(&path, b"not a sqlite file at all").expect("write");
        let e =
            CredentialStore::open_with_material(&path, &Secret::generate()).expect_err("garbage");
        assert!(matches!(e, KeyError::Store(_)), "{e}");
        let _ = std::fs::remove_file(&path);
    }

    /// The only test in this binary that touches the process environment, so
    /// there is no second writer to race it. `set_var` is `unsafe` under
    /// edition 2024's `std::env` contract, and a parallel test reading the same
    /// name is exactly the unsound case — hence one test, not two.
    #[test]
    fn reads_a_master_key_given_as_hex_in_the_environment() {
        let path = std::env::temp_dir().join(format!("ar-keys-env-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let material = Secret::generate();
        // SAFETY: the only writer and reader of this name in this test binary.
        unsafe { std::env::set_var(MASTER_KEY_VAR, hex::encode(material.as_bytes())) };
        let store = CredentialStore::open_with_env_key(&path).expect("open from $VAR");
        store
            .insert("openai", "openai", &Secret::new(b"sk-env".to_vec()))
            .expect("insert");
        assert_eq!(
            store.get("openai").expect("get").expect("a row").as_bytes(),
            b"sk-env"
        );
        drop(store);

        // SAFETY: as above, and this thread is the only one that can observe it.
        unsafe { std::env::remove_var(MASTER_KEY_VAR) };
        let e = CredentialStore::open_with_env_key(&path).expect_err("unset now");
        assert!(matches!(e, KeyError::MasterKey(_)), "{e}");
        assert!(
            e.to_string().contains(MASTER_KEY_VAR),
            "the reason names the variable: {e}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reports_a_sqlite_failure_as_a_store_error_carrying_no_value() {
        let s = store();
        s.insert("openai", "k", &Secret::new(b"sk-value".to_vec()))
            .expect("insert");
        let e = s.get("k' OR 1=1 --").expect("parameterised, so a miss");
        assert!(e.is_none(), "{e:?}");
        let rendered = sql(rusqlite::Error::InvalidQuery).to_string();
        assert!(!rendered.contains("sk-value"), "{rendered}");
    }

    /// One session's rows written to a store on disk: the access row, the refresh
    /// row, and the placement that names both.
    fn seeded_store(
        tag: &str,
        access: &str,
        refresh: &str,
    ) -> (std::path::PathBuf, Secret, CredentialStore) {
        let path =
            std::env::temp_dir().join(format!("ar-keys-rotate-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let material = Secret::generate();
        let store = CredentialStore::open_with_material(&path, &material).expect("on-disk store");
        store
            .insert_session(
                &OAuthSession::new("codex", SessionKind::Refresh)
                    .with_access_key("codex")
                    .with_refresh_key("codex_refresh"),
            )
            .expect("session row");
        store
            .insert("codex", "codex", &Secret::new(access.as_bytes().to_vec()))
            .expect("access row");
        store
            .insert(
                "codex",
                "codex_refresh",
                &Secret::new(refresh.as_bytes().to_vec()),
            )
            .expect("refresh row");
        (path, material, store)
    }

    /// Every rotation test here goes through a *reopened* store rather than reading
    /// the one that wrote: the claim is that the rotation outlives the process that
    /// made it, and reading the writer's own handle would pass even if nothing were
    /// committed.
    fn reopened(path: &std::path::Path, material: &Secret) -> CredentialStore {
        CredentialStore::open_with_material(path, material).expect("reopen")
    }

    #[tokio::test]
    async fn writes_the_rotated_token_so_a_fresh_process_reads_the_new_one() {
        // The end-to-end claim: a rotation that happened survives a restart. The
        // provider, the token exchanged, and the token received are what the
        // compare-and-swap reads, so asserting the write asserts the guard's input.
        let (path, material, store) = seeded_store("fresh", "at-spent", "rt-spent");

        let renewed = OAuthToken::new(ProviderSecret::new("at-renewed"))
            .with_refresh(ProviderSecret::new("rt-renewed"));
        let written = store.persist_rotation("codex", Some("rt-spent"), &renewed);
        drop(store);

        assert!(written, "an unchanged row takes the write");
        let later = reopened(&path, &material);
        assert_eq!(
            later
                .get("codex_refresh")
                .expect("refresh row")
                .expect("a row")
                .as_bytes(),
            b"rt-renewed",
            "a fresh process must not load the refresh token this one spent"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn skips_the_write_when_a_concurrent_writer_rotated_past() {
        // The `#4038` case. A sibling writer landed a fresher rotation, so the
        // stored refresh token is no longer the one we exchanged — overwriting it
        // would revert their rotation and cost them the live token.
        let (path, material, store) = seeded_store("cas", "at-theirs", "rt-theirs");

        let renewed = OAuthToken::new(ProviderSecret::new("at-ours"))
            .with_refresh(ProviderSecret::new("rt-ours"));
        let written = store.persist_rotation("codex", Some("rt-spent"), &renewed);
        drop(store);

        assert!(!written, "the guard reports a skip rather than clobbering");
        let later = reopened(&path, &material);
        assert_eq!(
            later
                .get("codex_refresh")
                .expect("refresh row")
                .expect("a row")
                .as_bytes(),
            b"rt-theirs",
            "the fresher rotation survives"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn writes_nothing_when_the_session_declares_no_rows() {
        // No `oauth_sessions` row means no non-secret way to name a row to write.
        // A guessed name would encrypt a token against an AAD no reader could
        // reproduce, so refusing is the only safe answer.
        let store = store();
        let renewed = OAuthToken::new(ProviderSecret::new("at-orphan"));

        assert!(!store.persist_rotation("codex", Some("rt-1"), &renewed));
    }

    #[test]
    fn round_trips_a_refresh_key_alongside_the_access_key() {
        // Both names are needed for a rotation to be writable back, so both
        // survive the round trip.
        let s = store();
        let session = OAuthSession::new("codex", SessionKind::Refresh)
            .with_access_key("codex")
            .with_refresh_key("codex_refresh");
        s.insert_session(&session).expect("insert");

        assert_eq!(
            s.get_session("codex")
                .expect("get")
                .expect("a row")
                .refresh_key(),
            Some("codex_refresh")
        );
    }

    #[test]
    fn adds_the_refresh_key_column_to_a_store_written_before_it_existed() {
        // `CREATE TABLE IF NOT EXISTS` does nothing for a column added to a table
        // that is already there, so the shape is asserted at open time. A file
        // from before the column has to gain it or every rotation query fails.
        let path =
            std::env::temp_dir().join(format!("ar-keys-pre-refresh-key-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let material = Secret::generate();
        let first = CredentialStore::open_with_material(&path, &material).expect("open");
        first
            .conn
            .execute("ALTER TABLE oauth_sessions DROP COLUMN refresh_key", [])
            .expect("simulate the old file");
        drop(first);

        let second = CredentialStore::open_with_material(&path, &material).expect("reopen");
        second
            .insert_session(
                &OAuthSession::new("codex", SessionKind::Refresh).with_refresh_key("codex_refresh"),
            )
            .expect("the row the old file could not hold");

        assert_eq!(
            second
                .get_session("codex")
                .expect("get")
                .expect("a row")
                .refresh_key(),
            Some("codex_refresh")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn renders_no_secret_when_a_rotation_record_is_printed() {
        // The session row is what a leaked store file exposes, so the refresh
        // *name* is fine and the refresh token must never appear in it.
        let s = store();
        s.insert_session(
            &OAuthSession::new("codex", SessionKind::Refresh)
                .with_access_key("codex")
                .with_refresh_key("codex_refresh"),
        )
        .expect("insert");

        let rendered = format!("{:?}", s.get_session("codex").expect("get").expect("a row"));
        assert!(!rendered.contains("sk-"), "{rendered}");
    }

    #[test]
    fn never_names_a_credential_in_its_error_text() {
        let s = store();
        let e = s.get("openai").expect("get");
        assert!(e.is_none());
        assert!(
            !format!("{e:?}").contains("sk"),
            "a miss must not render key material"
        );
    }

    // `oauth_sessions` — the table F-MED-2's generated CHECK was waiting for.

    #[test]
    fn refuses_a_row_when_the_terminal_status_is_not_in_the_generated_list() {
        let s = store();
        // Raw SQL, not the store API: the claim under test is that the *database*
        // refuses it, so going through the API would test the wrong layer.
        let e = s
            .conn
            .execute(
                "INSERT INTO oauth_sessions (provider, kind, terminal_status, terminal_reason) \
                 VALUES ('codex', 'refresh', 418, 'teapot')",
                [],
            )
            .expect_err("not a terminal row");
        assert!(matches!(e, rusqlite::Error::SqliteFailure(..)), "{e}");
    }

    #[test]
    fn refuses_a_row_when_the_status_and_reason_are_mismatched() {
        let s = store();
        // `token_revoked` is terminal at 401 and at 410 — never at 400. A reason
        // checked without its status would let this through.
        let e = s
            .conn
            .execute(
                "INSERT INTO oauth_sessions (provider, kind, terminal_status, terminal_reason) \
                 VALUES ('codex', 'refresh', 400, 'token_revoked')",
                [],
            )
            .expect_err("half of a pair");
        assert!(matches!(e, rusqlite::Error::SqliteFailure(..)), "{e}");
    }

    #[test]
    fn refuses_a_row_when_the_kind_is_unknown() {
        let s = store();
        let e = s
            .conn
            .execute(
                "INSERT INTO oauth_sessions (provider, kind) VALUES ('codex', 'device_code')",
                [],
            )
            .expect_err("not one of the three kinds");
        assert!(matches!(e, rusqlite::Error::SqliteFailure(..)), "{e}");
    }

    #[test]
    fn stores_an_anonymous_session_when_it_carries_no_credential() {
        let s = store();
        s.insert_session(&OAuthSession::new("cursor", SessionKind::Anonymous))
            .expect("insert");
        assert_eq!(
            s.get_session("cursor").expect("get").expect("a row").kind(),
            SessionKind::Anonymous
        );
    }

    #[test]
    fn refuses_an_anonymous_session_when_it_carries_a_credential() {
        let s = store();
        let e = s
            .insert_session(
                &OAuthSession::new("cursor", SessionKind::Anonymous).with_access_key("cursor"),
            )
            .expect_err("an anonymous session has no secret to point at");
        assert!(
            matches!(&e, KeyError::AnonymousCredential { placement } if placement == "cursor"),
            "{e}"
        );
    }

    #[test]
    fn refuses_an_anonymous_credential_when_the_row_is_written_out_of_band() {
        let s = store();
        // The API refuses with a typed error; the CHECK is what holds when the
        // row arrives by some other route.
        let e = s
            .conn
            .execute("INSERT INTO oauth_sessions (provider, kind, access_key) VALUES ('cursor', 'anonymous', 'cursor')", [])
            .expect_err("CHECK holds too");
        assert!(matches!(e, rusqlite::Error::SqliteFailure(..)), "{e}");
    }

    #[test]
    fn round_trips_a_refresh_session_when_the_row_is_written() {
        let s = store();
        let session =
            OAuthSession::new("codex", SessionKind::Refresh).with_access_key("codex_refresh");
        s.insert_session(&session).expect("insert");
        assert_eq!(
            s.get_session("codex").expect("get").expect("a row"),
            session
        );
    }

    #[test]
    fn round_trips_a_device_session_when_the_row_is_written() {
        let s = store();
        let session =
            OAuthSession::new("gemini-cli", SessionKind::Device).with_access_key("gemini_cli");
        s.insert_session(&session).expect("insert");
        assert_eq!(
            s.get_session("gemini-cli").expect("get").expect("a row"),
            session
        );
    }

    #[test]
    fn round_trips_a_terminal_status_when_the_pair_matches_the_generated_list() {
        let s = store();
        let session = OAuthSession::new("claude", SessionKind::Refresh)
            .with_access_key("claude_refresh")
            .terminal(400, "invalid_grant");
        s.insert_session(&session).expect("insert");
        assert_eq!(
            s.get_session("claude")
                .expect("get")
                .expect("a row")
                .terminal_status(),
            Some(400)
        );
    }

    #[test]
    fn refuses_a_transient_status_when_the_row_is_written() {
        let s = store();
        // 503 is what a transient refresh failure reports; recording it would
        // retire a session the classifier explicitly called retryable.
        let e = s
            .insert_session(
                &OAuthSession::new("codex", SessionKind::Refresh)
                    .terminal(503, "refresh-endpoint-unavailable"),
            )
            .expect_err("transient is not terminal");
        assert!(matches!(e, KeyError::Store(_)), "{e}");
    }

    #[test]
    fn reports_no_session_when_the_provider_is_absent() {
        assert!(store().get_session("nope").expect("get").is_none());
    }

    #[test]
    fn forgets_a_session_when_the_provider_is_removed() {
        let s = store();
        s.insert_session(&OAuthSession::new("codex", SessionKind::Refresh))
            .expect("insert");
        assert!(s.remove_session("codex").expect("remove"));
    }

    #[test]
    fn reports_no_removal_when_the_provider_is_absent() {
        assert!(!store().remove_session("nope").expect("remove"));
    }

    #[test]
    fn counts_every_session_row_for_doctor() {
        let s = store();
        s.insert_session(&OAuthSession::new("codex", SessionKind::Refresh))
            .expect("insert");
        s.insert_session(&OAuthSession::new("cursor", SessionKind::Anonymous))
            .expect("insert");
        assert_eq!(s.session_count().expect("count"), 2);
    }

    #[test]
    fn replaces_a_session_when_the_same_provider_is_written_again() {
        let s = store();
        s.insert_session(&OAuthSession::new("codex", SessionKind::Refresh))
            .expect("insert");
        s.insert_session(
            &OAuthSession::new("codex", SessionKind::Refresh).terminal(401, "token_revoked"),
        )
        .expect("insert");
        assert_eq!(s.session_count().expect("count"), 1);
    }

    #[test]
    fn keeps_session_rows_beside_credential_rows() {
        let s = store();
        s.insert("codex", "codex", &Secret::generate())
            .expect("credential");
        s.insert_session(
            &OAuthSession::new("codex", SessionKind::Refresh).with_access_key("codex"),
        )
        .expect("session");
        assert_eq!(s.list_names().expect("names"), ["codex"]);
        assert_eq!(s.session_count().expect("count"), 1);
    }

    #[test]
    fn opens_a_store_written_before_the_sessions_table_existed() {
        // The upgrade path is `CREATE TABLE IF NOT EXISTS` and nothing else, so a
        // file carrying only the two original tables must gain the third.
        let path =
            std::env::temp_dir().join(format!("ar-keys-pre-sessions-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let material = Secret::generate();
        let first = CredentialStore::open_with_material(&path, &material).expect("open");
        first
            .insert("codex", "codex", &Secret::new(b"sk-old".to_vec()))
            .expect("insert");
        first
            .conn
            .execute("DROP TABLE oauth_sessions", [])
            .expect("simulate the pre-table file");
        drop(first);

        let second = CredentialStore::open_with_material(&path, &material).expect("reopen");
        assert_eq!(
            second.get("codex").expect("get").expect("a row").as_bytes(),
            b"sk-old"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn renders_no_secret_when_a_session_row_is_printed() {
        let s = store();
        s.insert_session(
            &OAuthSession::new("codex", SessionKind::Refresh).with_access_key("codex_refresh"),
        )
        .expect("insert");
        let rendered = format!("{:?}", s.get_session("codex").expect("get").expect("a row"));
        assert!(!rendered.contains("sk-"), "{rendered}");
    }

    fn snapshot(
        provider: &str,
        window: &str,
        remaining: Option<f64>,
        exhausted: bool,
        recorded_at: u64,
    ) -> QuotaSnapshot {
        QuotaSnapshot::new(
            provider,
            window,
            remaining,
            exhausted,
            Some(1_800_000_000),
            recorded_at,
        )
    }

    #[test]
    fn round_trips_a_quota_snapshot() {
        let s = store();
        s.insert_quota_snapshot(&snapshot(
            "openai",
            "session-a",
            Some(0.25),
            false,
            1_700_000_000,
        ))
        .expect("insert");
        assert_eq!(
            s.latest_quota_snapshot("openai", "session-a")
                .expect("read"),
            Some(snapshot(
                "openai",
                "session-a",
                Some(0.25),
                false,
                1_700_000_000
            ))
        );
    }

    #[test]
    fn reads_the_newest_snapshot_when_several_were_recorded() {
        // Latest-wins is the whole point of the time series: a stale reading
        // answering "what is it now" is worse than no reading at all.
        let s = store();
        s.insert_quota_snapshot(&snapshot(
            "openai",
            "session-a",
            Some(0.9),
            false,
            1_700_000_000,
        ))
        .expect("insert");
        s.insert_quota_snapshot(&snapshot(
            "openai",
            "session-a",
            Some(0.4),
            false,
            1_700_000_600,
        ))
        .expect("insert");
        let latest = s
            .latest_quota_snapshot("openai", "session-a")
            .expect("read")
            .expect("a row");
        assert_eq!(latest.remaining_percentage, Some(0.4));
    }

    #[test]
    fn breaks_a_timestamp_tie_by_insertion_order() {
        // Two readings in the same second: the one inserted last is the one
        // observed last, so it has to win.
        let s = store();
        s.insert_quota_snapshot(&snapshot(
            "openai",
            "session-a",
            Some(0.9),
            false,
            1_700_000_000,
        ))
        .expect("insert");
        s.insert_quota_snapshot(&snapshot(
            "openai",
            "session-a",
            Some(0.1),
            false,
            1_700_000_000,
        ))
        .expect("insert");
        let latest = s
            .latest_quota_snapshot("openai", "session-a")
            .expect("read")
            .expect("a row");
        assert_eq!(latest.remaining_percentage, Some(0.1));
    }

    #[test]
    fn reads_the_exhausted_flag_back() {
        // The flag a routing decision turns on, readable without parsing prose.
        let s = store();
        s.insert_quota_snapshot(&snapshot("claude", "weekly", None, true, 1_700_000_000))
            .expect("insert");
        let latest = s
            .latest_quota_snapshot("claude", "weekly")
            .expect("read")
            .expect("a row");
        assert!(latest.exhausted);
    }

    #[test]
    fn keeps_an_unexhausted_window_distinguishable_from_an_exhausted_one() {
        let s = store();
        s.insert_quota_snapshot(&snapshot(
            "openai",
            "session-a",
            Some(0.0),
            false,
            1_700_000_000,
        ))
        .expect("insert");
        s.insert_quota_snapshot(&snapshot("openai", "session-b", None, false, 1_700_000_000))
            .expect("insert");
        assert!(
            !s.latest_quota_snapshot("openai", "session-a")
                .expect("read")
                .expect("a row")
                .exhausted
        );
        assert!(
            !s.latest_quota_snapshot("openai", "session-b")
                .expect("read")
                .expect("a row")
                .exhausted
        );
    }

    #[test]
    fn separates_windows_of_the_same_provider() {
        let s = store();
        s.insert_quota_snapshot(&snapshot(
            "openai",
            "session-a",
            Some(0.5),
            false,
            1_700_000_000,
        ))
        .expect("insert");
        s.insert_quota_snapshot(&snapshot(
            "openai",
            "session-b",
            Some(0.1),
            false,
            1_700_000_900,
        ))
        .expect("insert");
        assert_eq!(
            s.latest_quota_snapshot("openai", "session-a")
                .expect("read")
                .expect("a row")
                .remaining_percentage,
            Some(0.5)
        );
    }

    #[test]
    fn returns_none_when_no_snapshot_was_recorded() {
        assert_eq!(
            store()
                .latest_quota_snapshot("openai", "session-a")
                .expect("read"),
            None
        );
    }

    #[test]
    fn counts_the_rows_it_holds() {
        let s = store();
        s.insert_quota_snapshot(&snapshot(
            "openai",
            "session-a",
            Some(0.5),
            false,
            1_700_000_000,
        ))
        .expect("insert");
        s.insert_quota_snapshot(&snapshot(
            "openai",
            "session-a",
            Some(0.4),
            false,
            1_700_000_600,
        ))
        .expect("insert");
        assert_eq!(s.quota_snapshot_count().expect("count"), 2);
    }

    #[test]
    fn reads_an_unmetered_window_as_unmetered() {
        // `QuotaWindow` reads a zero limit as unlimited headroom; storing `0.0`
        // here would report it as drained.
        let s = store();
        s.insert_quota_snapshot(&snapshot("openai", "session-a", None, false, 1_700_000_000))
            .expect("insert");
        assert_eq!(
            s.latest_quota_snapshot("openai", "session-a")
                .expect("read")
                .expect("a row")
                .remaining_percentage,
            None
        );
    }

    #[test]
    fn ages_a_snapshot_by_its_recorded_time() {
        assert_eq!(
            snapshot("openai", "w", Some(1.0), false, 1_700_000_000).age_secs(1_700_000_300),
            300
        );
    }

    #[test]
    fn treats_a_snapshot_past_the_age_bound_as_unknown_not_empty() {
        let s = snapshot("openai", "w", Some(1.0), false, 1_700_000_000);
        assert!(!s.is_stale(1_700_000_000 + MAX_QUOTA_SNAPSHOT_AGE_SECS));
    }

    #[test]
    fn treats_a_snapshot_beyond_the_age_bound_as_stale() {
        let s = snapshot("openai", "w", Some(1.0), false, 1_700_000_000);
        assert!(s.is_stale(1_700_000_000 + MAX_QUOTA_SNAPSHOT_AGE_SECS + 1));
    }

    #[test]
    fn reads_a_future_timestamp_as_brand_new_rather_than_ancient() {
        // A wrapped `u64` would make a clock-skewed reading look centuries old and
        // drop the connection for a reason that does not exist.
        assert_eq!(
            snapshot("openai", "w", Some(1.0), false, 1_700_000_600).age_secs(1_700_000_000),
            0
        );
    }

    #[test]
    fn opens_a_store_written_before_the_quota_snapshots_table_existed() {
        // `CREATE TABLE IF NOT EXISTS` again: a file carrying only the two
        // original tables must gain the third and keep its credentials.
        let path =
            std::env::temp_dir().join(format!("ar-keys-pre-quota-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let material = Secret::generate();
        let first = CredentialStore::open_with_material(&path, &material).expect("open");
        first
            .insert("codex", "codex", &Secret::new(b"sk-old".to_vec()))
            .expect("insert");
        first
            .conn
            .execute("DROP TABLE quota_snapshots", [])
            .expect("simulate the pre-table file");
        drop(first);

        let second = CredentialStore::open_with_material(&path, &material).expect("reopen");
        assert_eq!(
            second.get("codex").expect("get").expect("a row").as_bytes(),
            b"sk-old"
        );
        second
            .insert_quota_snapshot(&snapshot(
                "codex",
                "weekly",
                Some(0.5),
                false,
                1_700_000_000,
            ))
            .expect("insert");
        assert!(
            second
                .latest_quota_snapshot("codex", "weekly")
                .expect("read")
                .is_some()
        );
        let _ = std::fs::remove_file(&path);
    }
}
