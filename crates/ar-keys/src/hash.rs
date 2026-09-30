//! The `enc:v2:` credential envelope: argon2id KDF, 12-byte nonce, bound AAD.
//!
//! Four changes over OmniRoute's `enc:v1:` (`../OmniRoute/src/lib/db/encryption.ts`),
//! each a defect the v1 code is cited for in `docs/04-subsystems.md`:
//!
//! | | v1 | v2 |
//! |---|---|---|
//! | KDF | `scryptSync(master, "omniroute-field-encryption-v1", 32)`, Node defaults `N=16384,r=8,p=1`, **never stored** | [`HashParams`] persisted in [`crate::KeyMeta`], argon2id |
//! | Salt | a 28-byte literal in a public repository | per-install [`crate::Salt`] |
//! | Nonce | 16 bytes | 12 bytes |
//! | AAD | **none anywhere in the repo** | `provider|key-id|v2` |
//!
//! The AAD is the load-bearing one. With no AAD, GCM authenticates only the
//! ciphertext, so a v1 envelope copied from one provider row to another decrypts
//! cleanly in the new row — the tag verifies because the key and the nonce
//! travelled with it. Binding `provider` and `key-id` into the tag makes the
//! ciphertext a function of its identity, so a spliced envelope fails to
//! authenticate.
//!
//! The envelope is `enc:v2:<nonce>:<ct>:<tag>`, the same three-field shape as v1's
//! `enc:v1:<iv>:<ct>:<tag>`. What changed is what those fields mean and what
//! the KDF behind them is, not the grammar.

use aes_gcm::aead::AeadCore;
use aes_gcm::aead::rand_core::OsRng;
use aes_gcm::AeadInPlace as _;

use crate::codec;
use crate::error::KeyError;
use crate::secret::{MasterKey, Salt, Secret};

/// Envelope prefix. The version is in the string, so a future v3 is refused by
/// name instead of by a tag mismatch.
pub const PREFIX: &str = "enc:v2:";

/// Prefix of the unsupported v1 grammar. See [`KeyError::LegacyV1`].
pub const LEGACY_PREFIX: &str = "enc:v1:";

/// Envelope version, also the trailing element of the AAD.
pub const VERSION: u8 = 2;

/// GCM nonce length in bytes. The NIST SP 800-38D recommended size; v1 used 16.
pub const NONCE_LEN: usize = 12;

/// GCM authentication tag length in bytes, pinned.
///
/// v1's main decrypt path pinned this at 16, but its diagnostic CLI
/// (`bin/cli/commands/doctor.mjs:164`) passed `authTagLength: authTagBuf.length`
/// read out of the stored value, which reopens the tag-truncation forgery the
/// pin exists to close. Here the length is a constant that a caller cannot
/// influence.
pub const TAG_LEN: usize = 16;

/// Per-install argon2id salt length in bytes.
///
/// argon2id requires at least 8. 16 matches the v1 dynamic-salt generation
/// (`sha256(master)[0..16]`) so a future migration reads like v1 did.
pub const SALT_LEN: usize = 16;

/// argon2id cost. Persisted, not baked in, so raising it on an existing install
/// re-derives correctly instead of producing a key that verifies nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct HashParams {
    /// Memory cost in KiB.
    pub m_cost: u32,
    /// Iterations.
    pub t_cost: u32,
    /// Parallel lanes.
    pub p_cost: u32,
}

impl HashParams {
    /// OWASP's second recommended argon2id option: 19 MiB, 2 passes, 1 lane.
    ///
    /// // ponytail: 19 MiB is transient, not resident — argon2id frees its block
    /// on return, so peak RSS grows by 19 MiB during the ~40 ms derivation and
    /// then falls. It runs once per key load, never per request. If the <35 MB
    /// idle budget in `docs/00-overview.md` ever has to absorb it, `HashParams`
    //  is the knob and the value is a per-install decision, not a constant.
    pub const RECOMMENDED: Self = Self { m_cost: 19 * 1024, t_cost: 2, p_cost: 1 };

    /// Test fixture only: 64 KiB, one pass.
    ///
    /// Not a calibration. A release-mode suite of a few dozen derivations at
    /// `RECOMMENDED` costs seconds of CPU for no extra coverage, and the
    /// property under test is "the same key re-derives", not "argon2id is
    /// slow". One test derives at `RECOMMENDED` to keep the defaults honest.
    pub const FAST: Self = Self { m_cost: 64, t_cost: 1, p_cost: 1 };
}

/// Derives the AEAD key: `argon2id(master, salt, params) → 32 bytes`.
///
/// Uses `hash_password_into`, the raw KDF, and **not** `hash_password`. The PHC
/// string `hash_password` returns embeds the salt and the parameters in its
/// output; here the salt lives in [`KeyMeta`] and the envelope carries only
/// nonce/ct/tag, so a PHC string would be a second, redundant copy of both.
///
/// # Errors
/// [`KeyError::BadParams`] if argon2id rejects the cost.
pub fn derive_aead_key(master: &Secret, salt: &Salt, params: HashParams) -> Result<Secret, KeyError> {
    use argon2::{Algorithm, Argon2, Params, Version};

    let hash = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(params.m_cost, params.t_cost, params.p_cost, Some(crate::KEY_LEN))
            .map_err(|e| KeyError::BadParams(e.to_string()))?,
    );
    let mut out = vec![0_u8; crate::KEY_LEN];
    hash.hash_password_into(master.as_bytes(), salt.as_bytes(), &mut out)
        .map_err(|e| KeyError::BadParams(e.to_string()))?;
    Ok(Secret::new(out))
}

/// The additional authenticated data: `provider|key-id|v2`.
///
/// Separators are `|` and a `provider` or `key-id` containing one can forge
/// another identity's AAD (`a|b` + `c` vs `a` + `b|c`). Both are install-local
/// identifiers, not user input, so the realistic case is a provider id
/// containing a pipe. Restricting them to the observed character set at the
/// caller's config load is the fix; the cheap version here refuses the two
/// characters that would make a collision ambiguous.
fn aad(provider: &str, key_id: &str) -> Result<String, KeyError> {
    if provider.contains('|') || key_id.contains('|') {
        return Err(KeyError::Malformed("provider and key-id must not contain `|`"));
    }
    Ok(format!("{provider}|{key_id}|v{VERSION}"))
}

/// Encrypts `secret` into an `enc:v2:` envelope.
///
/// # Errors
/// [`KeyError::Malformed`] if `provider` or `key_id` contains `|`,
/// [`KeyError::Crypto`] if the cipher refuses (only reachable above
/// `P_MAX_MESSAGE_LEN`), or [`KeyError::BadMasterKeyLen`] if the derived key is
/// not 32 bytes.
pub fn encrypt(
    master: &MasterKey,
    provider: &str,
    key_id: &str,
    secret: &Secret,
) -> Result<String, KeyError> {
    let aad = aad(provider, key_id)?;
    let cipher = master.cipher()?;
    let nonce = aes_gcm::Aes256Gcm::generate_nonce(&mut OsRng);
    let mut buf = secret.as_bytes().to_vec();
    let tag = cipher
        .encrypt_in_place_detached(&nonce, aad.as_bytes(), &mut buf)
        .map_err(|_| KeyError::Crypto("encrypt"))?;
    Ok(format!(
        "{PREFIX}{}:{}:{}",
        codec::b64::encode(&nonce),
        codec::b64::encode(&buf),
        codec::b64::encode(&tag)
    ))
}

/// Decrypts an `enc:v2:` envelope.
///
/// # Errors
/// [`KeyError::NotEncrypted`] or [`KeyError::LegacyV1`] for a foreign prefix,
/// [`KeyError::Malformed`] for a bad field count or field length, and
/// [`KeyError::TagMismatch`] for anything that fails authentication.
///
/// The three are separate on purpose. v1 collapses a wrong key, a spliced
/// envelope and a corrupt file into one `null`
/// (`encryption.ts:260-262` swallows the error), so a broken credential store is
/// indistinguishable from a bad password.
pub fn decrypt(
    master: &MasterKey,
    provider: &str,
    key_id: &str,
    envelope: &str,
) -> Result<Secret, KeyError> {
    let aad = aad(provider, key_id)?;
    let body = envelope.strip_prefix(PREFIX).ok_or_else(|| classify(envelope))?;
    let mut fields = body.split(':');
    let (nonce_b64, ct_b64, tag_b64) = match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (Some(n), Some(c), Some(t), None) => (n, c, t),
        _ => return Err(KeyError::Malformed("expected exactly 3 `:`-separated fields")),
    };
    // Length checks before any decode, so a wrong key reports `TagMismatch` and
    // a wrong *shape* reports `Malformed` rather than both.
    if nonce_b64.len() != codec::b64::encoded_len(NONCE_LEN) {
        return Err(KeyError::Malformed("nonce field is not 12 bytes"));
    }
    if tag_b64.len() != codec::b64::encoded_len(TAG_LEN) {
        return Err(KeyError::Malformed("tag field is not 16 bytes"));
    }
    let (nonce, ct, tag) = match (
        codec::b64::decode(nonce_b64),
        codec::b64::decode(ct_b64),
        codec::b64::decode(tag_b64),
    ) {
        (Some(n), Some(c), Some(t)) => (n, c, t),
        _ => return Err(KeyError::Malformed("field is not base64")),
    };

    let cipher = master.cipher()?;
    let mut buf = ct;
    // Through fixed-size arrays, so the `from_slice` conversions below cannot
    // panic on a short field. Both lengths are already implied by the base64
    // width checks above; these conversions restate it defensively rather than
    // trusting it.
    let nonce: [u8; NONCE_LEN] =
        nonce.try_into().map_err(|_| KeyError::Malformed("nonce field is not 12 bytes"))?;
    let tag: [u8; TAG_LEN] = tag.try_into().map_err(|_| KeyError::Malformed("tag field is not 16 bytes"))?;
    cipher
        .decrypt_in_place_detached(
            aes_gcm::Nonce::from_slice(&nonce),
            aad.as_bytes(),
            &mut buf,
            aes_gcm::Tag::from_slice(&tag),
        )
        .map_err(|_| KeyError::TagMismatch)?;
    Ok(Secret::new(buf))
}

/// Whether `secret` is the plaintext behind `envelope`, in constant time.
///
/// # Errors
/// As [`decrypt`]. A wrong master key is [`KeyError::TagMismatch`], not
/// `Ok(false)` — "this envelope is not mine" and "these two secrets differ" are
/// different questions, and answering the first as the second would let a
/// caller treat an unreadable key store as a wrong password.
pub fn verify(
    master: &MasterKey,
    provider: &str,
    key_id: &str,
    envelope: &str,
    secret: &Secret,
) -> Result<bool, KeyError> {
    Ok(decrypt(master, provider, key_id, envelope)?.ct_eq(secret))
}

/// Names a foreign prefix instead of reporting a tag mismatch.
fn classify(envelope: &str) -> KeyError {
    if envelope.starts_with(LEGACY_PREFIX) {
        KeyError::LegacyV1
    } else {
        KeyError::NotEncrypted
    }
}

#[cfg(test)]
mod tests {
    use super::{HashParams, MasterKey, Salt, Secret, decrypt, encrypt, verify};
    use crate::secret::{KeyMeta, MasterKeySource};

    fn test_master() -> MasterKey {
        let meta = KeyMeta::with_params(HashParams::FAST, Salt::generate());
        MasterKey::new(Secret::generate(), meta).expect("derive")
    }

    #[test]
    fn round_trips_a_credential() {
        let mk = test_master();
        let secret = Secret::new(b"sk-provider-key".to_vec());
        let env = encrypt(&mk, "openai", "k1", &secret).expect("encrypt");
        assert_eq!(decrypt(&mk, "openai", "k1", &env).expect("decrypt").as_bytes(), b"sk-provider-key");
    }

    #[test]
    fn prefixes_with_the_version() {
        let mk = test_master();
        let env = encrypt(&mk, "openai", "k1", &Secret::generate()).expect("encrypt");
        assert!(env.starts_with("enc:v2:"));
    }

    #[test]
    fn uses_a_twelve_byte_nonce_each_time() {
        let mk = test_master();
        let a = encrypt(&mk, "openai", "k1", &Secret::new(b"x".to_vec())).expect("encrypt");
        let b = encrypt(&mk, "openai", "k1", &Secret::new(b"x".to_vec())).expect("encrypt");
        assert_ne!(a, b, "a fresh nonce per encryption is the whole point of GCM");
    }

    #[test]
    fn aad_rejects_a_pipe_in_the_provider() {
        let mk = test_master();
        assert!(encrypt(&mk, "a|b", "k1", &Secret::generate()).is_err());
    }

    #[test]
    fn refuses_a_different_provider() {
        // The v1 splice: same envelope, new identity. Fails because the AAD
        // changed, which is why the tag no longer verifies.
        let mk = test_master();
        let env = encrypt(&mk, "openai", "k1", &Secret::new(b"sk-x".to_vec())).expect("encrypt");
        assert!(decrypt(&mk, "groq", "k1", &env).is_err());
    }

    #[test]
    fn refuses_a_different_key_id() {
        let mk = test_master();
        let env = encrypt(&mk, "openai", "k1", &Secret::new(b"sk-x".to_vec())).expect("encrypt");
        assert!(decrypt(&mk, "openai", "k2", &env).is_err());
    }

    #[test]
    fn refuses_a_foreign_master_key() {
        let env = encrypt(&test_master(), "openai", "k1", &Secret::new(b"sk-x".to_vec())).expect("encrypt");
        assert!(decrypt(&test_master(), "openai", "k1", &env).is_err());
    }

    #[test]
    fn refuses_a_tampered_ciphertext() {
        let mk = test_master();
        let env = encrypt(&mk, "openai", "k1", &Secret::new(b"sk-x".to_vec())).expect("encrypt");
        let mut bad: Vec<char> = env.chars().collect();
        let i = bad.len() - 4;
        bad[i] = if bad[i] == 'A' { 'B' } else { 'A' };
        assert!(decrypt(&mk, "openai", "k1", &bad.into_iter().collect::<String>()).is_err());
    }

    #[test]
    fn names_a_v1_envelope_instead_of_a_tag_mismatch() {
        let mk = test_master();
        assert!(matches!(decrypt(&mk, "openai", "k1", "enc:v1:aabb:ccdd:eeff"), Err(crate::KeyError::LegacyV1)));
    }

    #[test]
    fn names_an_unprefixed_string() {
        let mk = test_master();
        assert!(matches!(decrypt(&mk, "openai", "k1", "sk-plain"), Err(crate::KeyError::NotEncrypted)));
    }

    #[test]
    fn rejects_a_short_tag_field() {
        let mk = test_master();
        let env = format!("enc:v2:{}:{}:{}", "A".repeat(16), "A".repeat(8), "A".repeat(4));
        assert!(decrypt(&mk, "openai", "k1", &env).is_err());
    }

    #[test]
    fn verify_accepts_the_right_secret_and_rejects_a_wrong_one() {
        let mk = test_master();
        let secret = Secret::new(b"sk-right".to_vec());
        let env = encrypt(&mk, "openai", "k1", &secret).expect("encrypt");
        assert!(verify(&mk, "openai", "k1", &env, &secret).expect("verify"));
        assert!(!verify(&mk, "openai", "k1", &env, &Secret::new(b"sk-wrong".to_vec())).expect("verify"));
    }

    #[test]
    fn the_same_master_and_salt_re_derive_the_same_key() {
        let master = Secret::generate();
        let salt = Salt::generate();
        let a = MasterKey::new(master.to_owned_secret(), KeyMeta::with_params(HashParams::FAST, salt)).expect("a");
        let b = MasterKey::new(master.to_owned_secret(), KeyMeta::with_params(HashParams::FAST, salt)).expect("b");
        assert!(a.aead_key().ct_eq(b.aead_key()));
    }

    #[test]
    fn a_different_salt_derives_a_different_key() {
        let master = Secret::generate();
        let a = MasterKey::new(master.to_owned_secret(), KeyMeta::with_params(HashParams::FAST, Salt::generate()))
            .expect("a");
        let b = MasterKey::new(master.to_owned_secret(), KeyMeta::with_params(HashParams::FAST, Salt::generate()))
            .expect("b");
        assert!(!a.aead_key().ct_eq(b.aead_key()));
    }

    #[test]
    fn recommended_params_derive_a_key_that_round_trips() {
        // Keeps the shipped defaults honest: RECOMMENDED must be accepted by
        // argon2id, not just FAST.
        let mk = MasterKey::new(Secret::generate(), KeyMeta::with_params(HashParams::RECOMMENDED, Salt::generate()))
            .expect("derive at RECOMMENDED");
        let secret = Secret::new(b"sk-x".to_vec());
        let env = encrypt(&mk, "openai", "k1", &secret).expect("encrypt");
        assert_eq!(decrypt(&mk, "openai", "k1", &env).expect("decrypt").as_bytes(), b"sk-x");
    }

    #[test]
    fn ephemeral_source_loads() {
        assert!(MasterKey::load(&MasterKeySource::Ephemeral).is_ok());
    }
}
