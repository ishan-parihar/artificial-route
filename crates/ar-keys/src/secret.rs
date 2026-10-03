//! Secret material: a zeroizing heap, one redacting `Debug`, and the master key
//! that everything else is derived from.
//!
//! House precedent (`../agentgateway`) is `secrecy::SecretString` with **no
//! `Debug` impl at all** on the key type, and one hand-written redacting `Debug`
//! where the struct genuinely has to be printable
//! (`crates/agentgateway/src/http/basicauth.rs`). `Secret` takes the second
//! shape: a `Debug` that prints the length and nothing else. Omitting it
//! entirely would make every `Result` and error chain that touches a credential
//! unprintable, and an unprintable error is how secrets reach log lines.
//!
//! What is deliberately **not** implemented:
//!
//! * `Display`. A stray `{}` in a `tracing` field is the single most common way
//!   a secret escapes, and a `Display` impl is an invitation to write one.
//! * `Clone` on a cheap path. Cloning a secret is only ever wanted to hand it
//!   somewhere it was not meant to go; [`Secret::to_owned_secret`] is the one
//!   explicit, greppable way to do it.
//! * `PartialEq`/`Hash`. Comparison goes through [`Secret::ct_eq`], which is
//!   constant time, and a `Hash` impl would make a `HashMap<Secret, _>` whose
//!   bucket count leaks a prefix of the key.
//!
//! `zeroize` is a deliberate deviation from agentgateway, which carries no
//! zeroizing buffer at all. A `SecretString` overwrites a `Box<str>` it cannot
//! prove it overwrote; `Zeroizing` is in the dependency list precisely for this
//! crate (`docs/03-crates-and-deps.md`).

use std::fmt;
use std::path::{Path, PathBuf};

use aes_gcm::aead::rand_core::{OsRng, RngCore};
use zeroize::Zeroizing;

use crate::error::KeyError;
use crate::hash::{HashParams, SALT_LEN, derive_aead_key};

/// Length of the master key and of every key derived from it: AES-256.
pub const KEY_LEN: usize = 32;

/// A byte string that is zeroized on drop and never rendered.
///
/// Holds provider API keys, master keys and derived keys. Every field is
/// private, so the only ways out are [`Secret::as_bytes`] (borrowed) and
/// [`Secret::to_owned_secret`] (explicit copy).
///
/// No `Zeroize` derive: the `Zeroizing` wrapper's own `Drop` is the zeroization,
/// and a derive here would be a second mechanism for the same guarantee.
pub struct Secret(Zeroizing<Vec<u8>>);

impl Secret {
    /// Takes ownership of `bytes`, which is zeroized on drop.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Borrows `bytes` into a new `Secret`.
    ///
    /// # Errors
    /// Returns [`KeyError::BadMasterKeyLen`] if `bytes` is not
    /// [`KEY_LEN`], so a truncated read from disk is refused at the boundary
    /// instead of becoming a key that silently fails every tag check.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, KeyError> {
        if bytes.len() != KEY_LEN {
            return Err(KeyError::BadMasterKeyLen(bytes.len()));
        }
        Ok(Self::new(bytes.to_vec()))
    }

    /// Draws [`KEY_LEN`] bytes from the OS CSPRNG.
    ///
    /// `OsRng` is `aes-gcm`'s re-export of `rand_core::OsRng`; taking it from
    /// there rather than declaring `rand` keeps a direct dependency off a crate
    /// that otherwise needs no RNG of its own.
    #[must_use]
    pub fn generate() -> Self {
        // The scratch buffer is `Zeroizing` too, so the key never sits on the
        // stack in a plain array that outlives this function.
        let mut buf = Zeroizing::new(vec![0_u8; KEY_LEN]);
        OsRng.fill_bytes(&mut buf[..]);
        Self::new(buf.to_vec())
    }

    /// Borrows the bytes. The only way to read a secret's contents.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Byte length. Not a secret: every key here is 32 bytes by definition, and
    /// a provider API key's length is fixed by its issuer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the secret is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Clones the secret into an owned buffer.
    ///
    /// The only sanctioned copy. Named so that every place a secret is
    /// duplicated shows up in a grep.
    #[must_use]
    pub fn to_owned_secret(&self) -> Self {
        Self::new(self.0.to_vec())
    }

    /// Compares two secrets in constant time.
    ///
    /// A short-circuiting `==` would leak how many leading bytes matched through
    /// response timing, letting an attacker recover a presented key one byte at
    /// a time. This is the same threat model the gateway states in-house at
    /// `../agentgateway/crates/agentgateway/src/http/apikey_tests.rs:5`.
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq as _;
        self.as_bytes().ct_eq(other.as_bytes()).into()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Length only. OmniRoute logs `ciphertext.slice(0, 30)` on a decrypt
        // failure (`encryption.ts:277`), which is how a 30-character prefix of
        // an envelope reaches a log file; a `Debug` that printed any prefix of
        // this buffer would be the same mistake with a shorter fuse.
        write!(f, "Secret(<redacted, {} bytes>)", self.0.len())
    }
}

/// Per-install argon2id salt. Not secret — it is persisted next to the master
/// key by design, which is the whole point: OmniRoute's `STATIC_SALT` is a
/// 28-byte string literal in a public repository, so every install on earth
/// derives the same key from the same master.
#[derive(Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Salt(#[serde(with = "crate::codec::b64_salt")] [u8; SALT_LEN]);

impl Salt {
    /// Draws a fresh salt from the OS CSPRNG.
    #[must_use]
    pub fn generate() -> Self {
        let mut buf = [0_u8; SALT_LEN];
        OsRng.fill_bytes(&mut buf);
        Self(buf)
    }

    /// The raw salt bytes, as argon2id wants them.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; SALT_LEN] {
        &self.0
    }
}

impl fmt::Debug for Salt {
    /// Prints the salt in full. A salt is not a secret, and a credential
    /// envelope is unreproducible without it — so unlike [`Secret`], hiding it
    /// would only make bug reports harder.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Salt")
            .field(&crate::hex::encode(&self.0))
            .finish()
    }
}

/// The install's argon2id parameters and salt, persisted beside the master key.
///
/// Separated from the secret material because none of it is secret, and
/// because it is exactly what a rotation has to change: a new salt plus a new
/// master re-derives every key, and the parameters are what a future cost
/// increase has to bump. OmniRoute stores nothing —
/// `STORAGE_ENCRYPTION_KEY_VERSION` is written by the desktop bootstrap and
/// documented as the rotation knob, but no code reads it, so raising the KDF
/// cost there was impossible without breaking every stored row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct KeyMeta {
    /// Envelope format version this metadata belongs to.
    pub v: u8,
    /// argon2id memory cost in KiB.
    pub m_cost: u32,
    /// argon2id iterations.
    pub t_cost: u32,
    /// argon2id lanes.
    pub p_cost: u32,
    /// Per-install salt.
    pub salt: Salt,
}

impl KeyMeta {
    /// Fresh metadata: a new salt at the recommended cost.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            v: crate::hash::VERSION,
            ..Self::with_params(HashParams::RECOMMENDED, Salt::generate())
        }
    }

    /// Metadata with an explicit salt and cost.
    #[must_use]
    pub fn with_params(params: HashParams, salt: Salt) -> Self {
        Self {
            v: crate::hash::VERSION,
            m_cost: params.m_cost,
            t_cost: params.t_cost,
            p_cost: params.p_cost,
            salt,
        }
    }

    /// The argon2id cost this metadata pins.
    ///
    /// Read back from the record rather than assumed, so raising `m_cost` on an
    /// existing install re-derives correctly instead of silently producing a key
    /// that verifies nothing.
    #[must_use]
    pub fn params(&self) -> HashParams {
        HashParams {
            m_cost: self.m_cost,
            t_cost: self.t_cost,
            p_cost: self.p_cost,
        }
    }

    /// JSON form, for the key metadata file.
    ///
    /// # Errors
    /// Returns [`KeyError::Crypto`] if serialization fails. It cannot fail for
    /// these four scalar fields, so the variant names the operation rather than
    /// pretending the error is reachable.
    pub fn to_json(&self) -> Result<String, KeyError> {
        serde_json::to_string(self).map_err(|_| KeyError::Crypto("key-meta serialize"))
    }

    /// Parses [`KeyMeta::to_json`] output.
    ///
    /// # Errors
    /// Refuses a record whose `v` is not this build's envelope version. A
    /// v1-era record describes a scheme this build cannot read, and guessing at
    /// its salt would be exactly the fail-open this crate exists to avoid.
    pub fn from_json(s: &str) -> Result<Self, KeyError> {
        let meta: Self = serde_json::from_str(s).map_err(|_| KeyError::Crypto("key-meta parse"))?;
        if meta.v != crate::hash::VERSION {
            return Err(KeyError::Crypto("key-meta version"));
        }
        Ok(meta)
    }
}

/// Where the master key comes from.
#[derive(Clone, Debug)]
pub enum MasterKeySource {
    /// Random, process-lifetime only.
    ///
    /// For tests and for an explicit "run without a key store" mode. **Every
    /// credential encrypted under it becomes unreadable at restart**, because
    /// the salt is redrawn too. The name says `Ephemeral` rather than
    /// `Generate` so the failure mode is visible at the call site.
    Ephemeral,
    /// A `0600` file holding 64 hex chars, base64, or 32 raw bytes.
    File(PathBuf),
    /// The OS keychain (desktop). Behind the `keyring` feature.
    #[cfg(feature = "keyring")]
    Keyring {
        /// Keychain service name.
        service: String,
        /// Keychain account name.
        account: String,
    },
}

impl MasterKeySource {
    /// The `0600` file, if this source is one.
    #[must_use]
    pub fn file(&self) -> Option<&Path> {
        match self {
            Self::File(p) => Some(p),
            _ => None,
        }
    }
}

/// The install's master key, plus the argon2id key derived from it.
///
/// Two keys, not one: the derived key protects credential envelopes, the master
/// signs JWTs. Deriving rather than reusing means the envelope key is not the
/// token key.
///
/// `aes_gcm::Aes256Gcm` is deliberately **not** stored here. It does not zeroize
/// on drop, so holding one would leave a copy of the derived key in the heap
/// for the process lifetime — exactly what the rest of this module is for. The
/// key schedule is a handful of AES round-key XORs, so [`MasterKey::cipher`]
/// builds one per operation instead.
pub struct MasterKey {
    master: Secret,
    aead_key: Secret,
    meta: KeyMeta,
}

impl MasterKey {
    /// A process-lifetime master key with fresh metadata. See
    /// [`MasterKeySource::Ephemeral`] for what this costs.
    ///
    /// # Errors
    /// As [`MasterKey::new`]. Unreachable in practice: `KeyMeta::generate`
    /// emits a fresh salt at [`HashParams::RECOMMENDED`], which argon2id accepts.
    /// It is still a `Result` rather than an `expect` because a `#[must_use]` fn
    /// that panics is a footgun in a boot path, and this crate never unwraps.
    pub fn ephemeral() -> Result<Self, KeyError> {
        Self::new(Secret::generate(), KeyMeta::generate())
    }

    /// Builds a master key from raw material and metadata, deriving the AEAD
    /// key with the metadata's own parameters.
    ///
    /// # Errors
    /// Returns [`KeyError::BadMasterKeyLen`] if `master` is not [`KEY_LEN`],
    /// or [`KeyError::BadParams`] if argon2id rejects the recorded cost.
    pub fn new(master: Secret, meta: KeyMeta) -> Result<Self, KeyError> {
        let aead_key = derive_aead_key(&master, &meta.salt, meta.params())?;
        Ok(Self {
            master,
            aead_key,
            meta,
        })
    }

    /// Loads the master key from `source`.
    ///
    /// # Errors
    /// Fails closed on every path. In particular [`MasterKeySource::File`]
    /// refuses a file any group or other user can read: a master key whose
    /// protection is "the directory is private" is not a master key, and
    /// OmniRoute's `electron/main.js` writes its key to a plain dotenv file with
    /// default permissions.
    pub fn load(source: &MasterKeySource) -> Result<Self, KeyError> {
        match source {
            MasterKeySource::Ephemeral => Self::ephemeral(),
            MasterKeySource::File(path) => {
                check_private(path)?;
                let raw = std::fs::read_to_string(path)
                    .map_err(|e| KeyError::MasterKey(format!("{}: {e}", path.display())))?;
                let bytes = decode_key_material(raw.trim())?;
                Self::new(Secret::from_slice(&bytes)?, KeyMeta::generate())
            }
            #[cfg(feature = "keyring")]
            MasterKeySource::Keyring { service, account } => {
                let entry = keyring::Entry::new(service, account)
                    .map_err(|e| KeyError::MasterKey(e.to_string()))?;
                let raw = entry
                    .get_password()
                    .map_err(|e| KeyError::MasterKey(e.to_string()))?;
                let bytes = decode_key_material(raw.trim())?;
                Self::new(Secret::from_slice(&bytes)?, KeyMeta::generate())
            }
        }
    }

    /// The derived AEAD key, for [`crate::hash::encrypt`] and
    /// [`crate::hash::decrypt`].
    #[must_use]
    pub fn aead_key(&self) -> &Secret {
        &self.aead_key
    }

    /// The master key bytes, for HS256 signing.
    ///
    /// // ponytail: the master is uniform random, so using it directly as an
    // HS256 key is sound and costs one HMAC. Deriving a second, domain-separated
    // signing key would need `hkdf` (a new dependency) or a second argon2id run
    // at 19 MiB, and it defends only against a future key-recovery bug in the
    // HMAC path rather than any present threat. Add HKDF when a second party has
    /// to verify tokens without being able to decrypt credentials.
    #[must_use]
    pub fn signing_key(&self) -> &Secret {
        &self.master
    }

    /// The metadata a caller must persist for this key to be re-derivable.
    #[must_use]
    pub fn meta(&self) -> &KeyMeta {
        &self.meta
    }

    /// An AES-256-GCM cipher over the derived key.
    ///
    /// Rebuilt per operation so no key schedule outlives the call — see the
    /// type-level note on [`MasterKey`].
    ///
    /// # Errors
    /// [`KeyError::BadMasterKeyLen`] if the derived key is not 32 bytes, which
    /// argon2id guarantees but `KeyMeta::from_json` does not.
    pub(crate) fn cipher(&self) -> Result<aes_gcm::Aes256Gcm, KeyError> {
        use aes_gcm::KeyInit as _;
        if self.aead_key.len() != KEY_LEN {
            return Err(KeyError::BadMasterKeyLen(self.aead_key.len()));
        }
        aes_gcm::Aes256Gcm::new_from_slice(self.aead_key.as_bytes())
            .map_err(|_| KeyError::BadMasterKeyLen(self.aead_key.len()))
    }

    /// Re-derives under a new master and a new salt.
    ///
    /// Returns the new key together with the metadata to persist. The caller
    /// re-encrypts every envelope with it and only then drops the old key;
    /// nothing here touches stored credentials, because a rotation that
    /// half-applied is worse than one that has not started.
    ///
    /// # Errors
    /// As [`MasterKey::new`].
    pub fn rotate(&self, new_master: Secret) -> Result<Self, KeyError> {
        Self::new(new_master, KeyMeta::generate())
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MasterKey")
            .field("master", &self.master)
            .field("aead_key", &self.aead_key)
            .field("meta", &self.meta)
            .finish()
    }
}

/// Accepts hex, base64, or raw bytes.
///
/// Two encodings, because that is what key generators actually emit:
/// `openssl rand -base64 32` and Node's `randomBytes(32).toString("hex")` (what
/// OmniRoute's desktop bootstrap writes). Guessing a third format would only
/// reject a file the operator can plainly read.
///
/// `pub(crate)` because [`crate::CredentialStore`] reads the same three formats
/// out of `$AR_MASTER_KEY`; one decoder for "wherever the master key came from"
/// is the point.
pub(crate) fn decode_key_material(raw: &str) -> Result<Vec<u8>, KeyError> {
    // Hex first: 64 characters. A 44-character base64 string is not a valid hex
    // length, so the length check keeps the two attempts from shadowing each
    // other.
    if raw.len() == KEY_LEN * 2
        && let Some(bytes) = crate::hex::decode(raw)
    {
        return Ok(bytes);
    }
    crate::b64::decode(raw).ok_or(KeyError::BadMasterKeyLen(raw.len()))
}

/// Refuses a key file that any group or other user can read.
#[cfg(unix)]
fn check_private(path: &Path) -> Result<(), KeyError> {
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::metadata(path)
        .map_err(|e| KeyError::MasterKey(format!("{}: {e}", path.display())))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(KeyError::MasterKey(format!(
            "{}: mode {:o} is readable by group or other; chmod 600",
            path.display(),
            mode
        )));
    }
    Ok(())
}

/// Non-Unix builds have no mode bits to check.
#[cfg(not(unix))]
fn check_private(_path: &Path) -> Result<(), KeyError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{KEY_LEN, KeyMeta, MasterKey, MasterKeySource, Salt, Secret};

    #[test]
    fn debug_prints_length_and_never_content() {
        let s = Secret::new(b"super-secret-value".to_vec());
        let rendered = format!("{s:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }

    #[test]
    fn from_slice_refuses_wrong_length() {
        assert!(Secret::from_slice(&[0_u8; 16]).is_err());
    }

    #[test]
    fn ct_eq_matches_equal_and_rejects_different() {
        let a = Secret::new(vec![1, 2, 3]);
        let b = Secret::new(vec![1, 2, 3]);
        let c = Secret::new(vec![1, 2, 4]);
        assert!(a.ct_eq(&b) && !a.ct_eq(&c));
    }

    #[test]
    fn generated_master_is_key_len() {
        assert_eq!(Secret::generate().len(), KEY_LEN);
    }

    #[test]
    fn master_debug_carries_no_key_bytes() {
        let mk = MasterKey::ephemeral().expect("ephemeral");
        // The derived key is raw bytes; hex-encode a prefix and assert the
        // rendered Debug never contains it.
        let prefix = crate::codec::hex::encode(&mk.aead_key().as_bytes()[..4]);
        let rendered = format!("{mk:?}");
        assert!(!rendered.contains(&prefix), "{rendered}");
    }

    #[test]
    fn meta_round_trips_through_json() {
        let meta = KeyMeta::generate();
        let json = meta.to_json().expect("serialize");
        assert_eq!(KeyMeta::from_json(&json).expect("parse"), meta);
    }

    #[test]
    fn meta_refuses_a_foreign_version() {
        let mut meta = KeyMeta::generate();
        meta.v = 1;
        let json = meta.to_json().expect("serialize");
        assert!(KeyMeta::from_json(&json).is_err());
    }

    #[test]
    fn rotate_changes_the_derived_key() {
        let mk = MasterKey::ephemeral().expect("ephemeral");
        let rotated = mk.rotate(Secret::generate()).expect("rotate");
        assert!(!mk.aead_key().ct_eq(rotated.aead_key()));
    }

    #[test]
    fn salts_differ_between_installs() {
        assert_ne!(Salt::generate(), Salt::generate());
    }

    #[test]
    fn file_source_refuses_a_group_readable_key() {
        let dir = std::env::temp_dir().join("ar-keys-perm-test");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("master.hex");
        std::fs::write(&path, "00".repeat(KEY_LEN)).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
            assert!(MasterKey::load(&MasterKeySource::File(path.clone())).is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
            assert!(MasterKey::load(&MasterKeySource::File(path)).is_ok());
        }
        #[cfg(not(unix))]
        assert!(MasterKey::load(&MasterKeySource::File(path)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
