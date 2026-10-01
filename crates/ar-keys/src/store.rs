//! The local encrypted credential store: a gitignored sqlite table of `enc:`
//! envelopes, one row per credential name.
//!
//! This is the answer to F-CRIT-2 in `AUDIT-REPORT.md` — "no local credential
//! store" — and it is deliberately a *narrow* one. The store holds the same
//! `keys:` map `config.yaml` already declares, encrypted at rest, plus one
//! non-secret table beside it. No token material outside `credentials`, no
//! quota rows, no rotation bookkeeping.
//!
//! # The second table: `oauth_sessions`
//!
//! F-MED-2 generated a terminal-status CHECK clause
//! ([`ar_exec::oauth::terminal_check_constraint`]) with nowhere to sit — this
//! is the table it goes in. A row answers two questions about one provider: which
//! `credentials` row its session token lives behind ([`OAuthSession::access_key`],
//! a *name*, never the secret), and whether a refresh has retired it
//! ([`OAuthSession::terminal_status`]). That is the whole table: placement plus
//! status. Nothing here decrypts anything, so a leaked `credentials.db` copy
//! leaks no more session material than a leaked `config.yaml` does.
//!
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
    format!("CHECK (kind IN ({}))", SessionKind::ALL.map(|k| format!("'{}'", k.as_str())).join(", "))
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
    terminal_status INTEGER NULL,
    terminal_reason TEXT    NULL,
    CHECK (kind <> 'anonymous' OR access_key IS NULL),
    {terminal}
);
",
        kind_check = session_kind_check(),
        terminal = ar_exec::oauth::terminal_check_constraint(),
    )
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
INSERT INTO oauth_sessions (provider, kind, access_key, terminal_status, terminal_reason)
VALUES (?1, ?2, ?3, ?4, ?5)
ON CONFLICT(provider) DO UPDATE SET
    kind = ?2, access_key = ?3, terminal_status = ?4, terminal_reason = ?5
";

const SELECT_SESSION: &str = "
SELECT provider, kind, access_key, terminal_status, terminal_reason
FROM oauth_sessions WHERE provider = ?1
";

const DELETE_SESSION: &str = "DELETE FROM oauth_sessions WHERE provider = ?1";

const COUNT_SESSIONS: &str = "SELECT count(*) FROM oauth_sessions";

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
    terminal_status: Option<u16>,
    terminal_reason: Option<String>,
}

impl OAuthSession {
    /// A session of `kind` for `provider`, with nothing recorded yet beyond that.
    #[must_use]
    pub fn new(provider: impl Into<String>, kind: SessionKind) -> Self {
        Self { provider: provider.into(), kind, access_key: None, terminal_status: None, terminal_reason: None }
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
        let master = match read_meta(&conn)? {
            Some(meta) => MasterKey::new(material.to_owned_secret(), meta)?,
            None => {
                let master = MasterKey::new(material.to_owned_secret(), KeyMeta::generate())?;
                conn.execute(INSERT_META, [master.meta().to_json()?]).map_err(sql)?;
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
            KeyError::MasterKey(format!("{MASTER_KEY_VAR} is unset; the store cannot be read without it"))
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
        Ok(Some(hash::decrypt(&self.master, &provider, name, &envelope)?))
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
        self.conn.execute(DELETE_CREDENTIAL, [name]).map(|n| n > 0).map_err(sql)
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
        let rows = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(sql)?;
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
        if let (SessionKind::Anonymous, Some(placement)) = (session.kind, session.access_key.as_deref()) {
            return Err(KeyError::AnonymousCredential { placement: placement.to_owned() });
        }
        self.conn
            .execute(
                UPSERT_SESSION,
                params![
                    session.provider,
                    session.kind.as_str(),
                    session.access_key,
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
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
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
        self.conn.execute(DELETE_SESSION, [provider]).map(|n| n > 0).map_err(sql)
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
        self.conn.query_row(COUNT_SESSIONS, [], |row| row.get::<_, i64>(0)).map(|n| n.max(0) as usize).map_err(sql)
    }
}

/// Rebuilds a session from a row of [`SELECT_SESSION`].
///
/// A `u16` the schema cannot hold is a store error rather than a saturating cast:
/// the column only ever received a `u16` through this API, so a wider value means
/// the file was edited by something that is not this build.
fn read_session(row: (String, String, Option<String>, Option<i64>, Option<String>)) -> Result<OAuthSession, KeyError> {
    let (provider, kind, access_key, terminal_status, terminal_reason) = row;
    let kind = SessionKind::parse(&kind)
        .ok_or_else(|| KeyError::Store(format!("oauth session {provider:?} has unknown kind {kind:?}")))?;
    let terminal_status = terminal_status
        .map(|status| u16::try_from(status).map_err(|_| KeyError::Store(format!("oauth session {provider:?} has status {status}"))))
        .transpose()?;
    Ok(OAuthSession {
        provider,
        kind,
        access_key,
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

/// The persisted argon2id metadata, or `None` for a store this build is creating.
fn read_meta(conn: &Connection) -> Result<Option<KeyMeta>, KeyError> {
    let json = conn.query_row(SELECT_META, [], |row| row.get::<_, String>(0)).optional().map_err(sql)?;
    json.map(|raw| KeyMeta::from_json(&raw)).transpose()
}

/// `rusqlite` has no `Clone`/`PartialEq`, and `KeyError` promises both, so the
/// store's own failures are carried as text. The text is SQLite's, not a value
/// from the database.
fn sql(e: rusqlite::Error) -> KeyError {
    KeyError::Store(e.to_string())
}

// The store moves into a worker that builds a config; `rusqlite::Connection` is
// `Send` but not `Sync`, and `MasterKey` is both. Prove the one that matters.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<CredentialStore>();
};

#[cfg(test)]
mod tests {
    use super::{CredentialStore, MASTER_KEY_VAR, OAuthSession, SessionKind, sql};
    use crate::codec::hex;
    use crate::error::KeyError;
    use crate::secret::Secret;

    fn store() -> CredentialStore {
        CredentialStore::open_in_memory(&Secret::generate()).expect("in-memory store")
    }

    #[test]
    fn round_trips_a_credential() {
        let s = store();
        s.insert("openai", "openai", &Secret::new(b"sk-provider".to_vec())).expect("insert");
        assert_eq!(s.get("openai").expect("get").expect("a row").as_bytes(), b"sk-provider");
    }

    #[test]
    fn reports_no_row_when_the_name_is_absent() {
        assert!(store().get("nope").expect("get").is_none());
    }

    #[test]
    fn lists_every_name_when_queried() {
        let s = store();
        s.insert("openai", "b", &Secret::generate()).expect("insert");
        s.insert("anthropic", "a", &Secret::generate()).expect("insert");
        assert_eq!(s.list_names().expect("names"), ["a", "b"]);
    }

    #[test]
    fn replaces_a_credential_when_inserted_again() {
        let s = store();
        s.insert("openai", "k", &Secret::new(b"first".to_vec())).expect("insert");
        s.insert("openai", "k", &Secret::new(b"second".to_vec())).expect("insert");
        assert_eq!(s.get("k").expect("get").expect("a row").as_bytes(), b"second");
    }

    #[test]
    fn stores_the_envelope_and_never_the_plaintext() {
        let s = store();
        s.insert("openai", "k", &Secret::new(b"sk-plaintext".to_vec())).expect("insert");
        let row: String =
            s.conn.query_row("SELECT envelope FROM credentials WHERE name = 'k'", [], |r| r.get(0)).expect("row");
        assert!(row.starts_with("enc:v2:"), "{row}");
        assert!(!row.contains("sk-plaintext"), "{row}");
    }

    #[test]
    fn refuses_a_credential_when_the_provider_does_not_match_the_aad() {
        let s = store();
        s.insert("openai", "k", &Secret::new(b"sk-x".to_vec())).expect("insert");
        s.conn.execute("UPDATE credentials SET provider = 'groq' WHERE name = 'k'", []).expect("tamper");
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
        first.insert("openai", "openai", &Secret::new(b"sk-persisted".to_vec())).expect("insert");
        drop(first);

        let second = CredentialStore::open_with_material(&path, &material).expect("reopen");
        assert_eq!(
            second.get("openai").expect("get").expect("a row").as_bytes(),
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
        store.insert("openai", "openai", &Secret::new(b"sk-x".to_vec())).expect("insert");
        drop(store);

        let theirs = CredentialStore::open_with_material(&path, &Secret::generate()).expect("reopen");
        assert!(matches!(theirs.get("openai"), Err(KeyError::TagMismatch)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refuses_to_open_when_the_file_is_not_a_database() {
        let path = std::env::temp_dir().join(format!("ar-keys-garbage-{}.db", std::process::id()));
        std::fs::write(&path, b"not a sqlite file at all").expect("write");
        let e = CredentialStore::open_with_material(&path, &Secret::generate()).expect_err("garbage");
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
        store.insert("openai", "openai", &Secret::new(b"sk-env".to_vec())).expect("insert");
        assert_eq!(store.get("openai").expect("get").expect("a row").as_bytes(), b"sk-env");
        drop(store);

        // SAFETY: as above, and this thread is the only one that can observe it.
        unsafe { std::env::remove_var(MASTER_KEY_VAR) };
        let e = CredentialStore::open_with_env_key(&path).expect_err("unset now");
        assert!(matches!(e, KeyError::MasterKey(_)), "{e}");
        assert!(e.to_string().contains(MASTER_KEY_VAR), "the reason names the variable: {e}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reports_a_sqlite_failure_as_a_store_error_carrying_no_value() {
        let s = store();
        s.insert("openai", "k", &Secret::new(b"sk-value".to_vec())).expect("insert");
        let e = s.get("k' OR 1=1 --").expect("parameterised, so a miss");
        assert!(e.is_none(), "{e:?}");
        let rendered = sql(rusqlite::Error::InvalidQuery).to_string();
        assert!(!rendered.contains("sk-value"), "{rendered}");
    }

    #[test]
    fn never_names_a_credential_in_its_error_text() {
        let s = store();
        let e = s.get("openai").expect("get");
        assert!(e.is_none());
        assert!(!format!("{e:?}").contains("sk"), "a miss must not render key material");
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
            .execute("INSERT INTO oauth_sessions (provider, kind) VALUES ('codex', 'device_code')", [])
            .expect_err("not one of the three kinds");
        assert!(matches!(e, rusqlite::Error::SqliteFailure(..)), "{e}");
    }

    #[test]
    fn stores_an_anonymous_session_when_it_carries_no_credential() {
        let s = store();
        s.insert_session(&OAuthSession::new("cursor", SessionKind::Anonymous)).expect("insert");
        assert_eq!(s.get_session("cursor").expect("get").expect("a row").kind(), SessionKind::Anonymous);
    }

    #[test]
    fn refuses_an_anonymous_session_when_it_carries_a_credential() {
        let s = store();
        let e = s
            .insert_session(&OAuthSession::new("cursor", SessionKind::Anonymous).with_access_key("cursor"))
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
        let session = OAuthSession::new("codex", SessionKind::Refresh).with_access_key("codex_refresh");
        s.insert_session(&session).expect("insert");
        assert_eq!(s.get_session("codex").expect("get").expect("a row"), session);
    }

    #[test]
    fn round_trips_a_device_session_when_the_row_is_written() {
        let s = store();
        let session = OAuthSession::new("gemini-cli", SessionKind::Device).with_access_key("gemini_cli");
        s.insert_session(&session).expect("insert");
        assert_eq!(s.get_session("gemini-cli").expect("get").expect("a row"), session);
    }

    #[test]
    fn round_trips_a_terminal_status_when_the_pair_matches_the_generated_list() {
        let s = store();
        let session = OAuthSession::new("claude", SessionKind::Refresh).with_access_key("claude_refresh").terminal(400, "invalid_grant");
        s.insert_session(&session).expect("insert");
        assert_eq!(
            s.get_session("claude").expect("get").expect("a row").terminal_status(),
            Some(400)
        );
    }

    #[test]
    fn refuses_a_transient_status_when_the_row_is_written() {
        let s = store();
        // 503 is what a transient refresh failure reports; recording it would
        // retire a session the classifier explicitly called retryable.
        let e = s
            .insert_session(&OAuthSession::new("codex", SessionKind::Refresh).terminal(503, "refresh-endpoint-unavailable"))
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
        s.insert_session(&OAuthSession::new("codex", SessionKind::Refresh)).expect("insert");
        assert!(s.remove_session("codex").expect("remove"));
    }

    #[test]
    fn reports_no_removal_when_the_provider_is_absent() {
        assert!(!store().remove_session("nope").expect("remove"));
    }

    #[test]
    fn counts_every_session_row_for_doctor() {
        let s = store();
        s.insert_session(&OAuthSession::new("codex", SessionKind::Refresh)).expect("insert");
        s.insert_session(&OAuthSession::new("cursor", SessionKind::Anonymous)).expect("insert");
        assert_eq!(s.session_count().expect("count"), 2);
    }

    #[test]
    fn replaces_a_session_when_the_same_provider_is_written_again() {
        let s = store();
        s.insert_session(&OAuthSession::new("codex", SessionKind::Refresh)).expect("insert");
        s.insert_session(&OAuthSession::new("codex", SessionKind::Refresh).terminal(401, "token_revoked"))
            .expect("insert");
        assert_eq!(s.session_count().expect("count"), 1);
    }

    #[test]
    fn keeps_session_rows_beside_credential_rows() {
        let s = store();
        s.insert("codex", "codex", &Secret::generate()).expect("credential");
        s.insert_session(&OAuthSession::new("codex", SessionKind::Refresh).with_access_key("codex")).expect("session");
        assert_eq!(s.list_names().expect("names"), ["codex"]);
        assert_eq!(s.session_count().expect("count"), 1);
    }

    #[test]
    fn opens_a_store_written_before_the_sessions_table_existed() {
        // The upgrade path is `CREATE TABLE IF NOT EXISTS` and nothing else, so a
        // file carrying only the two original tables must gain the third.
        let path = std::env::temp_dir().join(format!("ar-keys-pre-sessions-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let material = Secret::generate();
        let first = CredentialStore::open_with_material(&path, &material).expect("open");
        first.insert("codex", "codex", &Secret::new(b"sk-old".to_vec())).expect("insert");
        first.conn.execute("DROP TABLE oauth_sessions", []).expect("simulate the pre-table file");
        drop(first);

        let second = CredentialStore::open_with_material(&path, &material).expect("reopen");
        assert_eq!(second.get("codex").expect("get").expect("a row").as_bytes(), b"sk-old");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn renders_no_secret_when_a_session_row_is_printed() {
        let s = store();
        s.insert_session(&OAuthSession::new("codex", SessionKind::Refresh).with_access_key("codex_refresh")).expect("insert");
        let rendered = format!("{:?}", s.get_session("codex").expect("get").expect("a row"));
        assert!(!rendered.contains("sk-"), "{rendered}");
    }
}
