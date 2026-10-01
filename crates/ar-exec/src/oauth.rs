//! OAuth dispatch: token injection, single-flight refresh, terminal quarantine.
//!
//! Closes AUDIT-REPORT F-CRIT-1 (`ar` catalogues `oauth` but cannot execute it,
//! so the codex/cline/grok-cli sessions are unusable) and the half of F-HIGH-4
//! that only bites once a token can expire (per-connection mutex + rotation
//! cache), and owns the single terminal-status list F-MED-2 asks for.
//!
//! # What is per-provider, and what is not
//!
//! The token *lifecycle* is the same everywhere: a bearer goes in, a 401 or a
//! clock says it is stale, a refresh exchanges the refresh token, a new bearer
//! comes out. So the executor is written once. What genuinely differs per
//! provider is which status/reason pairs are terminal, and that is the
//! per-provider part: [`OAuthKind::carve_out`].
//!
//! What is deliberately *absent* is any hardcoded refresh endpoint. AGENTS.md
//! forbids inventing provider wire formats, and an auth endpoint guessed from a
//! provider id would be exactly that invention. The endpoint comes from the
//! operator ([`Session::with_token_url`]) or refresh does not happen and the
//! executor says so out loud.
//!
//! # Why `Origin` is a parameter
//!
//! Audit red-team R4: OmniRoute's probe-origin dispatches skip proactive
//! refresh, because a health check that renews a rotating token spends one
//! refresh-token use to learn something the check did not ask for. There is no
//! probe dispatch in `ar` today (`/healthz` is a static body), so [`Origin`] has
//! one live value — and that is the point: the axis lives at the only place that
//! can refresh, so a future probe path has to name [`Origin::Probe`] to get it.
//! A probe grant reads the cached token and never writes the rotation pool.
//!
//! # Where token material comes from
//!
//! [`OAuthToken`] takes an already-decrypted [`Secret`]. Nothing in this crate
//! opens a database, derives a key, or parses a config file: that is
//! `ar_keys::CredentialStore`'s job (F-CRIT-2) and `ar-server`'s, and this file
//! is downstream of both. `ar-server`'s config reader is the single place that
//! pulls a token out of the store and hands it over.

use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ar_config::Secret;
use bytes::Bytes;
use reqwest::StatusCode;
use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::{ArExec, ChatStream, Dispatch, ExecError};

/// Seconds of headroom before expiry at which a token counts as already stale.
///
/// A token that expires *during* the upstream round trip would 401 a request
/// this proxy could have avoided. Thirty seconds is about one busy upstream's
/// `Retry-After`, and it is a constant rather than a knob because a refresh
/// costs a round trip whether it is needed or not.
const EXPIRY_SKEW_SECS: u64 = 30;

/// Ceiling on how much of a refresh-failure body is scanned for a reason.
///
/// The reason strings live in [`TERMINAL_REFRESH_STATUS`] and are short, so 2 KiB
/// carries a JSON error object. Bounded because the body is attacker-adjacent
/// and a provider error page can be megabytes of HTML.
const REFRESH_BODY_SCAN: usize = 2 * 1024;

/// Why a dispatch came from the proxy rather than from a client.
///
/// The axis R4 needs. [`Origin::Probe`] may read a cached token; it may never
/// refresh or rotate one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A client's request. May refresh.
    Client,
    /// A health or reachability check. Never refreshes, never rotates.
    Probe,
}

/// A provider whose OAuth flow this build knows how to drive.
///
/// One variant per provider, and this enum *is* the registry of which. A provider
/// that is not here is not "an API-key provider" — it is a provider this build
/// refuses to authenticate, and [`OAuthKind::parse`] returning `None` is what
/// makes `ar doctor` say so instead of listing it as known (F-CRIT-1).
///
/// # Why `Cursor` is a variant
///
/// It is not in the four-provider minimum. It is here because F-MED-2's carve-out
/// list names Cursor's `expired` as *retryable*, and a carve-out with no variant
/// to hang off is a comment rather than code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OAuthKind {
    /// OpenAI's coding-agent account, over the Responses wire.
    Codex,
    /// Cline's account.
    Cline,
    /// Anthropic's Claude account.
    Claude,
    /// Google's Gemini CLI account.
    GeminiCli,
    /// Cursor's account.
    Cursor,
}

impl OAuthKind {
    /// Every kind, so a caller can enumerate coverage without a second list.
    pub const ALL: [Self; 5] = [
        Self::Codex,
        Self::Cline,
        Self::Claude,
        Self::GeminiCli,
        Self::Cursor,
    ];

    /// The registry provider id this kind drives.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Cline => "cline",
            Self::Claude => "claude",
            Self::GeminiCli => "gemini-cli",
            Self::Cursor => "cursor",
        }
    }

    /// The kind for a registry provider id, or `None` when this build has no
    /// executor for it.
    ///
    /// `None` is the answer for most of the 21 `oauth` entries in the compiled-in
    /// catalog, and for `kilocode` in particular. Red-team R1 records two live
    /// `kilocode` sessions with no stored refresh token that are nonetheless
    /// active, so its mechanism is unknown; guessing here would port a bug.
    /// `None` keeps it failing loudly.
    #[must_use]
    pub fn parse(provider: &str) -> Option<Self> {
        // Over `ALL`, not a `match` on strings, so a new variant with no parse
        // arm fails to compile rather than quietly answering `None`.
        Self::ALL.into_iter().find(|k| k.as_str() == provider)
    }

    /// Provider-specific overrides of [`TERMINAL_REFRESH_STATUS`], consulted
    /// *before* it.
    ///
    /// Both carve-outs F-MED-2 names point the same way — from "this looks
    /// terminal" toward "retry" — which is the safe direction: the cost of a
    /// wrong terminal verdict is a dead account, the cost of a wrong transient
    /// verdict is one wasted round trip.
    #[must_use]
    pub fn carve_out(self, reason: &str) -> Option<RefreshFault> {
        match (self, reason) {
            // Cursor's `expired` means "this token is old", not "this account is
            // dead": the refresh path still works, so retiring here would kill a
            // connection one refresh would have fixed.
            (Self::Cursor, "expired" | "token_expired") => {
                Some(RefreshFault::Transient("cursor-expired-is-retryable"))
            }
            // Claude's refresh tokens survive a *transient* `invalid_grant` — an
            // IdP hiccup answers invalid_grant for a token that is still good.
            // Reading it as terminal retires a working session on a bad
            // afternoon, which is the failure mode F-MED-2 warns about.
            (Self::Claude, "invalid_grant") => {
                Some(RefreshFault::Transient("claude-invalid-grant-survives"))
            }
            _ => None,
        }
    }
}

/// Why a refresh failed, and whether the session is finished.
///
/// # The direction that matters
///
/// [`Transient`] is **excluded** from the terminal set, deliberately. Terminal
/// means "stop using this account": it is durable, it is what the store's CHECK
/// constraint records, and it is what `ar doctor` reports. Reading a transient as
/// terminal bricks a working account — F-HIGH-4's warning, restated. So the
/// classifier's fallthrough is [`Transient`], never [`Self::Unrecoverable`].
///
/// ponytail: an unrecognised auth failure therefore never becomes terminal, so a
/// revoked token missing from [`TERMINAL_REFRESH_STATUS`] retries with growing
/// backoff instead of retiring. Bounded by the router's three-attempt cap and the
/// per-key cooldown, and the fix is a row in the list — not a flipped default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshFault {
    /// The session survives. Retry later; never record it as terminal.
    Transient(&'static str),
    /// The session is finished. A human has to re-authorise it.
    Unrecoverable {
        /// Status the refresh endpoint returned.
        status: u16,
        /// Which [`TERMINAL_REFRESH_STATUS`] row matched.
        reason: &'static str,
    },
}

impl RefreshFault {
    /// Whether this fault retires the session.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Unrecoverable { .. })
    }

    /// The status this fault is reported under.
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::Transient(_) => 503,
            Self::Unrecoverable { status, .. } => status,
        }
    }

    /// The stable reason string, for a log line and for the client's body.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Transient(reason) | Self::Unrecoverable { reason, .. } => reason,
        }
    }
    /// The executor error this fault becomes.
    ///
    /// A transient becomes a transport error on purpose: it is the router's
    /// failover-and-backoff cue and says nothing about the client, so it must not
    /// be dressed as a verdict on the request.
    #[must_use]
    pub fn into_exec(self, provider: &str) -> ExecError {
        match self {
            Self::Transient(reason) => {
                tracing::warn!(provider, reason, "oauth refresh failed transiently");
                ExecError::Transport(format!(
                    "oauth refresh for {provider} did not succeed: {reason}"
                ))
            }
            Self::Unrecoverable { status, reason } => ExecError::OAuthTerminal(TerminalReport {
                provider: provider.to_owned(),
                refresh_status: status,
                reason,
            }),
        }
    }
}

impl std::fmt::Display for RefreshFault {
    /// The operator sentence. Same wording as [`TerminalReport`]'s for the
    /// terminal case, so a log line and a client body cannot describe one event
    /// two ways.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(reason) => write!(f, "transient ({reason}); the session is still usable"),
            Self::Unrecoverable { status, reason } => {
                write!(f, "terminal (refresh returned {status}: {reason})")
            }
        }
    }
}

/// The one terminal-status list (F-MED-2).
///
/// OmniRoute's terminal `test_status` sets diverge across four sites with no
/// database constraint, so the broadest and the narrowest disagree and the
/// disagreement is invisible. This list is the single source: the classifier
/// reads it, `ar doctor` reports against it, and [`terminal_check_constraint`]
/// generates the store's CHECK clause from it, so a fourth copy cannot drift from
/// the first three.
///
/// Rows are `(status, reason)` because the pair is what a provider actually
/// sends: a bare 401 does not mean "revoked", it also means "expired" and "wrong
/// scope".
pub const TERMINAL_REFRESH_STATUS: &[(u16, &str)] = &[
    (400, "invalid_grant"),
    (400, "unauthorized_client"),
    (400, "no_refresh_token"),
    (401, "invalid_token"),
    (401, "token_revoked"),
    (401, "token_expired"),
    (403, "account_disabled"),
    (403, "permission_denied"),
    (410, "token_revoked"),
];

/// The `CHECK` clause a credential store's terminal column must carry.
///
/// Generated from [`TERMINAL_REFRESH_STATUS`] rather than hand-written, so the
/// database cannot hold a status the classifier would call retryable — the exact
/// class of bug F-MED-2 describes. A function and not a `const` because a
/// `const fn` cannot build a `String`, and the list is small enough that building
/// it per call is free next to the sqlite open that consumes it.
#[must_use]
pub fn terminal_check_constraint() -> String {
    let mut sql = String::from("CHECK (terminal_status IS NULL OR (");
    for (i, (status, reason)) in TERMINAL_REFRESH_STATUS.iter().enumerate() {
        if i > 0 {
            sql.push_str(" OR ");
        }
        // Single quotes doubled: a reason is a literal in SQL, and a value
        // containing an apostrophe must not be able to close the quote.
        sql.push_str(&format!(
            "(terminal_status = {status} AND terminal_reason = '{}')",
            reason.replace('\'', "''")
        ));
    }
    sql.push_str("))");
    sql
}

/// Classifies one refresh failure.
///
/// The order is the design: carve-out first (it can move a row *either* way),
/// then the transient rules, then the terminal list, then a transient
/// fallthrough. Read as: the only way to become terminal is to be named.
///
/// Pure — no clock, no state, no I/O — so the table is testable as a table.
#[must_use]
pub fn classify_refresh(kind: OAuthKind, status: u16, body: &str) -> RefreshFault {
    let reason = reason_in(body);

    if let Some(fault) = kind.carve_out(reason) {
        return fault;
    }
    if is_transient_status(status) {
        return RefreshFault::Transient("refresh-endpoint-unavailable");
    }
    if TERMINAL_REFRESH_STATUS
        .iter()
        .any(|(s, r)| *s == status && *r == reason)
    {
        return RefreshFault::Unrecoverable { status, reason: static_reason(reason) };
    }
    RefreshFault::Transient("unrecognised-auth-failure")
}

/// The `&'static` half of [`TERMINAL_REFRESH_STATUS`] for a reason that matched.
///
/// `reason_in` borrows from a response buffer; a [`RefreshFault`] is compared,
/// logged and stored, so it cannot hold that borrow.
fn static_reason(matched: &str) -> &'static str {
    TERMINAL_REFRESH_STATUS
        .iter()
        .find(|(_, r)| *r == matched)
        .map_or("unrecognised-auth-failure", |(_, r)| *r)
}

/// Statuses that are never terminal, whatever the body says.
///
/// 429 and the 5xx family are the upstream saying "not now"; 408 and 425 are the
/// same answer from a proxy in front of it. None of them is evidence about the
/// *account*, which is the only thing a terminal verdict asserts.
fn is_transient_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429) || (500..600).contains(&status)
}

/// The first terminal reason named in `body`, or `"unauthorized"`.
///
/// Scans the bounded prefix and lowercases, because providers spell one condition
/// `invalid_grant`, `Invalid Grant` and `INVALID_GRANT`. Falls back rather than
/// returning `None`: the classifier needs *a* reason to compare against, and
/// `"unauthorized"` is not in the terminal list, which lands on the transient
/// fallthrough — the safe direction.
fn reason_in(body: &str) -> &str {
    let lower = body.to_ascii_lowercase();
    let end = lower.len().min(REFRESH_BODY_SCAN);
    let head = &lower[..end];
    TERMINAL_REFRESH_STATUS
        .iter()
        .find(|(_, reason)| head.contains(*reason))
        .map_or("unauthorized", |(_, reason)| *reason)
}

/// SHA-256 of an access token, and the rotation cache's key.
///
/// Keyed by this rather than by the token so the map never holds key material: a
/// heap dump shows 32 opaque bytes per rotation and nothing replayable. SHA-256
/// because that is what OmniRoute's `enc:v1` derivation already hashes
/// (`AUDIT-REPORT` R3) — a different digest here would make the two rotation maps
/// incomparable when they are ported across.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// The raw digest, for a log line that must correlate two rotations.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Hashes a token. The only way to build a [`TokenHash`].
#[must_use]
pub fn token_hash(token: &str) -> TokenHash {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    TokenHash(hasher.finalize().into())
}

impl std::fmt::Debug for TokenHash {
    /// Names the type and prints nothing. A hex digest is a correlation handle
    /// for someone who already holds the token; this crate's habit is to make
    /// that a decision rather than a leak.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenHash(<32 bytes>)")
    }
}

/// One connection's current token material.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthToken {
    /// The bearer to send.
    access: Secret,
    /// The token that renews it. `None` means the session can be *used* but not
    /// renewed, so the first 401 is terminal.
    refresh: Option<Secret>,
    /// Unix seconds at which `access` expires. `None` disables proactive
    /// refresh; the 401 path still renews.
    expires_at: Option<u64>,
}

impl std::fmt::Debug for OAuthToken {
    /// Never the tokens. `Secret`'s own `Debug` redacts; this restates it so a
    /// later edit that swaps the field for a bare `String` fails the eye rather
    /// than the review.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthToken")
            .field("can_refresh", &self.refresh.is_some())
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl OAuthToken {
    /// A token with no refresh path and no expiry.
    #[must_use]
    pub fn new(access: Secret) -> Self {
        Self { access, refresh: None, expires_at: None }
    }

    /// Attaches the refresh token.
    #[must_use]
    pub fn with_refresh(mut self, refresh: Secret) -> Self {
        self.refresh = Some(refresh);
        self
    }

    /// Sets the expiry, in unix seconds.
    #[must_use]
    pub fn with_expiry(mut self, expires_at: u64) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// The bearer. Named loudly: every call site reads as a decision.
    #[must_use]
    pub fn access(&self) -> &Secret {
        &self.access
    }

    /// Whether this token carries a refresh token.
    #[must_use]
    pub fn can_refresh(&self) -> bool {
        self.refresh.is_some()
    }

    /// Whether the token is at or past its expiry, with [`EXPIRY_SKEW_SECS`] of
    /// headroom.
    #[must_use]
    pub fn is_expiring(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|at| at <= now.saturating_add(EXPIRY_SKEW_SECS))
    }
}

/// Rotations a concurrent burst produced, keyed by the retired token's hash.
///
/// This is what keeps a multi-account rotating setup off `refresh_token_reused`:
/// N in-flight requests holding the same access token all get 401 at once, and
/// without this map each would independently present the same refresh token —
/// which most providers treat as theft and answer by revoking the whole family.
/// With it, exactly one refresh happens and the rest claim their replacement.
#[derive(Debug, Default)]
pub struct RotationPool {
    entries: Mutex<HashMap<TokenHash, OAuthToken>>,
}

impl RotationPool {
    /// An empty pool.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `stale` was replaced by `fresh`.
    ///
    /// Bounded by construction: keyed by the token just retired, so one entry is
    /// written per rotation and read once. Nothing else inserts.
    fn record(&self, stale: TokenHash, fresh: OAuthToken) {
        let Ok(mut entries) = self.entries.lock() else {
            // A poisoned lock means another thread panicked holding it. Rotation
            // is an optimisation — the slow path is a real refresh — so losing it
            // costs one extra round trip, not a failed request.
            tracing::warn!("rotation pool lock poisoned; rotation retry unavailable");
            return;
        };
        entries.insert(stale, fresh);
    }

    /// Claims the replacement for `stale`, if one was recorded.
    ///
    /// Claiming rather than reading: a rotation goes to exactly one waiter,
    /// because the next waiter to miss it takes the refresh lock and refreshes
    /// again — with the *new* refresh token, which is correct and is not the
    /// reuse this map exists to prevent.
    fn take(&self, stale: TokenHash) -> Option<OAuthToken> {
        self.entries.lock().ok()?.remove(&stale)
    }

    /// Unclaimed rotations pending. For a test and for `/metrics`.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }
}

/// A session's non-token configuration: where to refresh, and as whom.
///
/// Deliberately holds no token. The token arrives separately and already
/// decrypted, so nothing in this type can be `Debug`-printed into a log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    provider: String,
    kind: OAuthKind,
    token_url: Option<String>,
    client_id: Option<String>,
    scope: Option<String>,
}

impl Session {
    /// A session with a token but no refresh endpoint.
    #[must_use]
    pub fn new(provider: impl Into<String>, kind: OAuthKind) -> Self {
        Self {
            provider: provider.into(),
            kind,
            token_url: None,
            client_id: None,
            scope: None,
        }
    }

    /// Sets the refresh endpoint. Without one the session cannot renew.
    #[must_use]
    pub fn with_token_url(mut self, url: impl Into<String>) -> Self {
        self.token_url = Some(url.into());
        self
    }

    /// Sets the OAuth client id sent with the refresh.
    #[must_use]
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    /// Sets the scope requested on refresh.
    #[must_use]
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// The registry provider id.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The provider family, and therefore the carve-out table.
    #[must_use]
    pub fn kind(&self) -> OAuthKind {
        self.kind
    }

    /// The refresh endpoint, when one is configured.
    #[must_use]
    pub fn token_url(&self) -> Option<&str> {
        self.token_url.as_deref()
    }

    /// The client id, when configured.
    #[must_use]
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }

    /// The scope, when configured.
    #[must_use]
    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// Whether this session can renew at all.
    ///
    /// Two conditions, both required: a refresh token *and* somewhere to send it.
    /// `ar doctor` names the missing half rather than letting the first 401
    /// discover it.
    #[must_use]
    pub fn can_refresh(&self, token: &OAuthToken) -> bool {
        token.can_refresh() && self.token_url.is_some()
    }
}

/// One connection's terminal state, and the client-facing body for it.
///
/// Exists so "the account is finished" has exactly one spelling in a log, in
/// `/metrics` and in the response (R2: a visible terminal, never a bare 502).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalReport {
    /// Provider whose session is finished.
    pub provider: String,
    /// Status the refresh endpoint returned.
    pub refresh_status: u16,
    /// Which [`TERMINAL_REFRESH_STATUS`] row matched.
    pub reason: &'static str,
}

impl std::fmt::Display for TerminalReport {
    /// The one sentence a log line, a metric label and `ar doctor` all use, so
    /// "the account is finished" has a single spelling (R2).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is terminal (refresh returned {}: {})",
            self.provider, self.refresh_status, self.reason
        )
    }
}

impl TerminalReport {
    /// The JSON body returned to the client.
    ///
    /// `oauth_terminal` as the error type so a client can branch on it without
    /// parsing prose, and the provider's *own* refresh status kept alongside — a
    /// 400 `invalid_grant` reported as a 400 would be read as "your request was
    /// malformed", which is the opposite of the truth.
    #[must_use]
    pub fn client_body(&self) -> Bytes {
        let body = serde_json::json!({
            "error": {
                "type": "oauth_terminal",
                "message": format!(
                    "the oauth session for {} is terminal ({} from the refresh endpoint); re-authorise the account",
                    self.provider, self.reason
                ),
                "provider": self.provider,
                "refresh_status": self.refresh_status,
                "reason": self.reason,
            }
        });
        Bytes::from(serde_json::to_vec(&body).unwrap_or_else(|_| {
            br#"{"error":{"type":"oauth_terminal"}}"#.to_vec()
        }))
    }

    /// The fault a retired session reports on every later call.
    fn into_fault(self) -> RefreshFault {
        RefreshFault::Unrecoverable {
            status: self.refresh_status,
            reason: self.reason,
        }
    }
}

/// Exchanges a refresh token for a new access token.
///
/// One production implementation ([`HttpRefresher`]) and one test double. A trait
/// rather than an injected closure because the future is boxed either way, and a
/// named trait is what the connection's field can point at.
pub trait Refresher: Send + Sync {
    /// Renews `current`, or explains why not.
    fn refresh<'a>(
        &'a self,
        session: &'a Session,
        current: &'a OAuthToken,
    ) -> Pin<Box<dyn Future<Output = Result<OAuthToken, RefreshFault>> + Send + 'a>>;
}

/// A connection that has no token and therefore cannot dispatch.
///
/// [`Session::connect`]'s result is [`Connected`] and only that type has a
/// `dispatch`, so "sends a bearer it never obtained" is a compile error (AGENTS.md
/// §2, ch.7) rather than a runtime branch a later edit can get wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unconnected;

/// A connection that can dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Connected;

/// One authenticated provider connection, in state `S`.
///
/// TypeState rather than a `connected: bool`, for the reason [`Unconnected`]
/// documents.
pub struct Connection<S> {
    session: Session,
    token: RwLock<OAuthToken>,
    /// Serialises refresh for *this* connection (F-HIGH-4). Tokio's async mutex
    /// because it is held across the refresh `await`; a `std` mutex there would
    /// block the runtime thread.
    refresh_lock: AsyncMutex<()>,
    terminal: RwLock<Option<TerminalReport>>,
    pool: Arc<RotationPool>,
    refresher: Arc<dyn Refresher>,
    _state: PhantomData<fn() -> S>,
}

impl<S> Connection<S> {
    /// The registry provider id.
    #[must_use]
    pub fn provider(&self) -> &str {
        self.session.provider()
    }

    /// The provider family.
    #[must_use]
    pub fn kind(&self) -> OAuthKind {
        self.session.kind()
    }

    /// The session's non-token configuration.
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The terminal report, once the session has been retired.
    pub async fn terminal(&self) -> Option<TerminalReport> {
        self.terminal.read().await.clone()
    }

    /// Retires the session, keeping the first report.
    ///
    /// First report wins because the first verdict is the informative one: a
    /// later `permission_denied` after an `invalid_grant` is the *consequence* of
    /// the retirement, and overwriting would report the wrong cause.
    pub async fn quarantine(&self, report: TerminalReport) {
        let mut slot = self.terminal.write().await;
        if slot.is_none() {
            tracing::error!(
                provider = %report.provider,
                reason = report.reason,
                refresh_status = report.refresh_status,
                "oauth session retired; re-authorisation required"
            );
            *slot = Some(report);
        }
    }

    /// Writes a terminal fault to the slot and returns the error to report.
    async fn record(&self, fault: RefreshFault) -> ExecError {
        let err = fault.into_exec(self.session.provider());
        if let Some(report) = err.terminal_report() {
            self.quarantine(report).await;
        }
        err
    }
}

impl<S> std::fmt::Debug for Connection<S> {
    /// Names the provider and its kind. Never the token, and never the terminal
    /// reason — that reaches the log through [`Self::quarantine`]'s own span,
    /// once, at the moment it changed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("provider", &self.session.provider)
            .field("kind", &self.session.kind)
            .finish_non_exhaustive()
    }
}

impl Connection<Unconnected> {
    /// A connection with no token. It cannot dispatch, which is the point.
    #[must_use]
    pub fn pending(
        session: Session,
        pool: Arc<RotationPool>,
        refresher: Arc<dyn Refresher>,
    ) -> Self {
        Self {
            session,
            token: RwLock::new(OAuthToken::new(Secret::new(""))),
            refresh_lock: AsyncMutex::new(()),
            terminal: RwLock::new(None),
            pool,
            refresher,
            _state: PhantomData,
        }
    }

    /// Puts a token in and moves the connection to [`Connected`].
    ///
    /// Synchronous on purpose: it only builds a `RwLock` and moves five fields,
    /// so an `async` here would buy nothing and cost an await on config load.
    /// Everything that *waits* — the refresh `await` inside [`Self::rotate`] —
    /// is on the request path, not the connect path.
    ///
    /// # Errors
    ///
    /// An empty access token is refused as [`RefreshFault::Unrecoverable`] with
    /// `empty_access_token`: a session with no bearer would otherwise dispatch
    /// unauthenticated and be reported as the provider's own 401.
    pub fn connect(self, token: OAuthToken) -> Result<Connection<Connected>, RefreshFault> {
        if token.access().expose().trim().is_empty() {
            return Err(RefreshFault::Unrecoverable { status: 401, reason: "empty_access_token" });
        }
        Ok(Connection {
            session: self.session,
            token: RwLock::new(token),
            refresh_lock: self.refresh_lock,
            terminal: self.terminal,
            pool: self.pool,
            refresher: self.refresher,
            _state: PhantomData,
        })
    }
}

/// One token ready to be attached to a dispatch, plus the hash that identifies it
/// to the rotation pool.
#[derive(Clone, PartialEq, Eq)]
pub struct Grant {
    token: Secret,
    hash: TokenHash,
}

impl std::fmt::Debug for Grant {
    /// The hash is safe to print; the token is not.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant").field("hash", &self.hash).finish_non_exhaustive()
    }
}

impl Grant {
    /// The bearer. Named loudly, as everywhere else a secret is handed out.
    #[must_use]
    pub fn token(&self) -> &Secret {
        &self.token
    }

    /// The hash keying this grant in the rotation pool.
    ///
    /// The caller keeps it because the 401 path must say *which* token was
    /// refused; that hash is the whole input to a rotation lookup.
    #[must_use]
    pub fn hash(&self) -> TokenHash {
        self.hash
    }

    fn of(token: OAuthToken) -> Self {
        let hash = token_hash(token.access.expose());
        Self { token: token.access, hash }
    }
}

impl Connection<Connected> {
    /// The token to send for a request made at `now` (unix seconds).
    ///
    /// `now` is a parameter so the expiry branch is testable without a clock, and
    /// so a caller with a better time source can supply it.
    ///
    /// # Errors
    ///
    /// The session's terminal report if it has been retired, otherwise whatever
    /// the refresh attempt produced.
    ///
    /// # Why `Origin::Probe` short-circuits (R4)
    ///
    /// A probe returns the cached token verbatim: no proactive refresh, no write
    /// to the rotation pool, no retirement. A health check that renews a
    /// rotating token spends a refresh-token use to learn nothing.
    pub async fn grant_at(&self, origin: Origin, now: u64) -> Result<Grant, RefreshFault> {
        if let Some(report) = self.terminal().await {
            return Err(report.into_fault());
        }
        let current = self.token.read().await.clone();
        if origin == Origin::Probe || !current.is_expiring(now) {
            return Ok(Grant::of(current));
        }
        self.rotate(token_hash(current.access.expose())).await
    }

    /// The token to send, judged against the wall clock.
    ///
    /// # Errors
    ///
    /// As [`Self::grant_at`].
    pub async fn grant(&self, origin: Origin) -> Result<Grant, RefreshFault> {
        self.grant_at(origin, unix_now()).await
    }

    /// Renews whatever `stale` refers to, or claims the renewal a concurrent
    /// request already performed.
    ///
    /// This is both the proactive path and the 401 path, and the single-flight
    /// proof lives here:
    ///
    /// 1. claim a recorded rotation for `stale` — no lock, no await;
    /// 2. take the per-connection lock and re-check — this is where N concurrent
    ///    callers collapse onto one refresh;
    /// 3. refresh, record the rotation, install the new token.
    ///
    /// # Errors
    ///
    /// Whatever [`Refresher::refresh`] produced. A terminal fault is written to
    /// the session's terminal slot, so the *next* call fails without a network
    /// round trip at all.
    pub async fn rotate(&self, stale: TokenHash) -> Result<Grant, RefreshFault> {
        if let Some(fresh) = self.pool.take(stale) {
            *self.token.write().await = fresh.clone();
            return Ok(Grant::of(fresh));
        }

        // The lock is the connection's, not the pool's: two *connections* sharing
        // a refresh token is the reuse case the pool prevents, and serialising
        // across connections would serialise unrelated providers.
        let _single_flight = self.refresh_lock.lock().await;
        if let Some(fresh) = self.pool.take(stale) {
            *self.token.write().await = fresh.clone();
            return Ok(Grant::of(fresh));
        }

        // The guard is read across the refresh `await`, which is what the async
        // lock buys and why this clone is free of contention concerns: refresh is
        // already serialised per connection by `refresh_lock` above.
        let current = self.token.read().await.clone();
        let outcome = self.refresher.refresh(&self.session, &current).await;
        match outcome {
            Ok(renewed) => {
                self.pool.record(stale, renewed.clone());
                *self.token.write().await = renewed.clone();
                Ok(Grant::of(renewed))
            }
            Err(fault) => {
                // Quarantine here rather than leaving it to the caller: `rotate`
                // is the public 401 entry point, and a caller that forgets to
                // record the fault would re-attempt a dead account forever.
                if let RefreshFault::Unrecoverable { status, reason } = fault {
                    self.quarantine(TerminalReport {
                        provider: self.session.provider().to_owned(),
                        refresh_status: status,
                        reason,
                    })
                    .await;
                }
                Err(fault)
            }
        }
    }

    /// POSTs one canonical request, renewing at most once.
    ///
    /// `shape` supplies everything about the dispatch except the bearer: base
    /// URL, wire format, model spelling, streaming, provider headers. Its
    /// `api_key` is **never read** — a session that dispatched the config's
    /// static key instead of its own token would be the exact silent failure
    /// F-CRIT-1 is about.
    ///
    /// One retry, never a loop. A second 401 on a token minted seconds ago is
    /// not a stale-token problem, and retrying again would spend a second
    /// refresh-token use per request against a provider that has already said no.
    ///
    /// # Errors
    ///
    /// [`ExecError::OAuthTerminal`] when the session is retired, which
    /// `ar-server` turns into a 401 carrying [`TerminalReport::client_body`] so
    /// the client sees *which* account died rather than a bare 502 (R2).
    /// Otherwise `ar-exec`'s ordinary transport, wire and timeout errors.
    pub async fn dispatch(
        &self,
        core: &ArExec,
        shape: &Dispatch<'_>,
        canonical: &[u8],
        abort: &CancellationToken,
    ) -> Result<ChatStream, ExecError> {
        let first = match self.grant(Origin::Client).await {
            Ok(grant) => grant,
            Err(fault) => return Err(self.record(fault).await),
        };

        let stream = core.post(&shape.with_api_key(first.token.expose()), canonical, abort).await?;
        if stream.status() != StatusCode::UNAUTHORIZED {
            return Ok(stream);
        }

        tracing::warn!(provider = %self.provider(), "upstream refused the oauth token; rotating once");
        let second = match self.rotate(first.hash()).await {
            Ok(grant) => grant,
            Err(fault) => return Err(self.record(fault).await),
        };

        let retry = core.post(&shape.with_api_key(second.token.expose()), canonical, abort).await?;
        if retry.status() != StatusCode::UNAUTHORIZED {
            return Ok(retry);
        }

        // A token minted seconds ago is already refused. Classify from the body
        // so the report names the provider's own reason, then retire the session
        // so the next request fails here instead of burning another token.
        let (status, body, _) = retry.into_failure().await;
        let text = String::from_utf8_lossy(&body[..body.len().min(REFRESH_BODY_SCAN)]).into_owned();
        Err(self.record(classify_refresh(self.kind(), status.as_u16(), &text)).await)
    }
}

impl ExecError {
    /// The terminal report this error carries, or `None` for any other variant.
    ///
    /// `ar-server` needs this to turn a retired session into a 401 with a body
    /// that names the provider (R2); the alternative is a `match` on a variant
    /// this crate owns from two places.
    #[must_use]
    pub fn terminal_report(&self) -> Option<TerminalReport> {
        match self {
            Self::OAuthTerminal(report) => Some(report.clone()),
            _ => None,
        }
    }
}

/// The production [`Refresher`]: an RFC 6749 §6 form POST.
///
/// Standards-based rather than per-provider, because §6 is the wire every one of
/// these providers publishes and a provider-specific variant would be an
/// invented format (AGENTS.md). `client_id` and `scope` are optional in the
/// standard and are sent only when the operator configured them.
#[derive(Debug, Clone)]
pub struct HttpRefresher {
    client: reqwest::Client,
    timeout: Duration,
}

impl HttpRefresher {
    /// Builds a refresher over `client`. Cannot fail, so nothing is `Result`:
    /// a missing refresh endpoint is a per-session fact
    /// ([`RefreshFault::Unrecoverable`]), not a construction failure.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self { client, timeout: Duration::from_secs(30) }
    }

    /// Overrides the refresh timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The `reqwest` client this refresher shares with dispatch.
    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

impl Refresher for HttpRefresher {
    fn refresh<'a>(
        &'a self,
        session: &'a Session,
        current: &'a OAuthToken,
    ) -> Pin<Box<dyn Future<Output = Result<OAuthToken, RefreshFault>> + Send + 'a>> {
        Box::pin(async move {
            let Some(url) = session.token_url() else {
                return Err(RefreshFault::Unrecoverable { status: 400, reason: "no_refresh_token" });
            };
            let Some(refresh) = current.refresh.as_ref() else {
                // Nothing to present. Terminal by construction — there is no
                // second source for a refresh token.
                return Err(RefreshFault::Unrecoverable { status: 400, reason: "no_refresh_token" });
            };

            let mut form: Vec<(&str, &str)> = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.expose()),
            ];
            if let Some(client_id) = session.client_id() {
                form.push(("client_id", client_id));
            }
            if let Some(scope) = session.scope() {
                form.push(("scope", scope));
            }

            let response = match self.client.post(url).form(&form).timeout(self.timeout).send().await {
                Ok(r) => r,
                // A transport failure is the definition of transient: no verdict
                // was produced, so nothing about the account was learned.
                Err(e) => return Err(RefreshFault::Transient(transport_reason(&e))),
            };

            let status = response.status().as_u16();
            let success = response.status().is_success();
            let raw = response.bytes().await.unwrap_or_default();
            let body = String::from_utf8_lossy(&raw[..raw.len().min(REFRESH_BODY_SCAN)]).into_owned();
            if !success {
                return Err(classify_refresh(session.kind(), status, &body));
            }

            let parsed: serde_json::Value = serde_json::from_str(&body)
                .map_err(|_| RefreshFault::Transient("unreadable-refresh-response"))?;
            let Some(access) = parsed
                .get("access_token")
                .and_then(serde_json::Value::as_str)
                .map(Secret::new)
            else {
                return Err(RefreshFault::Transient("refresh-response-has-no-access-token"));
            };

            // §5.1: `refresh_token` is optional on a refresh response, and its
            // absence means "keep using the one you have". Providers that rotate
            // send it; a provider that does not must not lose its session here.
            let refreshed = parsed
                .get("refresh_token")
                .and_then(serde_json::Value::as_str)
                .map(Secret::new)
                .or_else(|| current.refresh.clone());

            let mut token = OAuthToken::new(access).with_expiry(expiry_at(&parsed, unix_now()));
            if let Some(refresh) = refreshed {
                token = token.with_refresh(refresh);
            }
            Ok(token)
        })
    }
}

/// Unix expiry from a §5.1 `expires_in`, relative to `now`.
///
/// Absent means no expiry, which reads as "never proactively refresh" — the 401
/// path still renews, so a provider that omits `expires_in` costs one round trip
/// rather than a session.
fn expiry_at(parsed: &serde_json::Value, now: u64) -> u64 {
    parsed
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .map_or(now, |secs| now.saturating_add(secs))
}

/// `reqwest`'s own reason, narrowed to the two the taxonomy distinguishes.
///
/// Its message is unbounded, and this crate does not echo provider text into
/// error strings, so a static reason is what the taxonomy gets.
fn transport_reason(e: &reqwest::Error) -> &'static str {    if e.is_timeout() { "refresh-transport-timeout" } else { "refresh-transport-failure" }
}

/// Unix seconds, as [`Connection::grant_at`] judges them.
///
/// Exposed so a caller can reason about expiry without holding a connection — a
/// health check, say — on the same clock the grant path uses, so the two cannot
/// disagree about whether a token is stale.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// A connection is shared across request tasks and holds two locks; the rotation
// pool is shared across connections. Prove it at compile time rather than
// discovering it from a spawn error on the first concurrent request (ch.9).
const _: () = {
    const fn assert_send<T: Send>() {}
    const fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    assert_send::<Connection<Connected>>();
    assert_send::<Connection<Unconnected>>();
    assert_send_sync::<RotationPool>();
    assert_send_sync::<dyn Refresher>();
};

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ar_config::Secret;
    use ar_registry::WireFormat;
    use axum::http::StatusCode;
    use tokio_util::sync::CancellationToken;

    use super::{
        Connected, Dispatch, EXPIRY_SKEW_SECS, ExecError, OAuthKind, OAuthToken, Origin, RefreshFault,
        Refresher, RotationPool, Session, TERMINAL_REFRESH_STATUS, TerminalReport, Unconnected,
        classify_refresh, terminal_check_constraint, token_hash, unix_now,
    };
    use crate::oauth::Connection;

    /// A refresher that counts calls and hands back a scripted token, so the
    /// single-flight test measures *one* refresh rather than one socket.
    struct Counting {
        calls: AtomicUsize,
        prefix: &'static str,
        fault: Option<RefreshFault>,
    }

    impl Counting {
        fn ok(prefix: &'static str) -> Self {
            Self { calls: AtomicUsize::new(0), prefix, fault: None }
        }

        fn failing(fault: RefreshFault) -> Self {
            Self { calls: AtomicUsize::new(0), prefix: "x", fault: Some(fault) }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Refresher for Counting {
        fn refresh<'a>(
            &'a self,
            _session: &'a Session,
            _current: &'a OAuthToken,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<OAuthToken, RefreshFault>> + Send + 'a>>
        {
            // Counted before the token is handed out, so a concurrent burst would
            // provably reach this line more than once if the lock were not held.
            let nth = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if let Some(fault) = self.fault {
                    return Err(fault);
                }
                Ok(OAuthToken::new(Secret::new(&format!("{}-{nth}", self.prefix)))
                    .with_refresh(Secret::new("rotated-refresh"))
                    .with_expiry(unix_now() + 3600))
            })
        }
    }

    fn session() -> Session {
        Session::new("cline", OAuthKind::Cline).with_token_url("https://auth.test/token")
    }

    /// Synthetic tokens only. Nothing in this file is a real credential.
    fn expired_token() -> OAuthToken {
        OAuthToken::new(Secret::new("synthetic-old-access"))
            .with_refresh(Secret::new("synthetic-old-refresh"))
            .with_expiry(unix_now().saturating_sub(1))
    }

    fn connect_with(refresher: Arc<dyn Refresher>, token: OAuthToken) -> Connection<Connected> {
        Connection::pending(session(), Arc::new(RotationPool::new()), refresher)
            .connect(token)
            .expect("an access token connects")
    }

    #[test]
    fn parses_the_four_minimum_providers() {
        for id in ["codex", "cline", "claude", "gemini-cli"] {
            assert!(OAuthKind::parse(id).is_some(), "{id}");
        }
    }

    #[test]
    fn refuses_a_provider_this_build_has_no_executor_for() {
        // R1: kilocode authenticates by a mechanism the audit could not explain.
        // `None` keeps it failing loudly instead of porting the mystery.
        assert_eq!(OAuthKind::parse("kilocode"), None);
    }

    #[test]
    fn parses_every_kind_through_its_own_spelling() {
        for kind in OAuthKind::ALL {
            assert_eq!(OAuthKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn treats_a_named_terminal_row_as_terminal() {
        assert!(classify_refresh(OAuthKind::Cline, 400, r#"{"error":"invalid_grant"}"#).is_terminal());
    }

    #[test]
    fn excludes_a_503_from_the_terminal_set() {
        assert!(!classify_refresh(OAuthKind::Cline, 503, "upstream busy").is_terminal());
    }

    #[test]
    fn excludes_a_429_from_the_terminal_set() {
        assert!(!classify_refresh(OAuthKind::Codex, 429, "slow down").is_terminal());
    }

    #[test]
    fn treats_an_unnamed_auth_failure_as_transient() {
        // The safe default. Naming a new reason is a row in the list; flipping
        // this to terminal would retire accounts on a guess.
        assert!(!classify_refresh(OAuthKind::Cline, 401, "something new").is_terminal());
    }

    #[test]
    fn treats_cursor_expired_as_retryable() {
        let fault = classify_refresh(OAuthKind::Cursor, 401, r#"{"error":"token_expired"}"#);
        assert_eq!(fault, RefreshFault::Transient("cursor-expired-is-retryable"));
    }

    #[test]
    fn treats_cursor_bare_expired_as_retryable() {
        assert!(!classify_refresh(OAuthKind::Cursor, 401, r#"{"error":"expired"}"#).is_terminal());
    }

    #[test]
    fn treats_claude_invalid_grant_as_transient() {
        let fault = classify_refresh(OAuthKind::Claude, 400, r#"{"error":"invalid_grant"}"#);
        assert_eq!(fault, RefreshFault::Transient("claude-invalid-grant-survives"));
    }

    #[test]
    fn keeps_invalid_grant_terminal_when_no_carve_out_applies() {
        assert!(classify_refresh(OAuthKind::Codex, 400, r#"{"error":"invalid_grant"}"#).is_terminal());
    }

    #[test]
    fn keeps_cursor_token_revoked_terminal() {
        // The carve-out is narrow: it rescues `expired`, not a revocation.
        assert!(classify_refresh(OAuthKind::Cursor, 401, r#"{"error":"token_revoked"}"#).is_terminal());
    }

    #[test]
    fn generates_a_check_constraint_covering_every_terminal_row() {
        let sql = terminal_check_constraint();
        for (status, reason) in TERMINAL_REFRESH_STATUS {
            assert!(
                sql.contains(&format!("terminal_status = {status} AND terminal_reason = '{reason}'")),
                "{sql}"
            );
        }
    }

    #[test]
    fn check_constraint_is_null_tolerant() {
        assert!(terminal_check_constraint().starts_with("CHECK (terminal_status IS NULL"));
    }

    #[test]
    fn keys_the_rotation_cache_by_a_hash_not_the_token() {
        assert_eq!(token_hash("abc"), token_hash("abc"));
        assert_ne!(token_hash("abc"), token_hash("abd"));
    }

    #[test]
    fn prints_no_token_material_from_a_hash_debug() {
        assert!(!format!("{:?}", token_hash("sk-synthetic-value")).contains("sk-synthetic-value"));
    }

    #[test]
    fn prints_no_token_material_from_a_token_debug() {
        assert!(!format!("{:?}", expired_token()).contains("synthetic-old-access"));
    }

    #[test]
    fn refuses_to_connect_when_the_access_token_is_empty() {
        let pending = Connection::<Unconnected>::pending(
            session(),
            Arc::new(RotationPool::new()),
            Arc::new(Counting::ok("n")),
        );
        assert_eq!(
            pending.connect(OAuthToken::new(Secret::new("  "))).err(),
            Some(RefreshFault::Unrecoverable { status: 401, reason: "empty_access_token" })
        );
    }

    #[tokio::test]
    async fn does_not_refresh_a_connection_whose_token_is_fresh() {
        // `checks_a_stale_token_against_the_injected_clock` uses one fixture with
        // two clocks; this asserts the wall-clock path took the same no-refresh
        // branch for a token nowhere near expiry.
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = connect_with(
            refresher.clone() as Arc<dyn Refresher>,
            OAuthToken::new(Secret::new("synthetic-live-access")).with_expiry(unix_now() + 3600),
        );
        assert!(conn.grant(Origin::Client).await.is_ok());
        assert_eq!(refresher.calls(), 0, "a fresh token needs no refresh");
    }

    #[tokio::test]
    async fn refreshes_once_when_n_concurrent_grants_find_the_token_expired() {
        // F-HIGH-4: without the per-connection mutex, every one of these would
        // present the same refresh token and trip `refresh_token_reused`.
        //
        // No barrier inside the refresher. The mutex means exactly *one* task
        // reaches it, so a barrier sized to the burst would deadlock — the proof
        // arrived at sideways. The concurrent tasks plus the call count are the
        // assertion.
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Arc::new(
            Connection::pending(session(), Arc::new(RotationPool::new()), refresher.clone())
                .connect(expired_token())
                .expect("an access token connects"),
        );
        let now = unix_now();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let conn = Arc::clone(&conn);
            tasks.push(tokio::spawn(async move { conn.grant_at(Origin::Client, now).await }));
        }
        for task in tasks {
            assert!(task.await.expect("no panic").is_ok(), "every caller gets a usable token");
        }
        assert_eq!(refresher.calls(), 1, "eight concurrent grants, one refresh");
    }

    #[tokio::test]
    async fn hands_a_rotated_token_to_the_next_401_waiter() {
        // The rotation map's job: the second 401 arrives after the first refresh
        // already rotated, and must claim that token instead of refreshing again.
        let pool = Arc::new(RotationPool::new());
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Connection::pending(session(), Arc::clone(&pool), refresher.clone() as Arc<dyn Refresher>)
            .connect(expired_token())
            .expect("an access token connects");
        let stale = token_hash("synthetic-old-access");

        let first = conn.rotate(stale).await.expect("first rotation");
        assert_eq!(first.token().expose(), "fresh-0");
        assert_eq!(refresher.calls(), 1);

        let second = conn.rotate(stale).await.expect("the recorded rotation is claimed");
        assert_eq!(second.token().expose(), "fresh-0", "the same renewal, not a second one");
        assert_eq!(refresher.calls(), 1, "no refresh_token_reuse");
    }

    #[tokio::test]
    async fn installs_the_rotated_token_so_the_next_grant_needs_no_refresh() {
        // The second half of the rotation contract: claiming a rotation also
        // moves the connection's own token forward, so a *later* dispatch is
        // served without a round trip rather than re-sending a stale bearer.
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Connection::pending(session(), Arc::new(RotationPool::new()), refresher.clone() as Arc<dyn Refresher>)
            .connect(expired_token())
            .expect("an access token connects");

        let rotated = conn.rotate(token_hash("synthetic-old-access")).await.expect("rotation");
        let later = conn.grant(Origin::Client).await.expect("the installed token");
        assert_eq!(later.token().expose(), rotated.token().expose());
        assert_eq!(refresher.calls(), 1, "one refresh served both");
    }

    #[tokio::test]
    async fn does_not_refresh_for_a_probe_origin() {
        // R4: a health check must not spend a rotating token.
        let pool = Arc::new(RotationPool::new());
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Connection::pending(session(), Arc::clone(&pool), refresher.clone() as Arc<dyn Refresher>)
            .connect(expired_token())
            .expect("an access token connects");
        let grant = conn.grant_at(Origin::Probe, unix_now()).await.expect("a probe grant");
        assert_eq!(grant.token().expose(), "synthetic-old-access", "the cached token, expired or not");
        assert_eq!(refresher.calls(), 0, "a probe never refreshes");
    }

    #[tokio::test]
    async fn writes_no_rotation_for_a_probe_origin() {
        let pool = Arc::new(RotationPool::new());
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Connection::pending(session(), Arc::clone(&pool), refresher.clone() as Arc<dyn Refresher>)
            .connect(expired_token())
            .expect("an access token connects");
        let _ = conn.grant_at(Origin::Probe, unix_now()).await;
        assert_eq!(pool.pending(), 0, "a probe leaves the rotation cache alone");
    }

    #[tokio::test]
    async fn does_not_quarantine_a_session_whose_refresh_failed_transiently() {
        // The point of the transient classification: a refresh endpoint having a
        // bad moment must not retire an account that still works.
        let refresher = Arc::new(Counting::failing(RefreshFault::Transient("refresh-endpoint-unavailable")));
        let conn = Connection::pending(session(), Arc::new(RotationPool::new()), refresher as Arc<dyn Refresher>)
            .connect(expired_token())
            .expect("an access token connects");
        let err = conn.rotate(token_hash("synthetic-old-access")).await.err();
        assert_eq!(err, Some(RefreshFault::Transient("refresh-endpoint-unavailable")));
        assert_eq!(conn.terminal().await, None, "no retirement on a transient");
    }

    #[tokio::test]
    async fn rotates_once_when_the_upstream_refuses_the_token() {
        // The 401 path, end to end against a mock upstream: 401, one refresh, one
        // retry that succeeds. The single `Counting` call is the whole contract
        // — a loop here would spend a second refresh-token use per request.
        //
        // Spawned locally rather than reusing `tests/mock_upstream.rs`, because
        // that harness replies from a fixed script and this one has to *branch*
        // on the bearer it was handed.
        let app = axum::Router::new().fallback(|req: axum::http::Request<_>| async move {
            let fresh = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                == Some("Bearer synthetic-access-0");
            if fresh {
                (StatusCode::OK, "data: {}\n\n")
            } else {
                (StatusCode::UNAUTHORIZED, r#"{"error":"invalid_token"}"#)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("loopback binds");
        let addr = listener.local_addr().expect("bound socket has an address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let refresher = Arc::new(Counting::ok("synthetic-access"));
        let conn = Connection::pending(
            Session::new("cline", OAuthKind::Cline).with_token_url("https://auth.test/token"),
            Arc::new(RotationPool::new()),
            refresher.clone() as Arc<dyn Refresher>,
        )
        .connect(OAuthToken::new(Secret::new("synthetic-stale")).with_refresh(Secret::new("r")))
        .expect("an access token connects");
        let core = crate::ArExec::new().expect("client");

        let shape = Dispatch {
            base_url: &format!("http://{addr}"),
            wire_format: WireFormat::Openai,
            api_key: "",
            upstream_model: "m",
            stream: false,
            headers: &BTreeMap::new(),
        };
        let stream = conn
            .dispatch(&core, &shape, br#"{"model":"m"}"#, &CancellationToken::new())
            .await
            .expect("the retry succeeds");
        server.abort();

        assert_eq!(stream.status(), StatusCode::OK);
        assert_eq!(refresher.calls(), 1, "one rotation, not a loop");
    }

    #[tokio::test]
    async fn reports_a_terminal_session_when_the_rotated_token_is_also_refused() {
        // R2's shape: a token minted seconds ago is already refused, so the
        // session is retired and the failure is the *typed* terminal error
        // `ar-server` turns into a visible 401 — never a bare 502.
        let app = axum::Router::new().fallback(|| async {
            (StatusCode::UNAUTHORIZED, r#"{"error":"invalid_token"}"#)
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("loopback binds");
        let addr = listener.local_addr().expect("bound socket has an address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let refresher = Arc::new(Counting::ok("synthetic-access"));
        let conn = Connection::pending(
            Session::new("kimi-coding", OAuthKind::Cline).with_token_url("https://auth.test/token"),
            Arc::new(RotationPool::new()),
            refresher.clone() as Arc<dyn Refresher>,
        )
        .connect(OAuthToken::new(Secret::new("synthetic-access-0")).with_refresh(Secret::new("r")))
        .expect("an access token connects");
        let core = crate::ArExec::new().expect("client");

        let shape = Dispatch {
            base_url: &format!("http://{addr}"),
            wire_format: WireFormat::Openai,
            api_key: "",
            upstream_model: "m",
            stream: false,
            headers: &BTreeMap::new(),
        };
        let err = conn
            .dispatch(&core, &shape, br#"{"model":"m"}"#, &CancellationToken::new())
            .await
            .err();
        server.abort();

        // `invalid_token` is a named terminal row and no carve-out applies, so
        // the two 401s collapse into one durable retirement.
        let Some(ExecError::OAuthTerminal(report)) = err else {
            panic!("expected a typed oauth terminal, got {err:?}");
        };
        assert_eq!((report.provider.as_str(), report.reason), ("kimi-coding", "invalid_token"));
        assert_eq!(refresher.calls(), 1, "one rotation, then stop");
    }

    #[tokio::test]
    async fn quarantines_a_session_when_the_refresh_fault_is_terminal() {
        let refresher = Arc::new(Counting::failing(RefreshFault::Unrecoverable {
            status: 400,
            reason: "invalid_grant",
        }));
        let conn = Connection::pending(session(), Arc::new(RotationPool::new()), refresher as Arc<dyn Refresher>)
            .connect(expired_token())
            .expect("an access token connects");
        let err = conn.grant(Origin::Client).await.err();
        assert!(err.expect("a fault").is_terminal());
        assert_eq!(
            conn.terminal().await.map(|r| r.reason),
            Some("invalid_grant"),
            "the terminal state is durable, not per-call"
        );
    }

    #[tokio::test]
    async fn leaves_a_session_alive_when_the_refresh_fault_is_transient() {
        let refresher = Arc::new(Counting::failing(RefreshFault::Transient("flaky")));
        let conn = Connection::pending(session(), Arc::new(RotationPool::new()), refresher as Arc<dyn Refresher>)
            .connect(expired_token())
            .expect("an access token connects");
        assert!(conn.grant(Origin::Client).await.is_err());
        assert_eq!(conn.terminal().await, None, "a transient must never retire an account");
    }

    #[tokio::test]
    async fn answers_a_retired_session_without_touching_the_network() {
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Connection::pending(session(), Arc::new(RotationPool::new()), refresher.clone() as Arc<dyn Refresher>)
            .connect(expired_token())
            .expect("an access token connects");
        conn.quarantine(TerminalReport {
            provider: "cline".to_owned(),
            refresh_status: 400,
            reason: "invalid_grant",
        })
        .await;
        assert!(conn.grant(Origin::Client).await.is_err());
        assert_eq!(refresher.calls(), 0, "a retired session costs no round trip");
    }

    #[tokio::test]
    async fn keeps_the_first_terminal_report_when_a_second_arrives() {
        // The first verdict is the cause; a later one is its consequence.
        let conn = connect_with(
            Arc::new(Counting::ok("fresh")),
            OAuthToken::new(Secret::new("synthetic-live-access")),
        );
        conn.quarantine(TerminalReport {
            provider: "cline".to_owned(),
            refresh_status: 400,
            reason: "invalid_grant",
        })
        .await;
        conn.quarantine(TerminalReport {
            provider: "cline".to_owned(),
            refresh_status: 403,
            reason: "permission_denied",
        })
        .await;
        assert_eq!(conn.terminal().await.map(|r| r.reason), Some("invalid_grant"));
    }

    #[test]
    fn names_the_provider_in_the_terminal_client_body() {
        let body = TerminalReport {
            provider: "kimi-coding".to_owned(),
            refresh_status: 400,
            reason: "invalid_grant",
        }
        .client_body();
        let text = String::from_utf8(body.to_vec()).expect("utf-8 json");
        assert!(text.contains("oauth_terminal"), "{text}");
        assert!(text.contains("kimi-coding"), "{text}");
    }

    #[test]
    fn reports_an_expiry_inside_the_skew_as_stale() {
        let soon = OAuthToken::new(Secret::new("a")).with_expiry(1_000 + EXPIRY_SKEW_SECS - 1);
        assert!(soon.is_expiring(1_000));
    }

    #[test]
    fn reports_an_expiry_beyond_the_skew_as_fresh() {
        let later = OAuthToken::new(Secret::new("a")).with_expiry(1_000 + EXPIRY_SKEW_SECS + 1);
        assert!(!later.is_expiring(1_000));
    }

    #[test]
    fn renders_a_terminal_fault_as_the_operator_sentence() {
        assert_eq!(
            RefreshFault::Unrecoverable { status: 400, reason: "invalid_grant" }.to_string(),
            "terminal (refresh returned 400: invalid_grant)"
        );
    }

    #[test]
    fn renders_a_transient_fault_as_surviving() {
        // The literal, not an interpolation: a test written from the
        // implementation's own format string cannot catch the wording drifting
        // from what an operator actually reads.
        assert_eq!(
            RefreshFault::Transient("flaky").to_string(),
            "transient (flaky); the session is still usable"
        );
    }

    #[test]
    fn never_expires_a_token_that_declares_no_expiry() {
        assert!(!OAuthToken::new(Secret::new("a")).is_expiring(u64::MAX));
    }

    #[tokio::test]
    async fn checks_a_stale_token_against_the_injected_clock() {
        // `grant_at`'s `now` parameter exists so the expiry branch is testable
        // without a clock. One session, two clocks, two verdicts — which is the
        // only way to prove the parameter is consulted rather than ignored.
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = connect_with(
            refresher.clone() as Arc<dyn Refresher>,
            OAuthToken::new(Secret::new("synthetic-live-access"))
                .with_refresh(Secret::new("synthetic-refresh"))
                .with_expiry(1_000_000),
        );
        // One second *inside* the window, then the boundary itself:
        // `is_expiring` is `<=`, so `expires_at == now + skew` is already stale.
        let inside = 1_000_000 - EXPIRY_SKEW_SECS - 1;
        assert!(conn.grant_at(Origin::Client, inside).await.is_ok());
        assert_eq!(refresher.calls(), 0, "still inside the window");
        assert!(conn.grant_at(Origin::Client, 1_000_000).await.is_ok());
        assert_eq!(refresher.calls(), 1, "at the expiry it refreshed once");
    }
}
