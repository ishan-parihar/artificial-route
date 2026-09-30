//! The bearer gate: `ar-keys` token verification, or nothing.
//!
//! Two states, and the pairing between them is the security property:
//!
//! * **A gate is configured.** Every chat request must carry
//!   `Authorization: Bearer <token>` carrying [`ar_keys::Scope::ExecuteCompletions`].
//! * **No gate.** No check — and [`crate::app::bind_addr`] then binds loopback
//!   only, unless the operator also declares the server public.
//!
//! "No auth" and "public bind" are therefore not two independent switches that
//! can be set apart: a server with no gate refuses a routable bind, and a server
//! on a routable bind has a gate or does not start.
//!
//! # Which `ar-keys` surface, and why not `Admission`
//!
//! [`ar_keys::Tokens`] is the credential check, and it is synchronous and
//! allocation-free after construction — a JWT signature plus an expiry, which is
//! what a request-path decision needs.
//!
//! [`ar_keys::Admission`] is a *concurrency* gate: it hands out a lease that
//! keeps a heavy request from starving an interactive one. Holding a lease for
//! the life of a streaming body means holding it across the response body, which
//! `AppState` cannot do — the lease would have to be owned by the body itself and
//! released when the client disconnects mid-stream. That is real work and it is
//! not an authorisation decision, so it is not smuggled in here as a second way
//! to say "allowed".
//!
//! TODO(#p1-admission): move lane control to a body middleware that owns the
//! lease, once a dropped connection is observable from the body future.
//!
//! # What a rejected token says
//!
//! The reason is `ar-keys`' own: a bad signature, an expired token, a missing
//! scope. No token value, ever — this response is a cacheable 401 that a
//! browser may keep, and a credential echoed into one would be a credential on
//! disk.

use ar_keys::{KeyError, KeyMeta, MasterKey, Scope, Secret, Tokens};

/// Verifies bearer tokens. Shared behind an `Arc`, never cloned.
pub struct AuthGate {
    tokens: Tokens,
}

impl std::fmt::Debug for AuthGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Tokens` holds the master key's bytes. Its count is the only safe
        // summary, and there is nothing else worth saying.
        f.debug_struct("AuthGate").finish_non_exhaustive()
    }
}

impl AuthGate {
    /// Builds a gate from a raw master key.
    ///
    /// The key is bytes of the operator's choosing — `ar_keys::MasterKey`
    /// derives the AEAD and signing subkeys from them, so the same value signs
    /// tokens and must be the same value on every node that verifies them.
    ///
    /// # Errors
    ///
    /// [`KeyError::Crypto`] when `master` is empty or the wrong length.
    pub fn new(master: &[u8]) -> Result<Self, KeyError> {
        // `ar_keys::Secret` enforces `KEY_LEN` and `MasterKey::new` derives the
        // AEAD and signing subkeys from it, so a truncated or short master key is
        // refused here rather than becoming a key that fails every check later.
        let master = MasterKey::new(Secret::from_slice(master)?, KeyMeta::generate())?;
        Ok(Self {
            tokens: Tokens::new(&master),
        })
    }

    /// Checks `token` for the completions scope.
    ///
    /// # Errors
    ///
    /// A client-facing reason: `ar-keys`' `Display` for a bad signature, an
    /// expiry, a revoked `jti` or a missing scope names which, and none of them
    /// echoes the token.
    pub fn verify(&self, token: &str) -> Result<(), String> {
        let verified = self.tokens.introspect(token).map_err(|e| reason(&e))?;
        if !verified.scopes.grants(Scope::ExecuteCompletions) {
            return Err(format!(
                "this token lacks the {} scope",
                Scope::ExecuteCompletions
            ));
        }
        Ok(())
    }

    /// Mints a token for `key_id` with every scope.
    ///
    /// The issuing side belongs to `ar-cli`'s key commands, not to the request
    /// path; it lives here only so a test can produce a token this gate accepts
    /// without reaching into `ar-keys`' internals.
    ///
    /// # Errors
    ///
    /// [`KeyError::Crypto`] when signing fails.
    pub fn issue_for_tests(&self, key_id: &str) -> Result<ar_keys::Issued, KeyError> {
        self.tokens.issue(ar_keys::Issue {
            key_id,
            scopes: ar_keys::ScopeSet::all(),
            device_id: None,
            ttl: None,
        })
    }
}

/// Turns a `ar-keys` failure into a client-safe sentence.
fn reason(e: &KeyError) -> String {
    match e {
        KeyError::Expired => "this token has expired".to_owned(),
        KeyError::Revoked { jti: _ } => "this token has been revoked".to_owned(),
        KeyError::ScopeDenied { needed, .. } => {
            format!("this token lacks the {needed} scope")
        }
        // Everything else is a signature or shape problem, and distinguishing
        // them for a caller who presented a bad token only helps them guess.
        _ => "this token is not valid".to_owned(),
    }
}

// Shared across every request; prove it rather than discovering it from a spawn
// error on the first concurrent request.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AuthGate>();
};

#[cfg(test)]
mod tests {
    use super::AuthGate;

    fn gate() -> AuthGate {
        AuthGate::new(b"0123456789abcdef0123456789abcdef").expect("master key accepted")
    }

    #[test]
    fn accepts_a_token_it_issued() {
        let g = gate();
        let issued = g.issue_for_tests("key-1").expect("token mints");
        assert!(g.verify(&issued.access).is_ok());
    }

    #[test]
    fn refuses_a_token_that_is_not_a_jwt() {
        let g = gate();
        assert!(g.verify("not-a-token").is_err());
    }

    #[test]
    fn refuses_a_token_signed_by_another_master() {
        let mine = gate();
        let theirs = AuthGate::new(b"ffffffffffffffffffffffffffffffff").expect("master key");
        let issued = theirs.issue_for_tests("key-1").expect("token mints");
        assert!(mine.verify(&issued.access).is_err());
    }

    #[test]
    fn refuses_a_token_without_the_completions_scope() {
        // Minted by `ar-keys` directly with a read-only scope set, which is the
        // shape an operator over-issues by accident.
        let master = ar_keys::MasterKey::new(
            ar_keys::Secret::from_slice(b"0123456789abcdef0123456789abcdef")
                .expect("key length"),
            ar_keys::KeyMeta::generate(),
        )
        .expect("master key");
        let tokens = ar_keys::Tokens::new(&master);
        let issued = tokens
            .issue(ar_keys::Issue {
                key_id: "key-1",
                scopes: ar_keys::ScopeSet::of([ar_keys::Scope::ReadAll]),
                device_id: None,
                ttl: None,
            })
            .expect("token mints");
        assert!(gate().verify(&issued.access).is_err());
    }

    #[test]
    fn never_renders_the_key_in_debug() {
        let text = format!("{:?}", gate());
        assert!(text.starts_with("AuthGate"), "unexpected Debug: {text}");
        assert!(!text.contains("0123456789abcdef"), "the key leaked: {text}");
    }

    #[test]
    fn refuses_a_master_key_of_the_wrong_length() {
        assert!(AuthGate::new(b"short").is_err());
    }
}
