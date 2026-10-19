//! The control panel's own credential check.
//!
//! `/ui` is a second surface with a different blast radius from `/v1`, so it
//! carries its own gate rather than inheriting one. The reasoning, in one
//! paragraph, because it is the whole argument for this module:
//!
//! * `/v1` **spends** the operator's money. A request there is bounded by the
//!   per-key ceilings in `limits:` and the key's own token.
//! * `/ui` **changes the routing** that decides where `/v1` sends traffic, and
//!   it **mints the tokens** that authorize `/v1` at all. A panel that can mint
//!   is a panel that can hand out spend.
//!
//! So the panel's gate is not "the same gate `/v1` has". It differs in one
//! specific way, and the difference is the whole point.
//!
//! ## The three gate states
//!
//! [`GateState`] is resolved **once at boot**, and it has three arms because
//! the operator's situation has three distinct shapes — and the difference
//! between them is the difference between a supported install and a silent
//! security downgrade:
//!
//! | state | what it means | reads | writes |
//! |---|---|---|---|
//! | [`GateState::Unarmed`] | no gate row was ever written | allowed | allowed |
//! | [`GateState::Armed`] | a gate row that decrypts | **token required** | **token required** |
//! | [`GateState::Broken`] | a gate row that will not decrypt | allowed | **refused** |
//!
//! `Unarmed` is the supported loopback install: the socket is the trust
//! boundary, exactly as `/v1` is on the same build, and the panel inherits it.
//!
//! `Armed` is the case that matters: the operator *believes* there is a gate,
//! so a request that carries no valid token is refused. This is the row the
//! 2026-10-19 audit found missing — the `/ui` router consulted nothing while
//! the gate sat armed underneath it, and an anonymous caller could mint a
//! spendable token.
//!
//! `Broken` is the case AR-3 is about: the store holds a gate row that the
//! current master cannot open. The server's own boot log already says
//! "serving ungated", so the operator believes the gate is on while it is
//! not. Reads may degrade — a stale credential should not black out the
//! operator's dashboard — but **writes must refuse**: the panel edits the
//! config that decides where traffic goes, and it mints the tokens that
//! authorize traffic, and neither is safe to do while the operator cannot
//! prove which credentials the panel is managing.
//!
//! ## The token the panel takes
//!
//! The panel accepts **either** an `execute:completions` token (what
//! `aroute keys mint` issues) **or** a `write:*` token. The first is what the
//! operator already holds; the second is the scope that exists to mean "may
//! change state" and is what an operator would mint for a panel-only token
//! later, without handing out completion rights.
//!
//! A `read:*`-only token is refused. It is the scope a token gets for reading
//! `/v1/models`, and a token that can only read models must not be able to
//! read the ledger or the credential inventory either — those are a different
//! kind of read.
//!
//! ## What the panel does NOT check
//!
//! `introspect` verifies the signature and the expiry and nothing else. **A
//! revoked token still opens the panel** — and that is not a regression this
//! module introduces, because `/v1`'s own `AuthGate::verify` does exactly the
//! same thing (`ar-server/src/keys.rs`). The two surfaces are consistent,
//! which is the right property while both share the same engine.
//!
//! Revocation is a real gap and it is a **separate change**: the revoke list
//! lives in the credential store, the panel's gate holds only the token
//! engine, and wiring the list in means opening a second connection and
//! consulting it on every request. It is worth doing for `/v1` and the panel
//! **together** — a panel-only fix would leave `/v1` with the same hole and
//! would make the two surfaces disagree, which is worse than either being
//! consistently incomplete. Recorded in `docs/28-gap-ledger.md` as **A7**.
//!
//! ## The refusal
//!
//! A refusal is a **401** with the same envelope shape `/v1` uses, and it never
//! echoes the token. The panel's own HTML is served **unauthenticated** — it
//! carries no data and no secret, and a login wall in front of a stylesheet
//! would make the page's own error handling impossible to read. Only the
//! `/ui/api/*` surface is gated.

use axum::http::{HeaderMap, header};

use ar_keys::{Scope, ScopeSet, Tokens};

/// The panel's gate, resolved once at boot.
///
/// `Tokens` is not `Clone` and holds the master key's bytes, so the engine is
/// built once and shared behind the panel's own `Arc` — one argon2id
/// derivation at boot, and a signature check per request.
pub enum GateState {
    /// No gate row exists. The socket's trust boundary applies: the panel
    /// inherits the loopback bind `/v1` has on this build.
    Unarmed,
    /// A gate row that decrypts. The API requires a valid token.
    Armed(Tokens),
    /// A gate row that will not decrypt under the current master.
    ///
    /// The reason is `ar-keys`' own and is redacted by construction: it names
    /// the master variable and the cipher's complaint, never a value.
    Broken(String),
}

impl GateState {
    /// Whether the gate is armed: a gate row that decrypts.
    ///
    /// A **broken** gate is not armed, and says so in the panel's badge: the
    /// row exists and the operator believes the gate is on, and it is not.
    /// The badge naming it "unarmed" is at least the truth.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        matches!(self, Self::Armed(_))
    }

    /// Whether an API request must carry a token.
    ///
    /// Only [`GateState::Armed`] says yes. `Broken` does not — reads degrade
    /// there — and the write handlers refuse it in their own right, which is
    /// where the refusal belongs.
    #[must_use]
    pub fn requires_token(&self) -> bool {
        matches!(self, Self::Armed(_))
    }

    /// The reason a write must refuse, or `None` when a write may proceed.
    ///
    /// `Broken` is the only refusing arm: an unarmed panel is a loopback
    /// install whose operator is editing their own config, and refusing that
    /// would break the supported default.
    #[must_use]
    pub fn write_refusal(&self) -> Option<&str> {
        match self {
            Self::Broken(reason) => Some(reason),
            _ => None,
        }
    }

    /// The token engine, when the gate is armed.
    #[must_use]
    pub fn tokens(&self) -> Option<&Tokens> {
        match self {
            Self::Armed(tokens) => Some(tokens),
            _ => None,
        }
    }
}

/// The gate's state, as the panel names it.
///
/// Three arms, and the third is the one the operator most needs to see: a
/// gate row that exists and will not decrypt is a gate the server is NOT
/// enforcing, while the operator believes it is. The badge says "broken"
/// rather than "unarmed" so the difference from a box that never armed one
/// is visible at a glance.
pub fn gate_badge(gate: &GateState) -> (&'static str, &'static str) {
    match gate {
        GateState::Armed(_) => ("gate armed", "on"),
        GateState::Unarmed => ("gate unarmed", "off"),
        GateState::Broken(_) => ("gate BROKEN - serving ungated", "bad"),
    }
}

impl std::fmt::Debug for GateState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Tokens` holds the master key's bytes. Its existence is the only safe
        // summary, and nothing else is worth saying.
        match self {
            Self::Unarmed => f.write_str("GateState::Unarmed"),
            Self::Armed(_) => f.write_str("GateState::Armed"),
            Self::Broken(reason) => f.debug_tuple("GateState::Broken").field(reason).finish(),
        }
    }
}

/// The credential a request presents, if any.
///
/// Only the `Authorization: Bearer` slot. The panel is a browser surface and
/// a browser sends exactly one credential header; the `x-api-key` and
/// `x-goog-api-key` slots exist for CLI clients that speak a different
/// dialect, and reading them here would let a Gemini client's key open the
/// control panel.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// The gate's answer for one request.
///
/// `Ok(())` means the request may proceed. `Err(reason)` carries a
/// client-safe sentence — never the token, and never the internal reason a
/// JWT library produced.
pub type Verdict = Result<(), String>;

/// Checks one request's credential against the panel's gate.
///
/// `state` is the boot-time [`GateState`]. A state that does not require a
/// token admits the request without looking at one — that is the loopback
/// install, and it is the *socket* that is the trust boundary there, not the
/// panel.
///
/// This is the difference from `/v1`, where an unreadable master degrades to
/// an ungated server on a loopback bind. The panel edits files and mints
/// tokens, so "we could not check" has to mean "no" — and the write handlers
/// carry that refusal themselves, in [`GateState::write_refusal`].
pub fn check(headers: &HeaderMap, state: &GateState) -> Verdict {
    if !state.requires_token() {
        return Ok(());
    }
    let Some(tokens) = state.tokens() else {
        return Err("the panel's gate is not armed".to_owned());
    };
    let Some(token) = bearer(headers) else {
        return Err(
            "this panel needs a credential; paste one from `aroute keys mint`".to_owned(),
        );
    };
    let scopes = match tokens.introspect(token) {
        Ok(verified) => verified.scopes,
        Err(_) => return Err("this token is not valid".to_owned()),
    };
    if admits(scopes) {
        Ok(())
    } else {
        Err(format!(
            "this token lacks the {} scope the panel needs",
            Scope::WriteAll
        ))
    }
}

/// Whether a scope set may use the panel.
///
/// `write:*` is the panel's own scope. `execute:completions` is what
/// `aroute keys mint` issues today, and it already implies full use of the
/// operator's own gateway — so a token carrying it may use the panel too,
/// which is what makes the panel reachable with the token already in the
/// operator's file.
///
/// Ponytail: one scope check, not a permission matrix. The panel has one
/// operator and one machine; a finer model would be a claim about a product
/// that does not exist yet.
fn admits(scopes: ScopeSet) -> bool {
    scopes.grants(Scope::WriteAll) || scopes.grants(Scope::ExecuteCompletions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ar_keys::{Issue, KeyMeta, MasterKey, Secret};

    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().expect("header value"),
        );
        h
    }

    fn engine() -> Tokens {
        let master = MasterKey::new(
            Secret::from_slice(&[7u8; 32]).expect("32 bytes"),
            KeyMeta::generate(),
        )
        .expect("master");
        Tokens::new(&master)
    }

    fn token_with(tokens: &Tokens, scopes: ScopeSet) -> String {
        tokens
            .issue(Issue {
                key_id: "panel",
                scopes,
                device_id: None,
                ttl: None,
            })
            .expect("token")
            .access
    }

    #[test]
    fn an_unarmed_panel_admits_a_request_without_a_credential() {
        // The supported loopback install: the socket is the trust boundary.
        assert!(check(&HeaderMap::new(), &GateState::Unarmed).is_ok());
    }

    #[test]
    fn a_broken_gate_still_admits_a_read_but_refuses_a_write() {
        // Reads degrade; writes do not. That split is the whole point of the
        // three-armed gate state.
        let state = GateState::Broken("authentication tag mismatch".to_owned());
        assert!(check(&HeaderMap::new(), &state).is_ok());
        assert!(state.write_refusal().is_some());
    }

    #[test]
    fn an_armed_panel_refuses_a_request_with_no_credential() {
        let state = GateState::Armed(engine());
        assert!(check(&HeaderMap::new(), &state).is_err());
    }

    #[test]
    fn an_armed_panel_admits_an_execute_completions_token() {
        let tokens = engine();
        let token = token_with(&tokens, ScopeSet::of([Scope::ExecuteCompletions]));
        let state = GateState::Armed(tokens);
        assert!(check(&headers(&token), &state).is_ok());
    }

    #[test]
    fn an_armed_panel_admits_a_write_token() {
        let tokens = engine();
        let token = token_with(&tokens, ScopeSet::of([Scope::WriteAll]));
        let state = GateState::Armed(tokens);
        assert!(check(&headers(&token), &state).is_ok());
    }

    #[test]
    fn an_armed_panel_refuses_a_read_only_token() {
        let tokens = engine();
        let token = token_with(&tokens, ScopeSet::of([Scope::ReadAll]));
        let state = GateState::Armed(tokens);
        assert!(
            check(&headers(&token), &state).is_err(),
            "a read-only token must not open the control panel"
        );
    }

    #[test]
    fn an_armed_panel_refuses_a_token_from_another_master() {
        let other = MasterKey::new(
            Secret::from_slice(&[9u8; 32]).expect("32 bytes"),
            KeyMeta::generate(),
        )
        .expect("master");
        let foreign = token_with(&Tokens::new(&other), ScopeSet::of([Scope::WriteAll]));
        let state = GateState::Armed(engine());
        assert!(check(&headers(&foreign), &state).is_err());
    }

    #[test]
    fn the_refusal_never_echoes_the_token() {
        let state = GateState::Armed(engine());
        let Err(reason) = check(&headers("secret-token-value"), &state) else {
            panic!("a bad token is refused");
        };
        assert!(
            !reason.contains("secret-token-value"),
            "the reason must not carry the token: {reason}"
        );
    }
}
