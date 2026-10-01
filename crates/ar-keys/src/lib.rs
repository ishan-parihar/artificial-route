//! `ar-keys` — credential protection, scoped access tokens, and lane admission.
//!
//! Three concerns that share one threat model, in one leaf crate, because they
//! share one master key:
//!
//! * [`hash`] — the `enc:v2:` envelope. argon2id KDF, 12-byte nonce, and
//!   `provider|key-id|v2` as AAD so a ciphertext cannot be moved between rows.
//! * [`token`] — HS256 access tokens with `read:*` / `write:*` /
//!   `execute:completions` scopes and a `jti` that [`revoke`] can withdraw.
//! * [`admit`] — three lanes (`interactive` / `batch` / `heavy`) where the heavy
//!   lane cannot starve interactive.
//!
//! [`audit`] and [`secret`] are the two cross-cutting pieces: a bounded ring that
//! structurally cannot hold a credential, and a zeroizing buffer with one
//! redacting `Debug`. [`store`] is where the encrypted envelopes land: a
//! gitignored sqlite table, one row per credential name, re-derivable across a
//! restart.
//!
//! ```
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use ar_keys::{
//!     Admission, Issue, KeyMeta, Lane, LaneSpec, MasterKey, Revocation, Scope, ScopeSet, Secret,
//!     Tokens, encrypt, verify,
//! };
//!
//! // A master key and an `enc:v2:` envelope.
//! let master = MasterKey::new(Secret::generate(), KeyMeta::generate())?;
//! let secret = Secret::new(b"sk-provider-key".to_vec());
//! let envelope = encrypt(&master, "openai", "key-1", &secret)?;
//! assert!(verify(&master, "openai", "key-1", &envelope, &secret)?);
//!
//! // A scoped token, revocable by `jti`.
//! let path = std::env::temp_dir().join("ar-keys-doctest.redb");
//! let _ = std::fs::remove_file(&path);
//! let revocations = Revocation::open(&path, 64)?;
//! let tokens = Tokens::new(&master);
//! let issued = tokens.issue(Issue {
//!     key_id: "key-1",
//!     scopes: ScopeSet::of([Scope::ReadAll, Scope::ExecuteCompletions]),
//!     device_id: Some("laptop"),
//!     ttl: None,
//! })?;
//!
//! assert!(tokens.verify(&issued.access, Scope::ExecuteCompletions, &revocations).is_ok());
//! tokens.revoke(&issued.access, &revocations)?;
//! assert!(tokens.verify(&issued.access, Scope::ExecuteCompletions, &revocations).is_err());
//!
//! // Lane admission.
//! let admission = Admission::new(
//!     [LaneSpec::INTERACTIVE, LaneSpec::BATCH, LaneSpec::HEAVY],
//!     600,
//! )?;
//! let lease = admission.acquire(Lane::Interactive, "key-1").await?;
//! assert_eq!(lease.lane(), Lane::Interactive);
//!
//! drop(lease);
//! let _ = std::fs::remove_file(&path);
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod admit;
mod audit;
mod codec;
mod error;
mod hash;
mod revoke;
mod secret;
mod store;
mod token;

// Re-exported so `hash.rs` and `secret.rs` can spell the encoders as
// `crate::b64` / `crate::hex` without making `codec` itself public.
pub(crate) use codec::{b64, hex};

pub use admit::{
    AdmitError, Admission, DEFAULT_IDLE_TTL, DEFAULT_MAX_CONNS, HEAVY_SHARE_DEN, HEAVY_SHARE_NUM, Lane, LaneSpec, Lease,
};
pub use audit::{Action, Audit, AuditLine, Outcome};
pub use error::KeyError;
pub use hash::{HashParams, LEGACY_PREFIX, NONCE_LEN, PREFIX, SALT_LEN, TAG_LEN, VERSION, decrypt, encrypt, verify};
pub use revoke::{DEFAULT_CAP, Revocation};
pub use secret::{KeyMeta, MasterKey, MasterKeySource, Salt, Secret, KEY_LEN};
pub use store::{CredentialStore, MASTER_KEY_VAR, OAuthSession, SessionKind};
pub use token::{
    ACCESS_TTL, DEFAULT_LEEWAY, Issue, Issued, REFRESH_TTL, Scope, ScopeSet, Tokens, Verified,
};
