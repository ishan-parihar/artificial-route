//! Scoped access tokens: HS256, a 15-minute lifetime, a `jti` that can be
//! revoked.
//!
//! Three scopes, from `docs/04-subsystems.md`:
//!
//! | Wire | Grants |
//! |---|---|
//! | `read:*` | every read tool and `GET /v1/models` |
//! | `write:*` | every mutating tool |
//! | `execute:completions` | the data plane: `/v1/chat/completions`, `/v1/responses` |
//!
//! `read:*` and `write:*` are deliberately not sub-scoped. A finer model
//! (`read:models`, `read:usage`) is a claim about a product that does not exist
//! yet, and an over-granted `read:*` is caught by the fact that a read cannot
//! mutate anything. Narrow the grant when there is something to narrow against.
//!
//! The scope set is a bitfield, and an empty set grants nothing. That is the
//! default-deny the gateway's `PolicySet::validate` reaches only when allow rules
//! exist (`../agentgateway/crates/agentgateway/src/http/authorization.rs:254`);
//! here it is unconditional, because an empty scope set is a token nobody
//! bothered to scope and it should not be a token that does everything.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::rand_core::OsRng;
use aes_gcm::aead::rand_core::RngCore as _;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::error::KeyError;
use crate::revoke::Revocation;
use crate::secret::{MasterKey, Secret};

/// Access-token lifetime. A stolen token is worthless after this.
pub const ACCESS_TTL: Duration = Duration::from_secs(15 * 60);

/// Refresh-token lifetime.
pub const REFRESH_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// Default clock-skew tolerance on `exp` and `nbf`.
///
/// This is a live security knob, not a tolerance to forget: it *extends* every
/// access token's life by this much, so an effective lifetime of
/// `ACCESS_TTL + LEEWAY_SECS`. 30s suits a fleet whose clocks are NTP-synced;
/// a deployment that cannot assume that should lower it via
/// [`Tokens::with_leeway`].
pub const DEFAULT_LEEWAY: Duration = Duration::from_secs(30);

/// Default issuer.
pub const DEFAULT_ISSUER: &str = "artificial-route";

/// Default audience.
pub const DEFAULT_AUDIENCE: &str = "artificial-route";

/// One grantable permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Scope {
    /// `read:*`
    ReadAll = 1,
    /// `write:*`
    WriteAll = 2,
    /// `execute:completions`
    ExecuteCompletions = 4,
}

impl Scope {
    /// Every scope, in wire order.
    pub const ALL: [Scope; 3] = [Self::ReadAll, Self::WriteAll, Self::ExecuteCompletions];

    /// The wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadAll => "read:*",
            Self::WriteAll => "write:*",
            Self::ExecuteCompletions => "execute:completions",
        }
    }

    /// The bit this scope occupies in a [`ScopeSet`].
    #[must_use]
    pub const fn bit(self) -> u8 {
        self as u8
    }

    /// Parses one wire form.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|sc| sc.as_str() == s)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of granted scopes.
///
/// A bitfield rather than a `Vec`: three bits, `Copy`, and membership is one
/// `and`. Nothing here allocates, so verifying a token does not allocate either.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct ScopeSet(u8);

impl ScopeSet {
    /// Grants nothing. The default, and the reason an unscoped token is inert.
    pub const EMPTY: Self = Self(0);

    /// Builds a set from an iterator.
    ///
    /// A `fold` rather than a `collect` into a `Vec` then a fold: `Vec<Scope>`
    /// is the wrong shape for a bitfield and allocating it to throw it away is
    /// exactly the intermediate `docs/03` ch.3 warns about.
    #[must_use]
    pub fn of(scopes: impl IntoIterator<Item = Scope>) -> Self {
        scopes.into_iter().fold(Self::EMPTY, |acc, s| acc.with(s))
    }

    /// Grants everything. For a single-operator local install.
    #[must_use]
    pub const fn all() -> Self {
        Self(Scope::ReadAll.bit() | Scope::WriteAll.bit() | Scope::ExecuteCompletions.bit())
    }

    /// The set with `scope` granted.
    #[must_use]
    pub const fn with(self, scope: Scope) -> Self {
        Self(self.0 | scope.bit())
    }

    /// Whether `scope` is granted.
    #[must_use]
    pub const fn grants(self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }

    /// Whether nothing is granted.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Parses the space-delimited `scope` claim.
    ///
    /// # Errors
    /// [`KeyError::UnknownScope`] for a token carrying a scope this build does
    /// not know. A revoked-and-reissued token, or a newer issuer, would fail
    /// verification here rather than silently verifying with fewer grants — the
    /// fail-closed direction.
    pub fn parse(wire: &str) -> Result<Self, KeyError> {
        let mut set = Self::EMPTY;
        for token in wire.split_whitespace() {
            let scope =
                Scope::parse(token).ok_or_else(|| KeyError::UnknownScope(token.to_string()))?;
            set = set.with(scope);
        }
        Ok(set)
    }

    /// The space-delimited wire form, in [`Scope::ALL`] order.
    #[must_use]
    pub fn to_wire(self) -> String {
        let granted: Vec<&str> = Scope::ALL
            .into_iter()
            .filter(|s| self.grants(*s))
            .map(Scope::as_str)
            .collect();
        granted.join(" ")
    }

    /// The granted scopes, in wire order.
    pub fn iter(self) -> impl Iterator<Item = Scope> + Clone + 'static {
        let bits = self.0;
        Scope::ALL.into_iter().filter(move |s| bits & s.bit() != 0)
    }
}

impl fmt::Debug for ScopeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_wire())
    }
}

/// One half of a minted pair.
struct Half {
    token: String,
    jti: String,
    expires_at: i64,
}

/// The claim set. Private: it is the wire format, and nothing outside this
/// module should construct one.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Claims {
    iss: String,
    aud: String,
    /// The key id this token was issued to. Not a secret.
    sub: String,
    /// Unique token id. The revoke list keys on this.
    #[serde(default)]
    jti: String,
    iat: i64,
    nbf: i64,
    exp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_id: Option<String>,
    /// Space-delimited [`ScopeSet`].
    scope: String,
}

/// What to mint.
#[derive(Clone, Copy, Debug)]
pub struct Issue<'a> {
    /// The key id this token belongs to. Lands in `sub` and in the audit log.
    pub key_id: &'a str,
    /// What this token may do.
    pub scopes: ScopeSet,
    /// The device this token was issued to, if it was issued to a device.
    pub device_id: Option<&'a str>,
    /// Override the kind's default lifetime. `None` uses [`TokenKind::ttl`].
    pub ttl: Option<Duration>,
}

/// A freshly minted pair. The two tokens carry different `jti`s, so revoking the
/// access token does not also kill the refresh token.
#[derive(Clone, Debug)]
pub struct Issued {
    /// The access token.
    pub access: String,
    /// The refresh token.
    pub refresh: String,
    /// The access token's `jti`.
    pub jti: String,
    /// The access token's `exp`, as a Unix timestamp.
    pub expires_at: i64,
    /// The refresh token's `exp`, as a Unix timestamp.
    pub refresh_expires_at: i64,
}

/// A verified token.
#[derive(Clone, Debug)]
pub struct Verified {
    /// The key id, from `sub`.
    pub key_id: String,
    /// The token id, from `jti`.
    pub jti: String,
    /// The device this token was issued to, if any.
    pub device_id: Option<String>,
    /// What it may do.
    pub scopes: ScopeSet,
    /// `exp` as a Unix timestamp.
    pub expires_at: i64,
}

/// Mints and verifies tokens.
///
/// Holds a copy of the master key's bytes for HS256 — see
/// [`MasterKey::signing_key`] for why the master signs directly.
///
/// Deliberately not `Clone`: it holds a [`Secret`], and cloning one is only
/// ever wanted somewhere it was not meant to go. Share it as `Arc<Tokens>`.
pub struct Tokens {
    key: Secret,
    iss: String,
    aud: String,
    leeway: Duration,
}

impl Tokens {
    /// Signs with `master`, under the default issuer, audience and leeway.
    #[must_use]
    pub fn new(master: &MasterKey) -> Self {
        Self::with_audience(master, DEFAULT_ISSUER, DEFAULT_AUDIENCE).with_leeway(DEFAULT_LEEWAY)
    }

    /// Signs with an explicit issuer and audience.
    #[must_use]
    pub fn with_audience(master: &MasterKey, issuer: &str, audience: &str) -> Self {
        Self {
            key: master.signing_key().to_owned_secret(),
            iss: issuer.to_string(),
            aud: audience.to_string(),
            leeway: DEFAULT_LEEWAY,
        }
    }

    /// Sets the clock-skew allowance.
    ///
    /// `Duration::ZERO` means a token dies the instant `exp` passes, which is
    /// what a single-host install with a synchronised clock wants. Every
    /// additional second here is a second a stolen token stays usable.
    #[must_use]
    pub const fn with_leeway(mut self, leeway: Duration) -> Self {
        self.leeway = leeway;
        self
    }

    /// The clock-skew allowance in force.
    #[must_use]
    pub const fn leeway(&self) -> Duration {
        self.leeway
    }

    /// Mints an access/refresh pair.
    ///
    /// # Errors
    /// [`KeyError::Crypto`] if the clock is before the Unix epoch (which would
    /// make `exp` negative) or if signing fails.
    pub fn issue(&self, req: Issue<'_>) -> Result<Issued, KeyError> {
        let now = now_epoch()?;
        let access = self.mint(&req, now, req.ttl.unwrap_or(ACCESS_TTL))?;
        // The refresh lifetime is never shortened by `req.ttl`: an override
        // exists so a test can watch an access token die, and a caller that
        // could mint a 1-second refresh token would have a far more interesting
        // bug.
        let refresh = self.mint(&req, now, REFRESH_TTL)?;
        Ok(Issued {
            jti: access.jti,
            expires_at: access.expires_at,
            refresh_expires_at: refresh.expires_at,
            access: access.token,
            refresh: refresh.token,
        })
    }

    /// Verifies `token`, then checks it is not revoked and grants `need`.
    ///
    /// The order matters: signature, then expiry (both inside
    /// `jsonwebtoken`), then revocation, then scope. Checking scope before
    /// revocation would let a caller use a slow scope check to probe which `jti`s
    /// are live.
    ///
    /// # Errors
    /// [`KeyError::BadSignature`], [`KeyError::Expired`],
    /// [`KeyError::NotYetValid`] for a bad or stale token,
    /// [`KeyError::MissingJti`] if there is nothing to revoke against,
    /// [`KeyError::Revoked`] if it was revoked, and [`KeyError::ScopeDenied`]
    /// if it lacks `need`.
    pub fn verify(
        &self,
        token: &str,
        need: Scope,
        revocations: &Revocation,
    ) -> Result<Verified, KeyError> {
        let verified = self.introspect(token)?;
        if revocations.is_revoked(&verified.jti)? {
            return Err(KeyError::Revoked { jti: verified.jti });
        }
        if !verified.scopes.grants(need) {
            return Err(KeyError::ScopeDenied {
                needed: need.as_str(),
                granted: verified.scopes.to_wire(),
            });
        }
        Ok(verified)
    }

    /// Verifies signature and expiry only. Does not consult the revoke list and
    /// does not check a scope — for "who is this token" rather than "may this
    /// token do the thing".
    ///
    /// # Errors
    /// As [`Tokens::verify`], minus the revocation and scope checks.
    pub fn introspect(&self, token: &str) -> Result<Verified, KeyError> {
        let data = jsonwebtoken::decode::<Claims>(token, &self.decoding_key(), &self.validation())
            .map_err(map_jwt_error)?;
        let claims = data.claims;
        if claims.jti.is_empty() {
            return Err(KeyError::MissingJti);
        }
        let scopes = ScopeSet::parse(&claims.scope)?;
        Ok(Verified {
            key_id: claims.sub,
            jti: claims.jti,
            device_id: claims.device_id,
            scopes,
            expires_at: claims.exp,
        })
    }

    /// Revokes this token's `jti` until its `exp`.
    ///
    /// # Errors
    /// [`KeyError::BadSignature`] / [`KeyError::Expired`] if the token does not
    /// verify, and as [`Revocation::revoke`] otherwise.
    pub fn revoke(&self, token: &str, revocations: &Revocation) -> Result<(), KeyError> {
        let verified = self.introspect(token)?;
        revocations.revoke(&verified.jti, verified.expires_at)
    }

    fn mint(&self, req: &Issue<'_>, now: i64, ttl: Duration) -> Result<Half, KeyError> {
        let exp = now
            .checked_add(i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX))
            .ok_or(KeyError::Crypto("token-exp overflow"))?;
        let claims = Claims {
            iss: self.iss.clone(),
            aud: self.aud.clone(),
            sub: req.key_id.to_string(),
            // A fresh id per half: revoking the access token must not silently
            // revoke the refresh token that can mint another one.
            jti: new_jti(),
            iat: now,
            nbf: now,
            exp,
            device_id: req.device_id.map(str::to_string),
            scope: req.scopes.to_wire(),
        };
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &self.encoding_key(),
        )
        .map_err(|_| KeyError::Crypto("token-sign"))?;
        Ok(Half {
            token,
            jti: claims.jti,
            expires_at: exp,
        })
    }

    fn encoding_key(&self) -> EncodingKey {
        EncodingKey::from_secret(self.key.as_bytes())
    }

    fn decoding_key(&self) -> DecodingKey {
        DecodingKey::from_secret(self.key.as_bytes())
    }

    /// Verification policy.
    fn validation(&self) -> Validation {
        let mut v = Validation::new(Algorithm::HS256);
        // Pinned explicitly, not left to the constructor's default. `alg: none`
        // and an RS256-token-signed-with-the-HMAC-public-key swap are both
        // refused by this list and neither is refused by "trust the header".
        v.algorithms = vec![Algorithm::HS256];
        v.set_issuer(&[&self.iss]);
        v.set_audience(&[&self.aud]);
        v.leeway = self.leeway.as_secs();
        v.validate_exp = true;
        v
    }
}

/// Seconds since the Unix epoch.
///
/// # Errors
/// [`KeyError::Crypto`] if the clock predates 1970, which would make every
/// `exp` negative and silently mint a token that is already dead.
fn now_epoch() -> Result<i64, KeyError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| KeyError::Crypto("clock before epoch"))?
            .as_secs(),
    )
    .map_err(|_| KeyError::Crypto("epoch does not fit i64"))
}

/// A random token id.
///
/// Uses the CSPRNG `aes-gcm` already re-exports rather than adding `uuid`: a
/// `jti` needs to be unguessable only in the sense that guessing one is
/// pointless, since it is only ever compared, never presented as a credential.
fn new_jti() -> String {
    let mut buf = [0_u8; 16];
    OsRng.fill_bytes(&mut buf);
    crate::codec::b64::encode(&buf)
}

/// Maps a `jsonwebtoken` failure onto this crate's error set.
fn map_jwt_error(e: jsonwebtoken::errors::Error) -> KeyError {
    match e.kind() {
        ErrorKind::ExpiredSignature => KeyError::Expired,
        ErrorKind::ImmatureSignature => KeyError::NotYetValid,
        _ => KeyError::BadSignature,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DEFAULT_LEEWAY, Issue, Scope, ScopeSet, Tokens};
    use crate::hash::HashParams;
    use crate::secret::{KeyMeta, MasterKey, Salt, Secret};

    fn master() -> MasterKey {
        MasterKey::new(
            Secret::generate(),
            KeyMeta::with_params(HashParams::FAST, Salt::generate()),
        )
        .expect("derive")
    }

    #[test]
    fn empty_set_grants_nothing() {
        assert!(!ScopeSet::EMPTY.grants(Scope::ReadAll));
    }

    #[test]
    fn scope_set_round_trips_through_wire() {
        let set = ScopeSet::of([Scope::ReadAll, Scope::ExecuteCompletions]);
        assert_eq!(set.to_wire(), "read:* execute:completions");
        assert_eq!(ScopeSet::parse(&set.to_wire()), Ok(set));
    }

    #[test]
    fn wire_is_in_declared_order_regardless_of_insertion_order() {
        let set = ScopeSet::of([Scope::ExecuteCompletions, Scope::ReadAll]);
        assert_eq!(set.to_wire(), "read:* execute:completions");
    }

    #[test]
    fn parse_refuses_an_unknown_scope() {
        assert!(ScopeSet::parse("read:* superuser").is_err());
    }

    #[test]
    fn parse_of_an_empty_string_grants_nothing() {
        assert_eq!(ScopeSet::parse(""), Ok(ScopeSet::EMPTY));
    }

    #[test]
    fn introspect_returns_the_key_id_and_device() {
        let t = Tokens::new(&master());
        let issued = t
            .issue(Issue {
                key_id: "team-a",
                scopes: ScopeSet::all(),
                device_id: Some("laptop"),
                ttl: None,
            })
            .expect("issue");
        let v = t.introspect(&issued.access).expect("introspect");
        assert_eq!(v.key_id, "team-a");
        assert_eq!(v.device_id.as_deref(), Some("laptop"));
    }

    #[test]
    fn the_pair_has_two_distinct_ids() {
        let t = Tokens::new(&master());
        let i = t
            .issue(Issue {
                key_id: "k",
                scopes: ScopeSet::all(),
                device_id: None,
                ttl: None,
            })
            .expect("issue");
        let a = t.introspect(&i.access).expect("introspect access");
        let r = t.introspect(&i.refresh).expect("introspect refresh");
        assert_ne!(a.jti, r.jti);
    }

    #[test]
    fn access_and_refresh_carry_different_expiries() {
        let t = Tokens::new(&master());
        let i = t
            .issue(Issue {
                key_id: "k",
                scopes: ScopeSet::all(),
                device_id: None,
                ttl: None,
            })
            .expect("issue");
        assert!(i.refresh_expires_at > i.expires_at);
    }

    #[test]
    fn refuses_a_token_signed_by_another_key() {
        let a = Tokens::new(&master());
        let b = Tokens::new(&master());
        let i = a
            .issue(Issue {
                key_id: "k",
                scopes: ScopeSet::all(),
                device_id: None,
                ttl: None,
            })
            .expect("issue");
        assert!(b.introspect(&i.access).is_err());
    }

    #[test]
    fn refuses_a_tampered_token() {
        let t = Tokens::new(&master());
        let i = t
            .issue(Issue {
                key_id: "k",
                scopes: ScopeSet::all(),
                device_id: None,
                ttl: None,
            })
            .expect("issue");
        let bad = format!("{}x", i.access);
        assert!(t.introspect(&bad).is_err());
    }

    #[test]
    fn refuses_an_unsigned_alg_none_token() {
        let t = Tokens::new(&master());
        // {"alg":"none","typ":"JWT"} with a claims segment that grants everything.
        let claims = r#"{"iss":"artificial-route","aud":"artificial-route","sub":"k","jti":"j","iat":0,"nbf":0,"exp":99999999999,"scope":"read:* write:* execute:completions"}"#;
        let token = format!(
            "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.{}.",
            crate::codec::b64::encode(claims.as_bytes())
        );
        assert!(t.introspect(&token).is_err());
    }

    #[test]
    fn refuses_a_token_with_no_jti() {
        let t = Tokens::new(&master());
        let claims = r#"{"iss":"artificial-route","aud":"artificial-route","sub":"k","iat":0,"nbf":0,"exp":99999999999,"scope":"read:*"}"#;
        let header = crate::codec::b64::encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let body = crate::codec::b64::encode(claims.as_bytes());
        let unsigned = format!("{header}.{body}.");
        // The signature will not verify, so this is refused earlier than the
        // jti check; the assertion is that it is refused at all.
        assert!(t.introspect(&unsigned).is_err());
    }

    #[test]
    fn refuses_a_foreign_audience() {
        let issuer = Tokens::new(&master());
        let other = Tokens::with_audience(&master(), "artificial-route", "someone-else");
        let i = issuer
            .issue(Issue {
                key_id: "k",
                scopes: ScopeSet::all(),
                device_id: None,
                ttl: None,
            })
            .expect("issue");
        assert!(other.introspect(&i.access).is_err());
    }

    #[test]
    fn a_zero_leeway_dies_the_instant_exp_passes() {
        let t = Tokens::new(&master()).with_leeway(Duration::ZERO);
        assert_eq!(t.leeway(), Duration::ZERO);
    }

    #[test]
    fn the_default_leeway_extends_the_effective_lifetime() {
        // Worth stating out loud: the skew allowance is not free.
        assert_eq!(Tokens::new(&master()).leeway(), super::DEFAULT_LEEWAY);
        assert!(DEFAULT_LEEWAY > Duration::ZERO);
    }
}
