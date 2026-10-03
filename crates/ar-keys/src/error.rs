//! Every refusal the credential and token half of `ar-keys` can produce.
//! `thiserror` in a library, per `docs/03-crates-and-deps.md` ch.4.

use crate::secret::KEY_LEN;

/// Why a credential or an access token was refused.
///
/// No catch-all variant, mirroring `ar_route::RouteError`: an unmatched error
/// means a real unhandled case, and a `#[non_exhaustive]` enum would only move
/// that compile error somewhere less useful. Admission has its own error type
/// ([`crate::admit::AdmitError`]) because it carries a `Retry-After` and
/// nothing here does.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The string is neither an `enc:v2:` envelope nor a recognised legacy form.
    #[error("not an `enc:v2:` credential envelope")]
    NotEncrypted,

    /// An `enc:v1:` envelope was handed in.
    ///
    /// v1 (`../OmniRoute/src/lib/db/encryption.ts`) was
    /// `scryptSync(master, "omniroute-field-encryption-v1", 32)` at Node's
    /// defaults — `N=16384, r=8, p=1`, never stored — then AES-256-GCM with a
    /// **16-byte IV, no AAD**, and it **failed open**: an encryption error
    /// returned the plaintext secret to the caller. It is refused rather than
    /// decrypted because `ar-keys` has never written a v1 envelope, so there is
    /// nothing to migrate and `scrypt` would be a dependency bought for a format
    /// with no deployed keys in this codebase.
    ///
    /// The variant exists so a v1 string is refused *by name* instead of
    /// surfacing as a `TagMismatch` — the tag really would not match, because
    /// the KDF and the IV length differ. It is not a permanent verdict:
    /// reproducing v1 is mechanical, and the salt is a literal in the OmniRoute
    /// tree, not a secret. Supporting it means adding `scrypt`, the 28-byte
    /// static salt, and the `sha256(master)[0..16]` dynamic-salt generation that
    /// older rows may still use.
    #[error(
        "`enc:v1:` credential (scrypt + static salt, 16B IV, no AAD, fail-open) is not supported; add `scrypt` to migrate it"
    )]
    LegacyV1,

    /// A primitive refused. The payload names the operation, never the input.
    #[error("crypto operation `{0}` failed")]
    Crypto(&'static str),

    /// The envelope is `enc:v2:` but its field count or field lengths are wrong.
    #[error("`enc:v2:` envelope is malformed: {0}")]
    Malformed(&'static str),

    /// AES-GCM refused the ciphertext.
    ///
    /// The tag did not verify: wrong master key, wrong `provider`/`key-id` AAD,
    /// or a tampered envelope. Deliberately does not say which — a caller that
    /// can tell "wrong provider" from "wrong key" learns about the key store.
    #[error("authentication tag mismatch: wrong master key, wrong AAD, or a tampered envelope")]
    TagMismatch,

    /// argon2id rejected the requested cost parameters.
    #[error("argon2id parameters rejected: {0}")]
    BadParams(String),

    /// The master key is not [`crate::KEY_LEN`] bytes.
    #[error("master key must be {KEY_LEN} bytes, got {0}")]
    BadMasterKeyLen(usize),

    /// The token's signature did not verify under the master key.
    #[error("token signature is invalid")]
    BadSignature,

    /// The token is past `exp`. A stolen copy is useless from here on.
    ///
    /// Carries no "how long ago". The verifier learns from `jsonwebtoken` that
    /// the expiry failed, not when it was, and inventing a number from a second
    /// clock read would be a worse answer than none — the actionable cause is
    /// almost always the caller's clock skew.
    #[error("token has expired")]
    Expired,

    /// The token is before `nbf` (clock skew, or minted ahead of time).
    #[error("token is not valid yet")]
    NotYetValid,

    /// A scope string is not one of the three this build knows.
    ///
    /// Refused loudly rather than dropped, so a typo in an operator's config
    /// surfaces at issue time instead of showing up later as a client that
    /// mysteriously cannot do something.
    #[error("unknown scope `{0}`; valid scopes are `read:*`, `write:*`, `execute:completions`")]
    UnknownScope(String),

    /// The token's `jti` is on the revocation list.
    ///
    /// This is the whole point of the `jti`: signing alone cannot be withdrawn,
    /// so a leaked token stays live for its full 15 minutes unless something
    /// remembers it. See [`crate::revoke::Revocation`].
    #[error("token `{jti}` is revoked")]
    Revoked {
        /// The revoked token id. Safe to log; it is not a credential.
        jti: String,
    },

    /// The token is valid but does not carry the scope this action needs.
    #[error("scope `{needed}` is not granted; token carries `{granted}`")]
    ScopeDenied {
        /// The scope the caller asked for.
        needed: &'static str,
        /// The scopes the token actually carries.
        granted: String,
    },

    /// The token has no `jti`, so it cannot be revoked.
    ///
    /// Issued tokens always carry one, so this is a forged or hand-built token
    /// reaching the verifier — a claim set signed by nobody.
    #[error("token carries no `jti` and therefore cannot be revoked")]
    MissingJti,

    /// The revocation list is at its cap with no entry eligible for sweep.
    ///
    /// Fail-closed on purpose. The alternatives are both worse: dropping a live
    /// revocation silently un-revokes a stolen token, and growing past the cap
    /// trades a bounded memory budget for an unbounded one on the request path.
    #[error("revocation list is full ({cap} live entries); refusing to drop a live revocation")]
    RevokeListFull {
        /// The configured cap.
        cap: usize,
    },

    /// An `anonymous` OAuth session was handed a credential reference.
    ///
    /// `Anonymous` is the kind with no credential at all: the browser-session
    /// case, where the provider's token lives in a cookie this proxy never holds
    /// and there is nothing to point a row at. Pointing one at a `credentials`
    /// row asserts the opposite — that a secret this process can decrypt backs
    /// the session — so both the store API and the table's CHECK refuse it.
    ///
    /// Carries the offending *name*, which is a `keys:` label and therefore
    /// already public in `config.yaml`; [`crate::CredentialStore::list_names`]
    /// hands the same strings to `ar doctor` unencrypted.
    #[error(
        "an `anonymous` session holds no credential; drop the reference to `{placement}` or change the session kind"
    )]
    AnonymousCredential {
        /// The credential name the caller tried to attach.
        placement: String,
    },

    /// A local store refused a read or a write — the redb revocation ledger
    /// ([`crate::Revocation`]) or the sqlite credential table
    /// ([`crate::CredentialStore`]).
    ///
    /// Carries SQLite's or redb's own message as text rather than the error
    /// itself: `KeyError` promises `Clone`/`PartialEq`, and neither library's
    /// error type offers them. The text is the library's, never a value read out
    /// of the store.
    #[error("store: {0}")]
    Store(String),

    /// The master key could not be loaded from its source.
    #[error("master key unavailable: {0}")]
    MasterKey(String),
}
