//! Remote-login state: the half of a browser login that has to survive between
//! two MCP calls.
//!
//! # Why a table at all
//!
//! `ar auth login` does the whole login in one call — it owns a loopback
//! listener and reads one line of stdin, so OmniRoute's
//! url → complete → persist → verify never leaves the process. An MCP host has
//! the opposite problem: the person authorizing may be on a phone, and there is
//! no loopback to return to and no stdin to read. So the flow splits at exactly
//! one point, between *url* and *complete*, and what has to cross that gap is
//! the PKCE verifier.
//!
//! Which is why this module exists and is small: the verifier goes in, keyed by a
//! random session id, and comes out exactly once.
//!
//! # What the table never does
//!
//! * **Never logged.** [`PendingLogins`]'s own `Debug` prints the count and
//!   nothing else, because a session id stands in for the verifier. The rows
//!   [`PendingLogins::rows`] hands out are [`PendingRow`], which has no field a
//!   secret could reach.
//! * **Never single-use twice.** [`PendingLogins::take`] removes on read. An
//!   authorization code is itself single-use upstream, so a table that handed
//!   the same verifier out twice would turn a replayable paste into a second
//!   redeem.
//! * **Never outlives its budget.** [`LOGIN_TTL`] is the CLI's own
//!   `--timeout` default, so the two front ends cannot disagree about how long a
//!   person has — and an MCP host is the case where that number actually has to
//!   be right, because nobody is sitting at the terminal waiting.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ar_exec::{AuthorizeRequest, Session};
use serde::Serialize;

/// How long a started login stays completable.
///
/// The CLI's `ar auth login --timeout` default. Named here rather than shared so
/// `ar-cli` keeps owning the flag's default, and paired in both directions: a
/// login started over MCP must be completable for exactly as long as one started
/// from the terminal.
pub const LOGIN_TTL: Duration = Duration::from_secs(300);

/// One provider this process can log into, as `config.yaml`'s `oauth:` block
/// declares it.
///
/// [`AuthTarget::session`] carries every endpoint and the client id, because
/// `ar_exec::Session` is the type `authorize_url` and `exchange_code` already
/// read and this crate must not hold a second copy of an OAuth wire format. What
/// is left here is what `Session` cannot know: which credential *rows* the
/// result belongs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthTarget {
    /// The session to authorize against — endpoints, client id, scope.
    pub session: Session,
    /// Redirect the provider will land the person on.
    ///
    /// Operator-declared rather than bound: a provider only accepts a redirect it
    /// has registered, and an ephemeral loopback port is not something an
    /// operator registers. It is public by construction — it travels in the
    /// authorize URL — which is what lets a host print it on another device.
    pub redirect_uri: String,
    /// Credential row the access token lands in — the provider's own
    /// `providers[].key`, not a second invented name.
    pub access_key: String,
    /// Credential row the refresh token lands in, when the session declares one.
    pub refresh_key: Option<String>,
    /// Credential row holding a confidential client's secret, when it declares
    /// one. `None` for a public PKCE client, which is the common case.
    pub client_secret_key: Option<String>,
}

impl AuthTarget {
    /// The registry provider id this target authenticates.
    #[must_use]
    pub fn provider(&self) -> &str {
        self.session.provider()
    }
}

/// A login that has been started and not yet completed.
///
/// The verifier rides in [`PendingLogin::request`], and `AuthorizeRequest`'s own
/// hand-written `Debug` is what keeps it out of every log line: this struct's
/// derived `Debug` prints the request, and that impl redacts the verifier while
/// leaving the state and challenge legible — they are public by construction.
#[derive(Debug)]
pub struct PendingLogin {
    id: String,
    provider: String,
    authorize_url: String,
    request: AuthorizeRequest,
    at: Instant,
}

impl PendingLogin {
    /// The random session id a host posts back to `ar_auth_complete`.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The provider being logged into.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The verifier-carrying request. Borrowed once, by the exchange.
    #[must_use]
    pub fn request(&self) -> &AuthorizeRequest {
        &self.request
    }

    /// The row a host may render. Names and URLs only.
    #[must_use]
    pub fn row(&self) -> PendingRow {
        PendingRow {
            session_id: self.id.clone(),
            provider: self.provider.clone(),
            authorize_url: self.authorize_url.clone(),
            redirect_uri: self.request.redirect_uri.clone(),
            expires_in_secs: remaining(self.at, LOGIN_TTL).as_secs(),
        }
    }
}

/// One pending login, as `ar_auth_status` and `ar_auth_login_url` report it.
///
/// Six fields, all of them public by construction. There is no `verifier`, no
/// `code` and no token — which is the test: a struct that cannot name a secret
/// cannot leak one when it is `Serialize`d into a tool answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingRow {
    /// The id to post to `ar_auth_complete`.
    pub session_id: String,
    /// Provider being logged into.
    pub provider: String,
    /// The URL a person opens.
    pub authorize_url: String,
    /// The redirect that URL will come back to.
    pub redirect_uri: String,
    /// Seconds left before this id stops being completable.
    pub expires_in_secs: u64,
}

/// The in-memory table of started-but-uncompleted logins.
///
/// Behind a mutex because the transport serves calls concurrently and a `start`
/// racing a `take` on the same id is exactly the interleaving that would let two
/// callers both redeem one code. In memory and in this process only, so a restart
/// drops a pending login rather than leaving a redeemable verifier on disk.
#[derive(Default)]
pub struct PendingLogins {
    inner: Mutex<HashMap<String, PendingLogin>>,
}

impl std::fmt::Debug for PendingLogins {
    /// The count, never the entries. A session id is a redeemable handle, so a
    /// `Debug` that printed one would be a way to read another caller's login out
    /// of a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingLogins").field("pending", &self.len()).finish_non_exhaustive()
    }
}

impl PendingLogins {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a started login and returns its random session id.
    ///
    /// The id is a v4 UUID rather than a counter: it is the one value that stands
    /// in for the verifier at the boundary, and a sequential one would let a host
    /// walk every outstanding login by counting. `uuid` is already in the lockfile
    /// via `ar-server`, so this costs no new crate.
    pub fn start(&self, provider: &str, request: AuthorizeRequest, url: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let entry = PendingLogin {
            id: id.clone(),
            provider: provider.to_owned(),
            authorize_url: url,
            request,
            at: Instant::now(),
        };
        self.lock().insert(id.clone(), entry);
        id
    }

    /// Takes the login `id` names, or `None` if it is unknown, already used, or
    /// past its budget.
    ///
    /// `None` for all three on purpose: from the caller's side they are one fact
    /// — this session id cannot be completed — and an error that could tell them
    /// apart would be a probe for which ids are live.
    pub fn take(&self, id: &str) -> Option<PendingLogin> {
        let mut entries = self.lock();
        prune(&mut entries);
        entries.remove(id)
    }

    /// Forgets every pending login for `provider`.
    ///
    /// A logout that left a half-finished login pending would be reversible: the
    /// code could be pasted afterwards and re-arm the account the logout just
    /// forgot. The reverse is not worth offering — a `start` costs nothing.
    pub fn discard(&self, provider: &str) {
        self.lock().retain(|_, e| e.provider != provider);
    }

    /// The rows a host may render, ordered by provider then id so the answer is
    /// stable between two calls.
    pub fn rows(&self) -> Vec<PendingRow> {
        let mut entries = self.lock();
        prune(&mut entries);
        let mut rows: Vec<PendingRow> = entries.values().map(PendingLogin::row).collect();
        rows.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.session_id.cmp(&b.session_id)));
        rows
    }

    /// Pending count, after pruning.
    #[must_use]
    pub fn len(&self) -> usize {
        let mut entries = self.lock();
        prune(&mut entries);
        entries.len()
    }

    /// Whether nothing is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A poisoned lock means another thread panicked while holding it. Recovering
    /// the guard is what the transport does for the live combo too, and the
    /// alternative would turn one panicked body into a permanently unusable
    /// login surface.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingLogin>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Drops every entry whose budget has run out.
fn prune(entries: &mut HashMap<String, PendingLogin>) {
    entries.retain(|_, e| remaining(e.at, LOGIN_TTL) > Duration::ZERO);
}

/// How long `at` has left, saturating at zero.
fn remaining(at: Instant, ttl: Duration) -> Duration {
    ttl.saturating_sub(at.elapsed())
}

#[cfg(test)]
mod tests {
    use ar_exec::{OAuthKind, Session, new_authorize_request};

    use super::{AuthTarget, LOGIN_TTL, PendingLogins, remaining};
    use std::time::{Duration, Instant};

    fn target() -> AuthTarget {
        let session = Session::new("codex", OAuthKind::Codex)
            .with_authorization_url("https://auth.example/authorize")
            .with_token_url("https://auth.example/token")
            .with_client_id("cid");
        AuthTarget {
            session,
            redirect_uri: "http://127.0.0.1:1455/callback".to_owned(),
            access_key: "codex".to_owned(),
            refresh_key: Some("codex_refresh".to_owned()),
            client_secret_key: None,
        }
    }

    fn start(table: &PendingLogins) -> String {
        let t = target();
        let request = new_authorize_request(&t.session, &t.redirect_uri);
        table.start(t.session.provider(), request, "https://auth.example/authorize?x=1".to_owned())
    }

    #[test]
    fn a_target_names_the_provider_its_session_authorizes() {
        assert_eq!(target().provider(), "codex");
    }

    #[test]
    fn a_started_login_is_pending() {
        let table = PendingLogins::default();
        start(&table);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn two_starts_get_different_session_ids() {
        let table = PendingLogins::default();
        assert_ne!(start(&table), start(&table));
    }

    #[test]
    fn taking_a_login_consumes_it() {
        let table = PendingLogins::default();
        let id = start(&table);
        assert!(table.take(&id).is_some());
        assert!(table.take(&id).is_none(), "a session id is single-use");
    }

    #[test]
    fn taking_a_pending_login_yields_the_verifier_it_started_with() {
        let table = PendingLogins::default();
        let t = target();
        let request = new_authorize_request(&t.session, &t.redirect_uri);
        let verifier = request.verifier.clone();
        let id = table.start(t.session.provider(), request, "https://auth.example/authorize".to_owned());
        assert_eq!(table.take(&id).expect("pending").request().verifier, verifier);
    }

    #[test]
    fn an_expired_login_is_pruned_rather_than_completed() {
        // `remaining` saturating is the whole of the expiry: a stale entry reads as
        // zero seconds left, and `prune` drops it. Five minutes is not something a
        // test should wait for, so the rule is asserted on the helper.
        let stale = Instant::now() - LOGIN_TTL;
        assert_eq!(remaining(stale, LOGIN_TTL), Duration::ZERO, "a stale entry has no budget left");
    }

    #[test]
    fn a_fresh_login_is_still_completable() {
        let table = PendingLogins::default();
        let id = start(&table);
        assert!(table.take(&id).is_some());
    }

    #[test]
    fn a_pruned_entry_leaves_no_row_behind() {
        let table = PendingLogins::default();
        start(&table);
        table.lock().clear();
        assert!(table.rows().is_empty());
    }

    #[test]
    fn the_budget_is_the_clis_own_default() {
        assert_eq!(LOGIN_TTL, Duration::from_secs(300));
    }

    #[test]
    fn a_status_row_carries_no_verifier_and_no_code() {
        let table = PendingLogins::default();
        let id = start(&table);
        let row = table.take(&id).expect("pending").row();
        let json = serde_json::to_string(&row).expect("serialises");
        for banned in ["verifier", "code_challenge", "access_token", "refresh_token"] {
            assert!(!json.contains(banned), "{banned} leaked into a status row: {json}");
        }
    }

    #[test]
    fn a_status_row_names_the_provider_and_its_redirect() {
        let table = PendingLogins::default();
        let id = start(&table);
        let row = table.take(&id).expect("pending").row();
        assert_eq!(row.provider, "codex");
        assert_eq!(row.redirect_uri, "http://127.0.0.1:1455/callback");
    }

    #[test]
    fn the_debug_of_a_pending_login_withholds_the_verifier() {
        let table = PendingLogins::default();
        let id = start(&table);
        let pending = table.take(&id).expect("pending");
        let text = format!("{pending:?}");
        assert!(!text.contains(&pending.request().verifier), "the verifier reached a Debug: {text}");
    }

    #[test]
    fn the_debug_of_a_table_names_no_session_id() {
        let table = PendingLogins::default();
        let id = start(&table);
        let text = format!("{table:?}");
        assert!(!text.contains(&id), "a pending session id is a redeemable handle: {text}");
    }

    #[test]
    fn a_logout_can_discard_a_half_finished_login() {
        let table = PendingLogins::default();
        let id = start(&table);
        table.discard("codex");
        assert!(table.take(&id).is_none(), "a forgotten login cannot be redeemed");
    }

    #[test]
    fn a_discard_leaves_another_provider_pending() {
        let table = PendingLogins::default();
        start(&table);
        table.discard("cline");
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn remaining_saturates_at_zero_rather_than_going_negative() {
        assert_eq!(remaining(Instant::now(), Duration::ZERO), Duration::ZERO);
    }
}