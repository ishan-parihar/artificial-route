//! Upstream dispatch for Artificial Route.
//!
//! One executor, one job: take a canonical chat, POST it to a provider, and hand
//! back a decoded SSE stream. Ports OmniRoute's `executors/default.ts` +
//! `base/{headers,mergeAbortSignals}` + `default/urlNormalizers` and drops the
//! other 158 executors (bedrock SigV4, vertex, claude-web, codex OAuth) as
//! `docs/02` directs.
//!
//! # Why `reqwest` and not `ar-pool`
//!
//! `docs/03` assigns `ar-pool` (a vendored hyper client) to the HTTP server
//! side. Adding it here too would be a second connection pool for the same
//! process, and `reqwest` already pools, negotiates H2 via ALPN and speaks
//! rustls. One pool per role.
//!
//! # Cancellation
//!
//! OmniRoute merges a caller abort signal with a response-start timeout
//! (`base/mergeAbortSignals.ts`). The port does the same job with an inline
//! `tokio::select!` instead of a merged `AbortSignal`: the two selects are
//! short-lived, so there is no listener to leak and no spawned watcher task to
//! keep alive after the response arrives. Aborting during the body phase drops
//! the `reqwest::Response`, which closes or releases the connection.
//!
//! # OAuth
//!
//! [`oauth`] adds the token lifecycle on top of this core: token injection,
//! refresh on expiry, one rotation retry on 401, and a per-connection mutex so a
//! concurrent burst cannot present the same refresh token twice. It is a P5 item
//! per `docs/02`.
//!
//! The browser-login half of [`oauth`] *obtains* the access token the connection
//! executor consumes: PKCE authorization-code, with the redirect read out of the
//! address bar by a person (or an MCP host) and pasted back. No browser is
//! driven, and no endpoint is guessed.
//!
//! ```
//! # use ar_config::Secret;
//! # use ar_exec::ArExec;
//! # use ar_registry::{AuthClass, ProviderDef, WireFormat};
//! # use ar_translate::CanonicalChat;
//! # use tokio_util::sync::CancellationToken;
//! let exec = ArExec::new().unwrap();
//! let provider = ProviderDef {
//!     base_url: "https://api.openai.com/v1".into(),
//!     wire_format: WireFormat::Openai,
//!     auth: AuthClass::ApiKey,
//!     env_hint: "OPENAI_API_KEY".into(),
//!     models: vec![],
//!     prices: Default::default(),
//!     executor: "default".into(),
//!     auth_kind: "api_key".into(),
//!     flat_rate: false,
//!     headers: Default::default(),
//! };
//! let chat = CanonicalChat {
//!     model: "gpt-5.4".into(),
//!     messages: vec![],
//!     temperature: None,
//!     max_tokens: None,
//!     stream: true,
//! };
//! let abort = CancellationToken::new();
//! // Not awaited: a doctest must not perform network I/O.
//! let _ = exec.post_chat(&chat, &provider, &Secret::new("sk-x".into()), &abort);
//! ```
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use ar_config::Secret;
use ar_registry::{AuthClass, ProviderDef, WireFormat};
use ar_translate::CanonicalChat;
use async_stream::stream;
use bytes::Bytes;
use futures::Stream;
use reqwest::StatusCode;
use tokio_util::sync::CancellationToken;

use crate::oauth::TerminalReport;
use crate::sse::{DEFAULT_FRAME_CAP, SseDecoder};
use crate::url::chat_url;

pub use crate::media::{MediaBody, MediaEndpoint, MediaResponse};

pub mod media;
/// The OAuth taxonomy, its single terminal-status list, and the
/// grant-and-rotate executor. `Origin::Client` is the only origin a live
/// dispatch uses; `Origin::Probe` exists so a future health path cannot renew a
/// rotating token by accident (audit R4).
///
/// `DOCS` note: OAuth is a P5 item in `docs/02-port-from-omniroute.md` ("skip …
/// codex OAuth until P5"), and browser-session login stays out of scope
/// entirely — this module consumes an already-obtained access token.
pub mod oauth;
pub mod sse;
pub mod url;

// The OAuth surface `ar-server` composes: the executor, the connection TypeState
// and its refresh seam, plus the taxonomy `ar doctor` reports from. Re-exported
// because every consumer needs more than one of these, and a path-per-item import
// list is a second place to forget an item.
pub use crate::oauth::{
    AuthorizeRequest, CallbackListener, Connected, Connection, HttpRefresher, LoginError, OAuthKind,
    OAuthToken, Origin, Refresher, RotationPool, Session, TERMINAL_REFRESH_STATUS, Unconnected,
    authorize_url, exchange_code, new_authorize_request, parse_callback_url,
    terminal_check_constraint,
};

// `into_sse` yields `SseEvent`, and `ExecError::Sse` wraps `SseError`, so both
// belong on the crate root too — not only under `ar_exec::sse`.
pub use crate::sse::{SseError, SseEvent};

/// OmniRoute's `FETCH_TIMEOUT_MS` default.
const BASE_START_TIMEOUT: Duration = Duration::from_secs(600);

/// OmniRoute's `DEFAULT_FETCH_START_TIMEOUT_CAP_MS`: a streaming request may not
/// take longer than this to produce its first byte, even when the provider
/// configures a longer budget. Providers that buffer whole generations behind a
/// gateway override it (`fetchStartTimeoutCapMs` in the TS registry).
const STREAM_START_TIMEOUT_CAP: Duration = Duration::from_secs(110);

/// Ceiling on how much of a non-2xx body is read before giving up.
///
/// An upstream error page can be megabytes of HTML; 8 KiB carries a JSON error
/// and keeps the failure path inside the RAM budget.
const ERROR_BODY_CAP: usize = 8 * 1024;

/// Longest error message surfaced to the client, matching OmniRoute's
/// 200-character truncation (`nonStreamingProviderLeg.ts`).
const ERROR_MESSAGE_CHARS: usize = 200;

/// Ceiling on an honoured `Retry-After`, matching OmniRoute's `MAX_RETRY_MS`.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Ceiling on how much of a *failure* body a caller that wants to classify it
/// gets back from [`ChatStream::into_failure`].
///
/// Distinct from the 200-character message cap below, which bounds what this
/// crate renders for a caller that only wants a human-readable reason. A router
/// has to *scan* the body (OpenAI 400 stop rows live in the first 2 KiB), so it
/// needs more than a summary and needs a bound of its own.
pub const FAILURE_BODY_CAP: usize = 64 * 1024;

/// Compile-time ceiling on the state one in-flight stream keeps.
///
/// `docs/03` asks for an `AssertSize<4K>`-style budget so per-request memory is
/// provable rather than hoped for. It is applied here to the *concrete* types on
/// the request path -- see [`STREAM_STATE_ASSERTS`] for why that distinction
/// matters.
const STREAM_STATE_CAP: usize = 4 * 1024;

/// Enforced compile-time bounds on the per-request state.
///
/// Only concrete types can be asserted: an `async fn`'s future type is
/// unnameable, and both candidate spellings of the generic guard are silently
/// unchecked on this toolchain (`const { assert!(size_of::<T>() <= MAX) }` in a
/// generic body never trips, and the `[(); MAX] = [(); size_of::<T>()]` array
/// trick fails unconditionally with "constant expression depends on a generic
/// parameter"). Asserting the named state is the part that can actually be
/// proved, and it is the part that shows up in a heap profile.
///
/// Measured: `ArExec` 8B, `ChatStream` 160B, `ExecError` 48B, `HeaderMap` 96B.
/// The real per-stream cost is the SSE buffer, which [`sse::DEFAULT_FRAME_CAP`]
/// bounds rather than this.
const _: () = {
    assert!(
        size_of::<ArExec>() <= STREAM_STATE_CAP,
        "ArExec exceeds the per-stream state budget"
    );
    assert!(
        size_of::<ChatStream>() <= STREAM_STATE_CAP,
        "ChatStream exceeds the per-stream state budget"
    );
    assert!(
        size_of::<ExecError>() <= STREAM_STATE_CAP,
        "ExecError exceeds the per-stream state budget"
    );
    assert!(
        size_of::<reqwest::header::HeaderMap>() <= STREAM_STATE_CAP,
        "request headers exceed the per-stream state budget"
    );
};

/// Why an upstream call failed.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The provider speaks a dialect this build cannot post a canonical
    /// OpenAI body to.
    #[error("provider wire format {0:?} is not implemented in P0")]
    UnsupportedWire(WireFormat),
    /// The caller aborted before the response headers arrived.
    #[error("upstream call aborted by caller")]
    Aborted,
    /// No first byte within the start budget.
    #[error("upstream produced no headers within {0:?}")]
    StartTimeout(Duration),
    /// The upstream replied with a non-2xx status.
    #[error("upstream {status}: {message}")]
    Upstream {
        /// HTTP status the upstream returned.
        status: u16,
        /// Best-effort message extracted from the body.
        message: String,
        /// `Retry-After` as the upstream expressed it, when present and valid.
        retry_after: Option<Duration>,
    },
    /// The transport failed (DNS, TLS, connect, reset).
    #[error("upstream transport failure: {0}")]
    Transport(String),
    /// The media guard rejected the request before any dispatch.
    ///
    /// The message is flattened because the guard's own wording is the useful
    /// one and the variant exists only to reach the `?` operator; the typed
    /// error stays `ar_translate::MediaError` at the guard.
    #[error("{0}")]
    Media(String),
    /// The SSE stream could not be decoded.
    #[error(transparent)]
    Sse(#[from] SseError),
    /// The canonical request could not be encoded.
    #[error("cannot encode canonical request: {0}")]
    Encode(#[from] serde_json::Error),
    /// The canonical body parsed but has no top-level `"model"` to rewrite,
    /// because it is not a JSON object.
    #[error("canonical body is not a JSON object")]
    NotAnObject,
    /// An OAuth session reached a terminal state: the refresh token is gone,
    /// revoked, or the provider says the account is closed.
    ///
    /// A distinct variant rather than [`Self::Upstream`] because the status is
    /// the *refresh endpoint's*, not the dispatch's, and because `ar-server` has
    /// to convert it into a 401 carrying [`TerminalReport::client_body`] — a
    /// visible terminal for the account, where the transport path would report a
    /// bare 502 and send the operator to the wrong dashboard (R2).
    #[error("oauth session is terminal: {0}")]
    OAuthTerminal(TerminalReport),
}

/// Pooled upstream client.
///
/// Cheap to share: one `reqwest::Client` holds the connection pool, which is the
/// entire reason to hold this value at all. Constructing one per request would
/// defeat pooling.
#[derive(Debug, Clone)]
pub struct ArExec {
    client: reqwest::Client,
}

impl ArExec {
    /// Builds a client with pooling and HTTP/2 enabled.
    ///
    /// Pool sizing is deliberately left at reqwest's defaults rather than
    /// tuned: `docs/00` budgets one concurrent stream, and the 300+ providers
    /// disagree about a useful idle pool. Revisit under measured load.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Transport`] if the TLS backend fails to initialise.
    pub fn new() -> Result<Self, ExecError> {
        let client = reqwest::Client::builder()
            .http2_adaptive_window(true)
            .pool_max_idle_per_host(8)
            .build()
            .map_err(|e| ExecError::Transport(e.to_string()))?;
        Ok(Self { client })
    }

    /// The pooled client, for a component that has to reuse the *same* pool.
    ///
    /// [`crate::oauth::HttpRefresher`] takes one rather than building its own:
    /// the module docs above make "one pool per role" the rule, and a second
    /// `reqwest::Client` for token refreshes would be a second pool for the same
    /// process. Cloning is cheap — the client is an `Arc` over its pool.
    #[must_use]
    pub fn client(&self) -> reqwest::Client {
        self.client.clone()
    }

    /// POSTs `chat` to `provider` and returns the response stream.
    ///
    /// `abort` propagates to both phases: while waiting for headers, and again
    /// while the body is read by [`ChatStream::into_sse`].
    ///
    /// # Errors
    ///
    /// [`ExecError::UnsupportedWire`] for a non-OpenAI provider, plus transport,
    /// abort, timeout and non-2xx failures. A non-2xx reply is an error rather
    /// than a stream, because its body is a JSON error object, not SSE.
    pub async fn post_chat(
        &self,
        chat: &CanonicalChat,
        provider: &ProviderDef,
        api_key: &Secret,
        abort: &CancellationToken,
    ) -> Result<ChatStream, ExecError> {
        let dispatch = Dispatch {
            base_url: &provider.base_url,
            wire_format: provider.wire_format,
            api_key: api_key.expose(),
            upstream_model: &chat.model,
            stream: chat.stream,
            headers: &provider.headers,
        };
        let encoded = serde_json::to_vec(chat)?;
        let start = self.post(&dispatch, &encoded, abort).await?;
        if !start.status().is_success() {
            let retry_after = start.retry_after();
            return Err(upstream_error(start.into_response(), retry_after).await);
        }
        Ok(start)
    }

    /// The one execution core: gate the wire format, merge headers, POST, and
    /// hand back the live response whatever its status.
    ///
    /// Both consumers go through here — [`ArExec::post_chat`], which folds a
    /// non-2xx into [`ExecError::Upstream`], and `ar-server`'s `HttpExec`, which
    /// needs the raw status and body so its router can classify the verdict.
    /// Neither owns a second copy of the header merge, the start budget, the
    /// abort race or the model rewrite.
    ///
    /// `canonical` is already-canonical JSON *bytes*. `ar-server`'s router speaks
    /// `ar_route::CanonicalRequest`, whose body is serialised, so accepting bytes
    /// is what lets it use this core without a lossy parse through
    /// [`CanonicalChat`] first.
    ///
    /// # Errors
    ///
    /// [`ExecError::UnsupportedWire`] for a provider this build cannot speak,
    /// plus transport, abort and start-timeout failures. **Not** for a non-2xx:
    /// an HTTP status is a verdict the caller has to see, not an executor
    /// failure.
    pub async fn post(
        &self,
        d: &Dispatch<'_>,
        canonical: &[u8],
        abort: &CancellationToken,
    ) -> Result<ChatStream, ExecError> {
        if d.wire_format != WireFormat::Openai {
            return Err(ExecError::UnsupportedWire(d.wire_format));
        }

        let body = rewrite_model(canonical, d.upstream_model)?;
        let start = await_start(
            self.client
                .post(chat_url(d.base_url))
                .headers(build_headers(d.api_key, d.stream, d.headers))
                .body(body),
            abort,
            start_timeout(d.stream),
        )
        .await?;

        Ok(ChatStream {
            retry_after: retry_after_of(start.headers()),
            response: start,
            abort: abort.clone(),
        })
    }
}

/// Everything one dispatch needs, borrowed from the caller's provider config.
///
/// `Copy`: an argument bundle, not a second source of truth. The provider's
/// routing signals (rank, price, weight, quota) stay in the caller's config
/// because this crate never reads them, and a field here that nothing reads is a
/// field that drifts.
#[derive(Clone, Copy, Debug)]
pub struct Dispatch<'a> {
    /// Upstream API root, trailing slashes allowed ([`chat_url`] trims them).
    pub base_url: &'a str,
    /// Dialect this provider speaks. Anything but OpenAI is
    /// [`ExecError::UnsupportedWire`] rather than a wrong-wire POST.
    pub wire_format: WireFormat,
    /// Bearer credential. Empty means no `Authorization` header.
    pub api_key: &'a str,
    /// Provider-local model name, or empty to send the caller's spelling as-is.
    pub upstream_model: &'a str,
    /// Whether the client asked for an SSE stream; selects `Accept`.
    pub stream: bool,
    /// Extra outbound headers from the provider definition, applied between
    /// `Content-Type` and the auth layer. Empty for every compiled-in entry.
    pub headers: &'a BTreeMap<String, String>,
}

impl<'a> Dispatch<'a> {
    /// This dispatch's shape with its bearer replaced.
    ///
    /// The OAuth path's only way to attach a token: `Connection::dispatch`
    /// receives the *unauthenticated* shape and swaps the bearer here, so there
    /// is no code path where a session could POST with the config's static key.
    /// `'b` is the borrow of the replacement credential, which is shorter than
    /// `'a` — a `Grant`'s token lives on the stack, not for `'a`.
    #[must_use]
    pub fn with_api_key<'b>(self, api_key: &'b str) -> Dispatch<'b>
    where
        'a: 'b,
    {
        Dispatch {
            base_url: self.base_url,
            wire_format: self.wire_format,
            api_key,
            upstream_model: self.upstream_model,
            stream: self.stream,
            headers: self.headers,
        }
    }
}

/// Replaces the top-level `"model"` with the provider's spelling.
///
/// Returns the original bytes unchanged when there is no `upstream_model`: the
/// request is already provider-shaped, and inventing a model would hide a
/// misconfigured mapping. `&[u8]` in, `Bytes` out, because both consumers write
/// it straight into a request body.
///
/// # Errors
///
/// [`ExecError::Encode`] when the body is not valid JSON, or
/// [`ExecError::NotAnObject`] when it is valid JSON but not an object — there is
/// no top-level `"model"` to rewrite in either case, and inventing a key would
/// post a body no client wrote.
pub fn rewrite_model(body: &[u8], upstream_model: &str) -> Result<Bytes, ExecError> {
    if upstream_model.is_empty() {
        return Ok(Bytes::copy_from_slice(body));
    }
    let mut value: serde_json::Value = serde_json::from_slice(body)?;
    let Some(obj) = value.as_object_mut() else {
        return Err(ExecError::NotAnObject);
    };
    obj.insert("model".to_owned(), serde_json::Value::String(upstream_model.to_owned()));
    serde_json::to_vec(&value).map(Bytes::from).map_err(ExecError::Encode)
}

/// Waits for response headers, racing the caller's abort against the start
/// budget.
///
/// Inline `select!` rather than OmniRoute's merged `AbortSignal`: nothing
/// outlives this await, so there is no listener to remove and no watcher task
/// to keep alive. The timeout covers response *start* only and is dropped as
/// soon as headers land, because a long stream legitimately takes minutes.
async fn await_start(
    send: reqwest::RequestBuilder,
    abort: &CancellationToken,
    budget: Duration,
) -> Result<reqwest::Response, ExecError> {
    tokio::select! {
        // `biased` so an already-aborted caller never pays for a dispatch.
        biased;
        () = abort.cancelled() => Err(ExecError::Aborted),
        () = tokio::time::sleep(budget) => Err(ExecError::StartTimeout(budget)),
        sent = send.send() => sent.map_err(|e| ExecError::Transport(e.to_string())),
    }
}

/// Turns a non-2xx reply into [`ExecError::Upstream`], reading only as much of
/// the body as the error path needs.
///
/// `retry_after` is passed rather than read off the headers so both callers
/// share the single [`parse_retry_after`] — one 24 h cap, one spelling of the
/// RFC, no second place to drift.
async fn upstream_error(
    response: reqwest::Response,
    retry_after: Option<Duration>,
) -> ExecError {
    let status = response.status().as_u16();
    let raw = response.bytes().await.unwrap_or_default();
    let body = String::from_utf8_lossy(&raw[..raw.len().min(ERROR_BODY_CAP)]);

    // OpenAI-shaped errors nest the message under `error`; falling back to the
    // truncated raw body means a non-JSON upstream still yields something.
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| body.to_string());

    ExecError::Upstream {
        status,
        message: message.chars().take(ERROR_MESSAGE_CHARS).collect(),
        retry_after,
    }
}

/// A successful upstream response, not yet consumed.
///
/// Holds the live connection: dropping it before the body is drained closes the
/// socket or returns it to the pool.
#[derive(Debug)]
pub struct ChatStream {
    response: reqwest::Response,
    retry_after: Option<Duration>,
    abort: CancellationToken,
}

impl ChatStream {
    /// The upstream status, always 2xx here.
    pub fn status(&self) -> reqwest::StatusCode {
        self.response.status()
    }

    /// `Retry-After` the upstream sent on a 2xx, if any.
    ///
    /// An overloaded stream uses this to ask the *client* to slow down, so it is
    /// surfaced on success too.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Gives up the wrapper and hands back the live connection.
    fn into_response(self) -> reqwest::Response {
        self.response
    }

    /// Consumes a non-2xx into the triple a router classifies on:
    /// `(status, body, retry_after)`.
    ///
    /// The body is bounded by [`FAILURE_BODY_CAP`] because it is
    /// attacker-adjacent: a provider's error page can be megabytes of HTML, and
    /// the router scans only the first 2 KiB for its stop rows. `retry_after` is
    /// the already-parsed value, so it went through the single
    /// [`parse_retry_after`] — one 24 h cap, one spelling of the RFC, no second
    /// place to drift.
    pub async fn into_failure(self) -> (StatusCode, Bytes, Option<Duration>) {
        let retry_after = self.retry_after;
        let status = self.status();
        let raw = self.into_response().bytes().await.unwrap_or_default();
        let end = raw.len().min(FAILURE_BODY_CAP);
        (status, Bytes::copy_from_slice(&raw[..end]), retry_after)
    }

    /// Relays the body as raw chunks, ending the stream on abort or EOF.
    ///
    /// The counterpart to [`ChatStream::into_sse`], and the one `ar-server` uses:
    /// it relays upstream framing *untouched*, so a provider's own `data:`
    /// boundaries and any non-SSE body survive byte-for-byte. Decoding to
    /// `SseEvent` and re-encoding would be a lossy round trip for a proxy whose
    /// job is to not have one.
    ///
    /// `Err` distinguishes an abort from a clean end, which is `None`.
    ///
    /// ponytail: an error *inside* the stream terminates the relay rather than
    /// being dropped. A truncated body is the client's cue that the answer is
    /// incomplete, and silently ending early looks identical to a short answer.
    pub fn into_bytes(self) -> impl Stream<Item = Result<Bytes, ExecError>> + Send + use<> {
        let Self { response, abort, .. } = self;
        stream! {
            let mut body = response.bytes_stream();
            loop {
                // `biased` so an already-aborted caller is observed before
                // another blocking read is attempted.
                let read = tokio::select! {
                    biased;
                    () = abort.cancelled() => Err(ExecError::Aborted),
                    next = tokio_stream::StreamExt::next(&mut body) => Ok(next),
                };
                match read {
                    Err(e) => {
                        yield Err(e);
                        break;
                    }
                    Ok(Some(Err(e))) => {
                        yield Err(ExecError::Transport(e.to_string()));
                        break;
                    }
                    Ok(Some(Ok(bytes))) => yield Ok(bytes),
                    Ok(None) => break,
                }
            }
        }
    }

    /// Decodes the body as an SSE stream of payloads.
    ///
    /// Returns `impl Stream`, not a boxed one: the generator type is unnamed but
    /// statically dispatched, so no `dyn` sits on this path.
    ///
    /// Backpressure is inherent rather than configured. The stream pulls one
    /// chunk per poll and yields only what it has decoded, so there is no
    /// channel to bound and no way to accumulate ahead of a slow consumer.
    ///
    /// Ends on `data: [DONE]` or on upstream EOF, whichever comes first.
    pub fn into_sse(self) -> impl Stream<Item = Result<SseEvent, ExecError>> + Send + use<> {
        let Self {
            response, abort, ..
        } = self;
        stream! {
            let mut decoder = SseDecoder::new(DEFAULT_FRAME_CAP);
            let mut body = response.bytes_stream();

            loop {
                // `biased` so an already-aborted caller is observed before
                // another blocking read is attempted. `Err` distinguishes an
                // abort from upstream EOF, which is also `None`.
                let read = tokio::select! {
                    biased;
                    () = abort.cancelled() => Err(ExecError::Aborted),
                    next = tokio_stream::StreamExt::next(&mut body) => Ok(next),
                };

                let chunk = match read {
                    Err(e) => {
                        yield Err(e);
                        break;
                    }
                    Ok(chunk) => chunk,
                };

                match chunk {
                    // Upstream closed: flush any unterminated frame so a
                    // provider that omits the final blank line still yields its
                    // last payload.
                    None => {
                        if let Some(event) = decoder.finish() {
                            yield Ok(event);
                        }
                        break;
                    }
                    Some(Err(e)) => {
                        yield Err(ExecError::Transport(e.to_string()));
                        break;
                    }
                    Some(Ok(bytes)) => {
                        if let Err(e) = decoder.feed(&bytes) {
                            yield Err(e.into());
                            break;
                        }
                    }
                }

                while let Some(event) = decoder.next_event() {
                    yield Ok(event);
                }
            }
        }
    }
}

/// Merges the outbound headers in OmniRoute's precedence order.
///
/// `base.ts:500` sets `Content-Type` first so provider config can override it,
/// then auth, then `Accept` last so provider config *cannot* override it.
///
/// # TODO(#p0-align)
/// `ProviderDef` still needs `auth_header` / `auth_prefix` (both present in
/// OmniRoute's `RegistryEntry`) before non-`Bearer` providers work without a
/// hand-written header: `gemini`'s `x-goog-api-key`, `azure-ai`'s `api-key`,
/// `clarifai`'s `Key`. A file-declared provider covers those today by spelling
/// the header out; the catalog still cannot.
fn build_headers(
    api_key: &str,
    stream: bool,
    extra: &BTreeMap<String, String>,
) -> reqwest::header::HeaderMap {
    headers_for(
        api_key,
        "application/json",
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
        extra,
    )
}

/// The shared header merge, parameterised by content type and accept.
///
/// [`build_headers`] is the chat spelling of this; the media family needs the
/// same layers with a different `Content-Type` (multipart for the audio
/// endpoints), and a second implementation would be a second place for the auth
/// precedence to drift.
pub(crate) fn headers_for(
    api_key: &str,
    content_type: &str,
    accept: &str,
    extra: &BTreeMap<String, String>,
) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::with_capacity(3);
    // A content type the caller computed cannot be a static value; an
    // unencodable one is dropped for the same reason an unencodable credential
    // is — the upstream's own 4xx is the honest signal.
    if let Ok(value) = reqwest::header::HeaderValue::from_str(content_type) {
        headers.insert(reqwest::header::CONTENT_TYPE, value);
    }

    // A provider-declared header with a name or value reqwest rejects is
    // dropped for the same reason: the upstream's 4xx names the bad header, and
    // failing the whole request here would hide which of the two it was.
    headers.extend(extra.iter().filter_map(|(name, value)| {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).ok()?;
        let value = reqwest::header::HeaderValue::from_str(value).ok()?;
        Some((name, value))
    }));

    // Exhaustive, not `_`: adding an `AuthClass` variant must fail to compile
    // here rather than silently ship an unauthenticated request.
    match AuthClass::ApiKey {
        AuthClass::ApiKey => attach_bearer(&mut headers, api_key),
    }

    // Set after auth so it wins, matching `default.ts:672`.
    if let Ok(value) = reqwest::header::HeaderValue::from_str(accept) {
        headers.insert(reqwest::header::ACCEPT, value);
    }
    headers
}

/// Sets `Authorization: Bearer <secret>`, skipping an unencodable secret.
///
/// A key containing a header-illegal byte cannot be a valid credential, and
/// reqwest has no per-header error to report — so the credential is dropped and
/// the upstream's 401 is the signal, rather than failing the whole request here
/// for a value that was never going to authenticate.
fn attach_bearer(headers: &mut reqwest::header::HeaderMap, secret: &str) {
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {secret}")) {
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
}

/// Response-start budget: the provider cap for a stream, the base budget
/// otherwise. Mirrors `resolveFetchStartTimeout` in
/// `utils/fetchStartTimeoutPolicy.ts`.
fn start_timeout(stream: bool) -> Duration {
    if stream {
        BASE_START_TIMEOUT.min(STREAM_START_TIMEOUT_CAP)
    } else {
        BASE_START_TIMEOUT
    }
}

/// Reads `Retry-After` off a response, ignoring an absent or non-UTF-8 value.
fn retry_after_of(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get("retry-after")?.to_str().ok()?;
    parse_retry_after(Some(raw), SystemTime::now())
}

/// Parses a `Retry-After` header into a delay.
///
/// Accepts both RFC forms — delta-seconds and HTTP-date — and returns `None`
/// when the header is absent, unparseable or already past. Never invents a
/// default: a missing header means "no instruction", and a guessed delay would
/// turn upstream silence into an invented backoff. Delta-seconds are parsed
/// strictly, so a fraction or unit suffix is rejected rather than truncated,
/// and the 24h ceiling matches OmniRoute's `MAX_RETRY_MS`.
///
/// `now` is injected so the HTTP-date branch is testable without a clock.
pub fn parse_retry_after(raw: Option<&str>, now: SystemTime) -> Option<Duration> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }

    let delay = match raw.parse::<u64>() {
        Ok(secs) if secs > 0 => Duration::from_secs(secs),
        // `Retry-After: 0` means "retry now", which the caller already is doing.
        Ok(_) => return None,
        Err(_) => {
            let at = httpdate::parse_http_date(raw).ok()?;
            at.duration_since(now).ok()?.max(Duration::ZERO)
        }
    };

    (delay > Duration::ZERO).then(|| delay.min(MAX_RETRY_AFTER))
}

// `ArExec` is shared across tasks and `ChatStream` moves between them; prove
// both at compile time rather than discovering it from a spawn error at runtime.
const _: () = {
    const fn assert_send<T: Send>() {}
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ArExec>();
    assert_send::<ChatStream>();
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Field-by-field rather than a `Default` spread: `ar-registry` extends
    /// `ProviderDef` in parallel, and a `Default` would silently inherit a
    /// default price or executor where "none" is what a test means.
    fn provider(stream_key: &str) -> ProviderDef {
        ProviderDef {
            base_url: "https://api.test/v1".into(),
            wire_format: WireFormat::Openai,
            auth: AuthClass::ApiKey,
            env_hint: stream_key.into(),
            models: vec![],
            prices: Default::default(),
            executor: "default".into(),
            auth_kind: "api_key".into(),
            flat_rate: false,
            headers: Default::default(),
        }
    }

    fn extra(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn sets_accept_sse_when_streaming() {
        let headers = build_headers("sk-x", true, &BTreeMap::new());
        assert_eq!(headers[reqwest::header::ACCEPT], "text/event-stream");
    }

    #[test]
    fn sets_accept_json_when_not_streaming() {
        let headers = build_headers("sk-x", false, &BTreeMap::new());
        assert_eq!(headers[reqwest::header::ACCEPT], "application/json");
    }

    #[test]
    fn sets_bearer_when_api_key_present() {
        let headers = build_headers("sk-abc", false, &BTreeMap::new());
        assert_eq!(headers[reqwest::header::AUTHORIZATION], "Bearer sk-abc");
    }

    #[test]
    fn skips_bearer_when_secret_has_illegal_header_byte() {
        let headers = build_headers("sk\ninjected", false, &BTreeMap::new());
        assert!(!headers.contains_key(reqwest::header::AUTHORIZATION));
    }

    #[test]
    fn sends_a_provider_declared_header_when_one_is_configured() {
        let headers = build_headers("sk-x", false, &extra(&[("x-api-key", "abc")]));
        assert_eq!(headers["x-api-key"], "abc");
    }

    #[test]
    fn keeps_accept_when_a_provider_declares_its_own() {
        // The precedence contract: a provider config overrides the content type
        // but cannot claim to be something other than SSE.
        let headers = build_headers("sk-x", true, &extra(&[("accept", "application/json")]));
        assert_eq!(headers[reqwest::header::ACCEPT], "text/event-stream");
    }

    #[test]
    fn skips_a_provider_header_when_its_name_is_invalid() {
        let headers = build_headers("sk-x", false, &extra(&[("bad header", "v")]));
        assert!(!format!("{headers:?}").contains("bad header"), "{headers:?}");
    }

    #[test]
    fn rewrites_model_to_provider_spelling() {
        let got = rewrite_model(br#"{"model":"auto","stream":true}"#, "llama-3.3-70b")
            .expect("object body");
        let v: serde_json::Value = serde_json::from_slice(&got).expect("still JSON");
        assert_eq!(v["model"], serde_json::json!("llama-3.3-70b"));
    }

    #[test]
    fn preserves_other_fields_when_rewriting_model() {
        let got = rewrite_model(br#"{"model":"a","stream":true}"#, "b").expect("object body");
        let v: serde_json::Value = serde_json::from_slice(&got).expect("still JSON");
        assert_eq!(v["stream"], serde_json::json!(true));
    }

    #[test]
    fn passes_body_through_when_no_model_configured() {
        let raw = br#"{"model":"x"}"#;
        assert_eq!(
            rewrite_model(raw, "").expect("passthrough"),
            Bytes::from_static(raw)
        );
    }

    #[test]
    fn reports_not_an_object_when_body_is_an_array() {
        assert!(matches!(
            rewrite_model(b"[1,2]", "m"),
            Err(ExecError::NotAnObject)
        ));
    }

    #[test]
    fn gates_non_openai_wire_before_any_dispatch() {
        // The point of the gate: no request reaches the socket, so there is no
        // way for an OpenAI body to be POSTed to an Anthropic endpoint.
        let exec = ArExec::new().unwrap();
        let d = Dispatch {
            base_url: "https://api.anthropic.com/v1",
            wire_format: WireFormat::Anthropic,
            api_key: "sk-x",
            upstream_model: "claude-sonnet-4-5",
            stream: true,
            headers: &BTreeMap::new(),
        };
        let err = futures::executor::block_on(exec.post(
            &d,
            br#"{"model":"m"}"#,
            &CancellationToken::new()
        ));
        assert!(matches!(
            err,
            Err(ExecError::UnsupportedWire(WireFormat::Anthropic))
        ));
    }

    #[test]
    fn caps_stream_start_budget_at_provider_cap() {
        assert_eq!(start_timeout(true), STREAM_START_TIMEOUT_CAP);
    }

    #[test]
    fn uses_base_start_budget_when_not_streaming() {
        assert_eq!(start_timeout(false), BASE_START_TIMEOUT);
    }

    #[test]
    fn parses_delta_seconds_when_header_present() {
        assert_eq!(
            parse_retry_after(Some("30"), SystemTime::UNIX_EPOCH),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn caps_delta_seconds_at_24h() {
        assert_eq!(
            parse_retry_after(Some("999999"), SystemTime::UNIX_EPOCH),
            Some(MAX_RETRY_AFTER)
        );
    }

    #[test]
    fn returns_none_when_header_absent() {
        assert!(parse_retry_after(None, SystemTime::UNIX_EPOCH).is_none());
    }

    #[test]
    fn returns_none_when_header_blank() {
        assert!(parse_retry_after(Some("  "), SystemTime::UNIX_EPOCH).is_none());
    }

    #[test]
    fn returns_none_when_delta_seconds_zero() {
        assert!(parse_retry_after(Some("0"), SystemTime::UNIX_EPOCH).is_none());
    }

    #[test]
    fn returns_none_when_delta_seconds_unit_suffixed() {
        assert!(parse_retry_after(Some("30s"), SystemTime::UNIX_EPOCH).is_none());
    }

    #[test]
    fn returns_none_when_http_date_in_past() {
        // 2033-03-05, well after the RFC 3339 date below, so the delta is
        // negative and no wait is invented.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        assert!(
            parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT"), now).is_none(),
            "a date already in the past yields no delay"
        );
    }

    #[test]
    fn parses_http_date_when_within_cap() {
        // 10 minutes after the epoch: the HTTP-date branch, under the 24h cap.
        let now = SystemTime::UNIX_EPOCH;
        assert_eq!(
            parse_retry_after(Some("Thu, 01 Jan 1970 00:10:00 GMT"), now),
            Some(Duration::from_secs(600))
        );
    }

    #[test]
    fn caps_http_date_at_24h() {
        // 2015 is 45 years after the epoch, so the cap -- not the date -- decides.
        let now = SystemTime::UNIX_EPOCH;
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT"), now),
            Some(MAX_RETRY_AFTER)
        );
    }

    #[test]
    fn returns_none_when_header_not_a_retry_after_form() {
        assert!(parse_retry_after(Some("soon"), SystemTime::UNIX_EPOCH).is_none());
    }

    #[tokio::test]
    async fn rejects_post_when_wire_format_not_openai() {
        let exec = ArExec::new().unwrap();
        let mut def = provider("ANTHROPIC_API_KEY");
        def.wire_format = WireFormat::Anthropic;
        let chat = CanonicalChat {
            model: "claude-sonnet-4-5".into(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            stream: true,
        };

        let err = exec
            .post_chat(&chat, &def, &Secret::new("sk-x"), &CancellationToken::new())
            .await
            .err();
        assert!(matches!(err, Some(ExecError::UnsupportedWire(WireFormat::Anthropic))));
    }
}