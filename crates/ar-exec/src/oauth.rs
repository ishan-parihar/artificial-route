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
//!
//! # Browser login: the redirect-catch half
//!
//! An access token has to come from somewhere, and the browser is how a human
//! gets one. The flow splits in two, and only the catch half lives here:
//!
//! * **Path A — same machine.** [`CallbackListener`] binds `127.0.0.1:0` once,
//!   hands its [`CallbackListener::redirect_uri`] to the authorize-URL builder as
//!   `redirect_uri`, and waits for the browser to be redirected back at it.
//! * **Path B — any other device.** [`parse_callback_url`] takes the same
//!   redirected URL as a pasted string, for a phone completing consent on a
//!   machine that has no listener to return to.
//!
//! Both converge on the same two questions about a §4.1.2 redirect — *was this
//! mine?* (`state`) and *what did the provider say?* (`code` or `error`) — so
//! they share one parser and one [`LoginError`] taxonomy rather than each
//! inventing a verdict.
//!
//! # Browser login: the authorize + exchange half
//!
//! The other two steps of the same flow live here too, because a code is worth
//! nothing until something can be done with it:
//!
//! * [`new_authorize_request`] mints a PKCE `S256` pair and a CSRF `state`.
//! * [`authorize_url`] builds the §4.1.1 URL.
//! * [`exchange_code`] posts the §4.1.3 code for tokens.
//!
//! Three properties this half holds to:
//!
//! * **PKCE `S256`, mandatory.** [`authorize_url`] emits
//!   `code_challenge_method=S256` with no knob for `plain`. `plain` protects
//!   nothing against a leaked authorization request, and a switch toward a
//!   weaker flow no provider needs is not a knob worth having.
//! * **No endpoint is ever inferred.** `authorization_url` and `token_url` are
//!   operator-supplied ([`Session::with_authorization_url`],
//!   [`Session::with_token_url`]); an absent one is [`LoginError`], never a
//!   guess — the same rule that keeps refresh from inventing a wire format.
//! * **Failures share the taxonomy.** An exchange failure is classified by
//!   [`classify_refresh`], so a 400 `invalid_grant` from the *login* is the same
//!   [`RefreshFault`] the refresh path produces and a provider's carve-out
//!   (Claude's transient `invalid_grant`) applies to a login too. One list, one
//!   meaning.
//!
//! # Device login: the RFC 8628 half
//!
//! Some providers have no browser redirect at all — Kilo Code publishes
//! `oauth.initiateUrl` / `oauth.pollUrlBase` and no authorization or refresh
//! endpoint, because it authenticates by RFC 8628 device flow: the client asks
//! for a short user code, a human types that code into a provider page on
//! another device, and the client polls until the approval lands.
//!
//! `initiate_device` is the §3.1 request and `poll_device` the §3.4 poll.
//! Three properties this half holds to, same as the browser half above:
//!
//! * **No endpoint is ever inferred.** `device_auth_url` and `device_poll_url`
//!   are operator-supplied ([`Session::with_device_auth_url`],
//!   [`Session::with_device_poll_url`]); an absent one is [`LoginError`], never a
//!   guess. A device-flow URL guessed from a provider id would be the same
//!   invention §3.1 refuses to make.
//! * **No refresh-token shaping.** RFC 8628 §3.4 returns whatever the provider
//!   returns, and a provider with no refresh grant gets none — the token records
//!   that by having no refresh half, and `Session::can_refresh` reports it. There
//!   is no path here that invents one.
//! * **Failures share the taxonomy.** A poll failure is classified by
//!   `classify_refresh`, so a terminal verdict here is the same `RefreshFault` the
//!   refresh path produces. §3.5's `authorization_pending`
//!   and `slow_down` are handled *before* the classifier, because the RFC makes
//!   them "keep asking" rather than verdicts; its `access_denied` and
//!   `expired_token` are reported as [`LoginError::LoginDenied`] and
//!   [`LoginError::LoginExpired`].
//!
//! The device code is the flow's only secret, and it is the one thing here that
//! is never printed: it lives in a `DevicePending` behind a [`Secret`], so it
//! cannot reach a log through `Debug` even by accident.
//!
//! # Surviving a restart
//!
//! A refresh that renews a token has to write it down, or the next process loads
//! the refresh token this one just spent. Four mechanisms, each mirroring a
//! shape the reference carries:
//!
//! * [`HttpRefresher`] retries with jittered exponential backoff
//!   ([`REFRESH_MAX_ATTEMPTS`] attempts) and short-circuits on an
//!   unrecoverable verdict, because a second attempt spends a second
//!   refresh-token use to learn the same thing.
//! * [`RefreshBreaker`] stops a provider that has failed
//!   [`REFRESH_BREAKER_THRESHOLD`] refreshes in a row from being asked again
//!   for [`REFRESH_BREAKER_COOLDOWN_SECS`]. It reports a *transient* fault and
//!   never retires an account — that is [`classify_refresh`]'s list alone.
//! * [`Connection::rotate`] bounds how long it waits for its own refresh lock
//!   ([`REFRESH_LOCK_BOUND_SECS`]), so a hung upstream faults instead of parking
//!   every waiter on that connection until restart.
//! * [`Connection::rotate`] persists the renewal through a [`RotationSink`]
//!   inside the lock, guarded so a concurrent writer's fresher rotation is not
//!   reverted.
//!
//! Proactive *when* is per-provider: [`OAuthKind::refresh_lead_secs`] plus a
//! per-session [`Session::with_refresh_lead_secs`] override, both defaulting to
//! five minutes. The reason it is a table rather than one constant is the
//! refresh-token *family*, not the access token — see that method.
//!
//! The socket is unauthenticated for its entire life, so it is bound to the
//! loopback address and never to `0.0.0.0`, it serves exactly one request, and it
//! is dropped the moment that request is answered or the deadline passes. A
//! login listener is not a server: leaving one open would keep a socket, a port
//! and a code inside the `docs/00` RAM budget for as long as the tab stayed open.

use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ar_config::Secret;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use reqwest::{StatusCode, Url};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{ArExec, ChatStream, Dispatch, ExecError};

/// Proactive-refresh lead time when neither a session override nor a
/// per-kind row says otherwise: five minutes.
///
/// Five minutes is generous enough that a token cannot expire during the
/// upstream round trip it was sent on, and small enough that a provider which
/// revokes a whole refresh-token *family* on sibling use is not asked to
/// rotate more often than it has to. The reference's `TOKEN_EXPIRY_BUFFER_MS`
/// is the same number for the same reason.
const REFRESH_LEAD_SECS: u64 = 300;

/// Lead time for a provider whose refresh tokens are permanent, so refreshing
/// costs nothing but a round trip.
///
/// Google's OAuth refresh tokens do not rotate, so a longer lead is free there
/// and saves upstream chatter. Mirrors the `antigravity`/`agy` rows of the
/// reference's `REFRESH_LEAD_MS`; [`OAuthKind::refresh_lead_secs`] is where the
/// choice is made.
const NON_ROTATING_REFRESH_LEAD_SECS: u64 = 900;

/// Attempts one refresh makes before it reports a transient fault.
///
/// Three because the reference's `refreshWithRetry` and the grok-cli executor
/// both use three, and because two failures in a row on the same endpoint is
/// already an outage rather than a blip.
const REFRESH_MAX_ATTEMPTS: usize = 3;

/// Floor of the jittered exponential backoff between refresh attempts.
///
/// 200ms is the reference grok-cli executor's `GROK_BUILD_REFRESH_MIN_DELAY_MS`.
const REFRESH_MIN_DELAY_MS: u64 = 200;

/// Ceiling of the jittered exponential backoff between refresh attempts.
///
/// 2s is the reference's cap: long enough that a struggling provider is not
/// hammered, short enough that three attempts still finish inside a client's
/// patience.
const REFRESH_MAX_DELAY_MS: u64 = 2_000;

/// Consecutive refresh failures for one provider before refreshes stop
/// entirely.
///
/// The reference's `CIRCUIT_BREAKER_THRESHOLD`. Five, because a provider that
/// has failed five refreshes in a row is not going to succeed on the sixth, and
/// every attempt spends a refresh-token use.
const REFRESH_BREAKER_THRESHOLD: u32 = 5;

/// How long a tripped breaker stays open, in seconds.
///
/// Thirty minutes, the reference's `CIRCUIT_BREAKER_COOLDOWN`: long enough for
/// an outage window to close on its own, short enough that an operator who
/// fixed the problem does not have to restart the process.
const REFRESH_BREAKER_COOLDOWN_SECS: u64 = 30 * 60;

/// Upper bound on how long a caller waits for the per-connection refresh lock.
///
/// The lock is held across the refresh `await`, so a hung upstream — a
/// blackholed proxy mid-outage, an endpoint that accepts the socket and never
/// answers — would park every waiting request on this connection until process
/// restart. 90s ≈ 3× the 30s per-attempt budget of
/// [`REFRESH_MAX_ATTEMPTS`] attempts, which is generous for a slow but healthy
/// refresh and short enough to unwedge a wedged one. Mirrors the reference's
/// `REFRESH_MUTEX_MAX_MS_DEFAULT`.
const REFRESH_LOCK_BOUND_SECS: u64 = 90;

/// `expires_in` assumed when a refresh response omits one.
///
/// The reference grok-cli executor's fallback: six hours, which is the
/// lifetime Grok Build actually issues. Without it the token would be born
/// already stale and every request would refresh on its way out.
const EXPIRES_IN_FALLBACK_SECS: u64 = 21_600;

/// Ceiling on how much of a refresh-failure body is scanned for a reason.
///
/// The reason strings live in [`TERMINAL_REFRESH_STATUS`] and are short, so 2 KiB
/// carries a JSON error object. Bounded because the body is attacker-adjacent
/// and a provider error page can be megabytes of HTML.
const REFRESH_BODY_SCAN: usize = 2 * 1024;

/// Ceiling on one token-endpoint round trip, shared by the refresher and the
/// login exchange.
///
/// One constant for both because they are the same operation against the same
/// operator-supplied host: two numbers would let the paths drift, and a login
/// that stalls where a refresh would have returned is a bug nobody can see.
/// Not a knob — a login the operator is watching a browser for is not a hot
/// path.
const TOKEN_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(30);

/// RFC 8628 §3.2's default poll interval, used when the provider names none
/// (or names zero).
///
/// §3.2 says a client that omits `interval` waits this long, and it is a floor
/// on *our* politeness in both directions: an endpoint that wants more
/// cadence than 5s should have said so, and one that says nothing cannot make
/// this build into a hot loop.
const DEVICE_DEFAULT_INTERVAL_SECS: u64 = 5;

/// The cadence a kilo-shaped grant is polled at when it names no `interval`.
///
/// §3.2's own default stays the answer for every grant that reads like the
/// RFC; this is the one provider whose endpoint wants a poll faster than that,
/// and it wants it *silently* — it publishes no `interval` field at all, so
/// the only evidence it is not an RFC grant is that its other fields are not
/// RFC either. Hence the trigger: applied only when the expiry itself arrived
/// camelCase (see [`initiate_device`]), so an RFC-shaped grant that omits
/// `interval` still gets [`DEVICE_DEFAULT_INTERVAL_SECS`].
///
/// Faster than the RFC floor by design, and not a hot loop: 3s is a poll
/// rate, not a spin, and it is the provider's own number rather than a guess
/// at one.
const DEVICE_CAMEL_INTERVAL_SECS: u64 = 3;

/// The device-code placeholder a `device_poll_url` may carry: `{code}`.
///
/// Some providers address the grant by path (`/poll/{code}`) rather than by
/// body parameter, and which one a given provider does cannot be inferred from
/// its id without inventing a wire format (AGENTS.md) — so the substitution is
/// the operator's to declare in `config.yaml` and [`poll_device`]'s to
/// perform. A URL without the placeholder is polled byte-identically, which is
/// what keeps every existing provider on exactly the request it had.
const DEVICE_POLL_CODE_PLACEHOLDER: &str = "{code}";

/// How much §3.5's `slow_down` adds to the wait, per its own wording.
///
/// "Increase the polling interval by 5 seconds for this and all subsequent
/// requests" — restated rather than derived, so a provider that widens its own
/// cadence is followed exactly instead of approximately.
const SLOW_DOWN_STEP_SECS: u64 = 5;

/// Ceiling on the wait between device polls.
///
/// A cap because `slow_down` is the one §3.5 code that widens the wait without
/// bound: repeated, it would push the next poll past the human's patience and
/// past the grant's own `expires_in`, so the login would report an expiry rather
/// than the provider's own rate limit. A constant, not a knob — the ceiling's
/// only job is to stay inside the grant's lifetime.
const DEVICE_POLL_INTERVAL_CEILING_SECS: u64 = 30;

/// RFC 7636 §4.1's floor on `code_verifier` length.
///
/// The RFC permits 43..=128 and this build always produces 64, so the test
/// asserts against the floor: shorter is a spec violation, longer is allowed.
pub const PKCE_VERIFIER_MIN_LEN: usize = 43;

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
/// # Why `Cursor` and `GrokCli` are variants
///
/// Neither is in the four-provider minimum. They are here because F-MED-2's
/// carve-out list names Cursor's `expired` as *retryable*, and the reference
/// `grok-cli` executor carries a refresh verdict this build's shared list does
/// not — and a carve-out with no variant to hang off is a comment rather than
/// code.
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
    /// xAI's Grok Build account, reached as the Grok CLI.
    ///
    /// # Authentication only
    ///
    /// The registry row is `authType: oauth` with `authHeader: bearer`, so this
    /// variant is the whole of what it needs: a bearer goes in, a refresh brings
    /// a new one back. Nothing about the *wire* is transcribed — the Responses
    /// body, the `x-grok-*` client headers and the model defaults belong to the
    /// dispatch layer, not here.
    ///
    /// # Nothing about the endpoints is hardcoded
    ///
    /// The reference registry points `tokenUrl` at `{ISSUER}/oauth2/token` on a
    /// Grok Build issuer. That shape is recorded here as documentation only: the
    /// endpoint arrives through [`Session::with_token_url`] from the operator,
    /// because an auth endpoint inferred from a provider id is exactly the
    /// invented wire format AGENTS.md forbids.
    ///
    /// # The client id is public, so it is a config value and not a secret
    ///
    /// The reference registry reads `clientIdEnv: GROK_OAUTH_CLIENT_ID` and
    /// falls back to a compiled-in *public* credential under the key name
    /// `grok_id`. A public-client id is not a credential — RFC 6749 §2.3.1 puts it
    /// in every authorization request in the clear — so it is supplied through
    /// [`Session::with_client_id`] like any other public value. No secret is
    /// compiled in and none is read from the environment here.
    GrokCli,
    /// KiloCode's account, reached as the Kilo router.
    ///
    /// # Why this is here when R1 said not to guess
    ///
    /// R1 declined to add this variant because two live `kilocode` sessions had no
    /// stored refresh token and were nonetheless active, so the *mechanism* looked
    /// unknown. The mechanism is now known and it is the simple one: the registry
    /// row is `auth_kind: oauth` over the OpenAI wire at
    /// `api.kilo.ai/api/openrouter`, and the credential OmniRoute stores is a
    /// bearer that the endpoint accepts. Measured 2026-10-05 against
    /// `api.kilo.ai/api/openrouter/chat/completions`, every model the router
    /// advertises answered 200 through that bearer.
    ///
    /// So this variant claims exactly what `Cline` claims — a bearer goes in — and
    /// nothing more. No token URL is inferred and no client id is compiled in, for
    /// the same reason `GrokCli` takes its endpoint from the operator: an auth
    /// endpoint guessed from a provider id is the invented wire format AGENTS.md
    /// forbids. A pasted token therefore works and simply cannot renew, which is
    /// the honest ceiling until a refresh flow is understood.
    KiloCode,
}

impl OAuthKind {
    /// Every kind, so a caller can enumerate coverage without a second list.
    pub const ALL: [Self; 7] = [
        Self::Codex,
        Self::Cline,
        Self::Claude,
        Self::GeminiCli,
        Self::Cursor,
        Self::GrokCli,
        Self::KiloCode,
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
            Self::GrokCli => "grok-cli",
            Self::KiloCode => "kilocode",
        }
    }

    /// The registry `alias` the same provider also answers to, when it has one.
    ///
    /// A second name for the same account, not a second provider: an alias
    /// resolves to the kind whose [`Self::as_str`] is the canonical id, so the
    /// carve-out table cannot end up with two rows for one session.
    fn alias(self) -> Option<&'static str> {
        match self {
            Self::GrokCli => Some("gc"),
            _ => None,
        }
    }

    /// The kind for a registry provider id — or for the `alias` a registry entry
    /// carries beside it — or `None` when this build has no executor for it.
    ///
    /// The canonical id is checked first, so an alias can never shadow a real
    /// provider id.
    ///
    /// `None` is the answer for most of the 21 `oauth` entries in the compiled-in
    /// catalog. Red-team R1 records two live `kilocode` sessions with no stored
    /// refresh token that are nonetheless active, so its mechanism was unknown then
    /// and this function deliberately kept it failing loudly rather than guessing.
    /// [`Self::KiloCode`] is that answer, on the evidence that the stored bearer is
    /// accepted by the endpoint — see the variant docs for what is and is not
    /// claimed.
    #[must_use]
    pub fn parse(provider: &str) -> Option<Self> {
        // Over `ALL`, not a `match` on strings, so a new variant with no parse
        // arm fails to compile rather than quietly answering `None`.
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == provider || k.alias() == Some(provider))
    }

    /// Provider-specific overrides of [`TERMINAL_REFRESH_STATUS`], consulted
    /// *before* it.
    ///
    /// Most of them point from "this looks terminal" toward "retry", which is the
    /// safe direction: the cost of a wrong terminal verdict is a dead account, the
    /// cost of a wrong transient verdict is one wasted round trip. [`Self::GrokCli`]
    /// is the exception and is deliberately the only one, because the verdict it
    /// ports is terminal for that provider and nothing else: naming the same row
    /// terminal everywhere would retire accounts over an error only Grok Build is
    /// known to send.
    #[must_use]
    pub fn carve_out(self, reason: &str) -> Option<RefreshFault> {
        match (self, reason) {
            // Claude's refresh tokens survive a *transient* `invalid_grant` — an
            // IdP hiccup answers invalid_grant for a token that is still good.
            // Reading it as terminal retires a working session on a bad
            // afternoon, which is the failure mode F-MED-2 warns about.
            (Self::Claude, "invalid_grant") => {
                Some(RefreshFault::Transient("claude-invalid-grant-survives"))
            }
            // Grok Build's token endpoint carries its own terminal set,
            // `invalid_grant` + `invalid_client`. `invalid_grant` is already a row
            // in [`TERMINAL_REFRESH_STATUS`], so only the second one differs — and
            // it is the reason this variant exists. A client id the issuer does
            // not recognise cannot become valid by refreshing again, so reading it
            // as retryable would spend the router's whole attempt budget on a
            // refresh that can never succeed. The status is the one RFC 6749 §5.2
            // registers for `invalid_client`, and it is listed in
            // [`CARVE_OUT_TERMINAL_STATUS`] so `reason_in` can find the reason in a
            // body and the generated CHECK can admit the row.
            (Self::GrokCli, "invalid_client") => Some(RefreshFault::Unrecoverable {
                status: 401,
                reason: "invalid_client",
            }),
            _ => None,
        }
    }

    /// How far ahead of expiry this provider should be refreshed, in seconds.
    ///
    /// Most rows take [`REFRESH_LEAD_SECS`], and the reason is the provider's
    /// refresh-token *family*, not its access token. Codex and Claude enforce
    /// "one active session per client", so refreshing one account can invalidate
    /// the refresh_token of every sibling under the same client id. Those kinds
    /// therefore wait until the access token is genuinely about to expire, and a
    /// session that still has more than the lead left keeps the token it has —
    /// which is the whole point of the row being 5min rather than an hour.
    ///
    /// [`Self::GeminiCli`] is the exception and gets
    /// [`NON_ROTATING_REFRESH_LEAD_SECS`]: Google's refresh tokens are permanent,
    /// so an early refresh buys no safety and only adds upstream chatter.
    ///
    /// A session may override this per connection with
    /// [`Session::with_refresh_lead_secs`], which is how an operator tunes one
    /// account without changing what every other codex session does.
    #[must_use]
    pub fn refresh_lead_secs(self) -> u64 {
        match self {
            Self::GeminiCli => NON_ROTATING_REFRESH_LEAD_SECS,
            _ => REFRESH_LEAD_SECS,
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
            Self::Transient(reason) => {
                write!(f, "transient ({reason}); the session is still usable")
            }
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
///
/// The absences are load-bearing too — each is an audit verdict, not an
/// oversight. Every row was checked against the reference executor's terminal
/// sets file:line before the list was cut, and a row with no terminal verdict
/// behind it did not survive the cut:
///
/// - `permission_denied` — the reference files it under `PROJECT_ROUTE_ERROR`
///   and counts it *recoverable*, so a 403 saying only "denied" is this
///   request's access refused, not evidence that the account is gone.
/// - `invalid_token` and `token_expired` — no terminal verdict anywhere in the
///   reference. Absent evidence is not evidence of death, so both fail toward
///   retry and land on the transient fallthrough; a row here would retire a
///   working account on a verdict no other implementation reached.
///
/// `invalid_token` is therefore *not* rescued with invalid-credential phrases:
/// the unrecognised fallthrough already lands it on retry, which is the
/// reference's own verdict.
pub const TERMINAL_REFRESH_STATUS: &[(u16, &str)] = &[
    (400, "invalid_grant"),
    (400, "unauthorized_client"),
    (400, "no_refresh_token"),
    (401, "token_revoked"),
    (403, "account_disabled"),
    (410, "token_revoked"),
];

/// Terminal rows a provider's [`OAuthKind::carve_out`] can produce that
/// [`TERMINAL_REFRESH_STATUS`] does not list.
///
/// A carve-out is normally a *narrowing* — it moves a listed row toward retry — so
/// this list is empty for every provider but Grok Build, which the reference
/// executor retires on an `invalid_client` nobody else is known to send. Listing it
/// separately rather than widening the shared table keeps the other five kinds'
/// verdicts exactly as they were.
///
/// It is read in three places, and all three need it: [`reason_in`] scans for these
/// reasons so [`classify_refresh`] can hand one to [`OAuthKind::carve_out`] at all
/// (a carve-out otherwise never sees a reason the shared scan did not already
/// find); [`terminal_check_constraint`] generates its CHECK from the union, or the
/// classifier could produce a terminal row the store would refuse to hold; and
/// `ar doctor` reads the union, or it could not spell a reason the store admits.
/// Finding one is the only new classification behaviour, for a kind that has no
/// carve-out for it: such a reason is not in [`TERMINAL_REFRESH_STATUS`] either, so
/// it still lands on the transient fallthrough, under the same reason as before.
pub const CARVE_OUT_TERMINAL_STATUS: &[(u16, &str)] = &[(401, "invalid_client")];

/// The `CHECK` clause a credential store's terminal column must carry.
///
/// Generated from [`TERMINAL_REFRESH_STATUS`] rather than hand-written, so the
/// database cannot hold a status the classifier would call retryable — the exact
/// class of bug F-MED-2 describes. A function and not a `const` because a
/// `const fn` cannot build a `String`, and the list is small enough that building
/// it per call is free next to the sqlite open that consumes it.
///
/// The carve-out-only rows are chained in for the same reason: the clause has to
/// admit every terminal pair [`classify_refresh`] can emit, for *any* kind.
#[must_use]
pub fn terminal_check_constraint() -> String {
    let mut sql = String::from("CHECK (terminal_status IS NULL OR (");
    for (i, (status, reason)) in TERMINAL_REFRESH_STATUS
        .iter()
        .chain(CARVE_OUT_TERMINAL_STATUS)
        .enumerate()
    {
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
        return RefreshFault::Unrecoverable {
            status,
            reason: static_reason(reason),
        };
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
///
/// The shared list is scanned first and the carve-out-only rows second, so a body
/// naming both keeps the shared verdict: precedence for a listed row is unchanged.
/// [`ACCOUNT_DISABLED_ALIASES`] is consulted last, so a phrase body can only ever
/// *add* the account-dead verdict, never displace a listed row.
fn reason_in(body: &str) -> &str {
    let lower = body.to_ascii_lowercase();
    let end = lower.len().min(REFRESH_BODY_SCAN);
    let head = &lower[..end];
    if let Some((_, reason)) = TERMINAL_REFRESH_STATUS
        .iter()
        .chain(CARVE_OUT_TERMINAL_STATUS)
        .find(|(_, reason)| head.contains(*reason))
    {
        return reason;
    }
    if ACCOUNT_DISABLED_ALIASES
        .iter()
        .any(|phrase| head.contains(*phrase))
    {
        "account_disabled"
    } else {
        "unauthorized"
    }
}

/// Phrase forms of "this account is dead", each resolving to the
/// `account_disabled` row.
///
/// A provider that kills an account often says so in prose rather than in the
/// `snake_case` error code the rows are keyed by, and a phrase body used to fall
/// through to retry — so the one verdict that is unambiguously terminal was the
/// one a human-readable body could not reach. Matched as substrings of the
/// lowercased prefix, so the trailing clauses providers add
/// (`…in this account for violation of …`) are covered by the shorter entry
/// rather than by a second near-duplicate line.
const ACCOUNT_DISABLED_ALIASES: &[&str] = &[
    "account_deactivated",
    "account has been deactivated",
    "account has been disabled",
    "your account has been suspended",
    "this account is deactivated",
    "this service has been disabled in this account",
];

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
        Self {
            access,
            refresh: None,
            expires_at: None,
        }
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

    /// The refresh token, when there is one.
    ///
    /// Paired with [`Self::can_refresh`]; a login that just minted a token needs
    /// to write *both* halves to the store, and there was no accessor for the
    /// second one.
    #[must_use]
    pub fn refresh(&self) -> Option<&Secret> {
        self.refresh.as_ref()
    }

    /// Whether this token carries a refresh token.
    #[must_use]
    pub fn can_refresh(&self) -> bool {
        self.refresh.is_some()
    }

    /// Whether the token is at or past its expiry, with [`lead_secs`] of
    /// headroom.
    ///
    /// `lead_secs` is a parameter rather than a constant because the headroom is
    /// a property of the *provider* — see [`OAuthKind::refresh_lead_secs`] — and
    /// a token cannot know which provider it is about to be sent to.
    #[must_use]
    pub fn is_expiring_within(&self, now: u64, lead_secs: u64) -> bool {
        self.expires_at
            .is_some_and(|at| at <= now.saturating_add(lead_secs))
    }

    /// Whether the token is at or past its expiry, with the default
    /// [`REFRESH_LEAD_SECS`] of headroom.
    ///
    /// The no-argument form, for a caller that has a lead time in hand already
    /// and only wants the comparison. Kept so the common question stays one call.
    #[must_use]
    pub fn is_expiring(&self, now: u64) -> bool {
        self.is_expiring_within(now, REFRESH_LEAD_SECS)
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
    authorization_url: Option<String>,
    token_url: Option<String>,
    device_auth_url: Option<String>,
    device_poll_url: Option<String>,
    client_id: Option<String>,
    scope: Option<String>,
    /// Per-session override of [`OAuthKind::refresh_lead_secs`], in seconds.
    refresh_lead_secs: Option<u64>,
}

impl Session {
    /// A session with a token but no refresh endpoint.
    #[must_use]
    pub fn new(provider: impl Into<String>, kind: OAuthKind) -> Self {
        Self {
            provider: provider.into(),
            kind,
            authorization_url: None,
            token_url: None,
            device_auth_url: None,
            device_poll_url: None,
            client_id: None,
            scope: None,
            refresh_lead_secs: None,
        }
    }

    /// Sets the authorization endpoint a browser login starts at.
    ///
    /// Absent one means there is no browser login — [`authorize_url`] answers
    /// [`LoginError::NoAuthorizationUrl`] rather than guessing an endpoint from
    /// the provider id, which would be an invented wire format (AGENTS.md).
    #[must_use]
    pub fn with_authorization_url(mut self, url: impl Into<String>) -> Self {
        self.authorization_url = Some(url.into());
        self
    }

    /// Sets the refresh endpoint. Without one the session cannot renew.
    #[must_use]
    pub fn with_token_url(mut self, url: impl Into<String>) -> Self {
        self.token_url = Some(url.into());
        self
    }

    /// Sets the RFC 8628 §3.1 device-authorization endpoint.
    ///
    /// Separate from `token_url` because a device-flow provider may publish one
    /// and not the other: Kilo Code declares only `initiateUrl`/`pollUrlBase`,
    /// so a session that reused `token_url` here would either demand an endpoint
    /// the provider does not have or point at the wrong path.
    #[must_use]
    pub fn with_device_auth_url(mut self, url: impl Into<String>) -> Self {
        self.device_auth_url = Some(url.into());
        self
    }

    /// Sets the RFC 8628 §3.4 endpoint the pending code is polled at.
    #[must_use]
    pub fn with_device_poll_url(mut self, url: impl Into<String>) -> Self {
        self.device_poll_url = Some(url.into());
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

    /// Overrides how far ahead of expiry this one session refreshes.
    ///
    /// Precedence over [`OAuthKind::refresh_lead_secs`], so an operator can tune
    /// a single account — the one behind a provider whose family invalidation is
    /// misbehaving, say — without changing what every other session of the same
    /// provider does. `None` restores the per-kind default.
    ///
    /// Zero is a legal value and means "refresh only once expired", which is a
    /// real thing to want from a provider with a long-lived access token.
    #[must_use]
    pub fn with_refresh_lead_secs(mut self, lead_secs: u64) -> Self {
        self.refresh_lead_secs = Some(lead_secs);
        self
    }

    /// This session's proactive-refresh lead time, in seconds.
    ///
    /// The override when one was set, otherwise the per-kind default. Read once
    /// at the grant decision rather than threaded through, because it cannot
    /// change while the session is in flight.
    #[must_use]
    pub fn refresh_lead_secs(&self) -> u64 {
        self.refresh_lead_secs
            .unwrap_or_else(|| self.kind.refresh_lead_secs())
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

    /// The authorization endpoint, when one is configured.
    #[must_use]
    pub fn authorization_url(&self) -> Option<&str> {
        self.authorization_url.as_deref()
    }

    /// The refresh endpoint, when one is configured.
    #[must_use]
    pub fn token_url(&self) -> Option<&str> {
        self.token_url.as_deref()
    }

    /// The device-authorization endpoint, when one is configured.
    #[must_use]
    pub fn device_auth_url(&self) -> Option<&str> {
        self.device_auth_url.as_deref()
    }

    /// The device-poll endpoint, when one is configured.
    #[must_use]
    pub fn device_poll_url(&self) -> Option<&str> {
        self.device_poll_url.as_deref()
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
        Bytes::from(
            serde_json::to_vec(&body)
                .unwrap_or_else(|_| br#"{"error":{"type":"oauth_terminal"}}"#.to_vec()),
        )
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

/// Where a rotation's renewed tokens get written, and the identity the write is
/// guarded by.
///
/// Splitting this out as a trait is what keeps rotation persistence out of this
/// crate's dependency graph: `ar-keys` depends on `ar-exec` (its
/// `oauth_sessions` CHECK is generated from [`terminal_check_constraint`]), so
/// `ar-exec` cannot name `CredentialStore`. The trait points the other way — the
/// store implements it, this crate only calls it.
///
/// Implemented by `ar_keys::CredentialStore`. A caller with no store wires none,
/// and the connection's rotations stay in memory exactly as they were.
///
/// # Why the bound is `Send` and not `Sync`
///
/// A store that owns a sqlite connection cannot be `Sync` — `rusqlite` holds an
/// internal `RefCell` — so requiring `Sync` here would make the only real sink
/// unimplementable. The connection holds its sink behind an async mutex instead,
/// which is where the exclusion belongs anyway: the write is a blocking sqlite
/// transaction, so it must not run concurrently with another one.
pub trait RotationSink: Send {
    /// Writes `renewed` as the current tokens for `provider`, and reports whether
    /// the write happened.
    ///
    /// `presented` is the refresh token the refresh exchanged. A sink that finds
    /// its stored refresh token already differs must return `false` and write
    /// nothing: a concurrent writer rotated past us, and overwriting would revert
    /// its rotation. The caller hands the renewed token to the request either
    /// way, because upstream already authenticated it; only the stored state is
    /// in question.
    ///
    /// `false` is never an error the caller must handle. It means "somebody
    /// fresher got there first", which is the good outcome.
    fn persist_rotation(
        &self,
        provider: &str,
        presented: Option<&str>,
        renewed: &OAuthToken,
    ) -> bool;
}

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
    /// How long [`Self::rotate`] will wait for [`Self::refresh_lock`], so a
    /// wedged refresh faults rather than parking the request forever.
    refresh_lock_bound: Duration,
    terminal: RwLock<Option<TerminalReport>>,
    pool: Arc<RotationPool>,
    refresher: Arc<dyn Refresher>,
    /// Where a rotation's renewed tokens are written, when a store is wired.
    ///
    /// Behind a `std` mutex because a sink may own a blocking sqlite connection,
    /// which is `Send` but not `Sync` — see [`RotationSink`]'s bound. The write is
    /// a short synchronous transaction that never awaits, so a std lock cannot
    /// stall the runtime the way an async one would be entitled to.
    sink: Option<Arc<Mutex<Box<dyn RotationSink>>>>,
    _state: PhantomData<fn() -> S>,
}

impl<S> Connection<S> {
    /// Wires a store for renewed tokens, on a not-yet-connected connection.
    ///
    /// A builder rather than a `pending` argument so the existing three-argument
    /// construction keeps compiling: a deployment with no credential store wires
    /// nothing, and its rotations are in-memory exactly as they were.
    #[must_use]
    pub fn with_sink(mut self, sink: Box<dyn RotationSink>) -> Self {
        self.sink = Some(Arc::new(Mutex::new(sink)));
        self
    }

    /// Narrows how long [`Connection::rotate`] waits for the refresh lock.
    ///
    /// Only for a test that would otherwise wait [`REFRESH_LOCK_BOUND_SECS`]. The
    /// production path takes the default, which is a real bound rather than a
    /// test-only affordance: the whole point is that a wedged refresh unwedges.
    #[must_use]
    pub fn with_refresh_lock_bound(mut self, bound: Duration) -> Self {
        self.refresh_lock_bound = bound;
        self
    }

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
    /// later `account_disabled` after an `invalid_grant` is the *consequence* of
    /// the retirement, and overwriting would report the wrong cause. (A
    /// `permission_denied` can no longer arrive as a second report at all — it is
    /// not a terminal row.)
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
            refresh_lock_bound: Duration::from_secs(REFRESH_LOCK_BOUND_SECS),
            terminal: RwLock::new(None),
            pool,
            refresher,
            sink: None,
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
            return Err(RefreshFault::Unrecoverable {
                status: 401,
                reason: "empty_access_token",
            });
        }
        Ok(Connection {
            session: self.session,
            token: RwLock::new(token),
            refresh_lock: self.refresh_lock,
            refresh_lock_bound: self.refresh_lock_bound,
            terminal: self.terminal,
            pool: self.pool,
            refresher: self.refresher,
            sink: self.sink,
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
        f.debug_struct("Grant")
            .field("hash", &self.hash)
            .finish_non_exhaustive()
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
        Self {
            token: token.access,
            hash,
        }
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
        if origin == Origin::Probe
            || !current.is_expiring_within(now, self.session.refresh_lead_secs())
        {
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
    ///
    /// # The wedge bound
    ///
    /// Waiting for [`Self::refresh_lock`] is itself bounded by
    /// [`REFRESH_LOCK_BOUND_SECS`]. Without it, one hung upstream parks every
    /// request on this connection indefinitely: the lock is held across the
    /// refresh `await`, and a socket that is accepted and never answered never
    /// releases it. Exceeding the bound is reported as
    /// `RefreshFault::Transient("refresh-lock-wedged")` — transient, because the
    /// account is not what is wrong, and a caller can fall through to another
    /// combo or retry in a moment.
    pub async fn rotate(&self, stale: TokenHash) -> Result<Grant, RefreshFault> {
        if let Some(fresh) = self.pool.take(stale) {
            *self.token.write().await = fresh.clone();
            return Ok(Grant::of(fresh));
        }

        // The lock is the connection's, not the pool's: two *connections* sharing
        // a refresh token is the reuse case the pool prevents, and serialising
        // across connections would serialise unrelated providers.
        let _single_flight =
            match tokio::time::timeout(self.refresh_lock_bound, self.refresh_lock.lock()).await {
                Ok(guard) => guard,
                Err(_) => {
                    tracing::error!(
                        provider = %self.session.provider(),
                        bound_secs = self.refresh_lock_bound.as_secs(),
                        "refresh lock held past its bound; a concurrent refresh is wedged"
                    );
                    return Err(RefreshFault::Transient("refresh-lock-wedged"));
                }
            };
        if let Some(fresh) = self.pool.take(stale) {
            *self.token.write().await = fresh.clone();
            return Ok(Grant::of(fresh));
        }

        // The guard is read across the refresh `await`, which is what the async
        // lock buys and why this clone is free of contention concerns: refresh is
        // already serialised per connection by `refresh_lock` above.
        let current = self.token.read().await.clone();
        let presented = current.refresh.clone();
        let outcome = self.refresher.refresh(&self.session, &current).await;
        match outcome {
            Ok(renewed) => {
                self.pool.record(stale, renewed.clone());
                // Persisted inside the lock, deliberately: the network call and
                // the stored state then advance as one step, so a waiter that
                // joins this lock reads the *new* token rather than the one the
                // refresh just retired. Outside the lock there is a window where
                // the store still holds a spent refresh token.
                //
                // A skip is logged, not propagated: it means a concurrent writer
                // already rotated past us, and the request still gets the token
                // upstream just accepted.
                if let Some(sink) = self.sink.as_ref() {
                    let persisted = sink
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .persist_rotation(
                            self.session.provider(),
                            presented.as_ref().map(|token| token.expose()),
                            &renewed,
                        );
                    if !persisted {
                        tracing::warn!(
                            provider = %self.session.provider(),
                            "rotation not persisted: a concurrent writer already rotated this session"
                        );
                    }
                }
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

        let stream = core
            .post(&shape.with_api_key(first.token.expose()), canonical, abort)
            .await?;
        if stream.status() != StatusCode::UNAUTHORIZED {
            return Ok(stream);
        }

        tracing::warn!(provider = %self.provider(), "upstream refused the oauth token; rotating once");
        let second = match self.rotate(first.hash()).await {
            Ok(grant) => grant,
            Err(fault) => return Err(self.record(fault).await),
        };

        let retry = core
            .post(&shape.with_api_key(second.token.expose()), canonical, abort)
            .await?;
        if retry.status() != StatusCode::UNAUTHORIZED {
            return Ok(retry);
        }

        // A token minted seconds ago is already refused. Classify from the body
        // so the report names the provider's own reason, then retire the session
        // so the next request fails here instead of burning another token.
        let (status, body, _) = retry.into_failure().await;
        let text = String::from_utf8_lossy(&body[..body.len().min(REFRESH_BODY_SCAN)]).into_owned();
        Err(self
            .record(classify_refresh(self.kind(), status.as_u16(), &text))
            .await)
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
    breaker: Arc<RefreshBreaker>,
}

impl HttpRefresher {
    /// Builds a refresher over `client`. Cannot fail, so nothing is `Result`:
    /// a missing refresh endpoint is a per-session fact
    /// ([`RefreshFault::Unrecoverable`]), not a construction failure.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            timeout: TOKEN_ENDPOINT_TIMEOUT,
            breaker: Arc::new(RefreshBreaker::new()),
        }
    }

    /// Overrides the refresh timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Shares one breaker across refreshers, so a blackout one refresher trips
    /// is observed by every other refresh on the same provider.
    ///
    /// Without this each refresher would carry its own and a multi-connection
    /// deployment would keep hammering a provider that has already failed five
    /// times. [`Self::new`] still builds a working one — a single refresher needs
    /// no coordination.
    #[must_use]
    pub fn with_breaker(mut self, breaker: Arc<RefreshBreaker>) -> Self {
        self.breaker = breaker;
        self
    }

    /// The breaker this refresher consults and reports to.
    #[must_use]
    pub fn breaker(&self) -> &Arc<RefreshBreaker> {
        &self.breaker
    }

    /// The `reqwest` client this refresher shares with dispatch.
    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

/// Per-provider refresh circuit breaker: consecutive failures in, a blackout out.
///
/// # What it is for
///
/// A refresh endpoint that is down answers the same way every time, and each
/// attempt spends a refresh-token use against a provider that may count them. So
/// after [`REFRESH_BREAKER_THRESHOLD`] consecutive failures for one provider,
/// refreshes for that provider stop being attempted for
/// [`REFRESH_BREAKER_COOLDOWN_SECS`] and report a transient fault instead — which
/// leaves the caller free to try again in a minute, a dispatch to fall through
/// to another combo, and the *session* alive.
///
/// # Why a transient and not a terminal verdict
///
/// The breaker records that refreshes are not working, never that the account is
/// dead. Deciding an account is finished is [`classify_refresh`]'s job and its
/// list alone; a breaker that retired sessions would be a second, invisible
/// taxonomy — exactly what F-MED-2 exists to prevent.
///
/// # Success clears it
///
/// One success resets the counter, so a provider that recovers mid-cooldown is
/// picked up immediately rather than waiting out the window. The reference's
/// `recordSuccess` deletes the entry outright and so does this.
///
/// In-memory and per-process, exactly like the reference: a restart is a fresh
/// start, which is the correct behaviour for an outage window.
#[derive(Debug, Default)]
pub struct RefreshBreaker {
    state: Mutex<HashMap<String, BreakerEntry>>,
}

/// One provider's breaker state. `failures` counts since the last success;
/// `blocked_until` is a unix second, and `0` means never blocked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BreakerEntry {
    failures: u32,
    blocked_until: u64,
}

impl RefreshBreaker {
    /// A breaker with nothing tripped.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `provider`'s refreshes are currently blacked out at `now`.
    ///
    /// A blackout that has expired clears itself here, so the next call proceeds
    /// with the counter it had — the reference's `isProviderBlocked` deletes the
    /// entry on cooldown expiry, which for our purposes is the same observable
    /// behaviour.
    #[must_use]
    pub fn is_blocked(&self, provider: &str, now: u64) -> bool {
        let Ok(mut state) = self.state.lock() else {
            // A poisoned lock means another thread panicked holding it. A breaker
            // is an optimisation over a real refresh attempt, so letting the
            // attempt through is the safe direction.
            return false;
        };
        let Some(entry) = state.get_mut(provider) else {
            return false;
        };
        if entry.blocked_until > now {
            return true;
        }
        entry.blocked_until = 0;
        false
    }

    /// Clears `provider`'s counter and blackout.
    pub fn record_success(&self, provider: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.remove(provider);
        }
    }

    /// Counts one failed refresh for `provider`, tripping a blackout at the
    /// threshold.
    pub fn record_failure(&self, provider: &str, now: u64) {
        let Ok(mut state) = self.state.lock() else {
            tracing::warn!(
                provider,
                "refresh breaker lock poisoned; blackout unavailable"
            );
            return;
        };
        let entry = state.entry(provider.to_owned()).or_default();
        entry.failures = entry.failures.saturating_add(1);
        if entry.failures >= REFRESH_BREAKER_THRESHOLD && entry.blocked_until <= now {
            entry.blocked_until = now.saturating_add(REFRESH_BREAKER_COOLDOWN_SECS);
            tracing::error!(
                provider,
                failures = entry.failures,
                cooldown_secs = REFRESH_BREAKER_COOLDOWN_SECS,
                "refresh circuit breaker tripped; refreshes paused for this provider"
            );
        }
    }

    /// Consecutive failures recorded for `provider` since its last success.
    #[must_use]
    pub fn failures(&self, provider: &str) -> u32 {
        self.state.lock().map_or(0, |state| {
            state.get(provider).map_or(0, |entry| entry.failures)
        })
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
                return Err(RefreshFault::Unrecoverable {
                    status: 400,
                    reason: "no_refresh_token",
                });
            };
            let Some(refresh) = current.refresh.as_ref() else {
                // Nothing to present. Terminal by construction — there is no
                // second source for a refresh token.
                return Err(RefreshFault::Unrecoverable {
                    status: 400,
                    reason: "no_refresh_token",
                });
            };

            // The breaker gates the whole refresh, before a single token use is
            // spent. A tripped provider is reported as transient so the caller
            // falls through to another combo rather than treating the session as
            // finished — the breaker never speaks for the account's validity.
            let provider = session.provider();
            if self.breaker.is_blocked(provider, unix_now()) {
                tracing::warn!(
                    provider,
                    "refresh circuit breaker open; not attempting a refresh"
                );
                return Err(RefreshFault::Transient("refresh-breaker-open"));
            }

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

            let mut last: Option<RefreshFault> = None;
            for attempt in 1..=REFRESH_MAX_ATTEMPTS {
                match self.attempt(session.kind(), url, &form, current).await {
                    Ok(token) => {
                        self.breaker.record_success(provider);
                        return Ok(token);
                    }
                    // An unrecoverable verdict is not retried: the reference
                    // short-circuits on it for the same reason we do — a second
                    // attempt spends a second refresh-token use to learn the same
                    // thing, and the session needs a human either way. It *does*
                    // still reach the caller as its own verdict rather than being
                    // flattened into the loop's last transient.
                    Err(fault) if fault.is_terminal() => return Err(fault),
                    Err(fault) => {
                        tracing::warn!(
                            provider,
                            attempt,
                            max_attempts = REFRESH_MAX_ATTEMPTS,
                            reason = %fault,
                            "refresh attempt failed; retrying with backoff"
                        );
                        last = Some(fault);
                        if attempt < REFRESH_MAX_ATTEMPTS {
                            tokio::time::sleep(retry_delay(attempt)).await;
                        }
                    }
                }
            }

            // Every attempt spent. One failure is counted, not three: the breaker
            // exists to stop a *failing provider*, and three attempts against a
            // dead endpoint is one failure with a longer latency.
            self.breaker.record_failure(provider, unix_now());
            Err(last.unwrap_or(RefreshFault::Transient("refresh-failed")))
        })
    }
}

impl HttpRefresher {
    /// One POST against the token endpoint, and the §5.1 parse of its answer.
    ///
    /// Split out of the loop so each attempt is one bounded unit of work with the
    /// per-attempt timeout applied to it, and so the loop above reads as retry
    /// policy rather than as wire format. `current` supplies the §5.1 fallback
    /// refresh token and nothing else.
    async fn attempt(
        &self,
        kind: OAuthKind,
        url: &str,
        form: &[(&str, &str)],
        current: &OAuthToken,
    ) -> Result<OAuthToken, RefreshFault> {
        let response = match self
            .client
            .post(url)
            .form(form)
            .timeout(self.timeout)
            .send()
            .await
        {
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
            return Err(classify_refresh(kind, status, &body));
        }

        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|_| RefreshFault::Transient("unreadable-refresh-response"))?;
        let Some(access) = parsed
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .map(Secret::new)
        else {
            return Err(RefreshFault::Transient(
                "refresh-response-has-no-access-token",
            ));
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
    }
}

/// Jittered exponential backoff before attempt `attempt + 1`, doubling from
/// [`REFRESH_MIN_DELAY_MS`] and capped at [`REFRESH_MAX_DELAY_MS`].
///
/// Full jitter over the exponential — a uniform draw from half the base delay to
/// one and a half times it — rather than the full range, so the cap is honoured
/// and two refreshers that failed together do not retry in lockstep. The
/// reference grok-cli executor's `getRefreshRetryDelayMs`, shape for shape.
fn retry_delay(attempt: usize) -> Duration {
    let base = REFRESH_MIN_DELAY_MS
        .saturating_mul(1u64 << (attempt - 1).min(8))
        .min(REFRESH_MAX_DELAY_MS);
    // `attempt >= 1` always here, and the shift is capped, so no overflow.
    let jitter_millis = u64::from(jitter_byte()) * base / 255;
    Duration::from_millis(
        (base.saturating_sub(base / 2))
            .saturating_add(jitter_millis)
            .max(1),
    )
}

/// One draw from the OS entropy pool, as a `0..=255` scale factor.
///
/// One byte of a fresh v4 UUID, which is a getrandom draw the crate already
/// makes elsewhere. A whole RNG for one jitter multiplier is not a dependency
/// worth having, and a seeded `u64` from the clock would be the wrong tool: two
/// refreshers failing in the same millisecond would then retry in lockstep, which
/// is the thing jitter exists to prevent.
fn jitter_byte() -> u8 {
    // The slice is fixed at 16 bytes by `uuid`, so the index cannot be out of
    // range and there is nothing to guard against.
    Uuid::new_v4().as_bytes()[0]
}

/// Unix expiry from a §5.1 `expires_in`, relative to `now`.
///
/// A response that omits `expires_in` gets [`EXPIRES_IN_FALLBACK_SECS`] rather
/// than `now`. Handing back `now` would mint a token that is *already* stale, so
/// every single dispatch would decide it had to refresh — one wasted refresh-token
/// use per request forever, on a provider that answered perfectly well. Six
/// hours is the reference grok-cli executor's fallback and the lifetime Grok
/// Build actually issues; a provider whose real lifetime is shorter still gets
/// its 401 path, which is what expiry is for.
fn expiry_at(parsed: &serde_json::Value, now: u64) -> u64 {
    parsed
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .map_or_else(
            || now.saturating_add(EXPIRES_IN_FALLBACK_SECS),
            |secs| now.saturating_add(secs),
        )
}

/// `reqwest`'s own reason, narrowed to the two the taxonomy distinguishes.
///
/// Its message is unbounded, and this crate does not echo provider text into
/// error strings, so a static reason is what the taxonomy gets.
fn transport_reason(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "refresh-transport-timeout"
    } else {
        "refresh-transport-failure"
    }
}

/// The provider-secret an [`OAuthToken`] carries.
///
/// Re-exported because a [`RotationSink`] implementation has to *construct* an
/// [`OAuthToken`] to say what it writes, and `ar-exec` cannot make every sink take
/// its own dependency on `ar-config` just to name this type. It is the same type,
/// so a sink that already depends on `ar-config` may keep using it.
pub use ar_config::Secret as ProviderSecret;

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

/// Every way a browser login can fail.
///
/// One sentence per variant, because this is what an operator reads: the caller
/// is usually a person looking at a phone, and "login failed" with no cause is
/// the failure mode this enum exists to remove. Nothing here names a code, a
/// `state`, or a token.
///
/// The enum is shared by both halves of the flow. The redirect-catch half
/// ([`CallbackListener`], [`parse_callback_url`]) produces the four
/// redirect variants; [`authorize_url`] produces [`Self::NoAuthorizationUrl`]
/// and [`Self::InvalidRedirectUri`]; [`exchange_code`] produces
/// [`Self::NoTokenUrl`] and [`Self::ExchangeFailed`]. The device-flow half
/// ([`initiate_device`], [`poll_device`]) adds the two endpoint-absence
/// variants and reuses [`Self::LoginDenied`] and [`Self::LoginExpired`] for
/// §3.5's two terminal codes. A caller matches on the variant, never on the
/// string.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    /// The provider refused: §4.1.2 answered with `?error=…` instead of a code.
    ///
    /// Carries the provider's own error code (`access_denied` and friends),
    /// bounded by [`PROVIDER_ERROR_SCAN`]. `error_description` is deliberately
    /// not carried — it is unbounded provider prose.
    #[error("provider refused the login: {0}")]
    LoginDenied(String),
    /// The token POST failed, or produced nothing usable.
    ///
    /// Carries a [`RefreshFault`], not a free-form string, so a login failure
    /// and a refresh failure answer the same question the same way: the exchange
    /// went through [`classify_refresh`], so a 400 `invalid_grant` here is the
    /// same terminal verdict the refresh path reaches and a provider's carve-out
    /// applies to a login too. A malformed callback reported through this
    /// variant is [`RefreshFault::Transient`] — an unrecognised condition must
    /// never retire an account.
    #[error("oauth exchange failed: {0}")]
    ExchangeFailed(RefreshFault),
    /// The callback's `state` was absent or did not match the one this client
    /// generated, so the redirect cannot be attributed to this request.
    #[error("oauth state mismatch")]
    StateMismatch,
    /// No callback arrived inside the deadline.
    #[error("oauth login expired before the provider redirected back")]
    LoginExpired,
    /// The loopback socket could not be bound, accepted on, or read from.
    #[error("oauth callback listener failed: {0}")]
    ListenerBind(String),
    /// The session declares no usable authorization endpoint.
    ///
    /// Either `authorization_url` or `client_id` is absent, and §4.1.1 requires
    /// both. Neither is ever inferred from the provider id.
    #[error("no authorization endpoint is configured; set authorization_url and client_id")]
    NoAuthorizationUrl,
    /// The session declares no token endpoint, so there is nowhere to post the
    /// authorization code.
    #[error("no token endpoint is configured; set token_url")]
    NoTokenUrl,
    /// The session declares no device-authorization endpoint, so §3.1 has
    /// nowhere to ask for a user code.
    #[error("no device authorization endpoint is configured; set device_auth_url")]
    NoDeviceAuthUrl,
    /// The session declares no device-poll endpoint, so §3.4 has nowhere to ask
    /// whether the human approved.
    #[error("no device poll endpoint is configured; set device_poll_url")]
    NoDevicePollUrl,
    /// The redirect URI is not an absolute URL, so §4.1.2 would send the
    /// provider's answer somewhere this client cannot read.
    ///
    /// A private-use scheme (`myapp://callback`) passes: RFC 8252 §7.1 names it
    /// for native clients, and refusing it would refuse the flow it exists for.
    #[error("the redirect uri is not an absolute url")]
    InvalidRedirectUri,
}

impl From<LoginError> for ExecError {
    /// Keeps a terminal exchange failure typed, and widens the rest.
    ///
    /// [`Self::ExchangeFailed`] defers to [`RefreshFault::into_exec`], so a
    /// terminal account arrives as [`ExecError::OAuthTerminal`] with a
    /// [`TerminalReport`] rather than as prose — the same visible verdict the
    /// refresh path produces. Everything else is a fact about the operator's
    /// configuration, and [`ExecError::Transport`] is the widest variant this
    /// enum owns; the sentence inside it says exactly what is missing, so the
    /// width costs no accuracy. A future `ar` that wants these as their own
    /// status adds one `ExecError` variant rather than four.
    fn from(err: LoginError) -> Self {
        match err {
            LoginError::ExchangeFailed(fault) => fault.into_exec("oauth-browser-login"),
            other => ExecError::Transport(format!("oauth browser login: {other}")),
        }
    }
}

/// Ceiling on a provider's `?error=` value.
///
/// The redirect lands on an unauthenticated local socket, so the string is
/// provider-supplied text arriving from an untrusted peer. Bounded for the same
/// reason [`REFRESH_BODY_SCAN`] bounds a refresh body: an error a caller might
/// print should not be able to be megabytes long.
const PROVIDER_ERROR_SCAN: usize = 64;

/// Ceiling on an `io::Error` message carried into [`LoginError::ListenerBind`].
const IO_REASON_SCAN: usize = 128;

/// Ceiling on a callback request head.
///
/// A browser's is well under 4 KiB. The cap is what stops an unauthenticated
/// local socket from being a memory-growth knob: the buffer stops growing here
/// whether or not the peer ever sends a blank line.
const CALLBACK_HEAD_MAX: usize = 8 * 1024;

/// The path a provider is told to redirect to, and the one the listener serves.
///
/// `/callback` rather than `/` so the shape of this socket is obvious in a port
/// listing and in a provider's registered-redirect allowlist.
const CALLBACK_PATH: &str = "/callback";

/// The page the browser lands on once the code is in hand.
const CALLBACK_PAGE_OK: &str = "<!doctype html><meta charset=\"utf-8\"><title>Signed in</title>\
     <p>Login complete. You can close this tab and return to the terminal.</p>";

/// The page the browser lands on when the redirect is refused.
const CALLBACK_PAGE_ERR: &str = "<!doctype html><meta charset=\"utf-8\"><title>Sign-in failed</title>\
     <p>Sign-in did not complete. Return to the terminal and retry.</p>";

/// A single-use loopback listener for one OAuth redirect (RFC 6749 §4.1).
///
/// Bind it, hand [`Self::redirect_uri`] to the authorize-URL builder, show that
/// URL to a human, then [`Self::wait_for_code`] for exactly one request. `self`
/// is consumed, so a listener cannot be reused: the second callback of a login
/// attempt has nothing listening, which is the whole point of the design — an
/// authorization code is single-use, and a socket that outlived the exchange
/// would be a credential waiting to be replayed by anything on the loopback.
///
/// The bind is `127.0.0.1:0` — loopback only, never a wildcard, and never a
/// fixed port, so two concurrent logins cannot collide and neither can be
/// predicted by a local process guessing the port.
pub struct CallbackListener {
    listener: TcpListener,
    port: u16,
}

impl CallbackListener {
    /// Binds an ephemeral port on the loopback address.
    ///
    /// The only construction failure is the OS refusing the socket, which is a
    /// per-machine fact rather than a per-session one.
    pub async fn bind() -> Result<Self, LoginError> {
        // Loopback and never `0.0.0.0`: this socket is unauthenticated for its
        // whole life, so a wildcard bind would offer the authorization code to
        // every host that can route to this machine.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| LoginError::ListenerBind(io_reason(&e)))?;
        let port = listener
            .local_addr()
            .map_err(|e| LoginError::ListenerBind(io_reason(&e)))?
            .port();
        Ok(Self { listener, port })
    }

    /// The `redirect_uri` to put on the authorize URL.
    ///
    /// Read it before moving the listener into [`Self::wait_for_code`]; the port
    /// is fixed from bind onwards, so the URI is stable for the attempt's life.
    #[must_use]
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{port}{CALLBACK_PATH}", port = self.port)
    }

    /// Waits for one callback and returns its authorization code.
    ///
    /// Answers the browser with a self-contained page — [`CALLBACK_PAGE_OK`] for
    /// a usable code, [`CALLBACK_PAGE_ERR`] for anything else — so the tab the
    /// human is looking at and the value the caller gets cannot disagree. The
    /// page never echoes the code, `state`, or the provider's error.
    ///
    /// The deadline covers the accept *and* the read, so a peer that opens the
    /// socket and then stalls cannot hold the listener past it; on expiry the
    /// socket is dropped, which is what keeps a login attempt from outliving
    /// itself in the `docs/00` RAM budget.
    pub async fn wait_for_code(
        self,
        expected_state: &str,
        timeout: Duration,
    ) -> Result<String, LoginError> {
        let Self { listener, port } = self;
        match tokio::time::timeout(timeout, serve_one(&listener, port, expected_state)).await {
            Ok(verdict) => verdict,
            Err(_elapsed) => Err(LoginError::LoginExpired),
        }
    }
}

/// Answers exactly one callback request, then returns.
///
/// The verdict is computed before the page is written so the 200 and the returned
/// code come from the same value, and the write is best-effort: a human who
/// closed the tab early has still completed the login, so a broken pipe must not
/// turn a captured code into an error.
async fn serve_one(
    listener: &TcpListener,
    port: u16,
    expected_state: &str,
) -> Result<String, LoginError> {
    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|e| LoginError::ListenerBind(io_reason(&e)))?;
    let head = read_head(&mut stream).await?;

    let verdict = request_target(&head, port).and_then(|url| code_in(&url, expected_state));
    let page = match &verdict {
        Ok(_) => CALLBACK_PAGE_OK,
        Err(_) => CALLBACK_PAGE_ERR,
    };
    let _ = write_page(&mut stream, if verdict.is_ok() { 200 } else { 400 }, page).await;
    verdict
}

/// Reads a request head, up to the blank line that ends it or [`CALLBACK_HEAD_MAX`].
///
/// Returns what arrived even if the peer hung up mid-head: the parse below is
/// what decides whether it was usable, and a truncated head that parses is still
/// a head a browser really sent.
async fn read_head(stream: &mut TcpStream) -> Result<String, LoginError> {
    let mut head = Vec::with_capacity(CALLBACK_HEAD_MAX / 2);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|e| LoginError::ListenerBind(io_reason(&e)))?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
        if head.windows(4).any(|window| window == b"\r\n\r\n") || head.len() >= CALLBACK_HEAD_MAX {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// The request line's target, parsed as a URL.
///
/// A request target is origin-form (`/callback?…`), so it is re-based onto this
/// listener's own loopback origin rather than hand-split — which also means
/// [`Url`] does the percent-decoding the callback's values need, and an
/// absolute-form target from a peer trying something clever fails to parse
/// instead of being followed.
fn request_target(head: &str, port: u16) -> Result<Url, LoginError> {
    let target = head
        .lines()
        .next()
        .and_then(|request_line| request_line.split_whitespace().nth(1))
        .ok_or(LoginError::ExchangeFailed(RefreshFault::Transient(
            "callback-request-has-no-target",
        )))?;
    Url::parse(&format!("http://127.0.0.1:{port}{target}")).map_err(|_| {
        LoginError::ExchangeFailed(RefreshFault::Transient("callback-target-is-not-a-url"))
    })
}

/// Writes one complete HTTP response and closes the write half.
///
/// `Connection: close` because there is no second request to serve: a browser
/// waiting on a keep-alive connection that will never be answered shows a
/// spinner, not the page that tells it the login is done.
async fn write_page(stream: &mut TcpStream, status: u16, page: &str) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\r\n",
        len = page.len(),
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(page.as_bytes()).await?;
    stream.shutdown().await
}

/// The authorization code `url` carries, or why it carries none.
///
/// §4.1.2 answers with `?code=…&state=…`, or with `?error=…` and no code, and
/// both paths return `state`. State is checked *first* and on its own: it is the
/// only thing that says this redirect belongs to the request this client made
/// (RFC 6749 §10.12), so a callback that fails it must not reach the exchange on
/// the strength of a code alone. Unknown parameters are ignored rather than
/// rejected — providers add their own, and §4.1.2 says to.
fn code_in(url: &Url, expected_state: &str) -> Result<String, LoginError> {
    let mut state = None;
    let mut code = None;
    let mut denied = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" => state = Some(value.into_owned()),
            "code" => code = Some(value.into_owned()),
            "error" => denied = Some(value.into_owned()),
            _ => {}
        }
    }

    if state.as_deref() != Some(expected_state) {
        return Err(LoginError::StateMismatch);
    }
    if let Some(error) = denied {
        return Err(LoginError::LoginDenied(provider_error(&error)));
    }
    code.filter(|value| !value.is_empty())
        .ok_or(LoginError::ExchangeFailed(RefreshFault::Transient(
            "callback-carries-no-code",
        )))
}

/// A provider's `?error=` value, bounded and defaulted.
///
/// `access_denied` is the case an operator needs to read off the error, so the
/// spec's short code is kept. Anything empty or oversized collapses to one
/// static name rather than being echoed.
fn provider_error(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > PROVIDER_ERROR_SCAN {
        return "provider_error".to_owned();
    }
    trimmed.to_owned()
}

/// An `io::Error`'s own message, truncated.
///
/// A socket error is a kernel message with no secret content and is what tells
/// an operator whether the bind failed because the port was taken or the
/// sandbox refused — a static reason would throw that away. A `std::io::Error`
/// can still be constructed from anything, so the string is cut before it
/// becomes an error somebody might print.
fn io_reason(e: &std::io::Error) -> String {
    e.to_string().chars().take(IO_REASON_SCAN).collect()
}

/// The authorization code in a full redirected URL, pasted by hand.
///
/// The Path B counterpart to [`CallbackListener::wait_for_code`], for a consent
/// completed on a device that has no listener to return to: the browser lands on
/// a dead loopback port and shows the URL it tried, which the human copies back
/// here. Same `state` check, same `?error=` handling, same [`LoginError`]
/// variants — so a caller cannot pass Path A output to Path B's parser and get a
/// weaker verdict.
pub fn parse_callback_url(url: &str, expected_state: &str) -> Result<String, LoginError> {
    let parsed = Url::parse(url)
        .map_err(|_| LoginError::ExchangeFailed(RefreshFault::Transient("not-a-callback-url")))?;
    code_in(&parsed, expected_state)
}

/// One browser login's PKCE pair, its CSRF nonce, and where it will land.
///
/// Holds the whole §4.1 state so a caller cannot pair one request's `state` with
/// another request's `verifier`: the two are generated together here and travel
/// together into [`authorize_url`]. Every field is public because the consumer is
/// a CLI or an MCP host that has to hand the URL to a browser and the verifier to
/// [`exchange_code`] — private would mean an accessor for each, and an accessor
/// that returns the verifier is the same exposure with more ceremony.
///
/// [`Debug`] is the exception and is hand-written: see that impl.
pub struct AuthorizeRequest {
    /// The session being authorized. Supplies the endpoints, client id and scope.
    pub session: Session,
    /// Where the provider will redirect, byte-for-byte.
    ///
    /// Carried verbatim into the authorize URL and into the exchange POST, because
    /// RFC 6749 §4.1.3 requires the exchange to repeat *exactly* what was
    /// authorized — a normalised copy is a `redirect_uri` mismatch.
    pub redirect_uri: String,
    /// CSRF nonce (§10.12), checked against the callback's `state`.
    pub state: String,
    /// The PKCE verifier (§4.1). The secret half of the pair; never logged.
    pub verifier: String,
    /// `BASE64URL(SHA256(ASCII(verifier)))`, the half that goes on the wire (§4.2).
    pub challenge: String,
}

impl std::fmt::Debug for AuthorizeRequest {
    /// Prints everything except the verifier.
    ///
    /// The verifier is the whole security of the flow: anyone holding it plus a
    /// leaked authorization code can redeem that code for tokens. [`Grant`] and
    /// [`TokenHash`] redact for the same reason and the same way — a type that
    /// holds a bearer says so in its `Debug` rather than relying on nobody
    /// printing it. The challenge and the state are public by construction (both
    /// travel in the authorize URL), so they stay legible.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizeRequest")
            .field("provider", &self.session.provider())
            .field("redirect_uri", &self.redirect_uri)
            .field("state", &self.state)
            .field("challenge", &self.challenge)
            .field("verifier", &"<32 bytes withheld>")
            .finish()
    }
}

/// Starts a login: a fresh PKCE `S256` pair and a fresh `state`.
///
/// Cannot fail, so it is not `Result` — every input is either generated here or
/// checked later, by [`authorize_url`] (endpoint, redirect URI) or by
/// [`exchange_code`] (endpoint). Splitting the failure that way keeps the
/// operator's mistake and the transport's verdict in the two functions that can
/// name them.
///
/// Two v4 UUIDs per value, rendered `simple()` as 64 hex characters: every
/// character is inside RFC 7636's unreserved set (`ALPHA / DIGIT / - . _ ~`) so
/// no percent-encoding can change what the provider hashes, and 64 clears the
/// 43-character [`PKCE_VERIFIER_MIN_LEN`] floor. `uuid`'s v4 draws from the OS
/// CSPRNG, which is the requirement here — a predictable verifier is the one bug
/// that turns PKCE into decoration.
#[must_use]
pub fn new_authorize_request(session: &Session, redirect_uri: &str) -> AuthorizeRequest {
    let verifier = random_unreserved();
    AuthorizeRequest {
        session: session.clone(),
        redirect_uri: redirect_uri.to_owned(),
        state: random_unreserved(),
        challenge: code_challenge_for(&verifier),
        verifier,
    }
}

/// 64 unreserved characters of CSPRNG output. See [`new_authorize_request`].
fn random_unreserved() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// `BASE64URL(SHA256(ASCII(verifier)))` — RFC 7636 §4.2, the only method used.
///
/// `URL_SAFE_NO_PAD` because §4.2 says `BASE64URL` with the trailing `=` removed,
/// and unpadded output is also what keeps the value legal in a query string
/// without escaping.
fn code_challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The §4.1.1 authorization URL to open in a browser.
///
/// Every parameter is one the RFCs define — `response_type`, `client_id`,
/// `redirect_uri`, `scope`, `code_challenge`, `code_challenge_method`, `state` —
/// and nothing provider-specific is appended, because a guess here would be an
/// invented wire format (AGENTS.md). `scope` is sent only when the operator
/// configured one, since §3.3 makes it optional; `client_id` is required, so its
/// absence is [`LoginError::NoAuthorizationUrl`].
///
/// `code_challenge_method` is `S256` with no alternative and no knob: `plain`
/// exists for clients that cannot hash, which is not this one.
///
/// # Errors
///
/// [`LoginError::InvalidRedirectUri`] when `redirect_uri` is not absolute — a
/// relative target would send the provider's answer nowhere this client reads.
/// [`LoginError::NoAuthorizationUrl`] when the session declares no
/// `authorization_url`, no `client_id`, or an endpoint that is not a URL.
///
/// Percent-encoding goes through [`Url`]'s query builder rather than string
/// concatenation, because `redirect_uri` carries a `?` and `&` of its own and a
/// hand-built query would silently split it.
pub fn authorize_url(req: &AuthorizeRequest) -> Result<String, LoginError> {
    // Validity gate only: the parameter sent below is the caller's original
    // string, because §4.1.3 wants the same bytes the authorize request carried.
    Url::parse(&req.redirect_uri).map_err(|_| LoginError::InvalidRedirectUri)?;
    let (Some(endpoint), Some(client_id)) =
        (req.session.authorization_url(), req.session.client_id())
    else {
        return Err(LoginError::NoAuthorizationUrl);
    };
    let mut url = Url::parse(endpoint).map_err(|_| LoginError::NoAuthorizationUrl)?;

    let mut query = url.query_pairs_mut();
    query
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id);
    // The original text, not `redirect`: §4.1.3 wants the same bytes the
    // authorize request carried, and `Url` normalises (`/path` gains a trailing
    // `/`, a host gains a lowercased case) which would break the match.
    query.append_pair("redirect_uri", req.redirect_uri.as_str());
    if let Some(scope) = req.session.scope() {
        query.append_pair("scope", scope);
    }
    query
        .append_pair("code_challenge", req.challenge.as_str())
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", req.state.as_str());
    drop(query);

    Ok(url.into())
}

/// Trades an authorization code for tokens: the RFC 6749 §4.1.3 form POST.
///
/// One `async fn`, not a second [`Refresher`] implementation. A login happens
/// once per account while a refresh happens on every expiry, so the trait's
/// shape — a boxed future behind a `dyn`, held by a connection for its lifetime
/// — would buy nothing here.
///
/// `client_secret` is a parameter rather than read from the session because it is
/// token material: the credential store holds it as its own row and the caller
/// passes it in already decrypted, the same seam [`OAuthToken`] establishes.
/// Confidential clients send it; public PKCE clients pass `None` and the field is
/// then absent rather than empty, since §2.3.1 treats the two differently.
///
/// `core`'s client is the one that already pools the connections this proxy
/// makes, so a login does not open a second pool for the same hosts.
///
/// # Errors
///
/// [`LoginError::NoTokenUrl`] when the session declares no `token_url`.
/// [`LoginError::ExchangeFailed`] otherwise, carrying a [`RefreshFault`] from
/// [`classify_refresh`] — so the caller learns whether the account is finished
/// rather than only that the POST failed. An absent `refresh_token` on a
/// successful response is *not* an error: §5.1 makes it optional and its absence
/// means "there is no renewal", which the returned token records by having none.
pub async fn exchange_code(
    core: &ArExec,
    session: &Session,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    client_secret: Option<&Secret>,
) -> Result<OAuthToken, LoginError> {
    let Some(url) = session.token_url() else {
        return Err(LoginError::NoTokenUrl);
    };

    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    if let Some(client_id) = session.client_id() {
        form.push(("client_id", client_id));
    }
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret.expose()));
    }

    let response = match core
        .client()
        .post(url)
        .form(&form)
        .timeout(TOKEN_ENDPOINT_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        // A transport failure is transient by definition: no verdict was
        // produced, so nothing was learned about the account.
        Err(e) => {
            return Err(LoginError::ExchangeFailed(RefreshFault::Transient(
                transport_reason(&e),
            )));
        }
    };

    let status = response.status().as_u16();
    let success = response.status().is_success();
    let raw = response.bytes().await.unwrap_or_default();
    let body = String::from_utf8_lossy(&raw[..raw.len().min(REFRESH_BODY_SCAN)]).into_owned();
    if !success {
        return Err(LoginError::ExchangeFailed(classify_refresh(
            session.kind(),
            status,
            &body,
        )));
    }

    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
        LoginError::ExchangeFailed(RefreshFault::Transient("unreadable-refresh-response"))
    })?;
    let Some(access) = parsed
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(Secret::new)
    else {
        return Err(LoginError::ExchangeFailed(RefreshFault::Transient(
            "refresh-response-has-no-access-token",
        )));
    };

    let mut token = OAuthToken::new(access).with_expiry(expiry_at(&parsed, unix_now()));
    // §5.1: optional, and its absence means this session cannot be renewed —
    // the first 401 is then terminal, which `Session::can_refresh` reports
    // rather than a later dispatch discovering.
    if let Some(refresh) = parsed
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .map(Secret::new)
    {
        token = token.with_refresh(refresh);
    }
    Ok(token)
}

/// What a human has to do to approve a device grant, and how long they have.
///
/// Everything in this type is *meant* for a person to read: the user code is
/// typed into another device, the URI is the page it is typed into. That is why
/// the fields are public and [`Debug`] is derived — a redacted grant would be
/// useless to the one caller that matters.
///
/// The secret half of the grant lives in [`DevicePending`], not here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceGrant {
    /// The short code the human types, §3.2's `user_code`.
    ///
    /// User-facing by design: a person reads this aloud or pastes it, so it
    /// prints and logs like any other display value. It is not a credential —
    /// §3.2's security argument is that it is useless without the bound device
    /// code.
    pub user_code: String,
    /// Where the human enters it, §3.2's `verification_uri`.
    ///
    /// User-facing for the same reason as the code. RFC 8628 §3.2 allows a
    /// pre-filled `verification_uri_complete` instead; a provider that sends only
    /// that one still has to hand out a URI to open, so this is required and the
    /// complete form is not read.
    pub verification_uri: String,
    /// Seconds the grant stays pollable, §3.2's `expires_in`.
    ///
    /// The grant's own budget. [`poll_device`] never waits past it, so a human
    /// who abandons the code cannot pin a poll loop open.
    pub expires_in_secs: u64,
    /// Seconds to wait between polls, §3.2's `interval`.
    ///
    /// Defaults to the RFC 8628 §3.2 default when the provider omits it.
    pub interval_secs: u64,
}

/// The secret half of a pending device grant: the code the polls carry.
///
/// Split from [`DeviceGrant`] so the two cannot be logged together by accident.
/// Every field is private and the only way out is [`Self::device_code`], which
/// exists so [`poll_device`] can post it; [`Debug`] is hand-written to name the
/// field and print nothing for it, the same treatment [`Grant`] and
/// [`TokenHash`] give key material.
#[derive(Clone)]
pub struct DevicePending {
    device_code: Secret,
    interval_secs: u64,
    expires_at: u64,
}

impl DevicePending {
    /// The §3.4 poll credential. Never printed, never logged.
    #[must_use]
    pub fn device_code(&self) -> &Secret {
        &self.device_code
    }

    /// The seconds to wait before the first poll.
    ///
    /// Public because a caller printing "checking again in Ns" needs it, and it
    /// is the provider's number rather than a secret.
    #[must_use]
    pub fn interval_secs(&self) -> u64 {
        self.interval_secs
    }

    /// Unix second at which the grant stops being pollable.
    #[must_use]
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }
}

impl std::fmt::Debug for DevicePending {
    /// Prints the interval and the deadline, and nothing for the device code.
    ///
    /// Same rule and same reason as [`AuthorizeRequest`]'s own `Debug`: the device
    /// code is the whole security of this flow, so a type that holds it says so
    /// in its `Debug` rather than trusting nobody to print it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePending")
            .field("device_code", &"<withheld>")
            .field("interval_secs", &self.interval_secs)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Asks the provider for a device grant: the RFC 8628 §3.1 request.
///
/// A `POST` with no grant fields — §3.2 defines nothing to send here beyond
/// `client_id`, and `scope` when the operator configured one. Standards-shaped
/// rather than provider-shaped, for the same reason [`HttpRefresher`] is: a
/// per-provider body would be an invented wire format (AGENTS.md).
///
/// Returns the grant and the pending half together because a caller cannot use
/// one without the other — the grant is what it shows the human, the pending is
/// what it polls with — and pairing them at construction is what stops a caller
/// polling a code it got from a different grant.
///
/// `core`'s client is the one that already pools connections for these hosts, so
/// a login opens no second pool.
///
/// # Errors
///
/// [`LoginError::NoDeviceAuthUrl`] when the session declares no
/// `device_auth_url`, or declares one that is not a URL — never a guessed
/// endpoint. [`LoginError::ExchangeFailed`] otherwise: a transport failure or an
/// unreadable body arrives as [`RefreshFault::Transient`], and a non-success
/// status is classified by [`classify_refresh`] like any other auth POST.
pub async fn initiate_device(
    core: &ArExec,
    session: &Session,
) -> Result<(DeviceGrant, DevicePending), LoginError> {
    let Some(url) = session.device_auth_url() else {
        return Err(LoginError::NoDeviceAuthUrl);
    };
    // Validity gate only; §3.1 posts to this endpoint either way.
    Url::parse(url).map_err(|_| LoginError::NoDeviceAuthUrl)?;

    let mut form: Vec<(&str, &str)> = Vec::new();
    if let Some(client_id) = session.client_id() {
        form.push(("client_id", client_id));
    }
    if let Some(scope) = session.scope() {
        form.push(("scope", scope));
    }

    let response = match core
        .client()
        .post(url)
        .form(&form)
        .timeout(TOKEN_ENDPOINT_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        // A transport failure is transient by definition: no verdict was
        // produced, so nothing was learned about the account.
        Err(e) => {
            return Err(LoginError::ExchangeFailed(RefreshFault::Transient(
                transport_reason(&e),
            )));
        }
    };

    let status = response.status().as_u16();
    let success = response.status().is_success();
    let raw = response.bytes().await.unwrap_or_default();
    let body = String::from_utf8_lossy(&raw[..raw.len().min(REFRESH_BODY_SCAN)]).into_owned();
    if !success {
        return Err(LoginError::ExchangeFailed(classify_refresh(
            session.kind(),
            status,
            &body,
        )));
    }

    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
        LoginError::ExchangeFailed(RefreshFault::Transient("unreadable-device-response"))
    })?;

    // Every one of these is required, and a missing one is named rather than
    // defaulted: a grant without the device code cannot be polled, one without
    // the user code cannot be displayed, and one without the URI leaves the human
    // nothing to open. A half-grant would be a login that looks started and then
    // fails minutes later, with the user code already typed into the void.
    //
    // Each lookup is a *fallback*, RFC name first. A provider need not be an RFC
    // provider to be a real one: kilocode publishes camelCase, and folds §3.2's
    // `device_code` and `user_code` into a single opaque `code` — which is exactly
    // what such a provider means, since the string the human types and the code
    // the polls carry are then one value. Renaming these instead of falling back
    // would reject every RFC-shaped grant, and that is the one shape that has to
    // keep working.
    let missing = |field: &'static str| LoginError::ExchangeFailed(RefreshFault::Transient(field));
    let shared_code = || string_field(&parsed, "code");
    let device_code = string_field(&parsed, "device_code")
        .or_else(shared_code)
        .ok_or_else(|| missing("device-response-has-no-device-code"))?;
    let user_code = string_field(&parsed, "user_code")
        .or_else(shared_code)
        .ok_or_else(|| missing("device-response-has-no-user-code"))?;
    let verification_uri = string_field(&parsed, "verification_uri")
        .or_else(|| string_field(&parsed, "verificationUrl"))
        .ok_or_else(|| missing("device-response-has-no-verification-uri"))?;

    // §3.2 makes `expires_in` required and `interval` optional; the default is
    // the RFC's own floor, so an absent interval is 5s rather than "as fast as
    // possible". `expires_in` is required rather than defaulted for the same
    // reason the code is: it is the grant's budget, and a login that invented
    // one would poll a code the provider had already discarded.
    //
    // `expiresIn` is the same fact camelCased. Its presence is also the marker for
    // the one signal a kilo-shaped grant cannot state for itself: it names no
    // cadence, so the cadence comes from the shape of the grant rather than from
    // a field. RFC name still wins whenever the provider sends both.
    let camel_expiry = parsed.get("expiresIn").and_then(serde_json::Value::as_u64);
    let expires_in_secs = parsed
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .or(camel_expiry)
        .ok_or_else(|| missing("device-response-has-no-expiry"))?;
    let interval_secs = parsed
        .get("interval")
        .and_then(serde_json::Value::as_u64)
        // Zero is not a faster poll, it is a missing field: §3.2's default is
        // the only safe reading, because the alternative is a hot loop against an
        // endpoint that named no cadence at all.
        .filter(|secs| *secs > 0)
        .or_else(|| camel_expiry.is_some().then_some(DEVICE_CAMEL_INTERVAL_SECS))
        .unwrap_or(DEVICE_DEFAULT_INTERVAL_SECS);

    Ok((
        DeviceGrant {
            user_code,
            verification_uri,
            expires_in_secs,
            interval_secs,
        },
        DevicePending {
            device_code: Secret::new(&device_code),
            interval_secs,
            expires_at: unix_now().saturating_add(expires_in_secs),
        },
    ))
}

/// A non-empty string field, or `None`.
///
/// Empty counts as absent: a provider answering `"verification_uri": ""` has not
/// configured the field, and handing an empty URI to a human is worse than
/// naming the missing one. Returns owned because every caller keeps the value
/// past the [`serde_json::Value`] it came from.
fn string_field(parsed: &serde_json::Value, key: &str) -> Option<String> {
    parsed
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Polls a pending device grant until the human approves it: RFC 8628 §3.4.
///
/// One `async fn`, for the same reason [`exchange_code`] is: a device login
/// happens once per account, so the [`Refresher`] trait's shape would buy
/// nothing.
///
/// The loop's three outcomes, and the spec's own words for each:
///
/// * **Approval** — a token response, parsed exactly as [`exchange_code`]
///   parses one. `refresh_token` is optional and its absence means no renewal.
/// * **`access_denied`** — [`LoginError::LoginDenied`], terminal. The human said
///   no; polling again cannot change that.
/// * **`expired_token`** — [`LoginError::LoginExpired`], terminal. §3.5 makes it
///   the code for "the grant is over".
///
/// `authorization_pending` and `slow_down` are not verdicts and never leave this
/// loop: the first keeps waiting, the second also widens the interval (§3.5's
/// "+5 seconds") up to `DEVICE_POLL_INTERVAL_CEILING_SECS`, so the next poll
/// still lands inside the grant's own lifetime. Every other
/// non-approval answer — one whose `error` code §3.5 does not name, or a
/// *transient* HTTP failure where the provider produced no answer about the human
/// at all — is classified by [`classify_refresh`], the same classifier the
/// refresh path uses: transient keeps the loop trying inside `timeout`, and a
/// terminal row ends it with the fault that names it.
///
/// `timeout` is the caller's own budget, checked against both the wall clock and
/// the grant's `expires_in`, so the loop cannot outlive the code the human is
/// holding.
///
/// # Errors
///
/// [`LoginError::NoDevicePollUrl`] when the session declares no
/// `device_poll_url`. Otherwise a terminal [`LoginError::LoginDenied`] or
/// [`LoginError::LoginExpired`] for §3.5's two terminal codes and for the
/// loop's own deadline, and [`LoginError::ExchangeFailed`] carrying a
/// [`RefreshFault`] from [`classify_refresh`] for a poll failure the classifier
/// calls terminal.
pub async fn poll_device(
    core: &ArExec,
    session: &Session,
    pending: &DevicePending,
    timeout: Duration,
) -> Result<OAuthToken, LoginError> {
    let Some(template) = session.device_poll_url() else {
        return Err(LoginError::NoDevicePollUrl);
    };
    // A provider that addresses the grant by path needs the device code in the
    // URL; one that takes it as a body parameter does not, and which of the two a
    // given provider does is not inferable from its id without inventing a wire
    // format (AGENTS.md) — so the operator declares it with
    // `DEVICE_POLL_CODE_PLACEHOLDER` and this is the substitution. `replace` on a
    // URL with no placeholder returns it unchanged, so every flat poll URL is
    // polled byte-identically. Built once, outside the loop, because the device
    // code it embeds is fixed for the whole grant; borrowed (`as_str`) at the
    // `.post` below, which would otherwise move it on the first poll. Built once here, outside the loop, because the
    // device code it embeds is fixed for the whole grant; borrowed (`as_str`) at
    // the `.post` below, which would otherwise move it on the first poll.
    let url = template.replace(DEVICE_POLL_CODE_PLACEHOLDER, pending.device_code.expose());

    let mut wait = Duration::from_secs(pending.interval_secs);
    // One deadline for both budgets: the caller's `timeout` is the outer bound,
    // the grant's `expires_in` the inner one, and the login must not outlive
    // either. Two `Instant::now()` calls would let the two bounds drift by the
    // time between them.
    let now = tokio::time::Instant::now();
    let deadline = std::cmp::min(
        now + timeout,
        now + Duration::from_secs(pending.expires_at.saturating_sub(unix_now())),
    );

    loop {
        // §3.2 hands back a `user_code` the human has to read and type, so a
        // poll before the first wait is a guaranteed `authorization_pending`.
        // Clamped to the remaining budget so a long interval cannot overshoot
        // the deadline on its own.
        tokio::time::sleep(
            wait.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
        if tokio::time::Instant::now() >= deadline {
            // The caller's budget or the grant's `expires_in`, whichever came
            // first. `LoginExpired` rather than an exchange fault: no provider
            // verdict exists, only a human who did not finish in time.
            return Err(LoginError::LoginExpired);
        }

        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", pending.device_code.expose()),
        ];
        if let Some(client_id) = session.client_id() {
            form.push(("client_id", client_id));
        }

        let response = match core
            .client()
            .post(url.as_str())
            .form(&form)
            .timeout(TOKEN_ENDPOINT_TIMEOUT)
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => {
                // Transport: the provider never answered, so nothing was learned
                // about the human and the loop keeps asking inside its budget.
                continue;
            }
        };

        let status = response.status().as_u16();
        let success = response.status().is_success();
        let raw = response.bytes().await.unwrap_or_default();
        let body = String::from_utf8_lossy(&raw[..raw.len().min(REFRESH_BODY_SCAN)]).into_owned();

        // Read the §3.5 codes from the parsed body rather than from the status:
        // `authorization_pending` and `slow_down` arrive as 400, so a status-only
        // read would report a successful request as a refusal. The three statuses
        // the arms below match on are read the other way round, because those are
        // exactly the cases where the body carries nothing.
        let code = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| string_field(&v, "error"));

        match (status, success, code.as_deref()) {
            // The two terminal verdicts, read off the status and never off a body.
            //
            // 403 is the human refusing and 410 is the grant being over, and a
            // provider signals either with an empty body as clearly as one that
            // sends §3.5's error code. Reading them from the body instead does not
            // merely lose the answer — it inverts it: the terminal list's own rows
            // for these statuses are `account_disabled` and `token_revoked`, both
            // *account* verdicts, so a body read falls through to the classifier's
            // transient default and the loop keeps asking someone who has already
            // answered, until the deadline reports a timeout instead of their choice.
            (403, _, _) => return Err(LoginError::LoginDenied("access_denied".to_owned())),
            (410, _, _) => return Err(LoginError::LoginExpired),
            // 202 Accepted *is* §3.5's `authorization_pending`, carried on the
            // status, with no body at all — which is why it has to be matched
            // before the success arm: `is_success()` is true for 202, so without
            // this arm an empty 202 reaches the token parser and reports an
            // unreadable response rather than waiting for a human who is still
            // typing the code.
            (202, _, _) => {}
            // Approval: the token response, parsed by the same rules as
            // `exchange_code`, so a provider cannot answer the two flows with
            // two different shapes without the difference showing here.
            (_, true, _) => return token_from_body(&body),
            (_, _, Some("access_denied")) => {
                return Err(LoginError::LoginDenied("access_denied".to_owned()));
            }
            (_, _, Some("expired_token")) => return Err(LoginError::LoginExpired),
            // §3.5's two loop states: neither is a verdict about the human.
            (_, _, Some("authorization_pending")) => {}
            (_, _, Some("slow_down")) => {
                // §3.5's own step, capped so the next poll still lands inside the
                // grant's lifetime rather than being overtaken by the expiry.
                wait = (wait + Duration::from_secs(SLOW_DOWN_STEP_SECS))
                    .min(Duration::from_secs(DEVICE_POLL_INTERVAL_CEILING_SECS));
            }
            // Any other answer, including one whose `error` code §3.5 does not
            // name, goes to the shared classifier rather than straight back into
            // the loop. The reason is the terminal list: a 400 `invalid_grant`
            // here means the provider is finished with this grant, and reading it
            // as "keep asking" would poll a dead code until the deadline. The
            // classifier's fallthrough is transient, so a code this build does
            // not recognise still keeps waiting — an unknown condition never
            // retires anything.
            _ => {
                let fault = classify_refresh(session.kind(), status, &body);
                if fault.is_terminal() {
                    return Err(LoginError::ExchangeFailed(fault));
                }
            }
        }
    }
}

/// The §5.1 token response, shared by the code exchange and the device poll.
///
/// Both flows answer with the same shape, so they share one parser: a provider
/// that returns an access token without a refresh token means "there is no
/// renewal" either way, and two parsers would let that drift.
fn token_from_body(body: &str) -> Result<OAuthToken, LoginError> {
    let parsed: serde_json::Value = serde_json::from_str(body).map_err(|_| {
        LoginError::ExchangeFailed(RefreshFault::Transient("unreadable-refresh-response"))
    })?;
    // `token` is the same fact under a name some providers publish, so it is a
    // fallback rather than a replacement: §5.1's own spelling is tried first and an
    // RFC-shaped response is read exactly as before.
    let Some(access) =
        string_field(&parsed, "access_token").or_else(|| string_field(&parsed, "token"))
    else {
        return Err(LoginError::ExchangeFailed(RefreshFault::Transient(
            "refresh-response-has-no-access-token",
        )));
    };

    let mut token =
        OAuthToken::new(Secret::new(&access)).with_expiry(expiry_at(&parsed, unix_now()));
    // §5.1: optional, and its absence means this session cannot be renewed —
    // the first 401 is then terminal, which `Session::can_refresh` reports
    // rather than a later dispatch discovering. No path here invents one.
    if let Some(refresh) = string_field(&parsed, "refresh_token") {
        token = token.with_refresh(Secret::new(&refresh));
    }
    Ok(token)
}

// A connection is shared across request tasks and holds two locks; the rotation
// pool is shared across connections. Prove it at compile time rather than
// discovering it from a spawn error on the first concurrent request (ch.9).
const _: () = {
    const fn assert_send<T: Send + ?Sized>() {}
    const fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    assert_send::<Connection<Connected>>();
    assert_send::<Connection<Unconnected>>();
    assert_send_sync::<RotationPool>();
    assert_send_sync::<dyn Refresher>();
    // The sink sits behind an async mutex in a `Send` connection, and the breaker
    // is shared across refreshers by `Arc`, so both bounds are load-bearing rather
    // than incidental.
    assert_send::<dyn RotationSink>();
    assert_send_sync::<RefreshBreaker>();
};

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ar_config::Secret;
    use ar_registry::WireFormat;
    use axum::http::StatusCode;
    use tokio_util::sync::CancellationToken;

    use super::{
        CallbackListener, Connected, DEVICE_DEFAULT_INTERVAL_SECS, Dispatch,
        EXPIRES_IN_FALLBACK_SECS, ExecError, HttpRefresher, LoginError,
        NON_ROTATING_REFRESH_LEAD_SECS, OAuthKind, OAuthToken, Origin, PKCE_VERIFIER_MIN_LEN,
        REFRESH_BREAKER_THRESHOLD, REFRESH_LEAD_SECS, REFRESH_MAX_ATTEMPTS, RefreshBreaker,
        RefreshFault, Refresher, RotationPool, RotationSink, Session, TERMINAL_REFRESH_STATUS,
        TerminalReport, Unconnected, authorize_url, classify_refresh, code_challenge_for,
        exchange_code, expiry_at, initiate_device, new_authorize_request, parse_callback_url,
        poll_device, terminal_check_constraint, token_hash, unix_now,
    };
    use crate::oauth::Connection;

    /// Plays the browser's part in a loopback callback: one GET at the
    /// listener's own redirect URI, answered by the listener itself.
    ///
    /// Returns the status the browser would have rendered, which is the only way
    /// to assert on the page — the code never appears in it.
    async fn browse(uri: &str, query: &str) -> StatusCode {
        reqwest::Client::new()
            .get(format!("{uri}?{query}"))
            .send()
            .await
            .expect("the loopback callback is answered")
            .status()
    }

    /// A refresher that counts calls and hands back a scripted token, so the
    /// single-flight test measures *one* refresh rather than one socket.
    struct Counting {
        calls: AtomicUsize,
        prefix: &'static str,
        fault: Option<RefreshFault>,
    }

    impl Counting {
        fn ok(prefix: &'static str) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                prefix,
                fault: None,
            }
        }

        fn failing(fault: RefreshFault) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                prefix: "x",
                fault: Some(fault),
            }
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
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OAuthToken, RefreshFault>> + Send + 'a>,
        > {
            // Counted before the token is handed out, so a concurrent burst would
            // provably reach this line more than once if the lock were not held.
            let nth = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if let Some(fault) = self.fault {
                    return Err(fault);
                }
                Ok(
                    OAuthToken::new(Secret::new(&format!("{}-{nth}", self.prefix)))
                        .with_refresh(Secret::new("rotated-refresh"))
                        .with_expiry(unix_now() + 3600),
                )
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
    fn refuses_a_provider_this_build_still_has_no_executor_for() {
        // `kiro` is `auth_kind: oauth` in the catalog with no `OAuthKind`, so it
        // keeps failing loudly. This was `kilocode` until it gained an executor
        // of its own; the guard is unchanged, only the example moved.
        for id in ["kiro", "github", "trae", "devin-cli", "xai-oauth"] {
            assert_eq!(OAuthKind::parse(id), None, "{id} must stay unroutable");
        }
    }

    #[test]
    fn kilocode_now_parses_to_its_own_executor() {
        // The 2026-10-05 measurement that let this variant exist: the bearer
        // OmniRoute stores for `kilocode` is accepted by api.kilo.ai.
        assert_eq!(OAuthKind::parse("kilocode"), Some(OAuthKind::KiloCode));
    }

    #[test]
    fn parses_every_kind_through_its_own_spelling() {
        for kind in OAuthKind::ALL {
            assert_eq!(OAuthKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn resolves_the_grok_cli_registry_alias_to_the_same_executor() {
        // The registry entry carries `alias: "gc"` beside its id; both name one
        // account, so both must reach one carve-out table.
        assert_eq!(OAuthKind::parse("gc"), Some(OAuthKind::GrokCli));
    }

    #[test]
    fn treats_a_named_terminal_row_as_terminal() {
        assert!(
            classify_refresh(OAuthKind::Cline, 400, r#"{"error":"invalid_grant"}"#).is_terminal()
        );
    }

    #[test]
    fn keeps_a_grok_cli_invalid_client_terminal() {
        // The reference executor's own terminal set, ported. Without the carve-out
        // this falls through to the transient default and retries a refresh that
        // can never succeed.
        assert!(
            classify_refresh(OAuthKind::GrokCli, 401, r#"{"error":"invalid_client"}"#)
                .is_terminal()
        );
    }

    #[test]
    fn keeps_an_unlisted_provider_retryable_on_the_same_body() {
        // The carve-out is scoped: the shared scan now finds `invalid_client`, but a
        // kind with no arm for it still lands on the transient fallthrough, exactly
        // as it did before the reason became findable.
        assert!(
            !classify_refresh(OAuthKind::Codex, 401, r#"{"error":"invalid_client"}"#).is_terminal()
        );
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
    fn treats_cursor_token_expired_as_transient_without_a_carve_out() {
        // The carve-out arm is gone: `token_expired` lost its terminal row in the
        // audit (the reference has no terminal verdict for it), so Cursor lands on
        // the same transient fallthrough as every other kind and no longer needs a
        // provider-specific rescue.
        let fault = classify_refresh(OAuthKind::Cursor, 401, r#"{"error":"token_expired"}"#);
        assert_eq!(fault, RefreshFault::Transient("unrecognised-auth-failure"));
    }

    #[test]
    fn treats_cursor_bare_expired_as_retryable() {
        assert!(!classify_refresh(OAuthKind::Cursor, 401, r#"{"error":"expired"}"#).is_terminal());
    }

    #[test]
    fn treats_a_demoted_invalid_token_as_transient() {
        // No terminal verdict in the reference, so this is retry. The whole reason
        // the row was cut: a 401 that names only "invalid token" must not retire a
        // session the reference still considers alive.
        let fault = classify_refresh(OAuthKind::Cline, 401, r#"{"error":"invalid_token"}"#);
        assert_eq!(fault, RefreshFault::Transient("unrecognised-auth-failure"));
    }

    #[test]
    fn treats_a_demoted_token_expired_as_transient() {
        let fault = classify_refresh(OAuthKind::Cline, 401, r#"{"error":"token_expired"}"#);
        assert_eq!(fault, RefreshFault::Transient("unrecognised-auth-failure"));
    }

    #[test]
    fn treats_a_demoted_permission_denied_as_transient() {
        // The reference files `permission_denied` under `PROJECT_ROUTE_ERROR` and
        // counts it recoverable: a refused request is not a dead account.
        let fault = classify_refresh(OAuthKind::Cline, 403, r#"{"error":"permission_denied"}"#);
        assert_eq!(fault, RefreshFault::Transient("unrecognised-auth-failure"));
    }

    #[test]
    fn treats_account_deactivated_as_terminal() {
        assert_account_disabled_phrase(r#"{"error":"account_deactivated"}"#);
    }

    #[test]
    fn treats_account_has_been_deactivated_as_terminal() {
        assert_account_disabled_phrase(r#"{"error":"account has been deactivated"}"#);
    }

    #[test]
    fn treats_account_has_been_disabled_as_terminal() {
        assert_account_disabled_phrase(r#"{"error":"account has been disabled"}"#);
    }

    #[test]
    fn treats_your_account_has_been_suspended_as_terminal() {
        assert_account_disabled_phrase(r#"{"error":"your account has been suspended"}"#);
    }

    #[test]
    fn treats_this_account_is_deactivated_as_terminal() {
        assert_account_disabled_phrase(r#"{"error":"this account is deactivated"}"#);
    }

    #[test]
    fn treats_this_service_disabled_in_this_account_as_terminal() {
        assert_account_disabled_phrase(
            r#"{"error":"this service has been disabled in this account"}"#,
        );
    }

    #[test]
    fn matches_the_longer_disabled_in_account_clause_by_substring() {
        // The trailing clause providers add is not a second alias: the shorter
        // entry already contains it, which is why the alias list has six entries
        // for seven observed wordings.
        assert_account_disabled_phrase(
            "this service has been disabled in this account for violation of the terms",
        );
    }

    /// A prose account-dead body must retire the session with the row's reason.
    fn assert_account_disabled_phrase(body: &str) {
        // A phrase body used to fall through to retry, so the one verdict that is
        // unambiguously terminal was the one a human-readable body could not reach.
        assert_eq!(
            classify_refresh(OAuthKind::Cline, 403, body),
            RefreshFault::Unrecoverable {
                status: 403,
                reason: "account_disabled"
            },
            "{body}",
        );
    }

    #[test]
    fn treats_claude_invalid_grant_as_transient() {
        let fault = classify_refresh(OAuthKind::Claude, 400, r#"{"error":"invalid_grant"}"#);
        assert_eq!(
            fault,
            RefreshFault::Transient("claude-invalid-grant-survives")
        );
    }

    #[test]
    fn keeps_invalid_grant_terminal_when_no_carve_out_applies() {
        assert!(
            classify_refresh(OAuthKind::Codex, 400, r#"{"error":"invalid_grant"}"#).is_terminal()
        );
    }

    #[test]
    fn keeps_cursor_token_revoked_terminal() {
        // The carve-out is narrow: it rescues `expired`, not a revocation.
        assert!(
            classify_refresh(OAuthKind::Cursor, 401, r#"{"error":"token_revoked"}"#).is_terminal()
        );
    }

    #[test]
    fn generates_a_check_constraint_covering_every_terminal_row() {
        let sql = terminal_check_constraint();
        for (status, reason) in TERMINAL_REFRESH_STATUS {
            assert!(
                sql.contains(&format!(
                    "terminal_status = {status} AND terminal_reason = '{reason}'"
                )),
                "{sql}"
            );
        }
    }

    #[test]
    fn check_constraint_is_null_tolerant() {
        assert!(terminal_check_constraint().starts_with("CHECK (terminal_status IS NULL"));
    }

    #[test]
    fn admits_the_grok_cli_carve_out_row_in_the_generated_check() {
        // The classifier can emit this pair, so a store that took the generated
        // clause has to be able to hold it: a terminal verdict the CHECK refuses
        // would be dropped rather than recorded.
        let fault = OAuthKind::GrokCli
            .carve_out("invalid_client")
            .expect("grok-cli's terminal row");
        let sql = terminal_check_constraint();
        assert!(
            sql.contains(&format!(
                "terminal_status = {} AND terminal_reason = '{}'",
                fault.status(),
                fault.reason()
            )),
            "{sql}"
        );
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
            Some(RefreshFault::Unrecoverable {
                status: 401,
                reason: "empty_access_token"
            })
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
            tasks.push(tokio::spawn(async move {
                conn.grant_at(Origin::Client, now).await
            }));
        }
        for task in tasks {
            assert!(
                task.await.expect("no panic").is_ok(),
                "every caller gets a usable token"
            );
        }
        assert_eq!(refresher.calls(), 1, "eight concurrent grants, one refresh");
    }

    #[tokio::test]
    async fn hands_a_rotated_token_to_the_next_401_waiter() {
        // The rotation map's job: the second 401 arrives after the first refresh
        // already rotated, and must claim that token instead of refreshing again.
        let pool = Arc::new(RotationPool::new());
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Connection::pending(
            session(),
            Arc::clone(&pool),
            refresher.clone() as Arc<dyn Refresher>,
        )
        .connect(expired_token())
        .expect("an access token connects");
        let stale = token_hash("synthetic-old-access");

        let first = conn.rotate(stale).await.expect("first rotation");
        assert_eq!(first.token().expose(), "fresh-0");
        assert_eq!(refresher.calls(), 1);

        let second = conn
            .rotate(stale)
            .await
            .expect("the recorded rotation is claimed");
        assert_eq!(
            second.token().expose(),
            "fresh-0",
            "the same renewal, not a second one"
        );
        assert_eq!(refresher.calls(), 1, "no refresh_token_reuse");
    }

    #[tokio::test]
    async fn installs_the_rotated_token_so_the_next_grant_needs_no_refresh() {
        // The second half of the rotation contract: claiming a rotation also
        // moves the connection's own token forward, so a *later* dispatch is
        // served without a round trip rather than re-sending a stale bearer.
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Connection::pending(
            session(),
            Arc::new(RotationPool::new()),
            refresher.clone() as Arc<dyn Refresher>,
        )
        .connect(expired_token())
        .expect("an access token connects");

        let rotated = conn
            .rotate(token_hash("synthetic-old-access"))
            .await
            .expect("rotation");
        let later = conn
            .grant(Origin::Client)
            .await
            .expect("the installed token");
        assert_eq!(later.token().expose(), rotated.token().expose());
        assert_eq!(refresher.calls(), 1, "one refresh served both");
    }

    #[tokio::test]
    async fn does_not_refresh_for_a_probe_origin() {
        // R4: a health check must not spend a rotating token.
        let pool = Arc::new(RotationPool::new());
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Connection::pending(
            session(),
            Arc::clone(&pool),
            refresher.clone() as Arc<dyn Refresher>,
        )
        .connect(expired_token())
        .expect("an access token connects");
        let grant = conn
            .grant_at(Origin::Probe, unix_now())
            .await
            .expect("a probe grant");
        assert_eq!(
            grant.token().expose(),
            "synthetic-old-access",
            "the cached token, expired or not"
        );
        assert_eq!(refresher.calls(), 0, "a probe never refreshes");
    }

    #[tokio::test]
    async fn writes_no_rotation_for_a_probe_origin() {
        let pool = Arc::new(RotationPool::new());
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Connection::pending(
            session(),
            Arc::clone(&pool),
            refresher.clone() as Arc<dyn Refresher>,
        )
        .connect(expired_token())
        .expect("an access token connects");
        let _ = conn.grant_at(Origin::Probe, unix_now()).await;
        assert_eq!(pool.pending(), 0, "a probe leaves the rotation cache alone");
    }

    #[tokio::test]
    async fn does_not_quarantine_a_session_whose_refresh_failed_transiently() {
        // The point of the transient classification: a refresh endpoint having a
        // bad moment must not retire an account that still works.
        let refresher = Arc::new(Counting::failing(RefreshFault::Transient(
            "refresh-endpoint-unavailable",
        )));
        let conn = Connection::pending(
            session(),
            Arc::new(RotationPool::new()),
            refresher as Arc<dyn Refresher>,
        )
        .connect(expired_token())
        .expect("an access token connects");
        let err = conn.rotate(token_hash("synthetic-old-access")).await.err();
        assert_eq!(
            err,
            Some(RefreshFault::Transient("refresh-endpoint-unavailable"))
        );
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback binds");
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
            .dispatch(
                &core,
                &shape,
                br#"{"model":"m"}"#,
                &CancellationToken::new(),
            )
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
        // `ar-server` turns into a visible 401 — never a bare 502. The body's
        // reason has to be a row that survived the audit, so this is
        // `token_revoked`; a `invalid_token` body now reads as retry.
        let app = axum::Router::new()
            .fallback(|| async { (StatusCode::UNAUTHORIZED, r#"{"error":"token_revoked"}"#) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback binds");
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
            .dispatch(
                &core,
                &shape,
                br#"{"model":"m"}"#,
                &CancellationToken::new(),
            )
            .await
            .err();
        server.abort();

        // `token_revoked` is a named terminal row and no carve-out applies, so
        // the two 401s collapse into one durable retirement.
        let Some(ExecError::OAuthTerminal(report)) = err else {
            panic!("expected a typed oauth terminal, got {err:?}");
        };
        assert_eq!(
            (report.provider.as_str(), report.reason),
            ("kimi-coding", "token_revoked")
        );
        assert_eq!(refresher.calls(), 1, "one rotation, then stop");
    }

    #[tokio::test]
    async fn retries_rather_than_retires_when_the_refreshed_invalid_token_is_also_refused() {
        // The demoted-row end of the same path: a 401 body naming only
        // `invalid_token` has no terminal verdict behind it, so the second 401
        // becomes a transport failure and the session stays usable.
        let app = axum::Router::new()
            .fallback(|| async { (StatusCode::UNAUTHORIZED, r#"{"error":"invalid_token"}"#) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback binds");
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
            .dispatch(
                &core,
                &shape,
                br#"{"model":"m"}"#,
                &CancellationToken::new(),
            )
            .await
            .err();
        server.abort();

        assert!(
            matches!(err, Some(ExecError::Transport(_))),
            "expected a retryable transport failure, got {err:?}",
        );
        assert_eq!(
            conn.terminal().await,
            None,
            "a transient verdict must not retire the session"
        );
        assert_eq!(refresher.calls(), 1, "one rotation, then stop");
    }

    #[tokio::test]
    async fn quarantines_a_session_when_the_refresh_fault_is_terminal() {
        let refresher = Arc::new(Counting::failing(RefreshFault::Unrecoverable {
            status: 400,
            reason: "invalid_grant",
        }));
        let conn = Connection::pending(
            session(),
            Arc::new(RotationPool::new()),
            refresher as Arc<dyn Refresher>,
        )
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
        let conn = Connection::pending(
            session(),
            Arc::new(RotationPool::new()),
            refresher as Arc<dyn Refresher>,
        )
        .connect(expired_token())
        .expect("an access token connects");
        assert!(conn.grant(Origin::Client).await.is_err());
        assert_eq!(
            conn.terminal().await,
            None,
            "a transient must never retire an account"
        );
    }

    #[tokio::test]
    async fn answers_a_retired_session_without_touching_the_network() {
        let refresher = Arc::new(Counting::ok("fresh"));
        let conn = Connection::pending(
            session(),
            Arc::new(RotationPool::new()),
            refresher.clone() as Arc<dyn Refresher>,
        )
        .connect(expired_token())
        .expect("an access token connects");
        conn.quarantine(TerminalReport {
            provider: "cline".to_owned(),
            refresh_status: 400,
            reason: "invalid_grant",
        })
        .await;
        assert!(conn.grant(Origin::Client).await.is_err());
        assert_eq!(
            refresher.calls(),
            0,
            "a retired session costs no round trip"
        );
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
            reason: "account_disabled",
        })
        .await;
        assert_eq!(
            conn.terminal().await.map(|r| r.reason),
            Some("invalid_grant")
        );
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
    fn reports_an_expiry_inside_the_lead_as_stale() {
        let soon = OAuthToken::new(Secret::new("a")).with_expiry(1_000 + REFRESH_LEAD_SECS - 1);
        assert!(soon.is_expiring(1_000));
    }

    #[test]
    fn reports_an_expiry_beyond_the_lead_as_fresh() {
        let later = OAuthToken::new(Secret::new("a")).with_expiry(1_000 + REFRESH_LEAD_SECS + 1);
        assert!(!later.is_expiring(1_000));
    }

    #[test]
    fn renders_a_terminal_fault_as_the_operator_sentence() {
        assert_eq!(
            RefreshFault::Unrecoverable {
                status: 400,
                reason: "invalid_grant"
            }
            .to_string(),
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
        // `is_expiring` is `<=`, so `expires_at == now + lead` is already stale.
        let inside = 1_000_000 - REFRESH_LEAD_SECS - 1;
        assert!(conn.grant_at(Origin::Client, inside).await.is_ok());
        assert_eq!(refresher.calls(), 0, "still inside the window");
        assert!(conn.grant_at(Origin::Client, 1_000_000).await.is_ok());
        assert_eq!(refresher.calls(), 1, "at the expiry it refreshed once");
    }

    #[tokio::test]
    async fn names_a_loopback_redirect_uri() {
        // The bind address is the security claim: a wildcard `0.0.0.0` here
        // would offer the authorization code to the whole network, and a fixed
        // port would let a local process aim at the next login before it binds.
        let listener = CallbackListener::bind().await.expect("loopback binds");
        let uri = listener.redirect_uri();
        assert!(
            uri.starts_with("http://127.0.0.1:") && uri.ends_with("/callback"),
            "loopback-only, ephemeral, /callback — got {uri}"
        );
    }

    #[tokio::test]
    async fn captures_the_code_when_the_provider_redirects_with_the_expected_state() {
        let listener = CallbackListener::bind().await.expect("loopback binds");
        let uri = listener.redirect_uri();
        let (verdict, _page) = tokio::join!(
            listener.wait_for_code("state-abc", Duration::from_secs(5)),
            browse(&uri, "code=auth-code-1&state=state-abc"),
        );
        assert_eq!(
            verdict.expect("a matching callback yields its code"),
            "auth-code-1"
        );
    }

    #[tokio::test]
    async fn answers_the_browser_with_a_page_when_the_code_is_captured() {
        let listener = CallbackListener::bind().await.expect("loopback binds");
        let uri = listener.redirect_uri();
        let (_verdict, page) = tokio::join!(
            listener.wait_for_code("state-abc", Duration::from_secs(5)),
            browse(&uri, "code=auth-code-1&state=state-abc"),
        );
        assert_eq!(page, StatusCode::OK, "the tab is told the login is done");
    }

    #[tokio::test]
    async fn rejects_a_callback_whose_state_does_not_match() {
        // RFC 6749 §10.12: `state` is the only thing binding a callback to the
        // request this client made, so a redirect carrying somebody else's state
        // must not produce a code even when it carries a plausible one.
        let listener = CallbackListener::bind().await.expect("loopback binds");
        let uri = listener.redirect_uri();
        let (verdict, _page) = tokio::join!(
            listener.wait_for_code("state-abc", Duration::from_secs(5)),
            browse(&uri, "code=auth-code-1&state=state-from-another-login"),
        );
        assert!(
            matches!(verdict, Err(LoginError::StateMismatch)),
            "a foreign state is refused, not exchanged: {verdict:?}"
        );
    }

    #[tokio::test]
    async fn answers_the_browser_with_a_failure_page_when_the_state_does_not_match() {
        let listener = CallbackListener::bind().await.expect("loopback binds");
        let uri = listener.redirect_uri();
        let (_verdict, page) = tokio::join!(
            listener.wait_for_code("state-abc", Duration::from_secs(5)),
            browse(&uri, "code=auth-code-1&state=state-from-another-login"),
        );
        assert_eq!(page, StatusCode::BAD_REQUEST, "the tab is told it failed");
    }

    #[tokio::test]
    async fn reports_expired_when_no_callback_arrives_before_the_deadline() {
        // One millisecond is the whole assertion: a listener that outlived its
        // deadline would keep a socket open inside the docs/00 RAM budget for as
        // long as the tab sat there.
        let listener = CallbackListener::bind().await.expect("loopback binds");
        let verdict = listener
            .wait_for_code("state-abc", Duration::from_millis(1))
            .await;
        assert!(
            matches!(verdict, Err(LoginError::LoginExpired)),
            "an unattended login gives up: {verdict:?}"
        );
    }

    #[test]
    fn extracts_the_code_from_a_pasted_callback_url() {
        // Path B: consent completed on a phone, URL copied back by hand.
        assert_eq!(
            parse_callback_url(
                "http://127.0.0.1:53219/callback?code=auth-code-2&state=state-abc",
                "state-abc",
            )
            .expect("a pasted callback yields its code"),
            "auth-code-2",
        );
    }

    #[test]
    fn extracts_the_code_when_the_pasted_url_carries_extra_params() {
        // §4.1.2 lets a provider add parameters; rejecting an unrecognised one
        // would break the flow on the provider's next release, not protect it.
        assert_eq!(
            parse_callback_url(
                "http://127.0.0.1:53219/callback\
                 ?code=auth-code-3&state=state-abc&scope=openid+profile&iss=https%3A%2F%2Fauth.test",
                "state-abc",
            )
            .expect("extra parameters are ignored, not rejected"),
            "auth-code-3",
        );
    }

    #[test]
    fn rejects_a_pasted_callback_url_whose_state_does_not_match() {
        // The paste path is the exposed one: the URL arrives from a human and
        // from whatever was in their clipboard, so the state check is what stops
        // a code minted for someone else's login being pasted in here.
        let verdict = parse_callback_url(
            "http://127.0.0.1:53219/callback?code=auth-code-4&state=state-from-another-login",
            "state-abc",
        );
        assert!(
            matches!(verdict, Err(LoginError::StateMismatch)),
            "a foreign state is refused: {verdict:?}"
        );
    }

    #[test]
    fn rejects_a_pasted_callback_url_carrying_a_provider_error() {
        // `?error=` with no code is a denial, not a malformed callback, and
        // `error_description` is unbounded provider prose — only the spec's short
        // code is kept.
        let verdict = parse_callback_url(
            "http://127.0.0.1:53219/callback\
             ?error=access_denied&error_description=The+user+declined&state=state-abc",
            "state-abc",
        );
        assert!(
            matches!(verdict, Err(LoginError::LoginDenied(ref reason)) if reason == "access_denied"),
            "an access_denied redirect reads as a denial: {verdict:?}"
        );
    }

    /// A session configured for a browser login: both endpoints the operator must
    /// supply, a client id, and a scope.
    ///
    /// Synthetic hostnames throughout; nothing here reaches a real provider.
    fn login_session() -> Session {
        Session::new("codex", OAuthKind::Codex)
            .with_authorization_url("https://auth.test/authorize")
            .with_token_url("https://auth.test/token")
            .with_client_id("client-synthetic")
            .with_scope("openid profile")
    }

    /// A token endpoint that records the raw form it was posted and answers
    /// `reply`.
    ///
    /// The raw body rather than a parsed form because the assertion is about the
    /// exact field names and values going out, and a parser would normalise away
    /// the very thing under test.
    async fn token_endpoint(reply: (StatusCode, &'static str)) -> (String, Arc<Mutex<String>>) {
        let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let slot = Arc::clone(&seen);
        let app =
            axum::Router::new().fallback(move |req: axum::http::Request<axum::body::Body>| {
                let slot = Arc::clone(&slot);
                async move {
                    let raw = axum::body::to_bytes(req.into_body(), 64 * 1024)
                        .await
                        .unwrap_or_default();
                    // `into_inner` rather than `expect`: a poisoned lock here would
                    // mean an earlier assertion panicked, and that panic is the
                    // failure the test already reported.
                    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        String::from_utf8_lossy(&raw).into_owned();
                    reply
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback binds");
        let addr = listener.local_addr().expect("bound socket has an address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    /// A urlencoded body or query as `name=value` pairs, percent-decoded.
    fn urlencoded_pairs(raw: &str) -> Vec<(String, String)> {
        raw.split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (name, value) = pair.split_once('=').expect("a urlencoded field has a name");
                (percent_decode(name), percent_decode(value))
            })
            .collect()
    }

    fn percent_decode(raw: &str) -> String {
        let bytes = raw.replace('+', " ");
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut rest = bytes.as_str();
        while let Some(at) = rest.find('%') {
            out.extend_from_slice(&rest.as_bytes()[..at]);
            let hex = rest
                .get(at + 1..at + 3)
                .expect("a percent escape carries two digits");
            out.push(u8::from_str_radix(hex, 16).expect("hex digits"));
            rest = rest.get(at + 3..).unwrap_or_default();
        }
        out.extend_from_slice(rest.as_bytes());
        String::from_utf8(out).expect("utf-8 form value")
    }

    #[test]
    fn derives_the_rfc7636_s256_challenge_from_the_specs_own_verifier() {
        // RFC 7636 Appendix B. Written from the RFC's literals, not from this
        // implementation's output, so a change in the digest or the encoding
        // fails here rather than passing a self-consistent round trip.
        assert_eq!(
            code_challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generates_a_verifier_the_rfc7636_alphabet_and_length_permit() {
        let request =
            new_authorize_request(&login_session(), "http://127.0.0.1:1455/auth/callback");
        assert!(
            request.verifier.len() >= PKCE_VERIFIER_MIN_LEN
                && request
                    .verifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)),
            "verifier {:?} is outside RFC 7636's unreserved set or shorter than 43",
            request.verifier
        );
    }

    #[test]
    fn generates_a_fresh_state_and_verifier_for_each_login() {
        // Two logins must not share a `state` (that would make one redirect
        // attributable to the other) nor a verifier (that would make one
        // intercepted request redeemable against the other).
        let session = login_session();
        let first = new_authorize_request(&session, "http://127.0.0.1:1455/auth/callback");
        let second = new_authorize_request(&session, "http://127.0.0.1:1455/auth/callback");
        assert!(
            first.state != second.state && first.verifier != second.verifier,
            "state {} / {} and verifier {} / {} must differ per login",
            first.state,
            second.state,
            first.verifier,
            second.verifier
        );
    }

    #[test]
    fn keeps_the_verifier_out_of_an_authorize_request_debug() {
        let request =
            new_authorize_request(&login_session(), "http://127.0.0.1:1455/auth/callback");
        assert!(
            !format!("{request:?}").contains(&request.verifier),
            "the verifier is the whole security of PKCE and must not be printable"
        );
    }

    #[test]
    fn builds_an_authorize_url_carrying_the_rfc6749_and_7636_parameters() {
        let request =
            new_authorize_request(&login_session(), "http://127.0.0.1:1455/auth/callback");
        let url = authorize_url(&request).expect("a configured session authorizes");
        let query = url
            .split_once('?')
            .expect("an authorize url carries a query")
            .1;
        let pairs: std::collections::HashMap<_, _> = urlencoded_pairs(query).into_iter().collect();

        // Borrowed pairs: the expected values live in the fixture or in `request`,
        // so nothing here copies a string to compare it.
        for (name, value) in [
            ("response_type", "code"),
            ("client_id", "client-synthetic"),
            ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
            ("scope", "openid profile"),
            ("code_challenge", request.challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", request.state.as_str()),
        ] {
            assert_eq!(
                pairs.get(name).map(String::as_str),
                Some(value),
                "{name}={value} in {query}"
            );
        }
    }

    #[test]
    fn encodes_a_redirect_uri_that_carries_its_own_query_string() {
        // The provider must receive one `redirect_uri` value, not two: a
        // hand-built query would split the target at its own `&`.
        let target = "http://127.0.0.1:1455/cb?tenant=acme&next=%2Fhome";
        let request = new_authorize_request(&login_session(), target);
        let url = authorize_url(&request).expect("a configured session authorizes");
        let pairs = urlencoded_pairs(url.split_once('?').expect("a query").1);
        assert_eq!(
            pairs
                .iter()
                .filter(|(name, _)| name == "redirect_uri")
                .collect::<Vec<_>>(),
            vec![&("redirect_uri".to_owned(), target.to_owned())],
            "the redirect target survives intact: {url}"
        );
    }

    #[test]
    fn refuses_to_build_an_authorize_url_when_the_session_declares_no_endpoint() {
        let request = new_authorize_request(
            &Session::new("codex", OAuthKind::Codex).with_client_id("client-synthetic"),
            "http://127.0.0.1:1455/auth/callback",
        );
        assert_eq!(
            authorize_url(&request).err(),
            Some(LoginError::NoAuthorizationUrl)
        );
    }

    #[test]
    fn refuses_to_build_an_authorize_url_for_a_relative_redirect_uri() {
        let request = new_authorize_request(&login_session(), "/auth/callback");
        assert_eq!(
            authorize_url(&request).err(),
            Some(LoginError::InvalidRedirectUri)
        );
    }

    #[tokio::test]
    async fn posts_the_rfc6749_code_exchange_fields_to_the_token_endpoint() {
        let (base, seen) = token_endpoint((
            StatusCode::OK,
            r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#,
        ))
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = login_session().with_token_url(format!("{base}/token"));

        let token = exchange_code(
            &core,
            &session,
            "auth-code-1",
            "verifier-1",
            "http://127.0.0.1:1455/auth/callback",
            None,
        )
        .await
        .expect("the mock grants the code");

        let pairs = urlencoded_pairs(&seen.lock().unwrap_or_else(|p| p.into_inner()));
        for (name, value) in [
            ("grant_type", "authorization_code"),
            ("code", "auth-code-1"),
            ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
            ("code_verifier", "verifier-1"),
            ("client_id", "client-synthetic"),
        ] {
            assert!(
                pairs.iter().any(|(n, v)| n == name && v == value),
                "{name}={value} is missing from {pairs:?}"
            );
        }
        assert_eq!(
            token.access().expose(),
            "at-1",
            "the access token is parsed"
        );
    }

    #[tokio::test]
    async fn keeps_the_old_refresh_token_when_a_grok_cli_refresh_omits_one() {
        // The reference executor answers a grok-cli refresh without a
        // `refresh_token` when it does not rotate. RFC 6749 §5.1 makes the field
        // optional and its absence mean "keep using the one you have", so a
        // grok-cli session must survive its own refresh rather than lose the row
        // that renews it.
        let (base, _seen) = token_endpoint((
            StatusCode::OK,
            r#"{"access_token":"at-2","expires_in":3600}"#,
        ))
        .await;
        let session = Session::new("grok-cli", OAuthKind::GrokCli)
            .with_token_url(format!("{base}/token"))
            .with_client_id("grok-public-client");
        let current = expired_token().with_refresh(Secret::new("rt-grok-1"));
        let refresher = HttpRefresher::new(reqwest::Client::new());

        let token = refresher
            .refresh(&session, &current)
            .await
            .expect("the mock grants the refresh");

        assert_eq!(
            token.refresh().map(|refresh| refresh.expose()),
            Some("rt-grok-1")
        );
    }

    #[tokio::test]
    async fn posts_the_grok_cli_refresh_as_the_rfc6749_refresh_form() {
        // The reference executor's body, field for field: the shared §6 refresher
        // already emits exactly this, which is why grok-cli needs no wire of its own.
        let (base, seen) = token_endpoint((
            StatusCode::OK,
            r#"{"access_token":"at-2","refresh_token":"rt-2"}"#,
        ))
        .await;
        let session = Session::new("grok-cli", OAuthKind::GrokCli)
            .with_token_url(format!("{base}/token"))
            .with_client_id("grok-public-client");
        let refresher = HttpRefresher::new(reqwest::Client::new());

        refresher
            .refresh(
                &session,
                &expired_token().with_refresh(Secret::new("rt-grok-1")),
            )
            .await
            .expect("the mock grants the refresh");

        let pairs = urlencoded_pairs(&seen.lock().unwrap_or_else(|p| p.into_inner()));
        for (name, value) in [
            ("grant_type", "refresh_token"),
            ("client_id", "grok-public-client"),
            ("refresh_token", "rt-grok-1"),
        ] {
            assert!(
                pairs.iter().any(|(n, v)| n == name && v == value),
                "{name}={value} is missing from {pairs:?}"
            );
        }
    }

    #[tokio::test]
    async fn sends_the_client_secret_when_the_operator_supplied_one() {
        // A confidential client's secret is a third store row, handed in already
        // decrypted — the same seam an access token uses.
        let (base, seen) = token_endpoint((StatusCode::OK, r#"{"access_token":"at-1"}"#)).await;
        let core = crate::ArExec::new().expect("client");
        let session = login_session().with_token_url(format!("{base}/token"));

        exchange_code(
            &core,
            &session,
            "auth-code-1",
            "verifier-1",
            "http://127.0.0.1:1455/auth/callback",
            Some(&Secret::new("secret-synthetic")),
        )
        .await
        .expect("the mock grants the code");

        let pairs = urlencoded_pairs(&seen.lock().unwrap_or_else(|p| p.into_inner()));
        assert!(
            pairs
                .iter()
                .any(|(name, value)| name == "client_secret" && value == "secret-synthetic"),
            "a confidential client authenticates: {pairs:?}"
        );
    }

    #[tokio::test]
    async fn omits_the_client_secret_for_a_public_pkce_client() {
        // §2.3.1 treats an empty `client_secret` and an absent one differently, so
        // a public client must send no field at all.
        let (base, seen) = token_endpoint((StatusCode::OK, r#"{"access_token":"at-1"}"#)).await;
        let core = crate::ArExec::new().expect("client");
        let session = login_session().with_token_url(format!("{base}/token"));

        exchange_code(
            &core,
            &session,
            "auth-code-1",
            "verifier-1",
            "http://127.0.0.1:1455/auth/callback",
            None,
        )
        .await
        .expect("the mock grants the code");

        let pairs = urlencoded_pairs(&seen.lock().unwrap_or_else(|p| p.into_inner()));
        assert!(
            !pairs.iter().any(|(name, _)| name == "client_secret"),
            "a public client sends no client_secret: {pairs:?}"
        );
    }

    #[tokio::test]
    async fn keeps_an_exchange_failure_out_of_the_refreshable_state_when_no_refresh_token_comes_back()
     {
        let (base, _seen) = token_endpoint((
            StatusCode::OK,
            r#"{"access_token":"at-1","expires_in":3600}"#,
        ))
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = login_session().with_token_url(format!("{base}/token"));

        let token = exchange_code(
            &core,
            &session,
            "auth-code-1",
            "verifier-1",
            "http://127.0.0.1:1455/auth/callback",
            None,
        )
        .await
        .expect("the mock grants the code");

        assert!(
            !token.can_refresh(),
            "§5.1 makes refresh_token optional; its absence means no renewal, which this records"
        );
    }

    #[tokio::test]
    async fn classifies_an_exchange_invalid_grant_through_the_terminal_set() {
        let (base, _seen) =
            token_endpoint((StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#)).await;
        let core = crate::ArExec::new().expect("client");
        let session = login_session().with_token_url(format!("{base}/token"));

        let err = exchange_code(
            &core,
            &session,
            "auth-code-1",
            "verifier-1",
            "http://127.0.0.1:1455/auth/callback",
            None,
        )
        .await
        .err();

        assert_eq!(
            err,
            Some(LoginError::ExchangeFailed(RefreshFault::Unrecoverable {
                status: 400,
                reason: "invalid_grant"
            })),
            "a login refusal is the same verdict a refresh refusal is"
        );
    }

    #[tokio::test]
    async fn refuses_to_exchange_a_code_when_the_session_declares_no_token_url() {
        let core = crate::ArExec::new().expect("client");
        // Endpoints are operator-supplied; a session without one has nowhere to
        // post the code, and this build refuses rather than guessing one.
        let session = Session::new("codex", OAuthKind::Codex).with_client_id("client-synthetic");
        let err = exchange_code(
            &core,
            &session,
            "auth-code-1",
            "verifier-1",
            "http://127.0.0.1:1455/auth/callback",
            None,
        )
        .await
        .err();
        assert_eq!(err, Some(LoginError::NoTokenUrl));
    }

    #[test]
    fn converts_a_terminal_exchange_fault_into_the_typed_exec_error() {
        // The point of carrying a RefreshFault rather than a string: a caller
        // that turns a login failure into a response must be able to reach the
        // same visible terminal verdict the refresh path produces.
        let exec: ExecError = LoginError::ExchangeFailed(RefreshFault::Unrecoverable {
            status: 400,
            reason: "invalid_grant",
        })
        .into();
        assert!(
            exec.terminal_report().is_some(),
            "a terminal login keeps its typed report: {exec:?}"
        );
    }

    #[test]
    fn renders_the_absence_variants_as_the_operator_sentence() {
        // The literals, not an interpolation of this file's own format strings:
        // these are what an operator reads when a login cannot start.
        assert_eq!(
            LoginError::NoAuthorizationUrl.to_string(),
            "no authorization endpoint is configured; set authorization_url and client_id"
        );
    }

    #[test]
    fn renders_a_missing_token_endpoint_as_the_operator_sentence() {
        assert_eq!(
            LoginError::NoTokenUrl.to_string(),
            "no token endpoint is configured; set token_url"
        );
    }

    #[test]
    fn renders_a_relative_redirect_as_the_operator_sentence() {
        assert_eq!(
            LoginError::InvalidRedirectUri.to_string(),
            "the redirect uri is not an absolute url"
        );
    }

    #[test]
    fn carries_the_exchange_faults_own_sentence_inside_the_login_sentence() {
        assert_eq!(
            LoginError::ExchangeFailed(RefreshFault::Unrecoverable {
                status: 400,
                reason: "invalid_grant"
            })
            .to_string(),
            "oauth exchange failed: terminal (refresh returned 400: invalid_grant)"
        );
    }

    /// A device-flow session: both endpoints a device provider has to declare,
    /// plus the client id §3.2 sends. No `token_url`, because the provider this
    /// is written for publishes none — reusing it here would demand an endpoint
    /// kilocode does not have.
    fn device_session(base: &str) -> Session {
        Session::new("kilocode", OAuthKind::Codex)
            .with_device_auth_url(format!("{base}/codes"))
            .with_device_poll_url(format!("{base}/poll"))
            .with_client_id("client-synthetic")
    }

    /// A device endpoint serving both halves: a fixed grant body at `POST /codes`
    /// and a queue of replies at `POST /poll`, with the poll count handed back.
    ///
    /// Two routes rather than one queue because the halves have to be told
    /// apart: a device login's whole behaviour is *what happens on the second
    /// and third poll* — it must survive a `pending` and a 500 before it can be
    /// approved — and a single shared queue would spend the first poll's answer
    /// on the initiate. The count is how the retry tests assert the loop kept
    /// asking rather than returning the first failure.
    async fn device_endpoint(
        grant: (StatusCode, &'static str),
        polls: Vec<(StatusCode, &'static str)>,
    ) -> (String, Arc<Mutex<usize>>) {
        let asked: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let counter = Arc::clone(&asked);
        let queue = Arc::new(Mutex::new(polls.into_iter()));
        let slot = Arc::clone(&queue);

        async fn pull(
            counter: Arc<Mutex<usize>>,
            queue: Arc<Mutex<std::vec::IntoIter<(StatusCode, &'static str)>>>,
        ) -> (StatusCode, &'static str) {
            *counter.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            // The last reply repeats, so a test that outlives its queue still gets
            // a deterministic answer rather than a handler panic.
            queue
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .next()
                .unwrap_or((
                    StatusCode::BAD_REQUEST,
                    r#"{"error":"authorization_pending"}"#,
                ))
        }

        let issues = Arc::new((grant.0, grant.1.to_owned()));
        let issued = Arc::clone(&issues);
        let app = axum::Router::new()
            .route(
                "/codes",
                // `Arc` rather than a moved capture: an axum handler must be `Fn`, so it
                // cannot consume what it captured. The `Arc` owns the body because
                // an `async` block may not hand back a borrow of its own capture.
                axum::routing::post(move || {
                    let issued = Arc::clone(&issued);
                    async move {
                        let (status, body) = issued.as_ref();
                        (*status, body.clone())
                    }
                }),
            )
            .route(
                "/poll",
                axum::routing::post(move || {
                    let counter = Arc::clone(&counter);
                    let slot = Arc::clone(&slot);
                    async move { pull(counter, slot).await }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback binds");
        let addr = listener.local_addr().expect("bound socket has an address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), asked)
    }

    /// A §3.2 grant body carrying the literals an RFC 8628 provider sends.
    ///
    /// `interval` of 1 is the smallest value a real poll loop can use, and the
    /// reason: these tests sleep for real (no paused clock in this crate), so
    /// §3.2's 5s default would add five seconds per poll.
    const DEVICE_GRANT_BODY: &str = r#"{"device_code":"dc-secret","user_code":"WXYZ-1234","verification_uri":"https://auth.test/device","expires_in":900,"interval":1}"#;

    /// The same grant with a nonsensical `interval`, for the fallback assertion.
    const DEVICE_GRANT_NO_INTERVAL: &str = r#"{"device_code":"dc-secret","user_code":"WXYZ-1234","verification_uri":"https://auth.test/device","expires_in":900,"interval":0}"#;

    /// A grant whose lifetime has already elapsed, for the deadline assertion.
    const DEVICE_GRANT_EXPIRED: &str = r#"{"device_code":"dc-secret","user_code":"WXYZ-1234","verification_uri":"https://auth.test/device","expires_in":0,"interval":1}"#;

    /// A kilo-shaped initiate body: one `code` serving as *both* the device code
    /// and the user code, camelCase URI and expiry, and no `interval` at all.
    ///
    /// Not RFC 8628: §3.2 names four required fields and this sends none of them
    /// under those names. A single opaque `code` is the whole grant, so the two
    /// halves of the poll credential and the string a human types are one value.
    const DEVICE_GRANT_CAMEL: &str = r#"{"code":"kilo-opaque-code","verificationUrl":"https://kilo.test/device","expiresIn":600}"#;

    /// The same provider's approval: the token under `token`, gated on `status`,
    /// with no RFC §5.1 field name anywhere in it.
    const DEVICE_APPROVAL_CAMEL: &str =
        r#"{"status":"approved","token":"kilo-at","userEmail":"dev@kilo.test"}"#;

    /// The poll queue's answer to remember the exact path it arrived on.
    #[derive(Clone)]
    struct PollSpy {
        paths: Arc<Mutex<Vec<String>>>,
        queue: Arc<Mutex<std::vec::IntoIter<(StatusCode, &'static str)>>>,
    }

    /// One handler for every poll path, recording the raw path it was called at.
    async fn poll_spy(
        axum::extract::State(spy): axum::extract::State<PollSpy>,
        uri: axum::http::Uri,
    ) -> (StatusCode, &'static str) {
        spy.paths
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(uri.path().to_owned());
        spy.queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next()
            .unwrap_or((
                StatusCode::BAD_REQUEST,
                r#"{"error":"authorization_pending"}"#,
            ))
    }

    /// A device endpoint that records the exact path each poll arrived on.
    ///
    /// A provider that addresses the grant by path (`/poll/{code}`) rather than
    /// by body parameter needs the substitution *provable from the request the
    /// server saw* — and a flat poll URL has to stay byte-identical, which is the
    /// same assertion from the other side. One handler behind two routes reads
    /// the raw path rather than a route parameter, so an unsubstituted `{code}`
    /// shows up as itself instead of being invisible to the assertion.
    async fn device_endpoint_recording_paths(
        grant: (StatusCode, &'static str),
        polls: Vec<(StatusCode, &'static str)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let issued = Arc::new((grant.0, grant.1.to_owned()));
        let spy = PollSpy {
            paths: Arc::clone(&paths),
            queue: Arc::new(Mutex::new(polls.into_iter())),
        };

        let issues = Arc::clone(&issued);
        let app = axum::Router::new()
            .route(
                "/codes",
                axum::routing::post(move || {
                    let issued = Arc::clone(&issues);
                    async move {
                        let (status, body) = issued.as_ref();
                        (*status, body.clone())
                    }
                }),
            )
            .route("/poll", axum::routing::post(poll_spy))
            .route("/poll/{code}", axum::routing::post(poll_spy))
            .with_state(spy);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback binds");
        let addr = listener.local_addr().expect("bound socket has an address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), paths)
    }

    #[tokio::test]
    async fn accepts_a_camel_case_grant_whose_single_code_is_both_halves() {
        // REPLAY: a provider that answers one `code` for the device code and the
        // user code, camelCase URI and expiry. §3.2's own field names are absent.
        let (base, _) = device_endpoint((StatusCode::OK, DEVICE_GRANT_CAMEL), vec![]).await;
        let core = crate::ArExec::new().expect("client");

        let (grant, pending) = initiate_device(&core, &device_session(&base))
            .await
            .expect("a grant carrying one code for both halves is still a device grant");

        assert_eq!(
            pending.device_code().expose(),
            "kilo-opaque-code",
            "`code` is the device code, the half the polls carry"
        );
        assert_eq!(
            grant.user_code, "kilo-opaque-code",
            "`code` is also the code a human types"
        );
        assert_eq!(
            grant.verification_uri, "https://kilo.test/device",
            "`verificationUrl` is where it is typed"
        );
        assert_eq!(
            grant.expires_in_secs, 600,
            "`expiresIn` is the grant's own budget"
        );
        assert_eq!(
            grant.interval_secs, 3,
            "this provider's own cadence is 3s, and naming none means it, not the RFC's 5s floor"
        );
    }

    #[tokio::test]
    async fn keeps_polling_when_the_provider_answers_202_with_an_empty_body() {
        // REPLAY: 202 Accepted with nothing in the body. `is_success()` is true for
        // it, so an arm ordered on success hands "" to the token parser and reports
        // a JSON failure instead of waiting for a human.
        let (base, asked) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![
                (StatusCode::ACCEPTED, ""),
                (StatusCode::ACCEPTED, ""),
                (
                    StatusCode::OK,
                    r#"{"access_token":"at-1","expires_in":3600}"#,
                ),
            ],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let token = poll_device(&core, &session, &pending, Duration::from_secs(60))
            .await
            .expect("202 is a loop state, not a verdict");

        assert_eq!(
            token.access().expose(),
            "at-1",
            "the approval after the 202s still lands"
        );
        assert_eq!(
            *asked.lock().unwrap_or_else(|p| p.into_inner()),
            3,
            "both empty 202s were polled again rather than parsed as a token response"
        );
    }

    #[tokio::test]
    async fn treats_a_403_as_a_denial_off_the_status_with_no_body() {
        // REPLAY: 403 with an empty body. The status is the whole signal, and the
        // terminal list's own 403 row is `account_disabled`, so a body read lands
        // on the transient fallthrough and the loop keeps asking a human who
        // already said no.
        let (base, asked) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![(StatusCode::FORBIDDEN, "")],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let err = poll_device(&core, &session, &pending, Duration::from_secs(30))
            .await
            .err();

        assert_eq!(
            err,
            Some(LoginError::LoginDenied("access_denied".to_owned())),
            "403 is the human refusing, and polling cannot change it"
        );
        assert_eq!(
            *asked.lock().unwrap_or_else(|p| p.into_inner()),
            1,
            "a denial ends the loop on the first answer rather than re-asking"
        );
    }

    #[tokio::test]
    async fn treats_a_410_as_an_expiry_off_the_status_with_no_body() {
        // REPLAY: 410 with an empty body. The terminal list's own 410 row is
        // `token_revoked`, which is an account verdict and not this one — the
        // status means the *grant* is over.
        let (base, asked) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![(StatusCode::GONE, "")],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let err = poll_device(&core, &session, &pending, Duration::from_secs(30))
            .await
            .err();

        assert_eq!(
            err,
            Some(LoginError::LoginExpired),
            "410 is the grant being over, which is the login's own expiry"
        );
        assert_eq!(
            *asked.lock().unwrap_or_else(|p| p.into_inner()),
            1,
            "an expiry ends the loop on the first answer"
        );
    }

    #[tokio::test]
    async fn returns_the_token_from_a_camel_case_approval() {
        // REPLAY: 200 `{status, token, userEmail}`. No `access_token` anywhere, so
        // the §5.1 parser names a missing field on a grant that was approved.
        let (base, _) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_CAMEL),
            vec![(StatusCode::OK, DEVICE_APPROVAL_CAMEL)],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let token = poll_device(&core, &session, &pending, Duration::from_secs(30))
            .await
            .expect("the provider approved the device");

        assert_eq!(
            token.access().expose(),
            "kilo-at",
            "`token` is the approved access token"
        );
        assert!(
            !token.can_refresh(),
            "this grant carries no refresh half, so none is invented"
        );
    }

    #[tokio::test]
    async fn substitutes_the_device_code_into_a_templated_poll_url() {
        // REPLAY: a provider that addresses the grant by path. With no `{code}`
        // placeholder in the config the poll has nowhere to put the code, so this
        // is unreachable — the endpoint would be asked with no code in it.
        let (base, paths) = device_endpoint_recording_paths(
            (StatusCode::OK, DEVICE_GRANT_CAMEL),
            vec![(StatusCode::OK, DEVICE_APPROVAL_CAMEL)],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base).with_device_poll_url(format!("{base}/poll/{{code}}"));
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        poll_device(&core, &session, &pending, Duration::from_secs(30))
            .await
            .expect("the approval lands at the path-suffixed poll endpoint");

        assert_eq!(
            *paths.lock().unwrap_or_else(|p| p.into_inner()),
            vec!["/poll/kilo-opaque-code".to_owned()],
            "the device code is substituted into the poll URL's path"
        );
    }

    #[tokio::test]
    async fn keeps_a_flat_poll_url_byte_identical() {
        // The other side of the same assertion: no placeholder, no rewrite. A poll
        // URL with nothing to substitute must reach the provider exactly as typed.
        let (base, paths) = device_endpoint_recording_paths(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![(
                StatusCode::OK,
                r#"{"access_token":"at-1","expires_in":3600}"#,
            )],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base).with_device_poll_url(format!("{base}/poll"));
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        poll_device(&core, &session, &pending, Duration::from_secs(30))
            .await
            .expect("the flat poll endpoint approves");

        assert_eq!(
            *paths.lock().unwrap_or_else(|p| p.into_inner()),
            vec!["/poll".to_owned()],
            "a flat poll URL is polled byte-identically, with nothing appended"
        );
    }

    #[tokio::test]
    async fn parses_the_device_grant_when_the_provider_answers_rfc8628_fields() {
        let (base, _) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![(
                StatusCode::BAD_REQUEST,
                r#"{"error":"authorization_pending"}"#,
            )],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);

        let (grant, pending) = initiate_device(&core, &session)
            .await
            .expect("the mock issues a grant");

        assert_eq!(
            grant.user_code, "WXYZ-1234",
            "§3.2's user_code is the code a human types"
        );
        assert_eq!(
            grant.verification_uri, "https://auth.test/device",
            "§3.2's verification_uri is where it is typed"
        );
        assert_eq!(
            grant.interval_secs, 1,
            "§3.2's interval is the provider's own cadence"
        );
        assert!(
            grant.expires_in_secs > 0,
            "§3.2's expires_in is the grant's budget and must survive parsing"
        );
        assert_eq!(
            pending.interval_secs(),
            grant.interval_secs,
            "the poll waits what the grant says"
        );
    }

    #[tokio::test]
    async fn falls_back_to_the_rfc_default_when_the_provider_names_no_interval() {
        let (base, _) = device_endpoint((StatusCode::OK, DEVICE_GRANT_NO_INTERVAL), vec![]).await;
        let core = crate::ArExec::new().expect("client");

        let (grant, _) = initiate_device(&core, &device_session(&base))
            .await
            .expect("the mock issues a grant");

        // An `interval` of 0 is nonsense from a provider, and §3.2's own default
        // is the answer: the poll must not become a hot loop because one field
        // was garbage.
        assert_eq!(
            grant.interval_secs, DEVICE_DEFAULT_INTERVAL_SECS,
            "a zero interval falls back to the RFC's default, not to a hot loop"
        );
    }

    #[tokio::test]
    async fn prints_no_device_code_from_a_pending_debug() {
        let (base, _) = device_endpoint((StatusCode::OK, DEVICE_GRANT_BODY), vec![]).await;
        let core = crate::ArExec::new().expect("client");

        let (_, pending) = initiate_device(&core, &device_session(&base))
            .await
            .expect("the mock issues a grant");

        assert!(
            !format!("{pending:?}").contains("dc-secret"),
            "the device code is the flow's secret and must not reach a log through Debug"
        );
    }

    #[tokio::test]
    async fn returns_the_token_when_the_device_is_approved() {
        let (base, asked) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![
                (
                    StatusCode::BAD_REQUEST,
                    r#"{"error":"authorization_pending"}"#,
                ),
                (
                    StatusCode::OK,
                    r#"{"access_token":"at-1","token_type":"Bearer","expires_in":3600}"#,
                ),
            ],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let token = poll_device(&core, &session, &pending, Duration::from_secs(30))
            .await
            .expect("the mock approves the device");

        assert_eq!(
            token.access().expose(),
            "at-1",
            "the approved poll returns the access token"
        );
        assert_eq!(
            *asked.lock().unwrap_or_else(|p| p.into_inner()),
            2,
            "an authorization_pending is a loop state, so the login asked again and then succeeded"
        );
    }

    #[tokio::test]
    async fn records_no_refresh_half_when_the_device_grant_returns_no_refresh_token() {
        let (base, _) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![(
                StatusCode::OK,
                r#"{"access_token":"at-1","expires_in":3600}"#,
            )],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let token = poll_device(&core, &session, &pending, Duration::from_secs(30))
            .await
            .expect("approval");

        // kilocode has no refresh grant, so nothing here may invent one: the
        // token records "no renewal" by having no refresh half, and
        // `can_refresh` reports it instead of the first 401 discovering it.
        assert!(
            !token.can_refresh(),
            "a device grant with no refresh_token is not renewable"
        );
    }

    #[tokio::test]
    async fn treats_a_denial_as_terminal_when_the_provider_answers_access_denied() {
        let (base, asked) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![(
                StatusCode::BAD_REQUEST,
                r#"{"error":"access_denied","error_description":"The user declined"}"#,
            )],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let err = poll_device(&core, &session, &pending, Duration::from_secs(30))
            .await
            .err();

        assert_eq!(
            err,
            Some(LoginError::LoginDenied("access_denied".to_owned())),
            "§3.5's denial is the human's verdict and polling cannot change it"
        );
        assert_eq!(
            *asked.lock().unwrap_or_else(|p| p.into_inner()),
            1,
            "a denial ends the loop on the first answer rather than re-asking"
        );
    }

    #[tokio::test]
    async fn treats_an_expiry_as_terminal_when_the_provider_answers_expired_token() {
        let (base, asked) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![(StatusCode::BAD_REQUEST, r#"{"error":"expired_token"}"#)],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let err = poll_device(&core, &session, &pending, Duration::from_secs(30))
            .await
            .err();

        assert_eq!(
            err,
            Some(LoginError::LoginExpired),
            "§3.5's expired_token is the grant being over, which reuses the login's own expiry"
        );
        assert_eq!(
            *asked.lock().unwrap_or_else(|p| p.into_inner()),
            1,
            "an expiry ends the loop on the first answer"
        );
    }

    #[tokio::test]
    async fn keeps_polling_when_the_provider_answers_a_transient_500() {
        let (base, asked) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    r#"{"error":"server_error"}"#,
                ),
                (StatusCode::SERVICE_UNAVAILABLE, "gateway busy"),
                (
                    StatusCode::OK,
                    r#"{"access_token":"at-1","expires_in":3600}"#,
                ),
            ],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let token = poll_device(&core, &session, &pending, Duration::from_secs(60))
            .await
            .expect("a transient failure is not a verdict, so the loop keeps asking");

        assert_eq!(
            token.access().expose(),
            "at-1",
            "the eventual approval still lands"
        );
        assert_eq!(
            *asked.lock().unwrap_or_else(|p| p.into_inner()),
            3,
            "both transient failures were retried rather than returned as the login's outcome"
        );
    }

    #[tokio::test]
    async fn refuses_to_initiate_when_the_session_declares_no_device_auth_url() {
        let core = crate::ArExec::new().expect("client");
        let session = Session::new("kilocode", OAuthKind::Codex);

        let err = initiate_device(&core, &session).await.err();

        assert_eq!(
            err,
            Some(LoginError::NoDeviceAuthUrl),
            "an absent endpoint is named, never inferred from the provider id"
        );
    }

    #[tokio::test]
    async fn refuses_to_poll_when_the_session_declares_no_device_poll_url() {
        let (base, _) = device_endpoint((StatusCode::OK, DEVICE_GRANT_BODY), vec![]).await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let err = poll_device(
            &core,
            &Session::new("kilocode", OAuthKind::Codex),
            &pending,
            Duration::from_secs(30),
        )
        .await
        .err();

        assert_eq!(
            err,
            Some(LoginError::NoDevicePollUrl),
            "an absent poll endpoint is named, never inferred"
        );
    }

    #[tokio::test]
    async fn refuses_to_initiate_when_the_device_response_omits_the_device_code() {
        let (base, _) = device_endpoint(
            (
                StatusCode::OK,
                r#"{"user_code":"WXYZ-1234","verification_uri":"https://auth.test/device","expires_in":900}"#,
            ),
            vec![],
        )
        .await;
        let core = crate::ArExec::new().expect("client");

        let err = initiate_device(&core, &device_session(&base)).await.err();

        // Transient, not terminal: the endpoint answered, it just answered
        // incompletely. A half-grant must never read as a verdict on the account.
        assert_eq!(
            err,
            Some(LoginError::ExchangeFailed(RefreshFault::Transient(
                "device-response-has-no-device-code"
            ))),
            "a grant with no device_code cannot be polled, so the user_code would be a string typed for nothing"
        );
    }

    #[tokio::test]
    async fn refuses_to_initiate_when_the_session_declares_a_relative_device_auth_url() {
        let core = crate::ArExec::new().expect("client");
        let session = Session::new("kilocode", OAuthKind::Codex)
            .with_device_auth_url("/api/device-auth/codes");

        let err = initiate_device(&core, &session).await.err();

        assert_eq!(
            err,
            Some(LoginError::NoDeviceAuthUrl),
            "a relative endpoint would send the request nowhere, so it is named rather than posted"
        );
    }

    #[tokio::test]
    async fn stops_polling_when_the_grants_own_lifetime_passes() {
        // `expires_in` of 0 means the grant is already over: the loop must end on
        // its own budget rather than sleeping out a human who never arrives.
        let (base, asked) = device_endpoint((StatusCode::OK, DEVICE_GRANT_EXPIRED), vec![]).await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let err = poll_device(&core, &session, &pending, Duration::from_secs(60))
            .await
            .err();

        assert_eq!(
            err,
            Some(LoginError::LoginExpired),
            "an elapsed grant is an expiry"
        );
        assert_eq!(
            *asked.lock().unwrap_or_else(|p| p.into_inner()),
            0,
            "the grant's own budget is checked before the first poll, so nothing is asked"
        );
    }

    #[tokio::test]
    async fn stops_polling_when_the_callers_own_budget_passes() {
        // The caller caps its own wait below the grant's lifetime; the login has
        // to honour the tighter of the two rather than outliving its budget.
        let (base, _) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![(
                StatusCode::BAD_REQUEST,
                r#"{"error":"authorization_pending"}"#,
            )],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let err = poll_device(&core, &session, &pending, Duration::ZERO)
            .await
            .err();

        assert_eq!(
            err,
            Some(LoginError::LoginExpired),
            "a zero budget ends the loop at once"
        );
    }

    #[tokio::test]
    async fn widens_the_poll_interval_when_the_provider_answers_slow_down() {
        let (base, _) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![
                (StatusCode::BAD_REQUEST, r#"{"error":"slow_down"}"#),
                (
                    StatusCode::OK,
                    r#"{"access_token":"at-1","expires_in":3600}"#,
                ),
            ],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let token = poll_device(&core, &session, &pending, Duration::from_secs(60))
            .await
            .expect("slow_down is a loop state, not a failure");

        assert_eq!(
            token.access().expose(),
            "at-1",
            "§3.5's slow_down delays the next ask rather than ending the login"
        );
    }

    #[tokio::test]
    async fn ends_the_poll_when_the_provider_answers_a_terminal_refresh_row() {
        // The classifier is shared with the refresh path, so a terminal status it
        // knows ends a device login too — as the same typed fault, not as prose.
        let (base, _) = device_endpoint(
            (StatusCode::OK, DEVICE_GRANT_BODY),
            vec![(StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#)],
        )
        .await;
        let core = crate::ArExec::new().expect("client");
        let session = device_session(&base);
        let (_, pending) = initiate_device(&core, &session).await.expect("grant");

        let err = poll_device(&core, &session, &pending, Duration::from_secs(30))
            .await
            .err();

        assert_eq!(
            err,
            Some(LoginError::ExchangeFailed(RefreshFault::Unrecoverable {
                status: 400,
                reason: "invalid_grant"
            })),
            "one terminal-status list governs the poll as well as the refresh"
        );
    }

    #[test]
    fn renders_the_device_endpoint_absence_variants_as_the_operator_sentence() {
        assert_eq!(
            LoginError::NoDeviceAuthUrl.to_string(),
            "no device authorization endpoint is configured; set device_auth_url"
        );
        assert_eq!(
            LoginError::NoDevicePollUrl.to_string(),
            "no device poll endpoint is configured; set device_poll_url"
        );
    }

    /// A token endpoint that answers each request from a scripted queue and
    /// records how many it was asked.
    ///
    /// Distinct from [`token_endpoint`], which answers every request the same way:
    /// retry and breaker tests need the *sequence* of verdicts, and a count that
    /// proves how many attempts were actually spent.
    async fn scripted_endpoint(
        script: Vec<(StatusCode, &'static str)>,
    ) -> (String, Arc<Mutex<usize>>) {
        let asked: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let counter = Arc::clone(&asked);
        let queue = Arc::new(Mutex::new(script.into_iter()));
        let slot = Arc::clone(&queue);
        let app = axum::Router::new().fallback(move || {
            let counter = Arc::clone(&counter);
            let queue = Arc::clone(&slot);
            async move {
                *counter.lock().unwrap_or_else(|p| p.into_inner()) += 1;
                queue
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .next()
                    .unwrap_or((StatusCode::BAD_REQUEST, r#"{"error":"server_error"}"#))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback binds");
        let addr = listener.local_addr().expect("bound socket has an address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), asked)
    }

    fn asked_how_often(asked: &Arc<Mutex<usize>>) -> usize {
        *asked.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[tokio::test]
    async fn grants_the_token_when_a_transient_503_is_followed_by_success() {
        // The retry loop's whole claim: two 503s are one problem, not three, and
        // the caller sees a granted token rather than a fault. A 503 is the
        // canonical transient — no verdict, nothing learned about the account.
        let (base, asked) = scripted_endpoint(vec![
            (StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"upstream"}"#),
            (StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"upstream"}"#),
            (
                StatusCode::OK,
                r#"{"access_token":"at-granted","expires_in":3600}"#,
            ),
        ])
        .await;
        let session =
            Session::new("codex", OAuthKind::Codex).with_token_url(format!("{base}/token"));
        let refresher = HttpRefresher::new(reqwest::Client::new());

        let token = refresher
            .refresh(&session, &expired_token().with_refresh(Secret::new("rt-1")))
            .await
            .expect("the third attempt grants");

        assert_eq!(token.access().expose(), "at-granted");
        assert_eq!(
            asked_how_often(&asked),
            REFRESH_MAX_ATTEMPTS,
            "every attempt was spent"
        );
    }

    #[tokio::test]
    async fn spends_one_attempt_when_the_refresh_grant_is_invalid() {
        // The short-circuit. `invalid_grant` is terminal: the refresh token is
        // spent or revoked, and a second attempt would spend a second one to learn
        // the same thing. Three attempts here would be three refresh-token uses
        // against a provider that has already said no.
        let (base, asked) = scripted_endpoint(vec![(
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_grant"}"#,
        )])
        .await;
        let session =
            Session::new("codex", OAuthKind::Codex).with_token_url(format!("{base}/token"));
        let refresher = HttpRefresher::new(reqwest::Client::new());

        let fault = refresher
            .refresh(&session, &expired_token().with_refresh(Secret::new("rt-1")))
            .await
            .expect_err("invalid_grant is terminal");

        assert_eq!(
            fault,
            RefreshFault::Unrecoverable {
                status: 400,
                reason: "invalid_grant"
            }
        );
        assert_eq!(
            asked_how_often(&asked),
            1,
            "a terminal verdict is not retried"
        );
    }

    #[tokio::test]
    async fn opens_the_breaker_after_five_consecutive_refresh_failures() {
        // The threshold, proven from the outside: five failing refreshes in, and
        // the sixth never reaches the endpoint at all.
        let (base, asked) = scripted_endpoint(Vec::new()).await;
        let session =
            Session::new("codex", OAuthKind::Codex).with_token_url(format!("{base}/token"));
        let refresher = HttpRefresher::new(reqwest::Client::new());
        let token = expired_token().with_refresh(Secret::new("rt-1"));

        for _ in 0..REFRESH_BREAKER_THRESHOLD {
            assert!(
                refresher.refresh(&session, &token).await.is_err(),
                "a queued 503 fails every attempt"
            );
        }
        let spent = asked_how_often(&asked);

        let fault = refresher
            .refresh(&session, &token)
            .await
            .expect_err("the breaker is open");

        assert_eq!(fault, RefreshFault::Transient("refresh-breaker-open"));
        assert_eq!(
            asked_how_often(&asked),
            spent,
            "the endpoint was not asked again"
        );
    }

    #[tokio::test]
    async fn clears_the_breaker_when_a_refresh_succeeds() {
        // One success resets the counter, so a provider that recovers mid-window
        // is picked up rather than left blacked out. Asserted on the counter
        // rather than only on `is_blocked`, because the reset is what makes the
        // next five failures start from zero.
        let breaker = RefreshBreaker::new();
        for _ in 0..REFRESH_BREAKER_THRESHOLD {
            breaker.record_failure("codex", 1_000);
        }
        breaker.record_success("codex");
        assert_eq!(breaker.failures("codex"), 0);
    }

    #[tokio::test]
    async fn reopens_the_breaker_once_the_cooldown_has_elapsed() {
        let breaker = RefreshBreaker::new();
        for _ in 0..REFRESH_BREAKER_THRESHOLD {
            breaker.record_failure("codex", 1_000);
        }
        assert!(breaker.is_blocked("codex", 1_000));
        assert!(
            !breaker.is_blocked("codex", 1_000 + super::REFRESH_BREAKER_COOLDOWN_SECS + 1),
            "the cooldown is finite, not a permanent retirement"
        );
    }

    #[tokio::test]
    async fn keeps_a_session_with_more_than_the_lead_on_its_current_token() {
        // The lead's outside edge. A codex session with six minutes left must
        // *not* refresh: refreshing now spends a refresh-token use, and codex
        // invalidates its siblings' refresh tokens when one rotates. This is the
        // whole reason the lead exists.
        let refresher = Arc::new(Counting::ok("synthetic-access"));
        let conn = Connection::pending(
            Session::new("codex", OAuthKind::Codex).with_token_url("https://auth.test/token"),
            Arc::new(RotationPool::new()),
            refresher.clone() as Arc<dyn Refresher>,
        )
        .connect(
            OAuthToken::new(Secret::new("synthetic-access"))
                .with_refresh(Secret::new("synthetic-refresh"))
                .with_expiry(1_000 + REFRESH_LEAD_SECS + 60),
        )
        .expect("an access token connects");

        assert!(conn.grant_at(Origin::Client, 1_000).await.is_ok());
        assert_eq!(
            refresher.calls(),
            0,
            "six minutes left is beyond the five-minute lead"
        );
    }

    #[tokio::test]
    async fn refreshes_a_session_inside_the_lead() {
        // The inside edge, at four minutes. A 5min lead means four minutes *is*
        // the window — the token cannot be allowed to expire during the round trip
        // it is about to be sent on. So this one refreshes, which is what makes the
        // previous test meaningful: the two differ only by the lead.
        let refresher = Arc::new(Counting::ok("synthetic-access"));
        let conn = Connection::pending(
            Session::new("codex", OAuthKind::Codex).with_token_url("https://auth.test/token"),
            Arc::new(RotationPool::new()),
            refresher.clone() as Arc<dyn Refresher>,
        )
        .connect(
            OAuthToken::new(Secret::new("synthetic-access"))
                .with_refresh(Secret::new("synthetic-refresh"))
                .with_expiry(1_000 + 240),
        )
        .expect("an access token connects");

        assert!(conn.grant_at(Origin::Client, 1_000).await.is_ok());
        assert_eq!(
            refresher.calls(),
            1,
            "four minutes left is inside the five-minute lead"
        );
    }

    #[test]
    fn honours_a_per_session_lead_over_the_per_kind_default() {
        // The override is the reason the lead is a table *and* a field: one
        // account can be tuned without changing every other session of the kind.
        let tuned = Session::new("codex", OAuthKind::Codex).with_refresh_lead_secs(60);
        assert_eq!(tuned.refresh_lead_secs(), 60);
        assert_eq!(
            Session::new("codex", OAuthKind::Codex).refresh_lead_secs(),
            REFRESH_LEAD_SECS
        );
    }

    #[test]
    fn gives_a_non_rotating_provider_the_longer_lead() {
        // Google refresh tokens are permanent, so an early refresh buys no safety
        // and only adds chatter. Every other kind rotates and waits.
        assert_eq!(
            OAuthKind::GeminiCli.refresh_lead_secs(),
            NON_ROTATING_REFRESH_LEAD_SECS
        );
        assert_eq!(OAuthKind::Codex.refresh_lead_secs(), REFRESH_LEAD_SECS);
    }

    #[tokio::test]
    async fn faults_within_the_bound_when_the_refresh_lock_is_wedged() {
        // The wedge. A refresher that never settles holds `refresh_lock` for ever,
        // so every waiter would be parked. Two tasks, one that wedges and one that
        // arrives after it: the second must get a fault back, not silence.
        struct Wedged;
        impl Refresher for Wedged {
            fn refresh<'a>(
                &'a self,
                _session: &'a Session,
                _current: &'a OAuthToken,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<OAuthToken, RefreshFault>> + Send + 'a>,
            > {
                Box::pin(std::future::pending())
            }
        }

        let conn = Arc::new(
            Connection::pending(
                Session::new("codex", OAuthKind::Codex).with_token_url("https://auth.test/token"),
                Arc::new(RotationPool::new()),
                Arc::new(Wedged) as Arc<dyn Refresher>,
            )
            .with_refresh_lock_bound(Duration::from_millis(50))
            .connect(
                OAuthToken::new(Secret::new("synthetic-access"))
                    .with_refresh(Secret::new("synthetic-refresh")),
            )
            .expect("an access token connects"),
        );

        let holder = Arc::clone(&conn);
        tokio::spawn(async move { holder.rotate(token_hash("synthetic-access")).await });

        // Let the holder take the lock before the waiter tries.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let fault = conn
            .rotate(token_hash("synthetic-access"))
            .await
            .expect_err("the lock is wedged");

        assert_eq!(fault, RefreshFault::Transient("refresh-lock-wedged"));
    }

    #[tokio::test]
    async fn yields_a_non_stale_token_when_the_response_omits_expires_in() {
        // `expires_in` is optional in §5.1. Treating its absence as "expires now"
        // would mint a token that is already stale, so every dispatch would decide
        // it had to refresh — one wasted refresh-token use per request, forever.
        let (base, _) =
            token_endpoint((StatusCode::OK, r#"{"access_token":"at-no-expiry"}"#)).await;
        let session =
            Session::new("grok-cli", OAuthKind::GrokCli).with_token_url(format!("{base}/token"));
        let refresher = HttpRefresher::new(reqwest::Client::new());

        let token = refresher
            .refresh(
                &session,
                &expired_token().with_refresh(Secret::new("rt-grok-1")),
            )
            .await
            .expect("the mock grants the refresh");

        assert!(
            !token.is_expiring_within(unix_now(), REFRESH_LEAD_SECS),
            "a token with no declared expiry must not be born stale"
        );
    }

    #[test]
    fn falls_back_to_the_grok_lifetime_when_expires_in_is_absent() {
        // The value itself, not just the behaviour: six hours is what Grok Build
        // actually issues, so a fallback below that would over-refresh and one far
        // above it would 401 in production.
        let parsed = serde_json::json!({"access_token": "at"});
        assert_eq!(expiry_at(&parsed, 1_000), 1_000 + EXPIRES_IN_FALLBACK_SECS);
    }

    /// One recorded `persist_rotation` call: provider, token exchanged, token
    /// received. The sink's whole input, and therefore the guard's.
    type RotationWrite = (String, Option<String>, String);

    /// A sink that records what it was handed and reports a scripted verdict.
    ///
    /// The sink shares its log with the test rather than owning it, because
    /// [`Connection::with_sink`] takes ownership and the assertion has to read
    /// through something the test still holds.
    struct RecordingSink {
        writes: Arc<Mutex<Vec<RotationWrite>>>,
        persisted: bool,
    }

    impl RotationSink for RecordingSink {
        fn persist_rotation(
            &self,
            provider: &str,
            presented: Option<&str>,
            renewed: &OAuthToken,
        ) -> bool {
            self.writes.lock().unwrap_or_else(|p| p.into_inner()).push((
                provider.to_owned(),
                presented.map(str::to_owned),
                renewed.access().expose().to_owned(),
            ));
            self.persisted
        }
    }

    /// A sink and the log it writes to: one closure, so a test cannot wire the
    /// assertion to a different sink than the connection holds.
    fn recording_sink(persisted: bool) -> (Arc<Mutex<Vec<RotationWrite>>>, RecordingSink) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        (Arc::clone(&writes), RecordingSink { writes, persisted })
    }

    #[tokio::test]
    async fn persists_a_rotation_through_the_sink() {
        // The rotation is only useful if it outlives the connection that made it:
        // without this write the next process reloads the refresh token this one
        // just spent. The tuple asserted is the sink's whole input — provider,
        // token exchanged, token received — so it is also the guard's.
        let (writes, sink) = recording_sink(true);
        let conn = Connection::pending(
            session(),
            Arc::new(RotationPool::new()),
            Arc::new(Counting::ok("rotated")),
        )
        .with_sink(Box::new(sink))
        .connect(expired_token().with_refresh(Secret::new("rt-1")))
        .expect("an access token connects");

        conn.rotate(token_hash("synthetic-old-access"))
            .await
            .expect("the refresh succeeds");

        assert_eq!(
            *writes.lock().unwrap_or_else(|p| p.into_inner()),
            vec![(
                "cline".to_owned(),
                Some("rt-1".to_owned()),
                "rotated-0".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn hands_the_token_to_the_request_when_a_concurrent_writer_rotated_first() {
        // The CAS outcome. A skip is *not* an error for the request: upstream just
        // accepted this token, so the caller must still get it. Only the stored
        // state is in question, and it is already fresher.
        let (_, sink) = recording_sink(false);
        let conn = Connection::pending(
            session(),
            Arc::new(RotationPool::new()),
            Arc::new(Counting::ok("rotated")),
        )
        .with_sink(Box::new(sink))
        .connect(expired_token().with_refresh(Secret::new("rt-1")))
        .expect("an access token connects");

        let grant = conn
            .rotate(token_hash("synthetic-old-access"))
            .await
            .expect("the request still succeeds");

        assert_eq!(grant.token().expose(), "rotated-0");
    }

    #[tokio::test]
    async fn refreshes_in_memory_when_no_sink_is_wired() {
        // A deployment with no credential store is the supported `$VAR`-only
        // install, and its rotations must keep working exactly as they did.
        let refresher = Arc::new(Counting::ok("rotated"));
        let conn = Connection::pending(
            session(),
            Arc::new(RotationPool::new()),
            refresher.clone() as Arc<dyn Refresher>,
        )
        .connect(expired_token().with_refresh(Secret::new("rt-1")))
        .expect("an access token connects");

        assert!(
            conn.rotate(token_hash("synthetic-old-access"))
                .await
                .is_ok()
        );
        assert_eq!(refresher.calls(), 1);
    }
}
