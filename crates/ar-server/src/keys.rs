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
//!
//! # The credential matrix
//!
//! One bearer slot, six ways in, because "the credential" is not one header in
//! the ecosystem this proxy serves. [`extract_credential`] tries them in the
//! reference gateway's order (`clientApi.ts:21-50`, `auth.ts:3571-3606`) and
//! the *first* one that yields a non-empty value is the credential; a header
//! that is present but empty or unparseable falls through rather than rejecting,
//! which is what keeps a client that sends both `Authorization` and `x-api-key`
//! from being refused on the strength of the one it got wrong.
//!
//! | # | source | gated on |
//! |---|---|---|
//! | 1 | `Authorization: Bearer <t>` | — |
//! | 2 | `x-api-key` | `anthropic-version` present, **or** a Claude/Anthropic `user-agent` |
//! | 3 | `x-goog-api-key` | nothing |
//! | 4 | a path segment on a tokenized alias | the path shape |
//!
//! Row 2's gate is the load-bearing one: a placeholder `x-api-key` is what a
//! non-Anthropic local-mode client sends, and reading it unconditionally would
//! turn that client's placeholder into an authentication *failure* rather than
//! into "no credential".
//!
//! Ponytail: no row for a query parameter. The reference gateway reads its
//! endpoint tokens from the path, and a key in a query string is a key that ends
//! up in every access log and proxy log between here and the client — the one
//! slot this matrix must never grow.
//!
//! # What a *presented* credential means is [`AuthMode`]'s business
//!
//! [`AuthGate::authorize`] answers one question — "may this request be served?" —
//! but it is *given* the answer to the second one rather than deciding it. That
//! second question is a deployment policy ([`AuthMode`]) and lives with the
//! configuration, because a server that degrades an invalid key to anonymous and
//! one that 401s it are the same code with a different operator decision, and
//! only the operator knows which one they deployed.
use ar_keys::{KeyError, KeyMeta, MasterKey, Scope, Secret, Tokens};
use axum::http::HeaderMap;

use crate::config::AuthMode;

/// The request-path view of a credential check: enough headers and a path to
/// find one in.
///
/// `(&HeaderMap, &str)` rather than a `Request`, because the caller already has
/// both parts destructured and threading the whole request through for two fields
/// would mean the two could not be read independently.
pub type CredentialSource<'a> = (&'a HeaderMap, &'a str);

/// The one credential a request presents, from whichever header or path slot
/// carried it.
///
/// Returns the value **trimmed** and never empty: a header present with an empty
/// value is the same as absent for every purpose here, and a caller that had to
/// remember that would eventually forget.
///
/// Allocates a `String` per request rather than borrowing out of the header map,
/// because the credential has to outlive the borrow to reach `verify`, and
/// `HeaderValue` is not a `str` anyway — a second copy of a token is exactly the
/// thing this crate does not do twice.
///
/// The order is the reference gateway's and each row exists because a named
/// client only ever sends that one:
///
/// 1. `Authorization: Bearer <t>`. A non-`Bearer` `Authorization` — an empty
///    `Bearer `, or a client's own unrelated token — does **not** short-circuit:
///    VS Code Copilot sends one of those even when its OmniRoute key lives in a
///    tokenized URL, and rejecting on it would refuse a request that carries a
///    perfectly good credential in row 4.
/// 2. `x-api-key`, but only for a request that declares itself an Anthropic
///    client (`anthropic-version`, or a `claude-code`/`claude-cli`/`anthropic`
///    user-agent). Unconditional, this reads a local-mode placeholder key as a
///    real credential and refuses a request nobody meant to authenticate.
/// 3. `x-goog-api-key`, unconditionally — `gemini-cli` and every
///    `@google/genai` client sends its key here and nowhere else.
/// 4. A path segment on a tokenized alias: `/vscode/<token>/…` and
///    `/api/v1/vscode/<token>/…`, the shapes for a client that cannot attach a
///    header at all.
#[must_use]
pub fn extract_credential((headers, path): CredentialSource<'_>) -> Option<String> {
    if let Some(token) = bearer(headers) {
        return Some(token);
    }
    if is_anthropic_client(headers)
        && let Some(token) = header_value(headers, "x-api-key")
    {
        return Some(token.to_owned());
    }
    if let Some(token) = header_value(headers, "x-goog-api-key") {
        return Some(token.to_owned());
    }
    path_token(path).map(str::to_owned)
}

/// The `Authorization: Bearer` value, if that is what the header carries.
fn bearer(headers: &HeaderMap) -> Option<String> {
    let raw = header_value(headers, "authorization")?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// Whether this request declares itself an Anthropic client.
///
/// Either signal is enough. `anthropic-version` is the spec's own header, so a
/// spec-conforming client always sends it; the user-agent arm is for the clients
/// that set the other Anthropic headers but not that one, which is the case the
/// reference gateway's own comment describes.
///
/// Matched as a substring on a lowercased value, so a version suffix
/// (`claude-cli/2.0.1 (external, cli)`) matches as well as a bare name. That is
/// deliberately loose: this decides only whether `x-api-key` is *read*, and the
/// gate that decides whether it is *accepted* is the signature check — a client
/// that can set this header can set `anthropic-version` too, so a loose match
/// cannot be used to get past anything.
fn is_anthropic_client(headers: &HeaderMap) -> bool {
    if header_value(headers, "anthropic-version").is_some() {
        return true;
    }
    let Some(agent) = header_value(headers, "user-agent") else {
        return false;
    };
    let agent = agent.to_ascii_lowercase();
    // `claude-code` is a Claude Code build; `claude-cli` and the bare `anthropic`
    // are the two spellings the reference gateway's own regex lists.
    ["claude-code", "claude-cli", "anthropic"]
        .iter()
        .any(|needle| agent.contains(needle))
}

/// One header's value, trimmed, with an empty or non-ASCII value read as absent.
///
/// `HeaderValue` is opaque bytes, so a non-UTF-8 credential is one this extractor
/// cannot hand to a token verifier as a `&str` — and reading it as absent is the
/// right answer, since a client that sent bytes no JWT is made of has presented
/// no usable credential rather than an unusual one.
fn header_value<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// The credential in a tokenized-alias path, or `None` for any other path.
///
/// Only the two documented shapes are recognised, and only their token slot:
/// `/vscode/<t>/…` and `/api/v1/vscode/<t>/…`. The `raw` and `combos` variants
/// the reference gateway also serves are *not* recognised, because this server
/// has no such routes — a token read from a path that 404s is a token that never
/// gets verified, and pretending otherwise would be a second grammar for paths
/// this router does not serve.
///
/// Percent-decoding is skipped for the same reason: a JWT is base64url and
/// contains nothing a URL parser would rewrite, so a decoder here would be a
/// second spelling of "what a path segment means" that the router's own matcher
/// does not share.
///
/// A `Vec` of segments rather than a hand-rolled walk: this is the only place the
/// request path is parsed as a path, it runs only after every header row came up
/// empty, and a second spelling of "what a segment is" is the kind of thing that
/// later disagrees with the router's.
fn path_token(path: &str) -> Option<&str> {
    let segments: Vec<&str> = path.split('/').map(str::trim).filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["vscode", token, ..] => Some(*token),
        ["api", "v1", "vscode", token, ..] => Some(*token),
        _ => None,
    }
}

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

    /// Resolves the credential `source` carries and applies `mode` to it.
    ///
    /// The one call the request path needs, so the extraction order in
    /// [`extract_credential`] and the mode's effect cannot drift apart: a caller
    /// that reached for the headers itself would be a second, subtly different
    /// matrix, and a caller that read the mode itself would be a second copy of
    /// the policy.
    ///
    /// The three answers are therefore: a verified credential serves the request
    /// under every mode, a refused one serves it under `DegradeInvalidToAnon` and
    /// `Open` and 401s under `Required`, and an absent one 401s only under
    /// `Required`.
    ///
    /// The degrade case is warned about rather than silent: a server serving
    /// anonymous requests because every client holds a stale key looks exactly
    /// like a server with no gate, and an operator watching only status codes
    /// cannot tell the two apart.
    ///
    /// # Errors
    ///
    /// A client-safe sentence when the mode refuses. For a presented credential
    /// that is `ar-keys`' own `Display` — which of a bad signature, an expiry, a
    /// revoked `jti` or a missing scope it was — and never the token. For an
    /// absent one it names the three slots a client can use.
    pub fn authorize(
        &self,
        source: CredentialSource<'_>,
        mode: AuthMode,
    ) -> Result<(), String> {
        let Some(token) = extract_credential(source) else {
            // No credential at all: only `Required` cares, and it is the mode
            // that names its own remedies.
            return match mode {
                AuthMode::Required => Err(
                    "this server requires an access token: send `Authorization: Bearer <token>`, \
                     `x-api-key`, or `x-goog-api-key`"
                        .to_owned(),
                ),
                AuthMode::DegradeInvalidToAnon | AuthMode::Open => Ok(()),
            };
        };
        match self.verify(&token) {
            Ok(()) => Ok(()),
            Err(reason) => match mode {
                // The stale-CLI-config case: an old key must not turn every request
                // into a 401, but it must stay visible in the log.
                AuthMode::DegradeInvalidToAnon => {
                    tracing::warn!(%reason, "credential refused; degrading to anonymous");
                    Ok(())
                }
                AuthMode::Open => Ok(()),
                AuthMode::Required => Err(reason),
            },
        }
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
    use axum::http::HeaderMap;

    use super::{AuthGate, CredentialSource, extract_credential};
    use crate::config::AuthMode;

    fn gate() -> AuthGate {
        AuthGate::new(b"0123456789abcdef0123456789abcdef").expect("master key accepted")
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::try_from(*name).expect("header name"),
                axum::http::HeaderValue::from_str(value).expect("header value"),
            );
        }
        headers
    }

    /// The request-path view the extractor takes.
    fn source(pairs: &[(&str, &str)]) -> (HeaderMap, String) {
        (headers(pairs), "/v1/chat/completions".to_owned())
    }

    #[test]
    fn an_options_request_carries_no_credential() {
        // The CORS layer answers OPTIONS before the handlers, so this is the
        // shape a preflight has when it arrives with no credential at all.
        assert_eq!(extract_credential((&HeaderMap::new(), "/v1/messages")), None);
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

    // --- the credential matrix -----------------------------------------

    #[test]
    fn reads_a_bearer_token() {
        let (h, p) = source(&[("authorization", "Bearer abc123")]);
        assert_eq!(extract_credential((&h, &p)).as_deref(), Some("abc123"));
    }

    #[test]
    fn reads_an_anthropic_x_api_key_when_the_request_declares_itself_anthropic() {
        // The spec's own header is signal enough on its own.
        let (h, p) = source(&[("anthropic-version", "2023-06-01"), ("x-api-key", "sk-ant")]);
        assert_eq!(extract_credential((&h, &p)).as_deref(), Some("sk-ant"));
    }

    #[test]
    fn reads_an_x_api_key_from_a_claude_user_agent() {
        let (h, p) = source(&[("user-agent", "claude-cli/1.2.3"), ("x-api-key", "sk-ant")]);
        assert_eq!(extract_credential((&h, &p)).as_deref(), Some("sk-ant"));
    }

    #[test]
    fn reads_an_x_api_key_from_a_claude_code_user_agent() {
        // The build name is `claude-code` where the CLI name is `claude-cli`;
        // both are in the reference gateway's regex and only one is a bare
        // substring of the other.
        let (h, p) = source(&[("user-agent", "claude-code/2.0.1 (external, cli)"), ("x-api-key", "sk-ant")]);
        assert_eq!(extract_credential((&h, &p)).as_deref(), Some("sk-ant"));
    }

    #[test]
    fn an_anthropic_user_agent_alone_is_enough() {
        // The `anthropic` arm, which is what a client naming itself only as
        // "anthropic-sdk/x" sends.
        let (h, p) = source(&[("user-agent", "anthropic-sdk/0.20"), ("x-api-key", "sk-ant")]);
        assert_eq!(extract_credential((&h, &p)).as_deref(), Some("sk-ant"));
    }

    #[test]
    fn ignores_an_x_api_key_from_a_client_that_is_not_anthropic() {
        // The load-bearing row of the matrix: a local-mode client sends a
        // placeholder here, and reading it unconditionally would turn that
        // placeholder into an authentication *failure*.
        let (h, p) = source(&[("user-agent", "curl/8.0"), ("x-api-key", "not-a-real-key")]);
        assert_eq!(extract_credential((&h, &p)), None);
    }

    #[test]
    fn reads_a_google_api_key_unconditionally() {
        // gemini-cli and every @google/genai client send it here and nowhere
        // else, so there is no version header to gate on.
        let (h, p) = source(&[("x-goog-api-key", "AIza")]);
        assert_eq!(extract_credential((&h, &p)).as_deref(), Some("AIza"));
    }

    #[test]
    fn reads_a_token_from_a_tokenized_alias_path() {
        // The client that cannot attach a header at all.
        let h = HeaderMap::new();
        assert_eq!(
            extract_credential((&h, "/vscode/sk-alias/chat/completions")).as_deref(),
            Some("sk-alias")
        );
        assert_eq!(
            extract_credential((&h, "/api/v1/vscode/sk-alias/responses")).as_deref(),
            Some("sk-alias")
        );
    }

    #[test]
    fn a_non_bearer_authorization_does_not_short_circuit_the_path_token() {
        // The reference gateway's case: a client's own non-OmniRoute scheme.
        // Rejecting on the first header would refuse a request that carries a
        // perfectly good credential in the path.
        let (h, p) = (
            headers(&[("authorization", "Token some-other-vendors-token")]),
            "/vscode/sk-alias/chat/completions".to_owned(),
        );
        assert_eq!(extract_credential((&h, &p)).as_deref(), Some("sk-alias"));
    }

    #[test]
    fn a_bearer_from_another_vendor_is_still_a_bearer() {
        // The other half of that rule: `Bearer <anything>` IS a bearer, and
        // preferring the path token over it would let a URL override a header the
        // client set on purpose.
        let (h, p) = (
            headers(&[("authorization", "Bearer some-other-vendors-token")]),
            "/vscode/sk-alias/chat/completions".to_owned(),
        );
        assert_eq!(extract_credential((&h, &p)).as_deref(), Some("some-other-vendors-token"));
    }

    #[test]
    fn an_empty_credential_reads_as_absent() {
        let (h, p) = source(&[("authorization", "Bearer   ")]);
        assert_eq!(extract_credential((&h, &p)), None);
    }

    #[test]
    fn the_bearer_scheme_is_matched_case_insensitively() {
        // `bearer` is what curl and half the SDKs send.
        let (h, p) = source(&[("authorization", "bearer abc123")]);
        assert_eq!(extract_credential((&h, &p)).as_deref(), Some("abc123"));
    }

    // --- the three modes -----------------------------------------------

    /// A bearer token this gate's master did not sign.
    fn foreign_credential() -> (HeaderMap, String) {
        let theirs = AuthGate::new(b"ffffffffffffffffffffffffffffffff").expect("master key");
        let token = theirs.issue_for_tests("key-1").expect("token mints").access;
        source(&[("authorization", &format!("Bearer {token}"))])
    }

    fn admit(gate: &AuthGate, source: (&HeaderMap, &str), mode: AuthMode) -> Result<(), String> {
        gate.authorize(CredentialSource::from(source), mode)
    }

    #[test]
    fn an_absent_credential_is_a_401_under_required() {
        let err = admit(&gate(), (&HeaderMap::new(), "/v1/messages"), AuthMode::Required)
            .expect_err("required means a credential is required");
        assert!(err.contains("access token"), "unhelpful error: {err}");
    }

    #[test]
    fn an_absent_credential_is_anonymous_under_degrade() {
        // Spelled out rather than folded into the matrix tests: "no credential"
        // is the row where `Open` and `Degrade` agree with each other and differ
        // from `Required`, and a client that never sends a key must not be
        // refused by either.
        let (h, p) = source(&[]);
        assert!(admit(&gate(), (&h, &p), AuthMode::DegradeInvalidToAnon).is_ok());
        assert!(admit(&gate(), (&h, &p), AuthMode::Open).is_ok());
    }

    #[test]
    fn a_refused_credential_is_anonymous_under_degrade() {
        // The stale-CLI-config case: an old key must not turn every request into
        // a 401, and the degradation is warned about rather than silent.
        let (h, p) = foreign_credential();
        assert!(
            admit(&gate(), (&h, &p), AuthMode::DegradeInvalidToAnon).is_ok(),
            "a stale key turned every request into a 401"
        );
    }

    #[test]
    fn a_refused_credential_is_a_401_under_required() {
        let (h, p) = foreign_credential();
        assert!(admit(&gate(), (&h, &p), AuthMode::Required).is_err());
    }

    #[test]
    fn a_refused_credential_is_anonymous_under_open() {
        // `open` means no check, so even a wrong credential is served: the mode
        // exists for a deployment that wants the gate *present* without the
        // request path enforcing it.
        let (h, p) = foreign_credential();
        assert!(admit(&gate(), (&h, &p), AuthMode::Open).is_ok());
    }

    #[test]
    fn a_valid_credential_is_admitted_under_every_mode() {
        let g = gate();
        let token = g.issue_for_tests("key-1").expect("token mints").access;
        let (h, p) = source(&[("authorization", &format!("Bearer {token}"))]);
        for mode in [AuthMode::Open, AuthMode::Required, AuthMode::DegradeInvalidToAnon] {
            assert!(admit(&g, (&h, &p), mode).is_ok(), "{mode:?} refused a valid token");
        }
    }

    #[test]
    fn a_mode_only_ever_changes_the_answer_once_a_gate_exists() {
        // The property the default relies on: with no gate, `authorize` is never
        // called, so `Required` is inert and the server is open.
        let no_gate: Option<&AuthGate> = None;
        assert!(no_gate.is_none());
        assert_eq!(AuthMode::default(), AuthMode::Required);
    }

    // --- authorize ------------------------------------------------------

    #[test]
    fn a_refusal_never_echoes_the_token() {
        // This response is a cacheable 401 a browser may keep; a credential in it
        // would be a credential on disk.
        let (h, p) = foreign_credential();
        let token = h
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .expect("the header was set")
            .to_owned();
        let reason = admit(&gate(), (&h, &p), AuthMode::Required)
            .expect_err("a foreign token must be refused");
        assert!(!reason.contains(&token), "the token leaked into the reason: {reason}");
    }

    // --- header parsing, since the case-insensitivity is load-bearing ----

    #[test]
    fn header_names_are_matched_case_insensitively_by_http() {
        let h = headers(&[("X-Api-Key", "sk-x"), ("Anthropic-Version", "2023-06-01")]);
        assert_eq!(extract_credential((&h, "/v1/messages")).as_deref(), Some("sk-x"));
    }

    #[test]
    fn a_credential_in_a_path_that_is_not_a_tokenized_alias_is_not_read() {
        let h = HeaderMap::new();
        assert_eq!(extract_credential((&h, "/v1/models")), None);
    }
}
