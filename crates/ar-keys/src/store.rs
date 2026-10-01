//! The local encrypted credential store: a gitignored sqlite table of `enc:`
//! envelopes, one row per credential name.
//!
//! This is the answer to F-CRIT-2 in `AUDIT-REPORT.md` — "no local credential
//! store" — and it is deliberately a *narrow* one. The store holds the same
//! `keys:` map `config.yaml` already declares, encrypted at rest, and nothing
//! else. No OAuth sessions (that is F-CRIT-1, a separate stream), no refresh
//! tokens, no quota rows, no rotation bookkeeping.
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

/// `store_meta` holds exactly one row, so the id is pinned to 1 by a CHECK
/// rather than left to a convention nobody would notice breaking.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS store_meta (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    meta TEXT    NOT NULL
);
CREATE TABLE IF NOT EXISTS credentials (
    name     TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    envelope TEXT NOT NULL
);
";

const SELECT_META: &str = "SELECT meta FROM store_meta WHERE id = 1";
const INSERT_META: &str = "INSERT OR REPLACE INTO store_meta (id, meta) VALUES (1, ?1)";

const UPSERT_CREDENTIAL: &str = "
INSERT INTO credentials (name, provider, envelope) VALUES (?1, ?2, ?3)
ON CONFLICT(name) DO UPDATE SET provider = ?2, envelope = ?3
";

const SELECT_CREDENTIAL: &str = "SELECT provider, envelope FROM credentials WHERE name = ?1";
const SELECT_NAMES: &str = "SELECT name FROM credentials ORDER BY name";

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
        conn.execute_batch(SCHEMA).map_err(sql)?;
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
        conn.execute_batch(SCHEMA).map_err(sql)?;
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
    use super::{CredentialStore, MASTER_KEY_VAR, sql};
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
}
