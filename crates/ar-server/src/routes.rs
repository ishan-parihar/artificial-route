//! The HTTP routes.
//!
//! One inbound chat pipeline shared by four dialects, plus the read-only
//! endpoints. Per request, in order:
//!
//! 1. **auth** — a credential when a gate is configured (see [`keys`]); nothing
//!    when none is, and then the bind is loopback-only (see
//!    [`crate::app::bind_addr`]).
//! 2. **translate** — [`to_canonical_for_route`], which refuses a body that is not
//!    the dialect the route claims, after [`require_json`] has refused one that is
//!    not JSON at all.
//! 3. **guard** — `ar-guard` stage 1 redacts credentials out of the prompt, stage 2
//!    refuses prompt injection. The *rewritten* body is what continues.
//! 4. **route** — the request `model` selects a combo; an unknown name is a 400
//!    that names the ones that exist. `auto/*` resolves through `ar_route`'s virtual
//!    factory and `simulate_route`.
//! 5. **compress** — `x-ar-compression` resolves a plan through `ar-compress` and
//!    the pipeline that ran is echoed back.
//! 6. **cache** — a non-streaming request is looked up in `ar-cache`; a stream
//!    bypasses, and says so.
//! 7. **attempt loop** — `ar_route::attempt_loop` under the model's own deadline
//!    ([`crate::config::ServerConfig::stream_deadline`]), then `decorate`.
//!
//! A combo's `pool:` bench rides at the tail of its chain, so step 7 walks the
//! bench only once every target has refused.
//!
//! # What the four dialects are not interchangeable in
//!
//! [`Dialect`] is a parameter rather than a path string because three things differ
//! per dialect, and each is a wire format this crate must not invent: the keepalive
//! frame (an Anthropic client resets its watchdog on a real `event: ping` and
//! ignores a comment), the frame that names a truncated stream (Anthropic defines
//! `event: error`; the two OpenAI shapes have no event names at all and must use a
//! bare `data:` line), and the terminator whose absence means the stream was cut.
//! Ollama gets none of the three: its stream is NDJSON, so a keepalive would have
//! to be a JSON line.
//!
//! # The 415 guard and the `Accept` opt-in
//!
//! [`require_json`] runs before the translator, not inside it. A `text/plain` body
//! on a JSON route is a client error at the edge, and the reference gateway's guard
//! is there for the same reason (`chat/completions/route.ts:105-118`): past the edge
//! it becomes a 400 about JSON syntax, which reads as a routing problem and sends
//! an operator to the wrong place.
//!
//! [`accept_forces_stream`] is the mirror of that: a client that sends no `stream`
//! field but names SSE in `Accept` gets a stream. It is a fallback rather than an
//! override, so an explicit `stream: false` still wins — see its own docs.
//!
//! Route shape follows `../OmniRoute/src/app/api/v1/chat/completions/route.ts`:
//! validate, translate, resolve a provider, run the attempt loop, relay, stamp
//! decision headers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ar_cache::{Cache, CacheKey, CacheState};
use ar_compress::Step;
use ar_route::{
    ArExec, AttemptOutcome, AutoCandidate, AutoSelector, Candidate, CanonicalRequest, ChainTarget,
    ChunkStream, CutFlag, JudgeOutcome, JudgePanel, JudgeTarget, ProviderId, Resilience,
    RouteError, Strategy, Strng, pick_for_model, settle_stream, simulate_route, synthesize,
    virtual_combo,
};
use ar_tokens::ResponseMeta;
use axum::body::Body;
use axum::extract::{OriginalUri, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde::Serialize;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::app::AppState;
use crate::config::{DefaultChain, RouteCombo};
use crate::reframe;
use crate::text::{
    COMPRESSION_ECHO, COMPRESSION_HEADER, COMPRESSION_HEADER_ALIAS, GUARD_HEADER, GuardVerdict,
    compress_body, compression_echo, compression_plan_with_alias, guard_body,
};
use crate::translate::to_canonical_for_route;

/// Sticky-session header consumed by `Strategy::Lkgp`.
pub const SESSION_HEADER: &str = "x-ar-session";
/// Routing verdict, for the client and for support.
pub const DECISION_HEADER: &str = "x-ar-decision";
/// The cap arm that denied a request: `usd_cap`, `token_cap`, `unpriced`,
/// `unauditable`. The arm, never the amounts — the operator knows their own
/// numbers, and a header log does not need them.
pub const DENY_HEADER: &str = "x-ar-deny-reason";
/// Attempt accounting. P0 counts attempts; token accounting is `ar-tokens` (P1).
pub const USAGE_HEADER: &str = "x-ar-usage";
/// Cache verdict: `hit`, `miss` or `bypass`.
pub const CACHE_HEADER: &str = "x-ar-cache";
/// Kill the cache on both sides for this request: no lookup, no store.
///
/// Value grammar is the reference's strict one — exactly `true`, after ASCII
/// case folding; `1`/`yes`/`on` do not count (the reference matches on the
/// string, not on truthiness, and so does this). The `x-omniroute-*` spelling
/// is the alias a client configured against the reference gateway sends.
pub const NO_CACHE_HEADER: &str = "x-ar-no-cache";
/// [`NO_CACHE_HEADER`], as a client configured against the reference gateway
/// spells it.
pub const NO_CACHE_HEADER_ALIAS: &str = "x-omniroute-no-cache";
/// Kill the cache *write* only: the lookup still happens and can still hit.
///
/// Distinct from [`NO_CACHE_HEADER`] in the reference too — there
/// `no-cache` is the both-sides bypass and `cache-no-store` is the
/// store-side guard, read on the store path only (`semanticCacheManager.ts`,
/// the `store()` refusal ahead of the shared bypass check).
pub const NO_STORE_HEADER: &str = "x-ar-cache-no-store";
/// [`NO_STORE_HEADER`], as a client configured against the reference gateway
/// spells it.
pub const NO_STORE_HEADER_ALIAS: &str = "x-omniroute-cache-no-store";
/// A caller-supplied segment folded into the cache key, on both sides.
///
/// Namespaces entries: two clients naming different segments never see each
/// other's answers, and an absent segment is the shared default. See
/// [`request_key_with`] for what lands in the digest.
pub const CACHE_KEY_HEADER: &str = "x-ar-cache-key";
/// [`CACHE_KEY_HEADER`], as a client configured against the reference gateway
/// spells it.
pub const CACHE_KEY_HEADER_ALIAS: &str = "x-omniroute-cache-key";
/// A caller-requested TTL for the entry this request's answer is stored under.
///
/// The reference's units and heuristic are kept: a value over `100_000` is
/// milliseconds as written, anything else is seconds (`#14484`, which also
/// clamped it — mirrored on the store side, where the policy ceiling wins).
pub const CACHE_TTL_HEADER: &str = "x-ar-cache-ttl";
/// [`CACHE_TTL_HEADER`], as a client configured against the reference gateway
/// spells it.
pub const CACHE_TTL_HEADER_ALIAS: &str = "x-omniroute-cache-ttl";
/// Prompt tokens the upstream reported.
///
/// Counted on non-streaming replies since wave D: the completed body is
/// buffered there, its `usage` object is read, and the figure is priced by the
/// same call that writes the ledger row, so the header and `ar cost-report`
/// cannot drift. Streamed replies stay at `0` — counting them costs SSE
/// chunk parsing in the hot path, which this build has chosen not to spend
/// (see `docs/07-parity-closeout.md`, wave D), and `0` remains the answer
/// when the upstream reported nothing or the reply was not parseable JSON.
pub const TOKENS_IN_HEADER: &str = "x-ar-tokens-in";
/// Completion tokens the upstream reported.
///
/// See [`TOKENS_IN_HEADER`] for which replies carry a count.
pub const TOKENS_OUT_HEADER: &str = "x-ar-tokens-out";
/// What the request cost, in USD with six decimals.
///
/// Six decimals because [`ar_tokens::Usd`] is integer micro-dollars and the
/// ledger prints the same width: a header at a different precision would read
/// as a different amount. Zero on streamed replies, on which the usage count
/// is not read at all — see [`TOKENS_IN_HEADER`].
pub const RESPONSE_COST_HEADER: &str = "x-ar-response-cost";
/// Prompt tokens compression removed before dispatch.
pub const SAVINGS_TOKENS_HEADER: &str = "x-ar-savings-tokens";
/// Completion tokens per second. Absent when the elapsed time is too short to
/// divide by — a `0` would read as instant, and a client dividing by it would
/// report nonsense rather than nothing. Derived from [`TOKENS_OUT_HEADER`], so it
/// is absent on every response until that one carries a count.
pub const TOKENS_PER_SECOND_HEADER: &str = "x-ar-tokens-per-second";
/// Milliseconds spent dispatching, through to building this response.
pub const LATENCY_MS_HEADER: &str = "x-ar-latency-ms";
/// Providers tried before the one that answered.
///
/// Restates [`USAGE_HEADER`]'s `attempts=` for a client that reads only the
/// accounting headers, so the two cannot be read as different counts.
pub const FALLBACK_ATTEMPTS_HEADER: &str = "x-ar-fallback-attempts";
/// The model this request named — the routing id the client sent, not the
/// upstream's spelling, which stays private to the dispatch.
pub const MODEL_HEADER: &str = "x-ar-model";
/// The provider the verdict describes, stamped only when there is one — an
/// absent header reads as "the router answered before any provider did",
/// which is the reference gateway's own conditional behaviour.
pub const PROVIDER_HEADER: &str = "x-ar-provider";
/// This server's own version, so a client can pin a behaviour without a
/// round trip to a health endpoint. The reference stamps its `APP_CONFIG`
/// version on every response the same way.
pub const VERSION_HEADER: &str = "x-ar-version";

/// The version [`VERSION_HEADER`] reports: this crate's, because the header is
/// this crate's product. Workspace members move together, and a split
/// version here would mean exactly the drift the header exists to expose.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Ceiling on the response body the cache will buffer for storage.
///
/// A non-streaming completion is one JSON chunk and is nearly always well under
/// this. A provider that answers a non-streaming request with a multi-megabyte
/// body is relayed anyway — the cap decides only whether the cache *keeps* a
/// copy, never whether the client gets one.
pub const CACHE_MAX_BODY: usize = 1024 * 1024;

/// How long a stream may go without producing a byte before the relay emits a
/// keepalive of its own.
///
/// A client watchdog — Claude Code's, the Anthropic SDK's — resets on any byte
/// and gives up after its own budget. A slow first token is indistinguishable
/// from a dead upstream to such a client, and the client's answer to "dead
/// upstream" is to abort and retry, which on a slow model is a retry loop rather
/// than a wait. 10s is inside every watchdog that exists and long enough that a
/// healthy first token never pays for a frame nobody reads.
///
/// Ponytail: one interval for every dialect. Per-client tuning would need a
/// per-client configuration this crate has no way to key on, and the wrong
/// interval in either direction is a latency cost rather than a correctness one.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// `POST /v1/chat/completions` — OpenAI-compatible, SSE pass-through.
///
/// The path is taken through [`OriginalUri`] rather than by owning the whole
/// `Request`, because the credential matrix reads it — a tokenized-alias URL
/// carries its credential only there — and the body keeps going through axum's
/// own `Bytes` extractor. That is deliberate: the extractor is what turns an
/// oversized request into the 413 the body-limit layer chose, with its own
/// message, and reading the body here would mean re-implementing that refusal
/// from a type that carries no status to forward.
pub async fn chat_completions(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_chat(&state, &headers, uri.path(), body, Dialect::OpenAi).await
}

/// `POST /v1/messages` — Anthropic Messages inbound.
pub async fn messages(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_chat(&state, &headers, uri.path(), body, Dialect::Anthropic).await
}

/// `POST /v1/responses` — OpenAI Responses inbound.
pub async fn responses(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_chat(&state, &headers, uri.path(), body, Dialect::Responses).await
}

/// `POST /api/chat` — Ollama chat inbound.
pub async fn ollama_chat(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_chat(&state, &headers, uri.path(), body, Dialect::Ollama).await
}

/// The Responses keepalive: a real `response.in_progress` event.
///
/// Recorded from the reference gateway's own Responses translator, which emits
/// `response.created` then `response.in_progress` with this shape
/// (`open-sse/translator/response/openai-responses.ts:272-285`). The id is a
/// placeholder and the output is empty, which is what "in progress" means on a
/// stream that has produced nothing yet — a client rendering a delta from here
/// would be rendering a token this server invented.
const RESPONSES_IN_PROGRESS: &str = concat!(
    "data: {\"type\":\"response.in_progress\",\"response\":{",
    "\"id\":\"resp_keepalive\",\"object\":\"response\",\"status\":\"in_progress\",",
    "\"output\":[],\"error\":null,\"background\":false}}\n\n"
);

/// The OpenAI chat keepalive: a real, inert `chat.completion.chunk`.
///
/// Recorded from the reference gateway's heartbeat, which picks its frame per
/// client format and answers `openai` with this chunk shape
/// (`open-sse/utils/sseHeartbeat.ts:62-70`,
/// `open-sse/utils/earlyStreamKeepalive.ts:44`).
///
/// The empty `delta` is what keeps it inert: it contributes no `content` and no
/// `role`, so a client that concatenates deltas appends nothing, and
/// `finish_reason: null` means it is never mistaken for a terminated stream.
const OPENAI_KEEPALIVE: &str = concat!(
    "data: {\"id\":\"chatcmpl-keepalive\",\"object\":\"chat.completion.chunk\",",
    "\"created\":0,\"model\":\"keepalive\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":null}]}\n\n"
);

/// The inbound wire dialect, which decides the keepalive frame and the
/// `Accept`-forces-stream rule.
///
/// One enum rather than a `&str` route because three of the four are only
/// reachable as a match arm: the frames are not interchangeable and a `&str`
/// route parameter would let a call site pass a path that has no frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// `POST /v1/chat/completions`.
    OpenAi,
    /// `POST /v1/messages`. Its keepalive is a real `ping` *event*, not a
    /// comment — see [`Dialect::keepalive`].
    Anthropic,
    /// `POST /v1/responses`.
    Responses,
    /// `POST /api/chat`. NDJSON, not SSE: a keepalive would have to be a JSON
    /// line, and inventing one would be inventing a wire format.
    Ollama,
}

impl Dialect {
    /// The path this dialect is served at, which is also the dialect selector
    /// [`to_canonical_for_route`] takes.
    const fn route(self) -> &'static str {
        match self {
            Self::OpenAi => "/v1/chat/completions",
            Self::Anthropic => "/v1/messages",
            Self::Responses => "/v1/responses",
            Self::Ollama => "/api/chat",
        }
    }

    /// The frame emitted while the upstream is idle, per dialect.
    ///
    /// The Anthropic arm is the load-bearing one and the reason this is a
    /// function rather than a constant: Anthropic clients reset their first-token
    /// watchdog on a real SSE *event* and ignore SSE comments (`: …`), so a
    /// comment keepalive on `/v1/messages` is invisible to them and the
    /// slow-first-token abort it was meant to prevent still happens. The reference
    /// gateway emits `event: ping` for exactly this reason
    /// (`open-sse/utils/earlyStreamKeepalive.ts:49-54`).
    ///
    /// The Responses arm is a real frame for the same reason: its stream
    /// discriminates on a `type` inside the payload rather than an SSE event name,
    /// so `response.in_progress` is what says "still working" in the vocabulary
    /// its clients already parse
    /// (`open-sse/translator/response/openai-responses.ts:272-285`).
    ///
    /// The OpenAI chat arm is a real chunk, not a comment. A comment is skipped
    /// by every SSE parser by definition, so it survives desync — but a client
    /// that resets a first-token watchdog on *decoded frames* never sees it, and
    /// the slow-first-token abort it was meant to prevent still happens. This is
    /// the arm every OpenAI-compatible client (opencode, omO, oh-my-pi) lands on,
    /// and the reference gateway ships precisely this shape for it
    /// (`open-sse/utils/earlyStreamKeepalive.ts:62-70`), so matching it is what
    /// makes a stream ar holds open look alive to them.
    ///
    /// `delta: {}` is what makes the frame inert: an empty delta has no `content`
    /// and no `role`, so it cannot be concatenated into an answer, and
    /// `finish_reason: null` keeps it from being read as the end of the stream.
    fn keepalive(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("event: ping\ndata: {\"type\":\"ping\"}\n\n"),
            Self::Responses => Some(RESPONSES_IN_PROGRESS),
            Self::OpenAi => Some(OPENAI_KEEPALIVE),
            Self::Ollama => None,
        }
    }

    /// The frame naming a truncated stream, if the stream was cut off.
    ///
    /// A stream that ends without [`Self::terminator`] is the case where a client
    /// waits forever for a frame that is never coming: the connection closed
    /// cleanly, so no error is visible, and the client's own read blocks until its
    /// watchdog fires. Naming the failure in-band is what turns that wait into a
    /// surfaceable error.
    ///
    /// Per-dialect, for the same reason the keepalive is: a bare `data:` line for
    /// the two OpenAI shapes, a real `event: error` for Anthropic, whose spec
    /// defines one. Never the token, never the upstream body — a truncated relay
    /// has no body to quote and a body it did have could name an account.
    fn stream_error(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some(concat!(
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"stream_error\",",
                "\"message\":\"the upstream stream ended before it was complete\"}}\n\n"
            )),
            Self::OpenAi => Some(concat!(
                "data: {\"error\":{\"type\":\"stream_error\",\"code\":\"stream_error\",",
                "\"message\":\"the upstream stream ended before it was complete\"}}\n\n"
            )),
            Self::Responses => Some(concat!(
                "data: {\"type\":\"error\",\"code\":\"stream_error\",",
                "\"message\":\"the upstream stream ended before it was complete\"}\n\n"
            )),
            Self::Ollama => None,
        }
    }

    /// The frame whose presence in the last relayed chunk means the stream
    /// finished rather than being cut off.
    ///
    /// Ponytail: substring matching on relayed bytes, which is the one place this
    /// module reads upstream content. A real fix is an SSE parser over the same
    /// stream; that is a crate and a dependency, and a false negative here costs
    /// one extra frame a strict client already tolerates.
    ///
    /// `None` means "do not check", for Ollama: its NDJSON stream ends with a
    /// `{"done":true}` object and the relay does not parse it, so asserting on it
    /// would be a guess about a body this crate never inspects.
    fn terminator(self) -> Option<&'static [u8]> {
        match self {
            Self::OpenAi => Some(b"data: [DONE]"),
            Self::Anthropic => Some(b"event: message_stop"),
            Self::Responses => Some(b"response.completed"),
            Self::Ollama => None,
        }
    }
}

/// The one entry every inbound dialect shares: serves the request, then logs
/// it for the /ui request log when it failed.
async fn handle_chat(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    body: Bytes,
    dialect: Dialect,
) -> Response {
    // `Bytes` clones by refcount; the copy is only parsed on a failure.
    let asked = body.clone();
    let mut seen_key = None;
    let mut lease = None;
    let resp = handle_chat_inner(
        state,
        headers,
        path,
        body,
        dialect,
        &mut seen_key,
        &mut lease,
    )
    .await;
    note_failure(state, &resp, seen_key.as_deref(), || model_asked(&asked));
    hold_lease(resp, lease)
}

/// Keeps a lane lease alive for as long as the reply body is.
///
/// The lease is moved into the body stream, so its permit returns when the body
/// is dropped: after the last byte, or when a client disconnects mid-stream and
/// the server drops the body. An error reply holds a lease too, which is harmless:
/// it is short and it is dropped with the body.
pub(crate) fn hold_lease(resp: Response, lease: Option<ar_keys::Lease>) -> Response {
    let Some(lease) = lease else {
        return resp;
    };
    let (parts, body) = resp.into_parts();
    let held = body.into_data_stream().map(move |chunk| {
        // Captured for its `Drop`: the closure owns the lease until the stream does.
        let _lane = &lease;
        chunk
    });
    Response::from_parts(parts, Body::from_stream(held))
}

/// Takes a permit on `lane` for `key`, or returns the reply to send instead.
///
/// `Ok(None)` when lane control is off, so a caller holds nothing and proceeds.
pub(crate) async fn admit(
    state: &AppState,
    lane: ar_keys::Lane,
    key: &str,
) -> Result<Option<ar_keys::Lease>, Box<Response>> {
    let Some(lanes) = state.lanes.as_ref() else {
        return Ok(None);
    };
    lanes
        .acquire(lane, key)
        .await
        .map(Some)
        .map_err(|refused| Box::new(lane_refusal(&refused)))
}

/// The reply for a request the lane controller did not admit.
///
/// `busy` and `queue_full` both mean "come back later", so both are a 429 with
/// the window the controller computed. `shutdown` is a 503: the gateway is going
/// away, and a retry window would be a lie.
pub(crate) fn lane_refusal(err: &ar_keys::AdmitError) -> Response {
    let (status, code) = match err {
        ar_keys::AdmitError::Shutdown | ar_keys::AdmitError::ZeroRpm => {
            (StatusCode::SERVICE_UNAVAILABLE, "shutting_down")
        }
        _ => (StatusCode::TOO_MANY_REQUESTS, "lane_busy"),
    };
    let mut resp = error_because(status, code, err.detail(), &err.to_string());
    if let Some(after) = err.retry_after() {
        stamp(&mut resp, "retry-after", &after.to_string());
    }
    resp
}

/// The `model` a JSON request body asked for, or empty when it names none.
///
/// The failure log reads it only after a failure, so the body is parsed once
/// and only then.
pub(crate) fn model_asked(asked: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct Asked {
        model: Option<String>,
    }
    serde_json::from_slice::<Asked>(asked)
        .ok()
        .and_then(|a| a.model)
        .unwrap_or_default()
}

/// Why a failed response failed, in one line, for the /ui request log.
///
/// Rides on the response as an extension so the log never re-reads a body.
#[derive(Clone)]
struct FailNote(String);

/// Writes a failed request to the usage ledger's `failures` table, when a
/// ledger is configured. Auth refusals (401/403) are skipped: they are probes,
/// not routed requests, and would let anyone fill the log.
pub(crate) fn note_failure(
    state: &AppState,
    resp: &Response,
    key_id: Option<&str>,
    model: impl FnOnce() -> String,
) {
    let status = resp.status();
    if status.as_u16() < 400 || matches!(status.as_u16(), 401 | 403) {
        return;
    }
    let Some(ledger) = state.ledger.as_ref() else {
        return;
    };
    // Read only here, on a failure: a successful request never pays for the parse.
    let model = model();
    // The provider the chain last tried, from the decision header every routed
    // reply carries. `-` when no provider was tried (a refusal before dispatch).
    let provider = resp
        .headers()
        .get(DECISION_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|d| d.split(';').find_map(|kv| kv.strip_prefix("provider=")))
        .filter(|p| !p.is_empty())
        .unwrap_or("-");
    let why = resp.extensions().get::<FailNote>().map_or_else(
        || status.canonical_reason().unwrap_or("error"),
        |note| note.0.as_str(),
    );
    if let Err(e) = ledger
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record_failure(
            key_id.unwrap_or("anonymous"),
            provider,
            &model,
            status.as_u16(),
            why,
            epoch_now(),
        )
    {
        // A log write must not turn a failed request into a different failure.
        tracing::warn!(error = %e, "failure log write failed");
    }
}

/// The one chat pipeline, for every inbound dialect.
///
/// Order is the module doc's seven steps, and each is where it is because the
/// next one depends on the previous: the credential is read before the body (a
/// tokenized-alias URL carries its key in the path), the media type before the
/// body is parsed, the translation before the guard so the guard sees canonical
/// turns, the route resolution before the cache so the key covers what is
/// dispatched, and the deadline only around the attempt loop because everything
/// before it is local work that cannot hang on a socket.
async fn handle_chat_inner(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    body: Bytes,
    dialect: Dialect,
    seen_key: &mut Option<String>,
    lease: &mut Option<ar_keys::Lease>,
) -> Response {
    let key_id = match authorize(state, headers, path) {
        Ok(key_id) => key_id,
        Err(reason) => return *reason,
    };
    // Handed back so a failure the wrapper logs names the key that made it.
    seen_key.clone_from(&key_id);
    // The policy layer's 429 arm, before any body work: a key past its
    // requests-per-minute bucket is told when its next token lands and
    // nothing else happens. Checked before the parse because a rate-limited
    // client should not cost the router a translation, and keyed by the
    // credential the gate authenticated — anonymous traffic shares the
    // `default` row, which is what that row is for.
    let policy_key = key_id.as_deref().unwrap_or("anonymous");
    if let Some(wait) = state.policy.rate_wait(policy_key) {
        let mut resp = error_because(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "rate_limited",
            &format!(
                "this key is past its requests-per-minute ceiling; its next token lands in {wait}s"
            ),
        );
        stamp(&mut resp, "retry-after", &wait.to_string());
        return resp;
    }
    if !state.config.has_provider() {
        return error_because(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_provider",
            "nothing_configured",
            "no provider is configured; `ar doctor` lists what is missing",
        );
    }
    // RFC 9111 §15.5 / the reference gateway's 415 guard: a non-JSON body on a
    // JSON route is a client error at the edge, and letting it through means it
    // reaches provider lookup and comes back as a 400 that reads as a routing
    // problem. Checked after auth so an unauthenticated probe learns nothing
    // about the body's shape.
    if let Some(reason) = require_json(headers) {
        return reason;
    }

    // Lane admission comes after the gate and the format check, so a refused
    // probe and a malformed body never take a permit. The lease is handed out
    // through `lease` and rides with the reply body.
    match admit(state, ar_keys::Lane::Interactive, policy_key).await {
        Ok(granted) => *lease = granted,
        Err(refusal) => return *refusal,
    }

    // A body that is not the dialect this route claims — including a body that
    // is not JSON at all — reaches the translator and comes back as the
    // unparsable-body 400, which is where a client that ignored the 415 lands.
    let mut canonical = match to_canonical_for_route(dialect.route(), &body) {
        Ok(c) => c,
        Err(e) => {
            return error_because(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "unparsable_body",
                &e,
            );
        }
    };
    // `Accept: text/event-stream` is an opt-in for a client that sends no
    // `stream` field. It is *not* allowed to override an explicit `stream: false`
    // — the body is the client's own statement about what it wants, and the
    // reference gateway's rule is the same
    // (`open-sse/utils/aiSdkCompat.ts:75-146`).
    //
    // Checked on the raw bytes rather than the canonical request, because the
    // canonical request has already collapsed "absent" and "false" into one
    // `false` and the distinction is the whole question.
    if accept_forces_stream(headers) && !declares_stream(&body) {
        canonical.stream = true;
    }
    canonical.session = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(|v| Strng::from(v.to_owned()));

    // Guard before anything can observe the prompt: stage 1 rewrites the bytes,
    // and the rewritten copy is what gets compressed, cached, forwarded and
    // logged downstream.
    let (guarded, guard_verdict) = match guard_body(&canonical.body) {
        Ok(pair) => pair,
        Err(reason) => {
            return error_because(
                StatusCode::BAD_REQUEST,
                "request_rejected",
                "guard_denied",
                &reason,
            )
            .with_guard(GuardVerdict::Deny);
        }
    };
    canonical.body = Bytes::from(guarded);

    let plan = match resolve(state, &canonical) {
        Ok(p) => p,
        Err(RouteReject::UnknownModel(reason)) => {
            return error_because(
                StatusCode::BAD_REQUEST,
                "model_not_found",
                "unknown_model",
                &reason,
            )
            .with_guard(guard_verdict);
        }
    };

    // Compression runs after the guard, so a rewrite cannot un-redact anything,
    // and before the cache lookup, so the key covers exactly what is dispatched.
    //
    // Routed through the alias resolver so a client configured against
    // OmniRoute's `x-omniroute-compression` stops being silently ignored:
    // native spelling wins, then the alias, then the combo, and the echo
    // still names the layer that chose, never the wire name that asked.
    let compression = compression_plan_with_alias(
        headers
            .get(COMPRESSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        headers
            .get(COMPRESSION_HEADER_ALIAS)
            .and_then(|v| v.to_str().ok()),
        plan.compression.as_ref().map(std::slice::from_ref),
    );
    let mut savings_tokens = 0;
    if let Some(rewritten) = compress_body(&canonical.body, &compression) {
        // Counted, not estimated: the reference's `X-OmniRoute-Savings-Tokens`
        // is an exact BPE figure, so a client comparing the two gateways sees
        // the same number for the same body. Both counts run on the rewrite
        // branch only — a request compression did not touch pays nothing, and
        // the character-count estimator that used to live here stays in `eval`,
        // where a savings *figure* really is a fidelity metric rather than a
        // counted promise.
        savings_tokens = ar_tokens::count_text(&String::from_utf8_lossy(&canonical.body))
            .saturating_sub(ar_tokens::count_text(&String::from_utf8_lossy(&rewritten)));
        canonical.body = Bytes::from(rewritten);
    }

    // A streaming request cannot be replayed from cache — frames arrive
    // incrementally and the client is already consuming them — so it reports
    // `bypass`, as does a request that said `no-cache`: both are requests the
    // lookup genuinely never happened for. A `cache-no-store` request still
    // looks, and its verdict is whatever the lookup said.
    let cache_control = CacheControl::read(headers);
    let cache = state.cache.clone();
    // K4: the key names the target that will serve the request, not the
    // client-facing alias. A combo retargeted in config keeps its alias, so an
    // alias-keyed entry would replay the old target's reply for its whole TTL.
    let planned_target = plan
        .chain
        .first()
        .map(|target| format!("{}/{}", target.provider.as_str(), target.model));
    let cache_key = if canonical.stream || cache_control.no_cache {
        None
    } else {
        let served = planned_target
            .clone()
            .unwrap_or_else(|| canonical.model.to_string());
        request_key(
            &canonical,
            &served,
            key_id.as_deref().unwrap_or("anonymous"),
            cache_control.key.as_deref(),
            dialect,
        )
    };

    if let (Some(key), Some(cache)) = (cache_key.as_ref(), cache.as_ref())
        && let Some(hit) = cache_hit(cache, key, plan.chain.first(), plan.strategy)
    {
        return hit.with_guard(guard_verdict);
    }

    // The policy layer's 402 arm: the projected cost of what is about to be
    // dispatched, checked against the spend this key already holds in the
    // ledger. After the cache because a hit serves from memory and is never
    // recorded as spend — what the ledger does not bill, the cap does not
    // deny — and before the dispatch, the only point at which refusing the
    // request costs the client nothing. Priced from the chain's first target
    // against the same rows the response path bills from, so the estimate and
    // the eventual bill agree to the row.
    if let (Some(ledger), Some(target)) = (state.ledger.as_ref(), plan.chain.first()) {
        let projection = crate::policy::Projection {
            asked: canonical.model.as_ref(),
            provider: target.provider.as_str(),
            model: &target.model,
            tokens_in: ar_tokens::count_text(&String::from_utf8_lossy(&canonical.body)),
            tokens_out: declared_output_ceiling(&canonical.body),
            prices: &state.config.prices,
        };
        let verdict = {
            // Poisoned is the record path's convention too: a ledger that
            // panicked mid-write is a bug to surface, not a ceiling to guess.
            let guard = ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.policy.spend_verdict(policy_key, &guard, &projection)
        };
        if let ar_tokens::Verdict::Deny(reason) = verdict {
            let mut resp = error_because(
                reason.status(),
                "payment_required",
                reason.code(),
                &reason.to_string(),
            );
            // The arm that denied, never the amounts: the operator knows
            // their own numbers, and a header log does not need them.
            stamp(&mut resp, DENY_HEADER, reason.code());
            return resp.with_guard(guard_verdict);
        }
    }

    // Started here so the latency the response reports is the provider's time and
    // not this server's own compression pass — the same span the attempt
    // accounting on `x-ar-decision` describes.
    let dispatched = Instant::now();
    // `fusion` is the one strategy with no chain semantics: every panel member
    // is asked in parallel and the winner is decided over the panel, not down a
    // chain. A judge, when the combo names one, synthesizes the whole panel
    // afterwards, which is a second dispatch over a composed prompt
    // (`fusion.ts::handleFusionChat`).
    if plan.strategy == Strategy::Fusion {
        return fusion_response(
            state,
            &canonical,
            &plan,
            dialect,
            dispatched,
            savings_tokens,
        )
        .await
        .with_guard(guard_verdict);
    }
    // `pipeline` is the other strategy with no chain semantics: each target is
    // a *stage*, and each stage's answer is the next stage's input. Routing it
    // through the ordinary attempt loop would turn the stages into fallbacks —
    // the second stage would answer the client's question rather than the
    // first stage's — so a `strategy: pipeline` combo silently did something
    // other than what it said. The engine is [`ar_route::dispatch_pipeline`].
    if plan.strategy == Strategy::Pipeline {
        return pipeline_response(state, &canonical, &plan, dialect, dispatched)
            .await
            .with_guard(guard_verdict);
    }
    let mut outcome = match tokio::time::timeout(
        state.config.stream_deadline(canonical.model.as_ref()),
        ar_route::attempt_loop_with_pins(
            &canonical,
            &plan.chain,
            state.exec.as_ref(),
            &state.resilience,
            Some(&state.lkgp),
        ),
    )
    .await
    {
        // The model-aware deadline is the narrower of the two, so it is the one
        // the client sees: the router-wide layer is set to the widest deadline
        // any model asked for precisely so it cannot pre-empt this.
        Err(_elapsed) => {
            let deadline = state.config.stream_deadline(canonical.model.as_ref());
            let reason = format!(
                "model {:?} produced no response within {}s",
                canonical.model.as_ref(),
                deadline.as_secs()
            );
            return error_because(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "model_deadline",
                &reason,
            )
            .with_guard(guard_verdict);
        }
        Ok(Err(e)) => return route_error(e).with_guard(guard_verdict),
        Ok(Ok(o)) => o,
    };

    state
        .metrics
        .observe_attempts(u64::from(outcome.attempts()));
    if matches!(outcome, AttemptOutcome::Failover { .. }) {
        state.metrics.observe_failover();
    }
    // Feed the router's per-target load table.
    //
    // The table existed and had a reader, but nothing ever wrote to it on the
    // request path: `reliability` and `latency_inv` were `None` on every
    // request, so every candidate scored the neutral pair and `p2c` — whose
    // whole claim is that it picks the *healthier* of two choices — degenerated
    // to "whichever of the two random indices came first", i.e. `random`.
    //
    // The two numbers are now observed from what the request actually
    // measured: whether it succeeded, and how long it took.
    observe_target_outcome(&outcome, &plan.chain, dispatched.elapsed());
    // Pin only on success, and only when the client actually sent a session
    // key — an empty-string key would collapse every anonymous client onto one
    // provider.
    if let (Some(s), Some(p), true) = (
        canonical.session.as_deref(),
        outcome.provider(),
        matches!(outcome, AttemptOutcome::Succeeded { .. }),
    ) {
        state.lkgp.record(s, p);
    }

    // The cache verdict is the real one: a stream genuinely never looked, and a
    // non-stream request that got this far genuinely missed. The old code wrote
    // `bypass` unconditionally, which reads on the client as "the cache is off"
    // on a server that had no cache at all.
    let cache_state = if canonical.stream || cache_control.no_cache {
        CacheState::Bypass
    } else {
        CacheState::Miss
    };

    // A non-streaming reply is buffered here and only here: the usage ledger
    // and the accounting headers need the whole body to read `usage`, and the
    // pieces the response carries report what that buffer held. A streamed
    // request relays untouched and its usage headers stay at zero — the count
    // is not knowable before the first byte goes out — but its usage is no
    // longer lost: `account_stream` wraps the relay so the ledger records
    // whatever the dialect's final frames carried. See `docs/07` (wave D) for
    // the split this half closes.
    let source = reframe_source(state, outcome.provider(), dialect, canonical.stream);
    let meta = if canonical.stream {
        if let Some(source) = source {
            reframe_stream(&mut outcome, dialect, &canonical.model, source);
        }
        account_stream(
            &mut outcome,
            state,
            dialect,
            key_id.as_deref(),
            &canonical.model,
            dispatched,
        );
        ResponseMeta::default()
            .with_latency(dispatched.elapsed())
            .with_savings_tokens(savings_tokens)
    } else {
        buffer_for_accounting(
            state,
            &mut outcome,
            dialect,
            &Accounted {
                key_id: key_id.as_deref(),
                combo: &canonical.model,
            },
            dispatched.elapsed(),
            savings_tokens,
            source,
        )
        .await
    };

    let continuation = continuation_for(
        state,
        &plan.chain,
        outcome.provider(),
        dialect,
        canonical.stream,
        &canonical.body,
    );
    decorate(
        outcome,
        state.resilience.clone(),
        canonical.stream,
        dialect,
        plan.strategy,
        Stages {
            continuation,
            failures: FailureSink {
                ledger: state.ledger.clone(),
                key_id: key_id.clone().unwrap_or_else(|| "anonymous".to_owned()),
            },
            compression: compression_echo(&compression),
            cache_state,
            cache_key,
            planned_target,
            cache,
            model: canonical.model.clone(),
            cache_control,
            meta,
        },
    )
    .with_guard(guard_verdict)
}

/// What one served request is accounted against.
///
/// The names the request itself carries, gathered so the accounting path does
/// not pass positional strings a caller can transpose. The `combo` is the
/// name the CLIENT asked for; the serving target's own model comes from the
/// attempt outcome, because it is what ANSWERED and the two are different
/// facts — two combos sharing a target are exactly the case where confusing
/// them would double-count a spend.
struct Accounted<'a> {
    /// The key the gate verified, or `None` for an anonymous request.
    key_id: Option<&'a str>,
    /// The combo the client asked for.
    combo: &'a str,
}

/// Drains a non-streaming reply now that its usage can be counted, records it,
/// and hands back the accounting figures — while the streamed arm keeps
/// relaying untouched frames.
///
/// Buffering here, rather than in [`decorate`], is what keeps the streamed path
/// paying nothing: it is the one place the bytes exist before the relay arms
/// take them. Once drained, the upstream is rebuilt as a one-chunk stream of
/// the exact same bytes, so what the client receives is byte-identical to the
/// baseline that streamed it straight through; the RAM the buffer peaks at is
/// the reply's own size, held once, instead of that size crossing twice.
///
/// The recorded figure is the upstream's own `usage` map (Ollama's
/// `prompt_eval_count`/`eval_count` live at the root, so the root is the usage
/// object for it), priced against the same [`ar_tokens::PricingTable`] the
/// config resolved: the headers and the ledger cannot drift, because one
/// [`ResponseMeta::from_upstream`] call computes both.
async fn buffer_for_accounting(
    state: &AppState,
    outcome: &mut AttemptOutcome,
    dialect: Dialect,
    accounted: &Accounted<'_>,
    elapsed: Duration,
    savings_tokens: u32,
    source: Option<ar_registry::WireFormat>,
) -> ResponseMeta {
    let defaults = || {
        ResponseMeta::default()
            .with_latency(elapsed)
            .with_savings_tokens(savings_tokens)
    };
    // Only a 2xx that completed is counted. A failover's last upstream failure
    // has nothing spent against it, and an abort has no body to read: zero is
    // the honest figure in both.
    // Before the destructure below, which holds the outcome's borrow.
    let attempts = outcome.attempts();
    let AttemptOutcome::Succeeded {
        upstream,
        provider,
        model,
        ..
    } = outcome
    else {
        return defaults();
    };
    // The serving target's own model, not the client's combo spelling: the
    // ledger row and its pricing lookup describe what the provider answered.
    let model = model.clone();

    let mut buffered = Vec::new();
    while let Some(chunk) = upstream.stream.next().await {
        buffered.extend_from_slice(&chunk);
    }
    let body = Bytes::from(buffered);
    // A reply the client reads in its own dialect is counted off the OpenAI body
    // it was rendered from: the usage keys are the same, and that is the shape
    // `read_and_record` is told to read.
    let client = source.and_then(|source| reframed_body(dialect, source, &body));
    let counted = if client.is_some() {
        Dialect::OpenAi
    } else {
        dialect
    };
    let meta = read_and_record(
        state,
        accounted.key_id,
        provider.as_str(),
        &model,
        accounted.combo,
        counted,
        &body,
        attempts,
        elapsed,
    );
    // `Bytes::clone` is a refcount bump, not a copy (zero-copy bodies, ch.2).
    let relayed = client.unwrap_or_else(|| body.clone());
    upstream.stream = Box::pin(futures::stream::once(async move { relayed }));

    let Some(meta) = meta else {
        // Unparseable replies (relay of a provider's HTML error page, say)
        // have no usage to count; the headers keep their zeros on purpose.
        return defaults();
    };
    meta.with_savings_tokens(savings_tokens)
}

/// The price/latency class a completed request falls into.
///
/// Derived from the cost that was actually computed rather than from the model
/// name: a label derived from a name is unbounded cardinality, which is the one
/// thing the metrics cardinality cap exists to prevent.
fn price_family(meta: &ar_tokens::ResponseMeta) -> ar_obs::Family {
    if !meta.cost().priced {
        // No pricing row: the request still happened, and saying "unpriced" is
        // more useful to an operator than bucketing it by a zero cost.
        return ar_obs::Family::Unpriced;
    }
    match meta.cost().usd.micros {
        1..=1_000 => ar_obs::Family::Balanced,
        0 => ar_obs::Family::Economy,
        _ => ar_obs::Family::Frontier,
    }
}

/// Unix seconds now, for the ledger row's `created_at`.
///
/// Clock skew degrades to a zero timestamp rather than a panic: a row with no
/// timestamp is an accounting nuisance, an accounting panic is an outage.
fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        // before-epoch clocks are deeply strange but not a reason to refuse a
        // completion that already happened
        .unwrap_or(0)
}

/// Reads the upstream's accounting out of a completed body and, when a ledger
/// is configured, writes it. Returns `None` when the body is not parseable
/// JSON — that relay is the error path's own upstream message, which has no
/// usage to count and no right to be recounted from.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is a distinct accounting field; a struct would only move them"
)]
fn read_and_record(
    state: &AppState,
    key_id: Option<&str>,
    provider: &str,
    model: &str,
    combo: &str,
    dialect: Dialect,
    body: &[u8],
    attempts: u16,
    elapsed: Duration,
) -> Option<ResponseMeta> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let usage: &serde_json::Value = if matches!(dialect, Dialect::Ollama) {
        // Ollama's count fields live at the body's root
        // (`prompt_eval_count`, `eval_count`); every other dialect nests them
        // under `usage`, falling back to the root so a flat reply still counts.
        &value
    } else {
        value.get("usage").unwrap_or(&value)
    };
    let pricing = &state.config.prices;
    // The trace and audit rows are written here, so the latency has to be on
    // the meta before `record_obs` reads it, not added to the return value.
    let meta = ResponseMeta::from_upstream(pricing, provider, model, usage).with_latency(elapsed);
    // The observability half. Buffered replies pass here; streamed ones reach
    // the same observation through `UsageTee::observe` on the last poll, so
    // neither arm is counted twice and neither is missed.
    state.metrics.work.observe(&ar_obs::Request {
        provider,
        // The price/latency class of the model that served the request. Derived
        // from the pricing table rather than the model name so an operator's
        // naming cannot inflate the label set.
        family: price_family(&meta),
        decision: ar_obs::Decision::Primary,
        cache: ar_obs::Cache::Miss,
        queue: ar_obs::Queue::Direct,
        queue_pos: 0,
        attempts,
        tokens_in: u64::from(meta.tokens_in()),
        tokens_out: u64::from(meta.tokens_out()),
        cost_micros: meta.cost().usd.micros,
        duration_us: meta.latency_ms().saturating_mul(1_000),
        queue_wait_us: 0,
    });
    if let Some(ledger) = state.ledger.as_ref()
        && let Err(e) = ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_response(
                &ar_tokens::Who {
                    key_id: key_id.unwrap_or("anonymous"),
                    provider,
                    model,
                    combo,
                },
                usage,
                epoch_now(),
                pricing,
            )
    {
        // The response headers already carry the computed figure, and a
        // ledger write failure must not fail a request that succeeded — the
        // accounting gap is one operator-visible log line, not the client's
        // problem to be told about.
        tracing::warn!(error = %e, "usage ledger write failed");
    }
    record_obs(state, key_id, provider, model, &meta, attempts);
    Some(meta)
}

/// Writes the observability write path's half of one served request.
///
/// The trace line carries the full figure — provider, model, tokens, cost,
/// attempts, latency — because a `String` will hold it. The audit row carries
/// only what `ar_keys::AuditLine` can: a key id, two bounded enums and a
/// `&'static str`, so the row is the same width on disk whatever a provider
/// answers. That split is the crate's own design (`ar-obs/src/lib.rs`), not a
/// narrowing imposed here: the bounded row is what a reviewer can read without
/// trusting the trace.
fn record_obs(
    state: &AppState,
    key_id: Option<&str>,
    provider: &str,
    model: &str,
    meta: &ResponseMeta,
    attempts: u16,
) {
    let Some(obs) = state.obs.as_ref() else {
        return;
    };
    let line = format!(
        r#"{{"provider":{},"model":{},"tokens_in":{},"tokens_out":{},"cost_micros":{},"attempts":{attempts},"latency_ms":{}}}"#,
        serde_json::Value::from(provider),
        serde_json::Value::from(model),
        meta.tokens_in(),
        meta.tokens_out(),
        meta.cost().usd.micros,
        meta.latency_ms(),
    );
    let audit = ar_keys::AuditLine {
        at: epoch_now(),
        key_id: Strng::from(key_id.unwrap_or("anonymous")),
        action: ar_keys::Action::Admit,
        outcome: ar_keys::Outcome::Ok,
        // `Admit` is the action `ar-keys`' own lane controller records
        // (`admit.rs:621`), so this row joins the same vocabulary rather than
        // inventing a second action for "a request was routed".
        detail: "routed",
    };
    if let Err(e) = obs.record(&line, &audit) {
        tracing::warn!(error = %e, "audit row not written");
    }
}

/// A frame kept for accounting is capped at this many bytes: usage frames are
/// tiny, and a chunk bigger than this is payload, not bookkeeping.
const USAGE_FRAME_CAP: usize = 64 * 1024;
/// How many trailing frames are kept: the frame before the terminator is
/// where three of the four dialects carry usage, and one spare covers a
/// terminator that arrives as its own chunk.
const USAGE_TAIL_FRAMES: usize = 2;

/// Records a streamed reply's usage when the upstream ends.
///
/// Wave D buffered only the non-streaming arm, on the argument that counting a
/// stream costs SSE parsing on the hot path — which left the ledger blank for
/// exactly the traffic agentic clients send most, and left the pre-dispatch
/// budget gate under-enforcing it. This closes that split without putting any
/// parsing on the frame path: the tee keeps a first frame and a two-frame tail
/// (each capped at [`USAGE_FRAME_CAP`]), relays every byte untouched, and only
/// when the upstream ends does it parse what it kept for the dialect's usage
/// shape and record it. A stream no ledger is configured for is not wrapped at
/// all — the relay keeps its zero-overhead shape, same as before.
///
/// # What this deliberately does not do
///
/// * No record on a client disconnect: the tee drops with the response, and a
///   stream that never delivered its final frame is one whose usage was
///   never observed. Billing a guess is worse than billing nothing.
/// * No record when the dialect's usage frame is absent — an OpenAI stream
///   without `stream_options.include_usage` carries no counts in-band at all,
///   and the honest entry is none, not an estimate.
/// * Headers stay zeroed on streams: they are sent before the first byte, so
///   the count is not knowable yet; the ledger is where a stream's usage
///   lands.
/// * A usage frame split across chunk boundaries is skipped, not pieced
///   together: whole events per write is how every SSE server this relay
///   speaks to behaves, and a framing-scanner is a parser by another name.
struct UsageTee {
    /// The upstream stream, wrapped in place inside [`Upstream::stream`].
    inner: Pin<Box<dyn Stream<Item = Bytes> + Send>>,
    /// The first frame seen — Anthropic puts its input count in `message_start`.
    head: Option<Bytes>,
    /// The trailing [`USAGE_TAIL_FRAMES`] frames — where every dialect's final
    /// counts ride.
    tail: Vec<Bytes>,
    /// State cloned once per request (all `Arc`s); read only at record time.
    state: AppState,
    /// Accounting name for whoever the gate verified, or the anonymous
    /// sentinel — the same spelling the non-streaming arm records under.
    key_id: String,
    provider: String,
    model: Strng,
    /// The COMBO the client asked for, recorded beside the serving model.
    ///
    /// A different fact from `model`: the model is what the provider answered
    /// and the combo is what the client called. The pricing lookup needs the
    /// first; a per-combo figure needs the second, and two combos sharing a
    /// target would otherwise be indistinguishable.
    combo: String,
    dialect: Dialect,
    /// Carried so the observation reports what the router actually spent, the
    /// same figure `ar_upstream_attempts_total` counts.
    attempts: u16,
    /// Taken when the request dispatched, not when the last frame arrived: a
    /// duration measured from stream completion is the stream's length, not
    /// the request's cost. `observe` reports `dispatched.elapsed()` so the
    /// duration histogram holds the request's full wait, not a zero.
    dispatched: Instant,
}

impl Stream for UsageTee {
    type Item = Bytes;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Bytes>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(chunk)) => {
                this.retain(&chunk);
                Poll::Ready(Some(chunk))
            }
            Poll::Ready(None) => {
                this.record();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl UsageTee {
    /// Keeps a frame for accounting, cheapest-first: first frame once, then a
    /// two-frame tail. A frame over the cap is not kept — usage never rides in
    /// megabytes.
    fn retain(&mut self, chunk: &Bytes) {
        if chunk.len() > USAGE_FRAME_CAP {
            return;
        }
        if self.head.is_none() {
            self.head = Some(chunk.clone());
            return;
        }
        self.tail.push(chunk.clone());
        if self.tail.len() > USAGE_TAIL_FRAMES {
            self.tail.remove(0);
        }
    }

    /// The same observation [`read_and_record`] makes for a buffered reply.
    ///
    /// Split out so the streaming arm reaches `metrics.work` too: `record` runs
    /// on the last poll, which is the only moment a stream's usage is knowable,
    /// and the headers carrying the counts are already gone by then.
    fn observe(&self, usage: &serde_json::Value) {
        let meta = ResponseMeta::from_upstream(
            &self.state.config.prices,
            &self.provider,
            &self.model,
            usage,
        );
        self.record_obs(usage);
        self.state.metrics.work.observe(&ar_obs::Request {
            provider: &self.provider,
            family: price_family(&meta),
            decision: ar_obs::Decision::Primary,
            cache: ar_obs::Cache::Miss,
            queue: ar_obs::Queue::Direct,
            queue_pos: 0,
            attempts: self.attempts,
            tokens_in: u64::from(meta.tokens_in()),
            tokens_out: u64::from(meta.tokens_out()),
            cost_micros: meta.cost().usd.micros,
            duration_us: u64::try_from(self.dispatched.elapsed().as_micros()).unwrap_or(u64::MAX),
            queue_wait_us: 0,
        });
        self.record_obs(usage);
    }

    /// The streamed arm's half of [`record_obs`], with the same split: full
    /// figure in the trace line, bounded tuple in the audit row. Takes the
    /// already-parsed usage rather than re-reading the retained frames.
    fn record_obs(&self, usage: &serde_json::Value) {
        if self.state.obs.is_none() {
            return;
        }
        // Latency is the request's full wait, as the work metric reports it. The
        // trace and audit rows read 0 for every stream without it.
        let meta = ResponseMeta::from_upstream(
            &self.state.config.prices,
            &self.provider,
            &self.model,
            usage,
        )
        .with_latency(self.dispatched.elapsed());
        record_obs(
            &self.state,
            Some(&self.key_id),
            &self.provider,
            &self.model,
            &meta,
            self.attempts,
        );
    }

    /// Parses what was kept and records it — the only place this tee reads
    /// frame content, on the last poll, after every byte has been relayed.
    fn record(&self) {
        let Some(usage) = usage_from_frames(self.dialect, self.head.as_ref(), &self.tail) else {
            return;
        };
        self.observe(&usage);
        self.record_obs(&usage);
        let Some(ledger) = self.state.ledger.as_ref() else {
            return;
        };
        if let Err(e) = ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_response(
                &ar_tokens::Who {
                    key_id: &self.key_id,
                    provider: &self.provider,
                    model: &self.model,
                    combo: &self.combo,
                },
                &usage,
                epoch_now(),
                &self.state.config.prices,
            )
        {
            // Same contract as the non-streaming arm: a ledger write failure is
            // an operator-visible gap, never the client's problem.
            tracing::warn!(error = %e, "usage ledger write failed for a streamed reply");
        }
    }
}

/// Wraps a succeeded stream so its usage is recorded, or leaves it alone.
///
/// Only the arm that can carry usage is touched: a non-stream outcome is
/// buffered and recorded by [`read_and_record`] before this runs, and a failed
/// dispatch has no usage to count. The model the ledger rows for this stream
/// carry is the target's own, read off the outcome before it moves: a provider
/// serving several models needs its rows separated, and the combo name the
/// client spelled cannot separate them.
fn account_stream(
    outcome: &mut AttemptOutcome,
    state: &AppState,
    dialect: Dialect,
    key_id: Option<&str>,
    combo: &str,
    dispatched: Instant,
) {
    // Before the destructure below, which holds the outcome's borrow.
    let attempts = outcome.attempts();
    let AttemptOutcome::Succeeded {
        provider,
        model,
        upstream,
        ..
    } = outcome
    else {
        return;
    };
    // No guard: /metrics is unconditional, so a streamed reply always needs
    // observing, and gating the tee on the (optional) ledger meant a no-ledger
    // config recorded no streamed usage at all. The ledger write inside
    // `record` stays conditional, which is where "no ledger" actually means
    // something.
    let inner = std::mem::replace(&mut upstream.stream, Box::pin(futures::stream::empty()));
    upstream.stream = Box::pin(UsageTee {
        inner,
        head: None,
        tail: Vec::with_capacity(USAGE_TAIL_FRAMES),
        state: state.clone(),
        key_id: key_id.unwrap_or("anonymous").to_owned(),
        provider: provider.as_str().to_owned(),
        // `Arc<str>` copy is a refcount bump; `to_owned` on the borrowed tail
        // of the destructure would allocate a second one for nothing.
        model: model.clone(),
        combo: combo.to_owned(),
        dialect,
        attempts,
        dispatched,
    });
}

/// Reads the usage value out of what a stream retained, per dialect.
///
/// Every branch works on whole `data:` lines and whole NDJSON lines only; a
/// line that fails to parse is dropped, which is the split-frame ceiling
/// documented on [`UsageTee`].
fn usage_from_frames(
    dialect: Dialect,
    head: Option<&Bytes>,
    tail: &[Bytes],
) -> Option<serde_json::Value> {
    let data_frames = |chunk: &Bytes| {
        chunk
            .split(|byte| *byte == b'\n')
            .filter_map(|line| line.strip_prefix(b"data: "))
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .collect::<Vec<_>>()
    };
    match dialect {
        Dialect::OpenAi => tail.iter().rev().find_map(|chunk| {
            data_frames(chunk).into_iter().rev().find_map(|frame| {
                frame
                    .get("usage")
                    .filter(|usage| usage.is_object())
                    .cloned()
            })
        }),
        Dialect::Anthropic => {
            // Input rides in the very first frame (`message_start`), output in
            // the last (`message_delta`) — both are kept, so the compose is
            // one lookup each.
            let input = head
                .and_then(|chunk| {
                    data_frames(chunk).into_iter().find(|frame| {
                        frame.get("type").and_then(serde_json::Value::as_str)
                            == Some("message_start")
                    })
                })
                .and_then(|frame| frame.pointer("/message/usage").cloned());
            let output = tail
                .iter()
                .rev()
                .find_map(|chunk| {
                    data_frames(chunk).into_iter().rev().find(|frame| {
                        frame.get("type").and_then(serde_json::Value::as_str)
                            == Some("message_delta")
                    })
                })
                .and_then(|frame| frame.get("usage").cloned());
            match (input, output) {
                // The delta names the input count too when a re-framed stream
                // has it: its `message_start` cannot know it yet.
                (Some(input), Some(output)) => Some(serde_json::json!({
                    "input_tokens": output
                        .get("input_tokens")
                        .or_else(|| input.get("input_tokens"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    "cache_read_input_tokens": input.get("cache_read_input_tokens").cloned().unwrap_or(serde_json::Value::Null),
                    "cache_creation_input_tokens": input.get("cache_creation_input_tokens").cloned().unwrap_or(serde_json::Value::Null),
                    "output_tokens": output.get("output_tokens").cloned().unwrap_or(serde_json::Value::Null),
                })),
                (Some(input), None) if input.get("input_tokens").is_some() => Some(input),
                _ => None,
            }
        }
        Dialect::Responses => tail.iter().rev().find_map(|chunk| {
            data_frames(chunk)
                .into_iter()
                .rev()
                .find(|frame| {
                    frame.get("type").and_then(serde_json::Value::as_str)
                        == Some("response.completed")
                })
                .and_then(|frame| frame.pointer("/response/usage").cloned())
        }),
        Dialect::Ollama => tail.iter().rev().find_map(|chunk| {
            chunk
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
                .rev()
                .find(|frame| frame.get("done").and_then(serde_json::Value::as_bool) == Some(true))
        }),
    }
}

/// Runs one `pipeline` request: each target is a stage, in order.
///
/// The engine is [`ar_route::dispatch_pipeline`]: the chain is strictly
/// sequential, stage N+1's request carries stage N's answer as its only
/// turn, and only the final stage's response is the answer. A failed stage
/// ends the chain — a fallback here would be a different strategy wearing a
/// pipeline's name, because the next provider answers a *different* question
/// when it receives the previous stage's output.
async fn pipeline_response(
    state: &AppState,
    canonical: &CanonicalRequest,
    plan: &RoutePlan,
    dialect: Dialect,
    dispatched: Instant,
) -> Response {
    let combo = state.config.combo(canonical.model.as_ref());
    let stage_candidates = state.config.candidates(combo);
    if stage_candidates.is_empty() {
        return error_because(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_provider",
            "nothing_configured",
            "the pipeline has no dispatchable stage; `ar doctor` lists what is missing",
        );
    }
    let outcome = match tokio::time::timeout(
        state.config.stream_deadline(canonical.model.as_ref()),
        ar_route::dispatch_pipeline(&stage_candidates, canonical, state.exec.as_ref()),
    )
    .await
    {
        Err(_elapsed) => {
            let deadline = state.config.stream_deadline(canonical.model.as_ref());
            return error_because(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "model_deadline",
                &format!(
                    "model {:?} produced no pipeline answer within {}s",
                    canonical.model.as_ref(),
                    deadline.as_secs()
                ),
            );
        }
        Ok(outcome) => outcome,
    };
    state
        .metrics
        .observe_attempts(u64::try_from(outcome.trace.len()).unwrap_or(u64::MAX));

    // A chain that ended early has no answer to relay: the stage that failed
    // is named, because a client that sent a pipeline wants to know *where* it
    // stopped, not that the gateway is unwell.
    let Some(upstream) = outcome.upstream else {
        let failed = outcome.failed_at.unwrap_or(0);
        let reason = outcome
            .trace
            .get(failed)
            .and_then(|v| v.error.clone())
            .unwrap_or_else(|| "the pipeline stopped before its last stage".to_owned());
        return error_because(
            outcome.status(),
            "pipeline_failed",
            "stage_failed",
            &format!("stage {} of {} failed: {reason}", failed + 1, outcome.trace.len()),
        );
    };

    let outcome = AttemptOutcome::Succeeded {
        // The **last** stage served the answer — the earlier stages are
        // inputs to it, not the answer. Naming the first stage would put a
        // provider in the `x-ar-decision` header that did not serve the
        // client, which is the one field an operator reads to find out who
        // answered.
        provider: outcome
            .trace
            .last()
            .map_or_else(|| ProviderId::new("-"), |v| v.provider.clone()),
        model: plan
            .chain
            .last()
            .map_or_else(|| ar_route::Strng::from("-"), |t| t.model.clone()),
        attempts: u16::try_from(outcome.trace.len()).unwrap_or(u16::MAX),
        upstream,
    };
    let meta = ResponseMeta::default().with_latency(dispatched.elapsed());
    decorate(
        outcome,
        state.resilience.clone(),
        canonical.stream,
        dialect,
        plan.strategy,
        Stages {
            failures: FailureSink::none(),
            continuation: None,
            compression: "default;engines=-".to_owned(),
            cache_state: ar_cache::CacheState::Bypass,
            cache_key: None,
            planned_target: None,
            cache: None,
            model: ar_route::Strng::from(canonical.model.as_ref()),
            cache_control: CacheControl::default(),
            meta,
        },
    )
}

/// Runs one `fusion` request: fan the panel out, then optionally synthesize.
///
/// The panel fan-out is [`ar_route::dispatch_fusion`], unchanged — it returns the
/// first member's 2xx and the full trace. With no judge configured (the
/// reference's default) that IS the answer and it is relayed as one. With a
/// judge, the panel's texts are composed into a second dispatch whose answer
/// replaces it; any failure of that second call — refused, non-2xx, unreadable —
/// degrades to the panel answer, because throwing away a completed fan-out over
/// one synthesis call is the worse outcome for the client.
async fn fusion_response(
    state: &AppState,
    canonical: &CanonicalRequest,
    plan: &RoutePlan,
    dialect: Dialect,
    dispatched: Instant,
    savings_tokens: u32,
) -> Response {
    let combo = state.config.combo(canonical.model.as_ref());
    let panel_candidates = state.config.candidates(combo);
    if panel_candidates.is_empty() {
        return error_because(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_provider",
            "nothing_configured",
            "the fusion panel has no dispatchable target; `ar doctor` lists what is missing",
        );
    }
    let mut outcome = match tokio::time::timeout(
        state.config.stream_deadline(canonical.model.as_ref()),
        ar_route::dispatch_fusion(
            canonical.session.as_deref(),
            canonical.model.as_ref(),
            &panel_candidates,
            canonical,
            state.exec.as_ref(),
        ),
    )
    .await
    {
        Err(_elapsed) => {
            let deadline = state.config.stream_deadline(canonical.model.as_ref());
            return error_because(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "model_deadline",
                &format!(
                    "model {:?} produced no panel answer within {}s",
                    canonical.model.as_ref(),
                    deadline.as_secs()
                ),
            );
        }
        Ok(outcome) => outcome,
    };

    let panel_attempts = outcome.trace.len();
    state.metrics.observe_attempts(panel_attempts as u64);

    // Build the panel texts before the winner's stream is consumed: the
    // synthesis needs every member's answer, and a streamed panel member would
    // have to be read twice otherwise.
    // The panel texts move into the judge call rather than being borrowed: an
    // outcome's own `Upstream` is a boxed stream that is `Send` but not `Sync`,
    // so holding `&outcome` across this await would make the handler's future
    // non-`Send`. The texts are all this needs, and they are plain data.
    let panel_answers = std::mem::take(&mut outcome.answers);
    let synthesized = match plan.judge.as_ref() {
        Some(judge) => Some(fusion_synthesis(state, canonical, panel_answers, judge).await),
        None => None,
    };

    let (upstream, provider, attempts) = match synthesized {
        // The judge answered: its own body is the response, relayed exactly as
        // it arrived, and the panel's members plus the judge's dispatch is the
        // request's real cost.
        //
        // A judged outcome always carries the judge's response — `synthesize`
        // clears `judged` on the two paths that produce none (a refused panel,
        // and a 2xx whose body extracted to nothing) — but the type cannot say
        // so, so an unreachable state degrades to the panel's winner rather than
        // panicking a request.
        Some(JudgeOutcome {
            judged: true,
            upstream: Some(upstream),
            ..
        }) => (
            upstream,
            plan.judge.clone(),
            panel_attempts.saturating_add(1) as u16,
        ),
        // No judge, or a judge that failed: the panel's first 2xx is the answer.
        // The provider is read before the upstream is moved out of the outcome.
        _ => {
            let provider = outcome.winner().cloned();
            match outcome.upstream {
                Some(upstream) => (upstream, provider, panel_attempts as u16),
                None => {
                    return fusion_empty_panel(&outcome);
                }
            }
        }
    };

    let outcome = AttemptOutcome::Succeeded {
        provider: provider.unwrap_or_else(|| ProviderId::new("unknown")),
        // The fusion panel's judge or winner is the serving target. Its provider
        // is resolved above, but the model stays whatever the request carried:
        // `synthesize` dispatches the judge with the request's own canonical
        // body, and a panel member's answer is not a model this outcome can
        // name. Stamping the combo keeps the header honest.
        model: Strng::from(canonical.model.as_ref()),
        attempts,
        upstream,
    };
    let meta = ResponseMeta::default()
        .with_latency(dispatched.elapsed())
        .with_savings_tokens(savings_tokens);
    decorate(
        outcome,
        state.resilience.clone(),
        canonical.stream,
        dialect,
        plan.strategy,
        Stages {
            // Fusion has no client key in scope, so its cut streams are not logged.
            failures: FailureSink::none(),
            continuation: None,
            compression: String::new(),
            cache_state: CacheState::Bypass,
            cache_key: None,
            planned_target: None,
            cache: None,
            model: canonical.model.clone(),
            cache_control: CacheControl::default(),
            meta,
        },
    )
}

/// The judge dispatch for one fusion panel, with its failure captured rather
/// than propagated.
///
/// The panel texts come from the fan-out's own buffer ([`FusionOutcome::answers`]),
/// so synthesis costs one upstream call and no extra reads. The judge's request
/// is non-streaming: a fusion answer is a complete answer, and relaying the
/// judge's token stream would hand the client a stream in place of the fusion
/// it asked for.
async fn fusion_synthesis(
    state: &AppState,
    canonical: &CanonicalRequest,
    answers: Vec<(ProviderId, String)>,
    judge: &ProviderId,
) -> JudgeOutcome {
    let panel = JudgePanel::new(answers);
    let task = request_text(canonical);
    synthesize(
        &panel,
        &JudgeTarget::provider(judge.as_str()),
        &task,
        state.exec.as_ref(),
    )
    .await
}

/// The user's request as the judge should see it: the original messages, or a
/// `task` label when the inbound body carried no readable text.
fn request_text(canonical: &CanonicalRequest) -> String {
    canonical
        .body
        .iter()
        .filter(|b| !b.is_ascii_whitespace())
        .map(|b| *b as char)
        .take(4096)
        .collect::<String>()
        .replace("\n", " ")
        .replace("\r", " ")
        .replace('"', " ")
}

/// The 502/503 a fusion panel produces when no member answered.
fn fusion_empty_panel(outcome: &ar_route::FusionOutcome) -> Response {
    let status = outcome.status();
    error_because(
        status,
        if status == StatusCode::SERVICE_UNAVAILABLE {
            "no_panel"
        } else {
            "panel_unavailable"
        },
        "fusion_panel",
        &format!(
            "no fusion panel member answered: {}",
            outcome
                .trace
                .iter()
                .map(|v| format!("{}={:?}", v.provider.as_str(), v.status))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
}

/// What the cache and compression stages decided, for the response to carry.
///
/// One struct because these four are one decision, made in one order and read in
/// one place: the compression plan resolved, then the cache consulted, then
/// whatever the cache said about this request. Threading them as four parameters
/// is what let [`decorate`] reach eight arguments.
/// What a cut stream needs to resume on another target: the executor, the
/// canonical body the client sent, and the one target that takes over. Built only
/// for a streamed OpenAI chat request, served by an OpenAI-wire provider, when a
/// later target on the chain is marked `continue_on_cut` and also speaks OpenAI.
#[derive(Clone)]
struct Continuation {
    exec: Arc<dyn ArExec>,
    body: Bytes,
    target: (ProviderId, Strng),
}

/// The assistant text a streamed OpenAI chat reply had already sent, read from its
/// SSE frames as they pass the relay.
///
/// A reply that called a tool is not resumable: a half-sent call's arguments cannot
/// be continued as prose. A reply that already finished is not resumable either,
/// because nothing is missing.
#[derive(Default)]
struct SentText {
    /// A frame split across chunks, kept until its newline arrives.
    pending: Vec<u8>,
    text: String,
    tools: bool,
    finished: bool,
}

impl SentText {
    fn feed(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                self.finished = true;
                continue;
            }
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            let Some(choice) = frame.pointer("/choices/0") else {
                continue;
            };
            if let Some(piece) = choice
                .pointer("/delta/content")
                .and_then(serde_json::Value::as_str)
            {
                self.text.push_str(piece);
            }
            if choice.pointer("/delta/tool_calls").is_some() {
                self.tools = true;
            }
            if choice.get("finish_reason").is_some_and(|f| !f.is_null()) {
                self.finished = true;
            }
        }
    }

    /// The prefill to resume with, or `None` when the reply cannot honestly be continued.
    fn resumable(&self) -> Option<&str> {
        (!self.tools && !self.finished && !self.text.is_empty()).then_some(self.text.as_str())
    }
}

/// The canonical body for a resumed request: the client's own messages, then the
/// text already sent as a trailing assistant message marked `prefix`, aimed at
/// `model`. `None` when the body has no `messages` array to extend.
fn resume_body(body: &[u8], prefill: &str, model: &str) -> Option<Bytes> {
    let mut request: serde_json::Value = serde_json::from_slice(body).ok()?;
    request
        .get_mut("messages")?
        .as_array_mut()?
        .push(serde_json::json!({ "role": "assistant", "content": prefill, "prefix": true }));
    request["model"] = serde_json::json!(model);
    request["stream"] = serde_json::json!(true);
    serde_json::to_vec(&request).ok().map(Bytes::from)
}

/// Splices a second target onto a cut OpenAI chat stream, so the client reads one
/// reply. Returns the stream to relay and the cut flag that decides how it ends.
///
/// One continuation per request: if the second target is cut too, the reply ends
/// cut. If the continuation cannot be dispatched, the reply ends cut, and the
/// relay's truncation frame says so, as it does without this wrapper.
fn resumed(stream: ChunkStream, cut: CutFlag, plan: Continuation) -> (ChunkStream, CutFlag) {
    let ends_cut: CutFlag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let out = Arc::clone(&ends_cut);
    let relayed: ChunkStream = Box::pin(async_stream::stream! {
        let mut sent = SentText::default();
        let mut first = stream;
        while let Some(chunk) = first.next().await {
            sent.feed(&chunk);
            yield chunk;
        }
        if !cut.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let (provider, model) = plan.target.clone();
        let Some(prefill) = sent.resumable().map(str::to_owned) else {
            out.store(true, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        let Some(body) = resume_body(&plan.body, &prefill, &model) else {
            out.store(true, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        let canonical = CanonicalRequest::new(&model, body).with_stream(true);
        let upstream = match plan.exec.post_chat(&provider, &canonical).await {
            Ok(upstream) if upstream.status.is_success() => upstream,
            _ => {
                out.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        };
        let mut second = upstream.stream;
        while let Some(chunk) = second.next().await {
            yield chunk;
        }
        if upstream.cut.load(std::sync::atomic::Ordering::SeqCst) {
            out.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    (relayed, ends_cut)
}

/// The wire shape the served provider's answer arrives in, when it has to be
/// re-spelled for `dialect`. `None` when it already is that shape, or when the
/// wire has no lift (Kiro, Clova, Custom and the rest relay their own framing).
///
/// A stream arrives as OpenAI SSE from the OpenAI wire, and from Bedrock and
/// GitLab, whose streams `ar_server::exec` re-spells on the way in. Anthropic,
/// Responses and Gemini streams are lifted by `reframe`. A whole Gemini reply is
/// rendered as OpenAI by `unwrap_response` before it gets here.
fn reframe_source(
    state: &AppState,
    served: Option<&ProviderId>,
    dialect: Dialect,
    stream: bool,
) -> Option<ar_registry::WireFormat> {
    use ar_registry::WireFormat;
    let served = served?;
    let wire = state
        .config
        .providers
        .iter()
        .find(|p| &p.id == served)?
        .wire_format;
    let source = match wire {
        WireFormat::Anthropic => WireFormat::Anthropic,
        WireFormat::OpenaiResponses => WireFormat::OpenaiResponses,
        WireFormat::Openai | WireFormat::Bedrock | WireFormat::Gitlab => WireFormat::Openai,
        // A streamed Gemini call is SSE of `GenerateContentResponse`s; a whole
        // reply is already rendered as OpenAI before it gets here.
        WireFormat::Gemini if stream => WireFormat::Gemini,
        WireFormat::Gemini => WireFormat::Openai,
        _ => return None,
    };
    let already = matches!(
        (source, dialect),
        (WireFormat::Openai, Dialect::OpenAi)
            | (WireFormat::Anthropic, Dialect::Anthropic)
            | (WireFormat::OpenaiResponses, Dialect::Responses)
    );
    (!already).then_some(source)
}

/// Swaps a streamed success's relay for its re-framed form. Runs before
/// [`account_stream`], so the usage tee reads the frames the client receives.
fn reframe_stream(
    outcome: &mut AttemptOutcome,
    dialect: Dialect,
    model: &str,
    source: ar_registry::WireFormat,
) {
    let AttemptOutcome::Succeeded { upstream, .. } = outcome else {
        return;
    };
    let inner = std::mem::replace(&mut upstream.stream, Box::pin(futures::stream::empty()));
    upstream.stream = reframe::stream(inner, dialect, model, source);
}

/// The buffered reply re-spelled in `dialect`, or `None` when the body is not a
/// completion in its source shape: an error object relays as it arrived.
fn reframed_body(dialect: Dialect, source: ar_registry::WireFormat, body: &[u8]) -> Option<Bytes> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let completion = reframe::completion(source, value)?;
    serde_json::to_vec(&reframe::reply(dialect, completion))
        .ok()
        .map(Bytes::from)
}

/// The continuation a streamed reply may take, when one is allowed. The first
/// later target on the chain that is marked `continue_on_cut` and speaks OpenAI
/// takes over. See [`Continuation`] for the conditions.
fn continuation_for(
    state: &AppState,
    chain: &[ChainTarget],
    served: Option<&ProviderId>,
    dialect: Dialect,
    stream: bool,
    body: &Bytes,
) -> Option<Continuation> {
    if !stream || !matches!(dialect, Dialect::OpenAi) {
        return None;
    }
    let served = served?;
    let openai = |id: &ProviderId| {
        state
            .config
            .providers
            .iter()
            .any(|p| &p.id == id && matches!(p.wire_format, ar_registry::WireFormat::Openai))
    };
    if !openai(served) {
        return None;
    }
    let target = chain
        .iter()
        .skip_while(|t| &t.provider != served)
        .skip(1)
        .find(|t| {
            state
                .config
                .providers
                .iter()
                .any(|p| p.id == t.provider && p.continue_on_cut)
                && openai(&t.provider)
        })?;
    Some(Continuation {
        exec: Arc::clone(&state.exec),
        body: body.clone(),
        target: (target.provider.clone(), target.model.clone()),
    })
}

/// Where a stream cut after its headers is written: the usage ledger, and the
/// client key that asked. With no ledger, nothing is written.
#[derive(Clone, Default)]
struct FailureSink {
    ledger: Option<Arc<std::sync::Mutex<ar_tokens::Ledger>>>,
    key_id: String,
}

impl FailureSink {
    /// A sink that writes nothing.
    fn none() -> Self {
        Self::default()
    }
}

/// Logs a streamed reply that was cut after its headers, when the body is dropped.
///
/// Written on drop, so a clean finish and a client that goes away both write
/// nothing: only a body the upstream broke is a failed request.
struct CutRecord {
    sink: FailureSink,
    /// The provider whose body was cut.
    provider: String,
    model: String,
    cut: CutFlag,
}

impl Drop for CutRecord {
    fn drop(&mut self) {
        if !self.cut.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let Some(ledger) = self.sink.ledger.as_ref() else {
            return;
        };
        if let Err(e) = ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_failure(
                &self.sink.key_id,
                &self.provider,
                &self.model,
                502,
                "upstream stream cut after its headers",
                epoch_now(),
            )
        {
            // A log write must not turn a cut stream into a different failure.
            tracing::warn!(error = %e, "failure log write failed");
        }
    }
}

/// Keeps `guard` alive for as long as `stream` is read, so it drops with the body.
fn guarded<G: Send + 'static>(stream: ChunkStream, guard: G) -> ChunkStream {
    Box::pin(async_stream::stream! {
        let _guard = guard;
        let mut stream = stream;
        while let Some(chunk) = stream.next().await {
            yield chunk;
        }
    })
}

struct Stages {
    /// The applied `x-ar-compression` value, echoed on the response.
    compression: String,
    /// The cache verdict this request earned, header and all.
    cache_state: CacheState,
    /// The key a non-streaming request was looked up under, when it had one.
    cache_key: Option<CacheKey>,
    /// The `provider/model` the plan named first. A reply is stored only when
    /// that target served it, so a fallback's reply never sits under the winner's
    /// key (K4). `None` when the request had no chain.
    planned_target: Option<String>,
    /// The cache itself, when one is configured.
    cache: Option<Arc<Cache>>,
    /// The model the client named, for [`MODEL_HEADER`]. Carried rather than
    /// a sixth parameter because `decorate` is the one emit point and the
    /// request's own spelling is the one a client comparing headers to its
    /// request can recognize.
    model: Strng,
    /// What the cache-control headers asked for, for the one store the relay
    /// makes. Rides here for the same reason the model does: the tee is
    /// `decorate`'s to call and the parsing is the handler's to do once.
    cache_control: CacheControl,
    /// What the request spent and how long it took, for the `x-ar-*` accounting
    /// headers.
    ///
    /// Carried here rather than recomputed in [`decorate`] because the emit
    /// point has no clock and no body: the arithmetic happens once, where the
    /// measurement is taken.
    meta: ResponseMeta,
    /// Where a stream cut after its headers is logged. Empty where no client key
    /// is in scope, so nothing is written.
    failures: FailureSink,
    /// The second target a cut OpenAI chat reply may resume on, when one is allowed.
    continuation: Option<Continuation>,
}

/// The cache key for a canonical request: `blake3(tenant | served | body)`.
///
/// `served` is the `provider/model` that will answer, not the client alias (K4).
///
/// `ar_cache::key::request_key` is the `docs/04` key. The tenant is the client
/// key the gate authenticated, or `anonymous` on an ungated gateway. A single
/// shared tenant let one client's cached reply answer another client's identical
/// request, and the hit skipped the second client's spend record.
/// `None` for a body that is not JSON, which the translator has already refused
/// before this runs, so the arm is unreachable rather than a guess.
fn request_key(
    canonical: &CanonicalRequest,
    served: &str,
    tenant: &str,
    caller_key: Option<&str>,
    dialect: Dialect,
) -> Option<CacheKey> {
    let body = serde_json::from_slice::<serde_json::Value>(&canonical.body).ok()?;
    // A stored reply is in the client's dialect once re-framed, so each dialect
    // keeps its own namespace: an OpenAI client never replays an Anthropic body.
    let tenant = format!("{tenant}/{}", dialect.route());
    Some(ar_cache::key::request_key_with(
        &tenant, served, &body, caller_key,
    ))
}

/// K4: whether a reply may be stored under the planned winner's key.
///
/// Only the planned winner's reply is kept. A fallback's reply stored under the
/// winner's key would replay for the whole TTL after the winner recovered. A
/// request with no planned target has no winner to be wrong about, so its reply
/// is kept as it always was.
fn storable_reply(planned: Option<&str>, served: &str) -> bool {
    planned.is_none_or(|planned| planned == served)
}

/// The cache-control headers one request carried, as the dispatch needs them.
///
/// Both spellings are read on every field, native first then the reference's
/// alias — the same precedence the compression header established in wave A: a
/// client configured against either gateway sends something this server
/// answers, and a client sending both gets its native word.
#[derive(Clone, Debug, Default)]
struct CacheControl {
    /// `no-cache: true` — the both-sides kill. No lookup, no store, and the
    /// verdict reads `bypass` because that is what happened.
    no_cache: bool,
    /// `cache-no-store: true` — the write-side kill. The lookup still runs.
    no_store: bool,
    /// The caller's key segment, folded into the digest on both sides.
    key: Option<Strng>,
    /// The caller-requested TTL, already normalized to a `Duration`. The
    /// policy's ceiling still wins at store time.
    ttl: Option<Duration>,
}

impl CacheControl {
    /// Reads the four fields off the request headers.
    ///
    /// Grammar decisions are the reference's, not ours: the two boolean
    /// fields match the exact string `true` after ASCII case folding, and an
    /// unparseable or non-positive TTL is "no opinion" rather than a 400 — a
    /// cache hint is advisory, and refusing a request over one would make a
    /// misconfigured client's cache control into an availability problem.
    fn read(headers: &HeaderMap) -> Self {
        fn pair<'a>(headers: &'a HeaderMap, native: &str, alias: &str) -> Option<&'a str> {
            [native, alias]
                .into_iter()
                .find_map(|name| headers.get(name).and_then(|v| v.to_str().ok()))
                .map(str::trim)
                .filter(|v| !v.is_empty())
        }
        let is_true = |native: &str, alias: &str| {
            pair(headers, native, alias).is_some_and(|v| v.eq_ignore_ascii_case("true"))
        };
        let ttl = pair(headers, CACHE_TTL_HEADER, CACHE_TTL_HEADER_ALIAS)
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .map(|v| {
                // The reference's heuristic: over 100_000 is milliseconds as
                // written, anything else is seconds.
                let millis = if v > 100_000 { v } else { v * 1_000 };
                Duration::from_millis(millis)
            });
        Self {
            no_cache: is_true(NO_CACHE_HEADER, NO_CACHE_HEADER_ALIAS),
            no_store: is_true(NO_STORE_HEADER, NO_STORE_HEADER_ALIAS),
            key: pair(headers, CACHE_KEY_HEADER, CACHE_KEY_HEADER_ALIAS).map(Strng::from),
            ttl,
        }
    }
}

/// Why a request could not be routed.
///
/// Its own enum rather than a `Response` so [`resolve`] can hand back a
/// `Result<RoutePlan, RouteReject>` without a 128-byte error arm; the caller turns
/// the reason into a 400. One enum rather than a `String` so a new rejection
/// cannot be written by accident as a bare sentence — and there is exactly one
/// today, which is the point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RouteReject {
    /// The request `model` names no configured combo. Carries the 400 text, which
    /// lists the ids that do exist.
    UnknownModel(String),
}

/// What one request resolved to: the dispatch chain, and the strategy that chose
/// it.
///
/// They travel together because `x-ar-decision` names the strategy, and a combo's
/// strategy is not the server's — `config.yaml`'s `cheap` is `cost-optimized`
/// while `default` is `lkgp`. The combo's `compression:` step rides along for
/// the same reason: compression is per-combo, and a client asking for `cheap`
/// must not inherit `default`'s engine. The chain carries the combo's `pool:`
/// bench at its tail for the third: that is this combo's failover depth, not
/// another combo's.
/// `Clone` rather than a borrow so the caller can hold the plan while the `&state`
/// it was resolved from is also on the stack; the struct is three fields, one of
/// them a `Vec` of small targets.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RoutePlan {
    /// Targets to try, winner first. Each carries the model that target
    /// serves, because a pool entry like `kilocode/deepseek-v4-flash` is a
    /// provider *and* a model: a chain of bare providers sends the combo id
    /// three times and never reaches the provider's own spelling.
    pub(crate) chain: Vec<ChainTarget>,
    pub(crate) strategy: Strategy,
    /// The resolved combo's compression setting, if it declared one.
    compression: Option<Step>,
    /// The `fusion` judge to synthesize the panel with, when the combo named
    /// one. `None` is the reference's own default and means the panel answers.
    judge: Option<ProviderId>,
}

/// Checks the credential when an [`crate::keys::AuthGate`] is configured, and
/// reports which key passed it.
///
/// No gate means no check, which is only safe because the same configuration
/// binds loopback-only — see [`crate::app::bind_addr`]. The two decisions live in
/// one struct on purpose: "no auth" and "public bind" must never be settable
/// independently.
///
/// `Ok(Some(key_id))` is a request a verified key is spending its own budget on;
/// `Ok(None)` is anonymous — the usage ledger writes those two apart, because a
/// server with no gate that recorded under the same id as a verified request
/// would read them as one. `Err` is the 401 to send: the body is built here
/// rather than at the call site so there is one place that knows a refused
/// credential is `code: invalid_api_key` — the OpenAI-compatible spelling a
/// client branches on — with a `reason` that separates "you sent nothing" from
/// "what you sent is wrong" without either reaching the client verbatim.
///
/// The 401 rides in a `Box` because the happy path is a zero-length `Option` and
/// a `Response` is wide; keeping the error arm a pointer keeps the check
/// allocation-free until it actually fires, which is the design the old
/// `Option<Response>` return was earning and the result's own comment refused.
///
/// The reason is never the token, for the reason in [`crate::keys`]: this
/// response is a cacheable 401 a browser may keep.
pub(crate) fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
) -> Result<Option<String>, Box<Response>> {
    // `path` is only read when a gate exists, which is the only case where a
    // tokenized-alias URL can carry a credential at all.
    let Some(gate) = state.auth.as_deref() else {
        return Ok(None);
    };
    // Resolved and checked once, and the mode decides what a refusal means — so
    // the matrix and the policy cannot drift into two answers for one request.
    match gate.authorize((headers, path), state.auth_mode) {
        Ok(key_id) => Ok(key_id),
        Err(reason) => Err(Box::new(error_because(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "credential_rejected",
            &reason,
        ))),
    }
}
/// The 415 for a body that is not JSON, or `None` when it is.
///
/// RFC 9110 §8.3: the media type is the client's own statement of the body's
/// shape, and every one of these routes parses JSON. A `text/plain` body that
/// reached the translator would be reported as a 400 about JSON syntax, which
/// sends an operator to the wrong place entirely.
///
/// `application/json; charset=utf-8` is accepted — the parameter is not a
/// different media type — and an absent `Content-Type` is refused, because "I
/// did not say" is not "I said JSON" and the reference gateway refuses it too
/// (`chat/completions/route.ts:105-118`).
///
/// Ponytail: no `Content-Encoding` check. A gzipped body is one this server cannot
/// translate either way, and it arrives as the translator's 400 rather than a 415
/// dressed up as a different error.
pub(crate) fn require_json(headers: &HeaderMap) -> Option<Response> {
    let ok = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim())
        .is_some_and(|essence| essence.eq_ignore_ascii_case("application/json"));
    if ok {
        return None;
    }
    Some(error_because(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
        "content_type_not_json",
        "this route accepts `application/json`; the request declared a different or absent Content-Type",
    ))
}

/// Whether the body states `stream` at all.
///
/// The distinction the `Accept` rule turns on: a client that said `false` has
/// answered, and a client that said nothing has not. Parsed from the raw bytes
/// rather than from [`CanonicalRequest::stream`], because by the time the
/// canonical request exists the two cases are the same `false`.
///
/// An explicit `null` counts as not declared: it is what a client that builds its
/// body programmatically sends for "unset", and reading it as a declaration would
/// refuse the `Accept` opt-in to a client that never opted out of it.
///
/// Ponytail: a whole-body `Value` parse for one boolean, on a path that only runs
/// when `Accept` already named SSE. `serde_json` has no partial-parse API, and a
/// hand-rolled scan for `"stream"` would be a second JSON parser.
fn declares_stream(body: &Bytes) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("stream").cloned())
        .is_some_and(|s| !s.is_null())
}

/// Whether `Accept` names SSE and does not also name JSON.
///
/// `text/event-stream` *and* `application/json` together is the OpenAI and Vercel
/// AI SDK non-stream signature — `doGenerate()` sends exactly that pair and
/// parses the response as JSON — so a request naming both is a JSON request. The
/// reference gateway's rule, and the reason a naive `contains("text/event-stream")`
/// streams a client that cannot read the stream it asked for
/// (`open-sse/utils/aiSdkCompat.ts:95-141`).
///
/// A `*/*` wildcard stays non-streaming too: `curl` sends it by default and means
/// "anything", and streaming a `curl -d …` is how a plain invocation ends up
/// printing SSE frames to a terminal that expected one object.
fn accept_forces_stream(headers: &HeaderMap) -> bool {
    let Some(accept) = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let accept = accept.to_ascii_lowercase();
    accept.contains("text/event-stream") && !accept.contains("application/json")
}

/// Resolves the request `model` into a [`RoutePlan`].
///
/// Three cases, in this order:
///
/// * an `auto/*` alias, scored by `ar_route::simulate_route` over the default
///   combo's candidates;
/// * a configured combo id;
/// * no combo table at all — the environment path, where the flat provider list
///   is the chain and `model` names a provider rather than a combo.
///
/// An unknown name with a combo table present is a 400 that lists what exists.
/// Silently falling back to the default chain is the defect this fixes: a client
/// asking for `gpt-4o` when the server serves `cheap` got `cheap`'s answer and
/// no way to know.
pub(crate) fn resolve(
    state: &AppState,
    canonical: &CanonicalRequest,
) -> Result<RoutePlan, RouteReject> {
    let model = canonical.model.as_ref();

    if let Some(chain) = auto_chain(state, model) {
        return Ok(RoutePlan {
            chain,
            strategy: state.config.strategy,
            compression: state.config.default_compression(),
            judge: None,
        });
    }

    match state.config.default_combo() {
        // A combo table is authoritative: a model that is not one of its ids is
        // a client error, not a hint.
        DefaultChain::Combo(_) => match state.config.combo(model) {
            Some(combo) => {
                // A combo whose *strategy* is an `auto/*` family name routes
                // through the virtual factory over its own targets — the same
                // sixteen-factor score the `auto/*` *model* name uses. Before
                // this, the two parsers disagreed about the name
                // (`ar-config` said `Auto`, `ar-route` said `Deferred`), the
                // server used the router's answer, and every request through
                // such a combo answered 501 naming a strategy this build
                // ships.
                let chain = match combo.strategy {
                    Strategy::Auto(variant) => {
                        auto_chain_over(state, combo, variant).unwrap_or_else(|| {
                            chain(state, combo, canonical.session.as_deref(), model)
                        })
                    }
                    _ => chain(state, combo, canonical.session.as_deref(), model),
                };
                Ok(RoutePlan {
                    chain,
                    strategy: combo.strategy,
                    compression: combo.compression,
                    judge: judge(state, combo),
                })
            }
            None => Err(RouteReject::UnknownModel(unknown_model(
                model,
                &state.config.combo_ids(),
            ))),
        },
        DefaultChain::Flat => Ok(RoutePlan {
            chain: order(
                state,
                state.config.strategy,
                state.config.candidates(None),
                canonical.session.as_deref(),
                model,
            ),
            strategy: state.config.strategy,
            compression: None,
            judge: None,
        }),
    }
}

/// Records what a request measured about the target that served it.
///
/// Two numbers, both from the request itself: whether the target succeeded,
/// and how long the request took. The success share is a running average per
/// target, so one failure does not erase a hundred successes and one success
/// does not erase a hundred failures.
///
/// The inverse-latency factor is `1.0` for an instant answer and `0.0` for
/// the slowest answer the target has produced, so the scale is relative: a
/// target that answers in 50 ms is `1.0` until something beats it.
fn observe_target_outcome(
    outcome: &AttemptOutcome,
    chain: &[ChainTarget],
    elapsed: Duration,
) {
    let (Some(provider), Some(model)) = (outcome.provider(), outcome.serving_model()) else {
        return;
    };
    let Some(target) = chain.iter().find(|t| {
        t.provider.as_str() == provider.as_str() && t.model.as_ref() == model
    }) else {
        return;
    };
    let candidate = ar_route::Candidate::new(target.provider.clone(), target.model.as_ref());
    let loads = ar_route::TargetLoads::global();
    let previous = loads.load(&candidate);
    let succeeded = matches!(outcome, AttemptOutcome::Succeeded { .. });
    // The running success share: the previous share weighted by the requests
    // behind it is not tracked, so this is a bounded update — the previous
    // share moves a quarter of the way toward the new observation. It is a
    // smoothed signal, not an exact count, and the point is to distinguish a
    // target that keeps failing from one that mostly works.
    let observed = if succeeded { 1.0 } else { 0.0 };
    let reliability = match previous.reliability {
        Some(share) => (share * 3.0 + observed) / 4.0,
        None => observed,
    };
    // The latency factor is relative to the target's own best: the first
    // observation is `1.0` (nothing to compare against), and a later, slower
    // answer pulls it down.
    let latency_inv = match previous.latency_inv {
        Some(previous_best) => {
            let millis = elapsed.as_millis() as f64;
            if millis <= 0.0 {
                previous_best
            } else {
                // `1 / (1 + millis/1000)` is the inverse-latency shape the
                // auto scorer uses: 1.0 at zero, 0.5 at one second.
                (1.0 / (1.0 + millis / 1000.0)).min(previous_best.max(1.0))
            }
        }
        None => 1.0,
    };
    loads.observe_outcome(&candidate, reliability, latency_inv);
}

/// The `fusion` combo's judge provider, when it named one.
///
/// A `provider/model` string whose provider serves the synthesis. Resolved at
/// config load (a dangling judge name is a load-time error there), so this is a
/// lookup, not a parse. `None` on any non-fusion combo: a judge is a fusion
/// concept and silently honouring one elsewhere would be a second dispatch
/// nobody asked for.
fn judge(state: &AppState, combo: &RouteCombo) -> Option<ProviderId> {
    if combo.strategy != Strategy::Fusion {
        return None;
    }
    let judge = combo.judge_model.as_deref()?;
    let provider = judge
        .split_once('/')
        .map_or(judge, |(provider, _)| provider);
    state
        .config
        .providers
        .iter()
        .find(|p| p.id.as_str() == provider && p.is_dispatchable())
        .map(|p| p.id.clone())
}

/// Winner-first chain over the combo's targets, then its `pool:` bench.
///
/// The order is the contract (audit F-HIGH-2, `docs/04` §route): `pick` sees
/// the targets alone, so a cheaper bench entry cannot win a healthy request, and
/// the bench is appended afterwards where the existing attempt loop already
/// knows how to walk it. Nothing here scores a pool entry or reorders the
/// targets — a pool widens *what is tried after a failure*, which is the whole
/// difference between the 2 targets and the 7 candidates the live `free-stack`
/// combo declares.
///
/// A pool provider already in the chain is skipped: the loop must not spend two
/// of its three attempt slots on one endpoint.
fn chain(
    state: &AppState,
    combo: &RouteCombo,
    session: Option<&str>,
    model: &str,
) -> Vec<ChainTarget> {
    let mut chain = order(
        state,
        combo.strategy,
        state.config.candidates(Some(combo)),
        session,
        model,
    );
    for target in state.config.pool(combo) {
        if !chain.iter().any(|t| t.provider == target.provider) {
            chain.push(ChainTarget::new(
                target.provider.clone(),
                Strng::from(target.model.as_str()),
            ));
        }
    }
    chain
}

/// `pick`, then the rest of the candidates in config order.
///
/// The candidate list may carry several models on one provider, so the chain is
/// deduplicated: attempting the same provider twice with a different model
/// spelling is a second round trip to an endpoint that already refused this
/// request shape.
fn order(
    state: &AppState,
    strategy: Strategy,
    candidates: Vec<Candidate>,
    session: Option<&str>,
    model: &str,
) -> Vec<ChainTarget> {
    if candidates.is_empty() {
        return Vec::new();
    }
    // The session and the model are what the pin-shaped strategies rank on:
    // `lkgp` reads the pin this server wrote for the last provider that
    // served the session, and the two affinity strategies hash the pair into
    // an HRW key. Passing `None` for the session — as this did — made all
    // three unreachable on the request path while their unit tests still
    // passed, because the unit tests call `pick` directly with a session.
    // `pick_for_model` is the model-scoped entry point: a conversation that
    // changes model gets a fresh pin instead of inheriting the old model's
    // provider.
    match pick_for_model(
        strategy,
        session,
        model,
        &candidates,
        &state.rr,
        Some(&state.lkgp),
    ) {
        Ok(picked) => build_chain(&picked, &candidates),
        Err(e) => {
            // `pick` refuses an empty list, which is checked above; anything else
            // is a strategy this build has not implemented yet. Config order is
            // the documented fallback rather than a 500.
            tracing::warn!(error = %e, strategy = %strategy, "no strategy winner; falling back to config order");
            candidates.iter().map(target_of).collect()
        }
    }
}

/// Winner-first fallback chain.
/// Winner-first fallback chain.
fn build_chain(picked: &ProviderId, candidates: &[Candidate]) -> Vec<ChainTarget> {
    // The winner's own (provider, model) pair is what this chain leads with;
    // every other candidate stays a fallback. A provider excluded wholesale
    // would strand a combo pairing two models on one multi-key provider —
    // a sibling model is a different failure domain under key cycling.
    let winner = candidates.iter().find(|c| c.provider == *picked);
    let mut chain = candidates
        .iter()
        .filter(|c| !matches!(winner, Some(w) if c.provider == w.provider && c.model == w.model))
        .map(target_of)
        .collect::<Vec<_>>();
    // The winner leads even when no candidate matches it (`pick` only ever
    // returns one of them, so this is the empty-list guard rather than a branch).
    chain.insert(
        0,
        winner.map_or_else(
            || ChainTarget::new(picked.clone(), Strng::from(picked.as_str())),
            target_of,
        ),
    );
    // The failure domain is the credential, not the provider: a combo pairing
    // two models on one multi-key provider must keep both targets, or its
    // "fallback chain" is the winner alone and a single 429 ends the request
    // with nothing left to fail over to. Only a duplicated (provider, model)
    // row — the same pair listed twice — collapses.
    chain.dedup_by(|a, b| a.provider == b.provider && a.model == b.model);
    chain
}

/// One candidate as a chain entry: the provider, and the model that provider
/// serves it under.
fn target_of(c: &Candidate) -> ChainTarget {
    ChainTarget::new(c.provider.clone(), c.model.clone())
}

/// Resolves an `auto/*` alias into a scored chain, or `None` for a concrete name.
///
/// The expected entry point is
/// `ar_route::auto::engine::auto_variant_for_model(name) -> Option<AutoVariant>`.
/// That function does not exist in this build, and `ar_route::auto` is a private
/// module — the same resolver is reached through the crate's public factory
/// [`virtual_combo`], which is what `ar_route`'s own `VirtualFactory` calls. When
/// the engine-level spelling lands, this function is the only thing that changes.
fn auto_chain(state: &AppState, model: &str) -> Option<Vec<ChainTarget>> {
    let combo = virtual_combo(model).ok()?;
    let candidates = state.config.candidates(match state.config.default_combo() {
        DefaultChain::Combo(c) => Some(c),
        DefaultChain::Flat => None,
    });
    if candidates.is_empty() {
        return None;
    }

    let pool: Vec<AutoCandidate> = candidates
        .iter()
        .map(|c| {
            let a = AutoCandidate::new(c.provider.clone(), c.model.as_ref()).with_rank(c.rank);
            match c.input_usd_per_mtok {
                Some(price) => a.with_price(price),
                None => a,
            }
        })
        .collect();

    // No latency or circuit-history signal exists yet, so the fitness callback
    // reports a neutral 0.5 rather than an invented number.
    // ponytail: constant task fitness; wire real per-provider latency when
    // `ar-obs` records it, and the ranking gains its other axis.
    let plan = simulate_route(&combo, &pool, &AutoSelector::new(), |_| 0.5).ok()?;
    if plan.chain().is_empty() {
        return None;
    }
    tracing::info!(
        variant = %combo.variant.as_str(),
        chain = plan.chain().len(),
        "auto variant resolved"
    );
    Some(plan.chain().to_vec())
}

/// The `auto/*` family's chain over one combo's own targets.
///
/// The same factory the `auto/*` *model* name uses ([`virtual_combo`] and
/// [`simulate_route`]), pointed at this combo's candidates rather than the
/// server's default pool — a combo that names `auto/cheap` means "score
/// *these* targets the cheap way", not "ignore my targets and use the
/// server's pool".
///
/// `None` when the variant is not one this build carries, or the pool is
/// empty: the caller falls back to the ordinary chain, which is the config
/// order — a documented fallback rather than a 500.
fn auto_chain_over(
    state: &AppState,
    combo: &RouteCombo,
    variant: &str,
) -> Option<Vec<ChainTarget>> {
    let auto = virtual_combo(variant).ok()?;
    let candidates = state.config.candidates(Some(combo));
    if candidates.is_empty() {
        return None;
    }
    let pool: Vec<AutoCandidate> = candidates
        .iter()
        .map(|c| {
            let a = AutoCandidate::new(c.provider.clone(), c.model.as_ref()).with_rank(c.rank);
            match c.input_usd_per_mtok {
                Some(price) => a.with_price(price),
                None => a,
            }
        })
        .collect();
    // No latency or circuit-history signal exists yet, so the fitness
    // callback reports a neutral 0.5 rather than an invented number.
    // ponytail: constant task fitness; wire real per-provider latency when
    // `ar-obs` records it, and the ranking gains its other axis.
    let plan = simulate_route(&auto, &pool, &AutoSelector::new(), |_| 0.5).ok()?;
    if plan.chain().is_empty() {
        return None;
    }
    tracing::info!(
        variant = %auto.variant.as_str(),
        chain = plan.chain().len(),
        "auto strategy resolved"
    );
    Some(plan.chain().to_vec())
}

/// The 400 body for a model that names nothing, with the ids that do exist.
///
/// One sentence rather than a catalog dump: the ids a client can actually pick
/// from are in that sentence, and a client that wants the whole list has
/// `/v1/models`.
fn unknown_model(model: &str, known: &[&str]) -> String {
    if known.is_empty() {
        return format!("model {model:?} is not routable; no combo is configured");
    }
    format!(
        "model {model:?} is not a configured combo; routable models are: {}",
        known.join(", ")
    )
}

/// A cache hit served without touching a provider.
///
/// A miss and a body that will not build are the same answer to the caller —
/// this is a cache, and a cache that cannot answer is a miss — so nothing here is
/// an error path.
fn cache_hit(
    cache: &Cache,
    key: &CacheKey,
    planned_winner: Option<&ChainTarget>,
    strategy: Strategy,
) -> Option<Response> {
    let lookup = cache.get(key);
    if lookup.state != CacheState::Hit {
        return None;
    }
    let entry = lookup.entry?;
    // Moved out of the entry rather than cloned: this response is about to
    // become the only owner of a cached body, and a `Bytes` clone is a refcount
    // bump on a payload that can be a megabyte.
    let status = entry.status;
    let content_type = entry.content_type;
    let body = entry.body;
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
        .header(header::CONTENT_TYPE, content_type)
        .header(CACHE_HEADER, CacheState::Hit.as_header())
        .header(USAGE_HEADER, "attempts=0;cache=hit")
        .header(
            DECISION_HEADER,
            // `planned=`, not `provider=`. The body came from whichever target
            // won when the entry was written, and the entry does not record it —
            // it is a fixed-layout binary record on disk, so adding a field would
            // invalidate every cached file. Naming the current plan's winner as
            // if it had served the response would be a false claim on exactly
            // the header an operator reads when a failover surprises them, so the
            // header says which target *this* request would have used instead.
            // The answer itself is still correct to serve: a combo is one logical
            // model to a client, and weighted/DRR strategies choose a different
            // winner per request by design.
            format!(
                "strategy={strategy};outcome=cache;planned={}:{};attempts=0",
                planned_winner.map_or("-", |t| t.provider.as_str()),
                planned_winner.map_or("-", |t| t.model.as_ref()),
            ),
        )
        .body(Body::from(body))
        .ok()
}

/// Turns a terminal outcome into an HTTP response with the `x-ar-*` headers.
///
/// The decision header carries both the strategy that *chose* the provider and
/// the *outcome*, because an operator debugging a fallback needs both: which
/// rule ran, and what it produced. Collapsing them to one field loses exactly
/// the half that explains the other.
fn decorate(
    outcome: AttemptOutcome,
    resilience: Arc<Resilience>,
    stream: bool,
    dialect: Dialect,
    strategy: Strategy,
    stages: Stages,
) -> Response {
    let decision = format!(
        "strategy={strategy};outcome={};provider={};model={};attempts={}",
        outcome_label(&outcome),
        outcome.provider().map_or("-", ProviderId::as_str),
        outcome.serving_model().unwrap_or("-"),
        outcome.attempts()
    );

    let Stages {
        compression,
        cache_state,
        cache_key,
        planned_target,
        cache,
        model,
        cache_control,
        meta,
        failures,
        continuation,
    } = stages;

    // Every accounting number the response reports, from one [`ResponseMeta`].
    // Cost is priced in `ar-tokens` against the same row the ledger stores, so a
    // client reading the header and an operator reading `ar cost-report` are
    // looking at the same arithmetic rather than two that can drift.
    let mut builder = Response::builder()
        .header(DECISION_HEADER, decision)
        .header(USAGE_HEADER, format!("attempts={}", outcome.attempts()))
        .header(CACHE_HEADER, cache_state.as_header())
        .header(COMPRESSION_ECHO, compression)
        .header(MODEL_HEADER, model.as_ref())
        .header(VERSION_HEADER, SERVER_VERSION)
        .header(TOKENS_IN_HEADER, meta.tokens_in().to_string())
        .header(TOKENS_OUT_HEADER, meta.tokens_out().to_string())
        .header(RESPONSE_COST_HEADER, meta.cost().usd.as_decimal_string())
        .header(SAVINGS_TOKENS_HEADER, meta.savings_tokens().to_string())
        .header(LATENCY_MS_HEADER, meta.latency_ms().to_string())
        .header(FALLBACK_ATTEMPTS_HEADER, outcome.attempts().to_string());

    // Conditional, matching the reference: an outcome with no provider — the
    // router answering before any dispatch — omits the header rather than
    // reporting a placeholder in a field a client may branch on.
    if let Some(provider) = outcome.provider() {
        builder = builder.header(PROVIDER_HEADER, provider.as_str());
    }

    // Conditional, not a zero: an elapsed time too short to divide by would make
    // every downstream division nonsense, and an absent header reads as unknown.
    if let Some(tps) = meta.tokens_per_second() {
        builder = builder.header(TOKENS_PER_SECOND_HEADER, format!("{tps:.3}"));
    }

    let resp = match outcome {
        AttemptOutcome::Succeeded {
            upstream,
            provider,
            model,
            ..
        } => {
            // Pass upstream framing through untouched. A `stream: true` request
            // gets `text/event-stream`; a non-stream completion is one JSON
            // chunk and gets `application/json`.
            let ct = if stream {
                "text/event-stream"
            } else {
                "application/json"
            };
            let status = upstream.status;
            // A keepalive only makes sense on a body the client is reading
            // incrementally. A non-stream completion arrives as one JSON chunk,
            // and a comment line injected into it would corrupt the JSON.
            let keepalive = stream.then(|| Keepalive::new(dialect, KEEPALIVE_INTERVAL));
            let served = format!("{}/{}", provider.as_str(), model);
            let store_key =
                cache_key.filter(|_| storable_reply(planned_target.as_deref(), &served));
            match (store_key, cache) {
                // Only a non-streaming completion reaches here with a key; the
                // caller already turned a stream into `CacheState::Bypass`.
                (Some(key), Some(cache)) => builder
                    .header(header::CONTENT_TYPE, ct)
                    .status(status)
                    .body(Body::from_stream(tee_and_store(
                        upstream.stream,
                        cache,
                        key,
                        status.as_u16(),
                        cache_control.no_store,
                        cache_control.ttl,
                    ))),
                _ => {
                    let cut = upstream.cut;
                    // R5: the breaker verdict is settled when this body ends, cut
                    // or clean, so the stream is wrapped before either relay reads it.
                    let (chunks, end_cut) = if stream {
                        let settled = settle_stream(
                            upstream.stream,
                            resilience,
                            provider.as_str(),
                            cut.clone(),
                        );
                        // C2: a cut after the headers is a failed request. The response is
                        // already 200, so the failure log is the only record it leaves.
                        let record = CutRecord {
                            sink: failures,
                            provider: provider.as_str().to_owned(),
                            model: model.to_string(),
                            cut: cut.clone(),
                        };
                        let kept = guarded(settled, record);
                        match continuation {
                            Some(plan) => resumed(kept, cut, plan),
                            None => (kept, cut),
                        }
                    } else {
                        (upstream.stream, cut)
                    };
                    let body = match keepalive.filter(|k| !k.is_noop()) {
                        Some(k) => Body::from_stream(
                            k.relay(chunks, provider.as_str(), model.as_ref())
                                .map(|r| r.map_err(|never| -> std::io::Error { match never {} })),
                        ),
                        None => Body::from_stream(abort_on_cut(chunks, end_cut)),
                    };
                    builder
                        .header(header::CONTENT_TYPE, ct)
                        .status(status)
                        .body(body)
                }
            }
        }
        AttemptOutcome::Retry { after, .. } => {
            // Honour the window we just advertised, in whole seconds. A
            // sub-second window rounds up to 1s rather than to 0.
            let secs = after.as_secs().max(1);
            builder
                .header(header::RETRY_AFTER, secs.to_string())
                .header(header::CONTENT_TYPE, "application/json")
                .extension(FailNote(
                    "every provider in the chain is throttled".to_owned(),
                ))
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(json_body(
                    ErrorSpec::of(
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limited",
                        "every provider in the chain is throttled",
                    )
                    .because("chain_throttled"),
                ))
        }
        AttemptOutcome::Failover {
            status,
            provider,
            error_body,
            retry_after,
            ..
        } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            // The last provider's own identifier, when it is one this build
            // passes through. The reference projects upstream codes through
            // an allowlist (`open-sse/utils/error.ts:43-352`, ~310 entries)
            // before they reach the client, and collapses anything unknown
            // onto the status-derived default — the projection below is that
            // behaviour with the six identifiers a client of this build can
            // actually branch on. Grow the list when a real client needs a new
            // one; never by echoing whatever a provider sent.
            let code = upstream_code(&error_body).unwrap_or("upstream_unavailable");
            let message = format!("all providers failed; last was {provider}");
            if let Some(retry_after) = retry_after {
                // A window the provider stated is information the client can
                // use; keeping it costs the same header the retry arm already
                // stamps. Sub-second rounds up, as there.
                builder = builder.header(
                    header::RETRY_AFTER,
                    retry_after.as_secs().max(1).to_string(),
                );
            }
            builder
                .header(header::CONTENT_TYPE, "application/json")
                .extension(FailNote(format!("{message} ({code})")))
                .status(status)
                .body(json_body(ErrorSpec::of(status, code, &message)))
        }
        AttemptOutcome::Abort(report) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .extension(FailNote(report.reason.to_string()))
            .status(report.status)
            .body(json_body(ErrorSpec::of(
                report.status,
                "aborted",
                &report.reason,
            ))),
    };

    resp.unwrap_or_else(|_| {
        // Every header above is built in-process from a bounded set of strings,
        // so an invalid one is a programming error — but the response still has
        // to be a response.
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })
}

/// Relays a completion while keeping a bounded copy for the cache.
///
/// Only the non-streaming arm: a stream is never cached, so there is no key and
/// no tee. The store happens on the generator's final poll, i.e. after the last
/// byte has already reached the client, so a cache write never delays a relay.
/// Relays a raw-dialect stream, and ends it with an error if the upstream was cut.
///
/// A raw dialect has no in-band frame that names a cut, so a partial reply
/// would reach the client as a clean end of stream. An errored body makes the
/// server drop the connection instead, which a client reads as a failed
/// request. Nothing is added to the bytes already sent.
fn abort_on_cut(
    mut stream: ar_route::ChunkStream,
    cut: ar_route::CutFlag,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        while let Some(chunk) = stream.next().await {
            yield Ok(chunk);
        }
        if cut.load(std::sync::atomic::Ordering::SeqCst) {
            yield Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "upstream stream was cut before its end",
            ));
        }
    }
}

/// Did this completion stop at the token ceiling?
///
/// Reads the stop marker of each dialect the reply can be in: OpenAI
/// `finish_reason`, Anthropic `stop_reason`, and Gemini `finishReason`. A reply
/// stopped at the ceiling is a partial answer, and storing it would replay a
/// truncated answer as a complete one (`ar_cache` names this as
/// `store_truncated`, which nothing called before).
fn stopped_at_ceiling(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let openai = value["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| matches!(c["finish_reason"].as_str(), Some("length" | "max_tokens")));
    let anthropic = value["stop_reason"].as_str() == Some("max_tokens");
    let gemini = value["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| c["finishReason"].as_str() == Some("MAX_TOKENS"));
    openai || anthropic || gemini
}

/// Past [`CACHE_MAX_BODY`] the buffer is dropped rather than grown: a partial
/// answer replayed from cache looks like a complete one, and `ar_cache` has a
/// named `store_truncated` for exactly that decision.
fn tee_and_store(
    mut stream: ar_route::ChunkStream,
    cache: Arc<Cache>,
    key: CacheKey,
    status: u16,
    no_store: bool,
    ttl: Option<Duration>,
) -> impl futures::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + use<> {
    async_stream::stream! {
        let mut buf: Vec<u8> = Vec::new();
        let mut keep = true;
        while let Some(chunk) = stream.next().await {
            if keep {
                if buf.len() + chunk.len() <= CACHE_MAX_BODY {
                    buf.extend_from_slice(&chunk);
                } else {
                    keep = false;
                    buf = Vec::new();
                }
            }
            yield Ok(chunk);
        }
        // `no-store` is the write-side kill: the lookup already happened, and
        // this tee's whole job was the write. A caller TTL, when present,
        // shortens the entry — the policy ceiling still wins on the store side
        // ([`ar_cache::Cache::store_with_ttl`]).
        if keep && status < 300 && !no_store && !stopped_at_ceiling(&buf) {
            match ttl {
                Some(ttl) => {
                    cache.store_with_ttl(
                        &key,
                        status,
                        "application/json",
                        Bytes::from(buf),
                        ttl,
                    );
                }
                None => {
                    cache.store(&key, status, "application/json", Bytes::from(buf));
                }
            }
        }
    }
}

/// A stream relay that keeps an idle client alive and names a truncated stream.
/// Two jobs, one wrapper, because they are the same observation from two sides: a
/// client is waiting on bytes that are not coming. While it waits, a keepalive
/// resets its watchdog; if the wait ends because the upstream died rather than
/// because bytes arrived, the in-band error frame is what lets the client say so
/// instead of hanging until its own read times out.
///
/// Both are dialect-aware, which is the whole reason this is not a constant
/// string — see [`Dialect::keepalive`] and [`Dialect::stream_error`].
///
/// The stream is *not* rewritten: every upstream byte reaches the client in order
/// and unchanged, and the two frames this adds are additions a client skips
/// rather than substitutions it has to re-parse.
///
/// The state is a `Copy` struct of `'static` slices and one [`Duration`], moved
/// into the relay body by value — so a twenty-minute generation allocates nothing
/// per frame beyond the `Bytes` it relays.
#[derive(Clone, Copy)]
struct Keepalive {
    /// The frame emitted while the upstream is idle.
    ///
    /// `None` for a dialect with no SSE framing (Ollama), which then relays
    /// untouched.
    frame: Option<&'static str>,
    /// The frame naming a truncated stream. `None` for the same dialect as
    /// `frame`, so no dialect can get a keepalive and no truncation signal.
    truncated: Option<&'static str>,
    /// The dialect whose completion marker [`completion_seen`] looks for.
    dialect: Dialect,
    /// How long a wait may be before a keepalive goes out.
    ///
    /// [`KEEPALIVE_INTERVAL`] in production; a test passes its own so the idle
    /// path runs in milliseconds rather than ten seconds.
    interval: Duration,
    /// Whether a terminator was seen in the frame just relayed.
    ///
    /// A field on the relay rather than a local in the `stream!` body so the
    /// invariant — `None` until the first chunk, `Some(false)` meaning truncated —
    /// is stated once beside the fields it is about rather than inside a macro
    /// expansion where a reader has to expand it to see.
    last_terminated: Option<bool>,
}
impl Keepalive {
    fn new(dialect: Dialect, interval: Duration) -> Self {
        Self {
            frame: dialect.keepalive(),
            truncated: dialect.stream_error(),
            dialect,
            interval,
            last_terminated: None,
        }
    }

    /// Whether this relay has anything to add at all.
    ///
    /// Ollama's is the false case: NDJSON has no comment and no event frame, so
    /// the relay is the identity and is not built at all.
    fn is_noop(&self) -> bool {
        self.frame.is_none() && self.truncated.is_none()
    }

    /// Relays `stream`, emitting a keepalive on every idle interval and a
    /// truncation error if the stream ends without a completion marker.
    ///
    /// The completion check reads a window that spans chunk boundaries (docs/22,
    /// B3) and latches: once a marker is seen, no later chunk can retract it, and
    /// nothing is emitted after the stream's own terminator. `provider` and `model`
    /// are the serving target, named in the truncation log so a real cut and a
    /// missing marker can be told apart (docs/22, B6).
    fn relay(
        self,
        mut stream: ar_route::ChunkStream,
        provider: &str,
        model: &str,
    ) -> impl futures::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + use<> {
        let mut keepalive = self;
        let (provider, model) = (provider.to_owned(), model.to_owned());
        async_stream::stream! {
            if keepalive.is_noop() {
                while let Some(chunk) = stream.next().await {
                    yield Ok(chunk);
                }
                return;
            }
            // The last TERMINATOR_WINDOW bytes relayed, kept only while no marker
            // has been seen: once latched, nothing reads them again.
            let mut tail: Vec<u8> = Vec::new();
            let mut relayed: u64 = 0;
            loop {
                let next = tokio::time::timeout(keepalive.interval, stream.next()).await;
                match next {
                    Ok(Some(chunk)) => {
                        relayed += 1;
                        if keepalive.last_terminated != Some(true) {
                            let mut window = std::mem::take(&mut tail);
                            window.extend_from_slice(&chunk);
                            keepalive.last_terminated =
                                Some(completion_seen(keepalive.dialect, &window));
                            let keep = window.len().saturating_sub(TERMINATOR_WINDOW);
                            tail = window.split_off(keep);
                        }
                        yield Ok(chunk);
                    }
                    // The upstream finished. If no completion marker crossed, the
                    // client is waiting on a frame that will never arrive, so name
                    // that in-band and say so in the log.
                    Ok(None) => {
                        // `!= Some(true)`, not `== Some(false)`: a stream that
                        // ended before its first chunk is `None`, and a client
                        // handed a silent 200 with no body is the same failure.
                        if keepalive.last_terminated != Some(true)
                            && let Some(frame) = keepalive.truncated
                        {
                            tracing::warn!(
                                dialect = ?keepalive.dialect,
                                provider = %provider,
                                model = %model,
                                relayed,
                                "upstream stream ended without a completion marker; named in-band"
                            );
                            yield Ok(Bytes::from_static(frame.as_bytes()));
                        }
                        return;
                    }
                    // Idle for a whole interval with the stream still open.
                    Err(_elapsed) => {
                        if let Some(frame) = keepalive.frame {
                            yield Ok(Bytes::from_static(frame.as_bytes()));
                        }
                    }
                }
            }
        }
    }
}

/// Bytes of relayed stream kept between chunks for the completion check. Sized
/// for the longest marker plus the whitespace an encoder may place around
/// `"finish_reason"`, with room to spare.
const TERMINATOR_WINDOW: usize = 128;

/// Whether `window` holds the completion marker for `dialect` (docs/22, D2).
///
/// OpenAI completes on `data: [DONE]` or on a `finish_reason` string. Anthropic
/// and Responses keep their own terminators. Ollama has none to look for.
fn completion_seen(dialect: Dialect, window: &[u8]) -> bool {
    match dialect {
        Dialect::OpenAi => openai_done_seen(window) || finish_reason_seen(window),
        Dialect::Anthropic => dialect
            .terminator()
            .is_some_and(|t| window.windows(t.len()).any(|w| w == t)),
        // `response.incomplete` is what a Responses stream ends with when its
        // token budget runs out, which closes it as surely as `response.completed`.
        Dialect::Responses => [&b"response.completed"[..], b"response.incomplete"]
            .iter()
            .any(|t| window.windows(t.len()).any(|w| w == *t)),
        Dialect::Ollama => false,
    }
}

/// `data: [DONE]` opening a line.
///
/// Escaped JSON never puts a raw newline inside a string, so a `\n` before the
/// marker separates a real frame from answer text that spells the same bytes.
fn openai_done_seen(window: &[u8]) -> bool {
    const DONE: &[u8] = b"data: [DONE]";
    window
        .windows(DONE.len())
        .enumerate()
        .any(|(i, w)| w == DONE && (i == 0 || window[i - 1] == b'\n'))
}

/// A `finish_reason` key whose value is a JSON string.
///
/// OpenAI spells the key `finish_reason`. A Gemini stream, relayed raw on the
/// streaming path (docs/22, D6), spells it `finishReason`. Intermediate chunks
/// carry `null`, which does not count. Answer text that spells the key arrives
/// escaped (`\"finish_reason\"`), which does not match the unescaped key.
fn finish_reason_seen(window: &[u8]) -> bool {
    [&b"\"finish_reason\""[..], &b"\"finishReason\""[..]]
        .iter()
        .any(|key| key_with_string_value(window, key))
}

/// Whether `key` occurs in `window` followed by `:` and a JSON string.
fn key_with_string_value(window: &[u8], key: &[u8]) -> bool {
    window.windows(key.len()).enumerate().any(|(i, w)| {
        w == key
            && window[i + key.len()..]
                .trim_ascii_start()
                .strip_prefix(b":")
                .is_some_and(|after| after.trim_ascii_start().first() == Some(&b'"'))
    })
}

/// A short outcome word for the decision header. One token, no spaces, so an
/// operator can `grep` it out of an access log.
fn outcome_label(outcome: &AttemptOutcome) -> &'static str {
    match outcome {
        AttemptOutcome::Succeeded { .. } => "ok",
        AttemptOutcome::Retry { .. } => "retry",
        AttemptOutcome::Failover { .. } => "failover",
        AttemptOutcome::Abort(_) => "abort",
    }
}

/// OpenAI-shaped error envelope.
///
/// The `message` is always a router-authored string: an upstream body never
/// reaches the client through this path, which is what keeps a provider's
/// account identifiers out of the response (`docs/04-obs`).
///
/// `type` and `reason` are the two identifiers the reference gateway's
/// `buildErrorBody` emits alongside `code` (`open-sse/utils/error.ts:395-403`).
/// They are additions, not a reshape: `code` and `message` are what every client
/// already parses, and the two new fields are omitted when there is nothing
/// honest to put in them. `type` is the coarse class a client switches on
/// (`invalid_request_error`, `not_found`, `authentication_error`,
/// `rate_limit_error`, `server_error`) and `reason` is the fine-grained,
/// machine-readable cause within it — the pair is what lets a client tell "my
/// API key is wrong" from "your server is down" without string-matching a
/// sentence.
/// The OpenAI-shaped envelope, serialised.
///
/// One function for every error in the crate: the shape is a wire contract, and
/// two places building it is two places a client learns to expect something else.
fn json_body(spec: ErrorSpec<'_>) -> Body {
    let payload = ErrorEnvelope {
        error: ErrorBody {
            kind: spec.kind,
            code: spec.code,
            reason: spec.reason,
            message: spec.message,
            path: spec.path,
        },
    };
    Body::from(serde_json::to_vec(&payload).unwrap_or_else(|_| {
        br#"{"error":{"code":"internal","message":"serialization failed"}}"#.to_vec()
    }))
}

/// The kind (`error.type`) an [`ErrorSpec`] gets for a status.
///
/// A function rather than a parameter because the status is already decided at
/// every call site and passing it twice is how the two drift; deriving it means a
/// new arm cannot claim a class its status does not have. Bounded to the seven
/// the OpenAI and Anthropic specifications name, so a client can switch on it.
const fn kind_for(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 415 => "invalid_request_error",
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found",
        409 => "conflict_error",
        422 => "unprocessable_entity_error",
        429 => "rate_limit_error",
        500..=599 => "server_error",
        _ => "api_error",
    }
}

/// One error body, as the fields the envelope carries.
///
/// `Copy` because a spec is built at a call site and passed by value to two
/// functions, and it is four `Option<&str>` — copying costs nothing an owned
/// `String` would not. Every `&str` in it is router-authored: the `message` is
/// always a sentence this crate wrote and the `code`/`reason` are always from a
/// closed set, which is what keeps an upstream body out of a response a client
/// caches.
#[derive(Clone, Copy)]
struct ErrorSpec<'a> {
    /// Coarse class, derived from the status.
    kind: &'static str,
    /// Specific machine-readable code.
    code: &'a str,
    /// Finer-grained cause within `kind`, when there is one to name.
    ///
    /// The pair `code` + `reason` is what lets a client tell "my API key is
    /// wrong" from "your server is down" without string-matching a sentence, and
    /// it is the addition that a client branching on `code` alone cannot make.
    reason: Option<&'a str>,
    /// Human-readable sentence. Always router-authored.
    message: &'a str,
    /// The path the request asked for, present only on the unknown-route 404.
    path: Option<&'a str>,
}

impl<'a> ErrorSpec<'a> {
    /// Builds a spec for a status and code, with no finer reason.
    fn of(status: StatusCode, code: &'a str, message: &'a str) -> Self {
        Self {
            kind: kind_for(status),
            code,
            reason: None,
            message,
            path: None,
        }
    }

    /// Adds a `reason`, which is the whole point of having one: a client
    /// branching on `code` alone cannot act on "which of the four things that
    /// produce a 400 happened".
    fn because(mut self, reason: &'a str) -> Self {
        self.reason = Some(reason);
        self
    }

    /// Adds the requested path, for the unknown-route 404.
    ///
    /// A field on the envelope rather than in the message alone: a client
    /// branching on `error.path` is a program, and a program should not have to
    /// parse a sentence to learn which of its own URLs was wrong.
    fn at(mut self, path: &'a str) -> Self {
        self.path = Some(path);
        self
    }
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    /// Coarse class. Serialized as `type` because `type` is a Rust keyword.
    #[serde(rename = "type")]
    kind: &'a str,
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>,
}

/// Builds a bare error response stamped with a null decision.
fn error(status: StatusCode, message: &str) -> Response {
    error_code(status, "request_rejected", message)
}

/// [`error`] with a caller-chosen `code`.
///
/// The envelope is identical either way — only the `code` string differs — so a
/// client parsing `error.code` sees `model_not_found` instead of a generic verdict
/// that would hide which half of the request was wrong.
fn error_code(status: StatusCode, code: &'static str, message: &str) -> Response {
    error_spec(status, ErrorSpec::of(status, code, message))
}

/// [`error_code`] with a `reason` as well, for a client that has to act on which
/// of several causes fired.
pub(crate) fn error_because(
    status: StatusCode,
    code: &'static str,
    reason: &'static str,
    message: &str,
) -> Response {
    error_spec(status, ErrorSpec::of(status, code, message).because(reason))
}

/// An [`ErrorSpec`] as a `Response`, with the status and the null decision header.
///
/// The one place a spec becomes a response, so every error in this module gets
/// the same `x-ar-decision: strategy=none` stamp — including the ones raised from
/// a layer that never resolved a strategy at all, which is precisely the case a
/// client needs to be able to tell apart.
fn error_spec(status: StatusCode, spec: ErrorSpec<'_>) -> Response {
    let mut resp = json_body(spec).into_response();
    *resp.status_mut() = status;
    resp.extensions_mut()
        .insert(FailNote(spec.message.to_owned()));
    resp.headers_mut().insert(
        HeaderName::from_static(DECISION_HEADER),
        HeaderValue::from_static("strategy=none"),
    );
    resp
}

/// The catch-all for a path no route matched.
///
/// A JSON 404 that names the path, so a client parsing every error as JSON gets
/// something to parse and an operator gets the typo in the log *and* in the
/// response. axum's own fallback is an empty body, which is the specific
/// failure the reference gateway's `[...omnirouteCatchAll]` exists to fix
/// (`route.ts:15-31`): an OpenAI-compatible SDK that hits a typo'd path gets a
/// parse failure on the error path and reports *that*, not the 404.
///
/// Takes the whole [`Request`] only because axum's `fallback` signature hands one
/// to a handler. The method is deliberately not consulted: the reference answers
/// 404 identically for every verb, and axum drops the body for `HEAD` on its own.
pub async fn not_found(req: Request) -> Response {
    let path = req.uri().path().to_owned();
    error_spec(
        StatusCode::NOT_FOUND,
        ErrorSpec::of(
            StatusCode::NOT_FOUND,
            "unknown_route",
            "no route matches this path; the routable paths are the four chat dialects, \
             /v1/completions (the legacy alias), /v1/embeddings, /v1/audio/transcriptions, \
             /v1/audio/translations, /v1/images/generations, /v1/ocr, /v1/models, /healthz \
             and /metrics",
        )
        .because("unknown_route")
        .at(&path),
    )
}

/// Stamps `x-ar-guard` onto a response.
///
/// A trait rather than a free function because the chain already reads as
/// `error(...).with_guard(verdict)` at six call sites, and a free function would
/// make every one of them a two-line expression.
trait WithGuard {
    /// The `x-ar-guard` header value, so a client can see what happened without the
    /// response body having to explain it.
    fn with_guard(self, verdict: GuardVerdict) -> Response;
}

impl WithGuard for Response {
    fn with_guard(mut self, verdict: GuardVerdict) -> Response {
        stamp(&mut self, GUARD_HEADER, verdict.as_header());
        self
    }
}

/// Sets a header on an already-built response, ignoring an invalid value.
///
/// Every header this module stamps is a compile-time constant, so the `Err` arm
/// is a programming error rather than a runtime one — and the alternative
/// (propagating it) would make each of the four call sites carry a `Result` for
/// a value that cannot be wrong.
/// The output-token ceiling a request declares for itself, `0` when it names
/// none.
///
/// A projection needs an upper bound on the response, and the only bound a
/// request carries is the one it chose: `max_tokens`, read straight from the
/// canonical body. A request that names no ceiling projects a `0`-token
/// response, which under-counts — and is the honest projection, because a
/// request that declared no ceiling has told the server nothing to project
/// from.
fn declared_output_ceiling(body: &[u8]) -> u32 {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("max_tokens").and_then(serde_json::Value::as_u64))
        .and_then(|t| u32::try_from(t).ok())
        .unwrap_or(0)
}

fn stamp(resp: &mut Response, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
        resp.headers_mut().insert(n, v);
    }
}

/// Maps a router error onto a response.
///
/// The status comes from `ar_route` rather than being chosen here, so the
/// attempt loop's own classification — throttle, upstream failure, abort — reaches
/// the client as the same class the router reached.
fn route_error(e: RouteError) -> Response {
    error(e.status(), &e.to_string())
}

/// Upstream error codes this build passes through to the client envelope.
///
/// Closed on purpose, and short on the same purpose: the reference does the
/// same thing at ~310 entries (`SAFE_PUBLIC_ERROR_IDENTIFIERS`,
/// `open-sse/utils/error.ts:43-352`) so a client may branch on `code` without
/// a provider inventing a value it was never promised to parse. Anything not
/// on the list collapses to the router's own `upstream_unavailable`, exactly
/// as an unknown identifier collapses to the reference's status-derived
/// default. Grow only when a real client branches on a new one.
const PASSTHROUGH_ERROR_CODES: &[&str] = &[
    "insufficient_quota",
    "rate_limit_exceeded",
    "model_not_found",
    "context_length_exceeded",
    "invalid_api_key",
    "billing_hard_limit_reached",
];

/// The upstream's error code, when it is one this build passes through.
///
/// Extraction order mirrors the reference (`error.ts:867-868`):
/// `error.code` first, then a top-level `code`; a non-JSON body names nothing,
/// which is the collapse case and not an error.
fn upstream_code(body: &[u8]) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let code = value
        .get("error")
        .and_then(|error| error.get("code"))
        .or_else(|| value.get("code"))
        .and_then(serde_json::Value::as_str)?;
    PASSTHROUGH_ERROR_CODES
        .iter()
        .copied()
        .find(|known| *known == code)
}

/// `GET /healthz` — liveness only.
///
/// Readiness is P1: there is nothing to be un-ready about until a provider chain
/// exists, and a proxy that 503s on boot because a key is missing is a proxy an
/// operator cannot debug. A gate does not apply here either — a health check that
/// needs a credential is a health check nobody can run from a shell.
pub async fn healthz() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "ok\n",
    )
        .into_response()
}

/// `GET /metrics` — Prometheus text exposition. Behind the gate when one is armed:
/// the series name providers, models and traffic, which a public listener must not
/// hand to an anonymous reader. `/healthz` stays open as a liveness probe.
pub async fn metrics(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Err(reason) = authorize(&state, &headers, uri.path()) {
        return *reason;
    }
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}

/// One `/v1/models` card, in the OpenAI catalog shape.
///
/// `created` is the registry snapshot's build stamp, not a per-model date: the
/// snapshot is the thing a card describes, and no per-model publication date
/// exists to report. `permission` is empty because nothing here grants a
/// per-model scope — auth is one gate for the whole server, and a card that
/// claimed a narrower scope would be describing a capability this crate does not
/// have. `context_length` is the provider's declared ceiling, and it is always
/// emitted: a discovery client drops a card that omits it, so leaving it out
/// would delete the model from every `/v1/models`-driven picker rather than
/// merely under-report it.
fn card_json(c: &crate::models::ModelCard) -> serde_json::Value {
    // A zero output ceiling is an absent figure, not a claim: no model emits
    // nothing. Serialised only when known, so a client reading the field can
    // trust that a number there is a real one.
    let mut card = serde_json::json!({
        "id": c.id,
        "object": "model",
        "created": ar_registry::BUILT_AT,
        "owned_by": c.provider,
        "context_length": c.context_length,
        "input_image": c.input_image,
        "permission": Vec::<String>::new(),
        "root": c.id,
        "ar_upstream_model": c.upstream_model,
    });
    if c.max_output_tokens > 0 {
        card["max_output_tokens"] = serde_json::json!(c.max_output_tokens);
    }
    card
}

/// The catalog in the order OmniRoute serves it.
///
/// Port of `sortCatalogModelsProviderGrouped`
/// (`src/app/api/v1/models/catalogOrder.ts:56-77`): the combo block first, then
/// providers in registry precedence, then unknown providers in code-unit order,
/// with input order preserved inside every group.
///
/// The stable sort is `sort_by` plus an input index, not `sort_by_key` alone —
/// Rust's slice sort is already stable, so the index is what carries *which* group
/// each card came from through the comparison. Without it two cards in one group
/// would compare equal and fall back to whichever the unstable comparison
/// happened to leave first.
///
/// A combo card is one whose id equals its `owned_by`, which is how
/// [`crate::config::ServerConfig::model_cards_with`] builds them: a combo's
/// targets are private to the router, so the card is named after the combo
/// itself.
fn catalog_order(cards: &[crate::models::ModelCard]) -> Vec<&crate::models::ModelCard> {
    let annotated: Vec<(i64, &str, usize, &crate::models::ModelCard)> = cards
        .iter()
        .enumerate()
        .map(|(index, c)| {
            let group = group_key(c);
            (
                ar_registry::order::group_sort_priority(group),
                group,
                index,
                c,
            )
        })
        .collect();
    let mut annotated = annotated;
    annotated.sort_by(|a, b| {
        a.0.cmp(&b.0)
            // Both unknown: the code-unit tiebreak upstream applies to two
            // `Infinity` priorities. `&str`'s `Ord` is byte order, which is the
            // same thing for the ASCII ids a provider prefix is.
            .then_with(|| {
                if a.0 == i64::MAX {
                    a.1.cmp(b.1)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| a.2.cmp(&b.2))
    });
    annotated.into_iter().map(|(_, _, _, c)| c).collect()
}

/// The group one card sorts in: the combo bucket, else its `owned_by`.
///
/// Port of `modelGroupKey` (`catalogOrder.ts:42-50`). The id-prefix fallback is
/// kept for a card whose `provider` is empty — upstream calls that path
/// defensive, and a card built here always has one.
fn group_key(c: &crate::models::ModelCard) -> &str {
    if c.id == c.provider {
        return ar_registry::order::COMBO_GROUP;
    }
    if c.provider.is_empty() {
        return c.id.split_once('/').map_or(c.id.as_str(), |(p, _)| p);
    }
    &c.provider
}

/// Reads the catalog, counting a revalidation when this read triggered one.
///
/// Both catalog routes go through it so `ar_models_refresh_total` follows the
/// refresh work actually done rather than one route's traffic — a counter that
/// tracked reads could not tell a revalidating catalog from an idle one.
fn cached_models(state: &AppState) -> crate::models::Cached {
    let cached = state.models.get();
    // Count revalidations, not reads: a counter that tracks traffic cannot tell a
    // revalidating catalog from an idle one.
    if cached.revalidated {
        state.metrics.observe_models_refresh();
    }
    cached
}

/// `GET /v1/models` — OpenAI-compatible catalog, stale-while-revalidate 60s.
pub async fn models(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Err(reason) = authorize(&state, &headers, uri.path()) {
        return *reason;
    }
    let cached = cached_models(&state);
    let payload = serde_json::json!({
        "object": "list",
        "data": catalog_order(&cached.cards).into_iter().map(card_json).collect::<Vec<_>>(),
    });
    let age = state.models.age().map_or(0, |a| a.as_secs());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        // `stale` is how an operator sees that a revalidation is pending without
        // reading the body.
        .header(CACHE_HEADER, if cached.stale { "stale" } else { "fresh" })
        .header("x-ar-models-age-seconds", age.to_string())
        .body(Body::from(serde_json::to_vec(&payload).unwrap_or_else(
            |_| br#"{"object":"list","data":[]}"#.to_vec(),
        )))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// `HEAD /v1/models` — availability probe, headers only, no body.
///
/// RFC 9110 §9.3.2. Registered explicitly because axum does not derive it: a
/// `.get()` route answers an unregistered method with 405, so without this an
/// SDK that probes the catalog with HEAD reads a refusal. It stays separate from
/// [`models`] rather than sharing it so the probe never enumerates the catalog —
/// a `HEAD` that built a card body to throw it away would be work per probe on
/// the one route an SDK hits hardest.
pub async fn models_head(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Err(reason) = authorize(&state, &headers, uri.path()) {
        return *reason;
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// `GET /v1/models/{model}` — one card, or a 404 naming the miss.
///
/// The whole reason this route exists: without it a single-model lookup falls
/// through to the JSON 404, which names a *path* and not a *model*, and a client
/// validating a model id cannot tell a typo from a missing route.
///
/// The lookup is exact-first, then case-insensitive, because a client that
/// normalises a mixed-case catalog id to lowercase should still resolve the real
/// card rather than report a miss. It resolves against the *configured* catalog,
/// so a miss here means this server does not serve that model — not that the
/// registry lacks it.
pub async fn model(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(model): Path<String>,
) -> Response {
    if let Err(reason) = authorize(&state, &headers, uri.path()) {
        return *reason;
    }
    let cached = cached_models(&state);
    let found = cached.cards.iter().find(|c| c.id == model).or_else(|| {
        cached
            .cards
            .iter()
            .find(|c| c.id.eq_ignore_ascii_case(&model))
    });

    match found {
        Some(card) => json_response(&card_json(card), cached.stale),
        None => error_because(
            StatusCode::NOT_FOUND,
            "model_not_found",
            "not_in_catalog",
            &format!("model {model:?} is not in the catalog"),
        ),
    }
}

/// A JSON body with the catalog's freshness verdict, 200 unless told otherwise.
///
/// The `x-ar-cache` header here is *catalog* freshness, not the response cache's
/// hit/miss: this is the one route with two caches, and the response cache's
/// verdict on a catalog read would mean nothing to a client.
fn json_response(payload: &serde_json::Value, stale: bool) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(CACHE_HEADER, if stale { "stale" } else { "fresh" })
        .body(Body::from(
            serde_json::to_vec(payload).unwrap_or_else(|_| b"null".to_vec()),
        ))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::DECISION_HEADER;
    use super::{
        CACHE_HEADER, COMPRESSION_ECHO, CacheControl, CutRecord, DENY_HEADER, Dialect,
        FALLBACK_ATTEMPTS_HEADER, FailureSink, Keepalive, LATENCY_MS_HEADER, MODEL_HEADER,
        PROVIDER_HEADER, RESPONSE_COST_HEADER, SAVINGS_TOKENS_HEADER, Stages, TOKENS_IN_HEADER,
        TOKENS_OUT_HEADER, TOKENS_PER_SECOND_HEADER, USAGE_FRAME_CAP, UsageTee, VERSION_HEADER,
        accept_forces_stream, build_chain, card_json, completion_seen, declares_stream, decorate,
        error, error_because, kind_for, model, models_head, not_found, outcome_label, require_json,
        resolve, unknown_model, usage_from_frames,
    };
    use super::{Continuation, SentText, resume_body, resumed};
    use crate::app::{AppState, Components};
    use crate::config::{ComboTarget, ServerConfig};
    use crate::models::ModelCard;
    use ar_route::{
        AbortReport, ArExec, AttemptOutcome, CanonicalRequest, ExecError, MediaReply, ProviderId,
        Strategy, Strng, Upstream,
    };
    use ar_route::{ChunkStream, CutFlag};
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::Response;
    use bytes::Bytes;

    fn candidate(provider: &str) -> ar_route::Candidate {
        ar_route::Candidate::new(ProviderId::new(provider), "m")
    }

    /// A server whose catalog is exactly `extra`, and which the routes that need
    /// a `State` can be driven against.
    fn state(extra: Vec<ModelCard>) -> AppState {
        Components {
            extra_models: extra,
            ..Components::unconfigured(ServerConfig::single(0, Strategy::Priority, Vec::new()))
        }
        .into_state()
    }

    /// A server with one dispatchable provider, for the request-path tests.
    ///
    /// The executor is `NullExec`, so a request that survives to dispatch fails
    /// there — which is the useful signal here: it distinguishes "refused at the
    /// edge" from "reached the pipeline", which is exactly what the 415 and Accept
    /// tests are about.
    fn routed() -> AppState {
        let components = Components::unconfigured(one_provider_config());
        let exec = components.exec.clone();
        Components { exec, ..components }.into_state()
    }

    /// The same one-provider server with the executor swapped, for the tests
    /// that must observe what dispatch forwards rather than only that it
    /// refused. `NullExec` records nothing, which is exactly what these tests
    /// cannot work with.
    fn routed_under(exec: Arc<dyn ArExec>) -> AppState {
        Components {
            exec,
            ..Components::unconfigured(one_provider_config())
        }
        .into_state()
    }

    /// [`routed_under`], plus the in-memory usage ledger an accounting test
    /// needs to observe the write rather than only the headers.
    fn routed_with_ledger(
        exec: Arc<dyn ArExec>,
        ledger: Arc<Mutex<ar_tokens::Ledger>>,
    ) -> AppState {
        Components {
            exec,
            ledger: Some(ledger),
            ..Components::unconfigured(one_provider_config())
        }
        .into_state()
    }

    /// Answers every chat dispatch with the canned chunks as a stream.
    struct CannedExec(Vec<&'static str>);

    impl ArExec for CannedExec {
        fn post_chat<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            let chunks: Vec<Bytes> = self.0.iter().map(|c| Bytes::from(*c)).collect();
            Box::pin(async move { Ok(Upstream::success(Box::pin(futures::stream::iter(chunks)))) })
        }

        fn post_media<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _model: Option<&'a str>,
            _endpoint: &'a str,
            _content_type: &'a str,
            _body: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
            Box::pin(async move { Err(ExecError::Transport("no media dispatch in this fixture".to_owned())) })
        }
    }

    /// The config every request-path test dispatches against: provider `p`
    /// serving model `m` on a loopback URL no test ever dials.
    fn one_provider_config() -> ServerConfig {
        let mut config = ServerConfig::single(
            0,
            Strategy::Priority,
            vec![
                crate::exec::ProviderConfig::new(
                    ProviderId::new("p"),
                    "http://127.0.0.1:1/v1",
                    "k",
                )
                .with_model("m"),
            ],
        );
        config.combos = vec![crate::config::RouteCombo::new(
            "m",
            Strategy::Priority,
            vec![ComboTarget::new(ProviderId::new("p"), "m")],
        )];
        config
    }

    /// A two-provider server whose combo routes with `lkgp`, so the pin
    /// table is what decides the chain.
    fn lkgp_state() -> AppState {
        let mut config = ServerConfig::single(
            0,
            Strategy::Lkgp,
            vec![
                crate::exec::ProviderConfig::new(
                    ProviderId::new("first"),
                    "http://127.0.0.1:1/v1",
                    "k",
                )
                .with_model("m"),
                crate::exec::ProviderConfig::new(
                    ProviderId::new("second"),
                    "http://127.0.0.1:1/v1",
                    "k",
                )
                .with_model("m"),
            ],
        );
        config.combos = vec![crate::config::RouteCombo::new(
            "combo",
            Strategy::Lkgp,
            vec![
                ComboTarget::new(ProviderId::new("first"), "m"),
                ComboTarget::new(ProviderId::new("second"), "m"),
            ],
        )];
        Components::unconfigured(config).into_state()
    }

    /// A config whose combo is a `fusion` panel over two providers, with an
    /// optional named judge — the shape the judge half exists for.
    fn fusion_config(judge: Option<&str>) -> ServerConfig {
        let mut config = ServerConfig::single(
            0,
            Strategy::Fusion,
            vec![
                crate::exec::ProviderConfig::new(
                    ProviderId::new("a"),
                    "http://127.0.0.1:1/v1",
                    "k",
                )
                .with_model("m"),
                crate::exec::ProviderConfig::new(
                    ProviderId::new("b"),
                    "http://127.0.0.1:1/v1",
                    "k",
                )
                .with_model("m"),
            ],
        );
        let mut combo = crate::config::RouteCombo::new(
            "m",
            Strategy::Fusion,
            vec![
                ComboTarget::new(ProviderId::new("a"), "m"),
                ComboTarget::new(ProviderId::new("b"), "m"),
            ],
        );
        combo.judge_model = judge.map(str::to_owned);
        config.combos = vec![combo];
        config
    }

    /// A two-stage pipeline server, and an executor that answers each stage
    /// with its own provider's name.
    fn pipeline_config() -> ServerConfig {
        let mut config = ServerConfig::single(
            0,
            Strategy::Pipeline,
            vec![
                crate::exec::ProviderConfig::new(
                    ProviderId::new("a"),
                    "http://127.0.0.1:1/v1",
                    "k",
                )
                .with_model("m"),
                crate::exec::ProviderConfig::new(
                    ProviderId::new("b"),
                    "http://127.0.0.1:1/v1",
                    "k",
                )
                .with_model("m"),
            ],
        );
        config.combos = vec![crate::config::RouteCombo::new(
            "m",
            Strategy::Pipeline,
            vec![
                ComboTarget::new(ProviderId::new("a"), "m"),
                ComboTarget::new(ProviderId::new("b"), "m"),
            ],
        )];
        config
    }

    /// Answers each stage with a body naming the provider that served it, and
    /// records the bodies it was handed so a test can see what the *next*
    /// stage received.
    struct StagedExec {
        seen: Mutex<Vec<(String, String)>>,
    }

    impl StagedExec {
        fn new() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl ArExec for StagedExec {
        fn post_chat<'a>(
            &'a self,
            provider: &'a ProviderId,
            canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            let body = String::from_utf8_lossy(&canonical.body).into_owned();
            let name = provider.as_str().to_owned();
            if let Ok(mut seen) = self.seen.lock() {
                seen.push((name.clone(), body));
            }
            let answer = format!(
                r#"{{"choices":[{{"message":{{"content":"from {name}"}}}}]}}"#
            );
            Box::pin(async move {
                Ok(Upstream::success(Box::pin(futures::stream::once(async move {
                    bytes::Bytes::from(answer)
                }))))
            })
        }

        fn post_media<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _model: Option<&'a str>,
            _endpoint: &'a str,
            _content_type: &'a str,
            _body: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
            Box::pin(async move {
                Err(ExecError::Transport("no media in this fixture".to_owned()))
            })
        }
    }

    /// The regression that pins `pipeline` as a **staged** dispatch.
    ///
    /// It used to be wired to nothing: the server had no `Strategy::Pipeline`
    /// branch, so a `strategy: pipeline` combo went through the ordinary
    /// attempt loop, where the stages became *fallbacks* — the second stage
    /// answered the client's question instead of the first stage's. A combo
    /// that said `pipeline` silently did something else.
    ///
    /// The proof is the second stage's input: it must carry the **first
    /// stage's answer**, not the client's question.
    #[tokio::test]
    async fn a_pipeline_stage_feeds_the_next_stage() {
        let exec = Arc::new(StagedExec::new());
        let router = crate::app::app(
            Components {
                exec: exec.clone(),
                ..Components::unconfigured(pipeline_config())
            }
            .into_state(),
        );
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"the question"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_of(resp).await;
        assert_eq!(
            body["choices"][0]["message"]["content"], "from b",
            "the LAST stage's answer is the client's answer"
        );

        let seen = exec.seen.lock().map(|s| s.clone()).unwrap_or_default();
        assert_eq!(seen.len(), 2, "both stages ran: {seen:?}");
        assert_eq!(seen[0].0, "a", "the first stage is the first target");
        assert_eq!(seen[1].0, "b", "the second stage is the second target");
        assert!(
            seen[1].1.contains("from a"),
            "the second stage must receive the first stage's answer: {:?}",
            seen[1].1
        );
        assert!(
            !seen[1].1.contains("the question"),
            "the second stage must NOT receive the client's question again: {:?}",
            seen[1].1
        );
    }

    fn routed_fusion(exec: Arc<dyn ArExec>, judge: Option<&str>) -> AppState {
        Components {
            exec,
            ..Components::unconfigured(fusion_config(judge))
        }
        .into_state()
    }

    /// Refuses every dispatch with the canned upstream verdict — the
    /// projection tests' provider-surfaces-errors fixture.
    struct FailingExec(u16, &'static str);

    impl ArExec for FailingExec {
        fn post_chat<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            let status = self.0;
            let body = self.1;
            Box::pin(async move {
                Ok(Upstream::failure(
                    StatusCode::from_u16(status).expect("a status the fixture picked"),
                    Bytes::from_static(body.as_bytes()),
                    None,
                ))
            })
        }

        fn post_media<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _model: Option<&'a str>,
            _endpoint: &'a str,
            _content_type: &'a str,
            _body: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
            Box::pin(async move { Err(ExecError::Transport("no media dispatch in this fixture".to_owned())) })
        }
    }

    /// Records the canonical body of every dispatch, then answers with an
    /// empty 200 stream — the smallest upstream an observable dispatch test
    /// can drive.
    struct RecordingExec(Arc<Mutex<Vec<Bytes>>>);

    impl ArExec for RecordingExec {
        fn post_chat<'a>(
            &'a self,
            _provider: &'a ProviderId,
            canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            self.0
                .lock()
                .expect("recorder lock")
                .push(canonical.body.clone());
            Box::pin(async move {
                // One inert OpenAI chunk, not an empty stream: since the MW5
                // first-byte gate, a 2xx whose stream yields no byte fails
                // over, and an empty fixture stream would turn every streamed
                // test into a 503.
                Ok(Upstream::success(Box::pin(futures::stream::once(async {
                    Bytes::from_static(b"data: {\"choices\":[{\"delta\":{}}]}\n\n")
                }))))
            })
        }

        fn post_media<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _model: Option<&'a str>,
            _endpoint: &'a str,
            _content_type: &'a str,
            _body: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
            // The chat recorder is only pointed at chat paths; a media dispatch
            // reaching it means a test wired the wrong route.
            Box::pin(async move {
                Err(ExecError::Transport("the recording exec serves no media path".to_owned()))
            })
        }
    }

    /// Drives one request through the real router, every layer included.
    async fn drive(app: &axum::Router, req: axum::http::Request<axum::body::Body>) -> Response {
        use tower::ServiceExt;
        app.clone().oneshot(req).await.expect("router answers")
    }

    /// A chat POST with a JSON body, plus any extra headers.
    fn chat(body: &str, extra: &[(&str, &str)]) -> axum::http::Request<axum::body::Body> {
        let mut req = axum::http::Request::builder()
            .header("host", "127.0.0.1:20128")
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_owned()))
            .expect("request builds");
        for (name, value) in extra {
            req.headers_mut().insert(
                axum::http::HeaderName::try_from(*name).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        req
    }

    #[test]
    fn names_the_ids_that_do_exist_in_an_unknown_model_error() {
        let got = unknown_model("gpt-4o", &["default", "cheap"]);
        assert!(got.contains("gpt-4o"), "unhelpful error: {got}");
        assert!(got.contains("default, cheap"), "unhelpful error: {got}");
    }

    #[test]
    fn says_so_plainly_when_no_combo_is_configured() {
        assert!(unknown_model("m", &[]).contains("no combo is configured"));
    }

    #[test]
    fn puts_the_picked_provider_first() {
        let cands = [candidate("a"), candidate("b")];
        let chain = build_chain(&ProviderId::new("b"), &cands);
        assert_eq!(chain[0].provider.as_str(), "b");
    }

    #[test]
    fn deduplicates_a_provider_appearing_on_two_models() {
        let cands = [candidate("a"), candidate("a"), candidate("b")];
        let chain = build_chain(&ProviderId::new("a"), &cands);
        assert_eq!(
            chain.len(),
            2,
            "a repeated provider must not be attempted twice: {chain:?}"
        );
    }

    #[test]
    fn keeps_both_models_of_one_multi_credential_provider() {
        // The failure domain is the credential, not the provider: a combo
        // pairing two models on one multi-key provider must keep both
        // targets, or its fallback chain is the winner alone and a single
        // 429 ends the request with nothing left to fail over to.
        let cands = [
            ar_route::Candidate::new(ProviderId::new("nvidia"), "glm-5.3"),
            ar_route::Candidate::new(ProviderId::new("nvidia"), "kimi-k3"),
        ];
        let chain = build_chain(&ProviderId::new("nvidia"), &cands);
        assert_eq!(
            chain.len(),
            2,
            "a same-provider fallback that dedupes away is no fallback: {chain:?}"
        );
    }

    /// The regression that pins the session reaching the strategy.
    ///
    /// The `lkgp` strategy reads the pin the server wrote for the session's
    /// last good provider. The server *wrote* that pin on every success, and
    /// the strategy's own unit tests passed, because the unit tests call
    /// `pick` directly with a session. The request path did not: it called
    /// `pick(strategy, None, …)`, so the pin was never read and `lkgp` behaved
    /// exactly like `priority` on every live request.
    ///
    /// This test drives the *server's* chain builder, so it fails on the old
    /// code (the chain leads with the config order) and passes on the fixed
    /// code (the chain leads with the pinned provider).
    #[test]
    fn the_lkgp_pin_reaches_the_live_routing_path() {
        let state = lkgp_state();
        // The pin names the *second* provider, so a config-order chain and a
        // pin-honouring chain differ.
        state
            .lkgp
            .record("sess-1", &ProviderId::new("second"));
        let plan = resolve(
            &state,
            &CanonicalRequest {
                model: Strng::from("combo"),
                body: Bytes::new(),
                stream: false,
                session: Some(Strng::from("sess-1")),
            },
        )
        .expect("the combo resolves");
        assert_eq!(
            plan.chain.first().map(|t| t.provider.as_str()),
            Some("second"),
            "the pinned provider must lead the chain: {:?}",
            plan.chain
        );
    }

    /// The same path, with no session: a client that sent no stickiness key
    /// must get the config order, not somebody else's pin.
    #[test]
    fn a_request_without_a_session_ignores_the_pin() {
        let state = lkgp_state();
        state
            .lkgp
            .record("sess-1", &ProviderId::new("second"));
        let plan = resolve(
            &state,
            &CanonicalRequest {
                model: Strng::from("combo"),
                body: Bytes::new(),
                stream: false,
                session: None,
            },
        )
        .expect("the combo resolves");
        assert_eq!(
            plan.chain.first().map(|t| t.provider.as_str()),
            Some("first"),
            "an anonymous request must not inherit a session's pin: {:?}",
            plan.chain
        );
    }

    #[test]
    fn labels_a_succeeded_outcome_ok() {
        assert_eq!(
            outcome_label(&AttemptOutcome::Succeeded {
                provider: ProviderId::new("p"),
                model: Strng::from("m"),
                attempts: 1,
                upstream: Upstream::success(Box::pin(futures::stream::empty())),
            }),
            "ok"
        );
    }

    #[test]
    fn labels_an_abort_as_abort() {
        let report = AbortReport {
            status: axum::http::StatusCode::BAD_REQUEST,
            reason: "bad".to_owned(),
            tried: 1,
            last_provider: None,
        };
        assert_eq!(outcome_label(&AttemptOutcome::Abort(report)), "abort");
    }

    // --- response-header emission ----------------------------------------

    fn succeeded(attempts: u16) -> AttemptOutcome {
        AttemptOutcome::Succeeded {
            provider: ProviderId::new("openai"),
            model: Strng::from("gpt-5.4"),
            attempts,
            upstream: Upstream::success(Box::pin(futures::stream::empty())),
        }
    }

    fn headers_for(meta: ar_tokens::ResponseMeta, attempts: u16) -> HeaderMap {
        let resp = decorate(
            succeeded(attempts),
            Arc::new(ar_route::Resilience::new()),
            false,
            Dialect::OpenAi,
            Strategy::Priority,
            Stages {
                failures: FailureSink::none(),
                continuation: None,
                compression: "default;engines=-".to_owned(),
                cache_state: ar_cache::CacheState::Miss,
                cache_key: None,
                planned_target: None,
                cache: None,
                model: ar_route::Strng::from("m"),
                cache_control: CacheControl::default(),
                meta,
            },
        );
        resp.headers().clone()
    }

    fn header_of(map: &HeaderMap, name: &str) -> String {
        map.get(name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .to_str()
            .expect("header text")
            .to_owned()
    }

    #[test]
    fn stamps_the_token_pair_the_response_meta_carries() {
        let meta = ar_tokens::ResponseMeta::from_upstream(
            &ar_tokens::PricingTable::default(),
            "openai",
            "gpt-4o",
            &serde_json::json!({ "prompt_tokens": 120, "completion_tokens": 34 }),
        )
        .with_latency(Duration::from_secs(2));
        let headers = headers_for(meta, 1);
        assert_eq!(
            (
                header_of(&headers, TOKENS_IN_HEADER).as_str(),
                header_of(&headers, TOKENS_OUT_HEADER).as_str()
            ),
            ("120", "34")
        );
    }

    #[test]
    fn stamps_the_cost_at_the_ledger_precision() {
        // Six decimals, because the ledger stores integer micro-dollars and a
        // header at another width would read as a different amount.
        let headers = headers_for(ar_tokens::ResponseMeta::default(), 1);
        assert_eq!(header_of(&headers, RESPONSE_COST_HEADER), "0.000000");
    }

    #[test]
    fn stamps_the_savings_a_compression_stage_reported() {
        let headers = headers_for(
            ar_tokens::ResponseMeta::default().with_savings_tokens(128),
            1,
        );
        assert_eq!(header_of(&headers, SAVINGS_TOKENS_HEADER), "128");
    }

    #[test]
    fn stamps_the_latency_in_whole_milliseconds() {
        let headers = headers_for(
            ar_tokens::ResponseMeta::default().with_latency(Duration::from_millis(42)),
            1,
        );
        assert_eq!(header_of(&headers, LATENCY_MS_HEADER), "42");
    }

    #[test]
    fn stamps_the_fallback_attempts_the_attempt_loop_spent() {
        let headers = headers_for(ar_tokens::ResponseMeta::default(), 3);
        assert_eq!(header_of(&headers, FALLBACK_ATTEMPTS_HEADER), "3");
    }

    #[test]
    fn names_the_serving_model_when_a_dispatcher_resolved_one() {
        // The combo the client spelled cannot identify which member answered:
        // a combo holding three models on one provider dispatches each of them
        // in turn, and only the target's own name separates the rows. The
        // fixture's `succeeded` carries `gpt-5.4` for exactly the case where
        // the client asked for something else.
        let headers = headers_for(ar_tokens::ResponseMeta::default(), 1);
        assert!(
            header_of(&headers, DECISION_HEADER).contains("model=gpt-5.4"),
            "the decision header names the target the loop resolved, not the request's spelling"
        );
    }

    #[test]
    fn omits_a_model_from_the_decision_when_nothing_served() {
        // A retry exhausted its chain: no target answered, so naming one would
        // be a guess. `-` is the honest placeholder the provider field uses.
        let resp = decorate(
            AttemptOutcome::Retry {
                after: Duration::from_secs(3),
                provider: ProviderId::new("nvidia"),
                tried: 3,
            },
            Arc::new(ar_route::Resilience::new()),
            false,
            Dialect::OpenAi,
            Strategy::Priority,
            Stages {
                failures: FailureSink::none(),
                continuation: None,
                compression: "default;engines=-".to_owned(),
                cache_state: ar_cache::CacheState::Miss,
                cache_key: None,
                planned_target: None,
                cache: None,
                model: ar_route::Strng::from("free-stack"),
                cache_control: CacheControl::default(),
                meta: ar_tokens::ResponseMeta::default(),
            },
        );
        assert!(
            header_of(resp.headers(), DECISION_HEADER).contains("model=-"),
            "nothing served, so the field is the same placeholder the provider field uses"
        );
    }

    #[test]
    fn stamps_generation_speed_when_the_elapsed_time_allows_a_division() {
        let meta = ar_tokens::ResponseMeta::from_upstream(
            &ar_tokens::PricingTable::default(),
            "openai",
            "gpt-4o",
            &serde_json::json!({ "completion_tokens": 100 }),
        )
        .with_latency(Duration::from_secs(2));
        let headers = headers_for(meta, 1);
        assert_eq!(header_of(&headers, TOKENS_PER_SECOND_HEADER), "50.000");
    }

    #[test]
    fn stamps_a_prompt_count_the_provider_reported_without_completion() {
        let meta = ar_tokens::ResponseMeta::from_upstream(
            &ar_tokens::PricingTable::default(),
            "openai",
            "gpt-4o",
            &serde_json::json!({ "prompt_tokens": 120 }),
        );
        let headers = headers_for(meta, 1);
        assert_eq!(header_of(&headers, TOKENS_IN_HEADER), "120");
    }

    #[test]
    fn omits_generation_speed_when_the_elapsed_time_was_zero() {
        // A `0` would read as instant, and a client dividing by it would report
        // nonsense rather than nothing.
        let headers = headers_for(ar_tokens::ResponseMeta::default(), 1);
        assert!(!headers.contains_key(TOKENS_PER_SECOND_HEADER));
    }

    #[test]
    fn stamps_a_cost_that_matches_the_ledger_row() {
        // The claim the whole surface rests on: what a client is told and what an
        // operator reads out of `ar cost-report` are one arithmetic, not two.
        let ledger = ar_tokens::Ledger::open_in_memory().expect("ledger");
        let meta = record_million_token_completion(&ledger);
        let mut report = ledger.report(1).expect("report");
        let row = report.rows.remove(0);
        assert_eq!(
            header_of(&headers_for(meta, 1), RESPONSE_COST_HEADER),
            row.cost_usd.as_decimal_string()
        );
    }

    #[test]
    fn stamps_a_token_count_that_matches_the_ledger_row() {
        // Completion-only usage, so the ledger's `total_tokens` *is* the prompt
        // count and the two can be compared without widening `LedgerRow`.
        let ledger = ar_tokens::Ledger::open_in_memory().expect("ledger");
        let meta = ledger
            .record_response(
                &ar_tokens::Who {
                    key_id: "k1",
                    provider: "openai",
                    model: "gpt-4o",
                    combo: "",
                },
                &serde_json::json!({ "prompt_tokens": 1_000_000 }),
                1_700_000_000,
                &priced_table(),
            )
            .expect("record");
        let mut report = ledger.report(1).expect("report");
        let row = report.rows.remove(0);
        assert_eq!(
            header_of(&headers_for(meta, 1), TOKENS_IN_HEADER),
            row.total_tokens.to_string()
        );
    }

    fn priced_table() -> ar_tokens::PricingTable {
        let mut t = ar_tokens::PricingTable::default();
        t.set(
            "openai",
            "gpt-4o",
            ar_tokens::Prices {
                input_micros_per_mtok: 2_500_000,
                output_micros_per_mtok: 10_000_000,
            },
        );
        t
    }

    fn record_million_token_completion(ledger: &ar_tokens::Ledger) -> ar_tokens::ResponseMeta {
        ledger
            .record_response(
                &ar_tokens::Who {
                    key_id: "k1",
                    provider: "openai",
                    model: "gpt-4o",
                    combo: "",
                },
                &serde_json::json!({ "prompt_tokens": 1_000_000, "completion_tokens": 1_000_000 }),
                1_700_000_000,
                &priced_table(),
            )
            .expect("record")
    }

    #[test]
    fn a_combo_target_carries_neutral_signals_by_default() {
        let t = ComboTarget::new(ProviderId::new("p"), "m");
        assert_eq!((t.input_usd_per_mtok, t.weight, t.quota), (None, 1, None));
    }

    // --- /v1/models card shape -----------------------------------------

    #[test]
    fn a_card_carries_the_openai_shape_plus_the_upstream_name() {
        let card = ModelCard::new("openai", "gpt-4o-mini");
        let got = card_json(&card);
        assert_eq!(got["id"], "openai/gpt-4o-mini");
        assert_eq!(got["object"], "model");
        assert_eq!(got["owned_by"], "openai");
        assert_eq!(got["ar_upstream_model"], "gpt-4o-mini");
    }

    #[test]
    fn a_card_carries_a_context_window_a_discovery_client_can_use() {
        // The bug this pins: `/v1/models` cards shipped without
        // `context_length`, and omp's `openai-models-list` provider drops every
        // card that omits one. ar answered 9715 models and omp registered
        // none of them — the provider appeared empty. A missing field does not
        // under-report a model here, it deletes it.
        let got = card_json(&ModelCard::new("openai", "gpt-4o-mini"));
        let ctx = got["context_length"]
            .as_u64()
            .expect("context_length must be a number a client can size a request with");
        assert!(
            ctx >= 128_000,
            "a card reporting {ctx} tokens is smaller than any real agent prompt"
        );
    }

    #[test]
    fn an_unknown_provider_still_reports_the_conservative_window() {
        // Never zero and never omitted: `0` reads as "no context" to a client
        // that then refuses the model, which is the same failure as the field
        // being absent, one step further from the cause.
        let card = ModelCard::new("no-such-provider", "m");
        assert_eq!(card.context_length, ModelCard::UNKNOWN_CONTEXT);
        let got = card_json(&card);
        assert_eq!(got["context_length"], ModelCard::UNKNOWN_CONTEXT);
    }

    #[test]
    fn a_discovered_figure_wins_over_the_provider_ceiling() {
        // The 128K complaint, pinned at its source. `openai`'s provider ceiling
        // is 1.05M, which is the *largest* window any of its models offers, so a
        // per-model card built from it over-reports every smaller model. The
        // figure models.dev published for this one model is the answer.
        let discovered = ar_registry::discovery::DiscoveredModel {
            context_length: 128_000,
            max_output_tokens: 16_384,
            input_image: true,
        };
        let card = ModelCard::with_discovered("openai", "gpt-4o", Some(discovered));
        assert_eq!(
            card.context_length, 128_000,
            "not the 1.05M provider ceiling"
        );
        assert_eq!(card.max_output_tokens, 16_384);
        assert!(card.input_image);
    }

    #[test]
    fn a_discovered_zero_falls_through_rather_than_reporting_zero() {
        // models.dev omits `limit` for a model declaring no ceiling. Reporting
        // that 0 verbatim reads as "no context" and makes a client drop the
        // model — the same failure as omitting the field, so it must fall
        // through to the same lookups the non-discovered path uses.
        let card = ModelCard::with_discovered(
            "openai",
            "gpt-4o",
            Some(ar_registry::discovery::DiscoveredModel::default()),
        );
        assert!(card.context_length > 0, "a client would refuse a 0 window");
    }

    #[test]
    fn a_discovered_output_ceiling_reaches_the_wire() {
        // A card that knows how much a model can answer should say so; the field
        // is omitted rather than zeroed when nothing declares it, because a zero
        // maximum is a claim and no model has one.
        let with_out = ModelCard::with_discovered(
            "p",
            "m",
            Some(ar_registry::discovery::DiscoveredModel {
                context_length: 1000,
                max_output_tokens: 999,
                input_image: false,
            }),
        );
        assert_eq!(card_json(&with_out)["max_output_tokens"], 999);
        assert_eq!(card_json(&with_out)["input_image"], false);

        let bare = ModelCard::new("no-such-provider", "m");
        let got = card_json(&bare);
        assert!(
            got.get("max_output_tokens").is_none(),
            "an unknown ceiling must be omitted, not reported as 0: {got}"
        );
    }

    #[test]
    fn a_card_dates_itself_from_the_registry_snapshot_build() {
        // Not a per-model timestamp: the snapshot is the thing being dated, and
        // inventing a per-model date would be a number nothing on disk backs.
        let got = card_json(&ModelCard::new("p", "m"));
        assert_eq!(got["created"], ar_registry::BUILT_AT);
        assert_ne!(got["created"], 0, "build.rs did not stamp the build time");
    }

    #[test]
    fn a_card_declares_no_permissions_and_is_its_own_root() {
        // Auth here is one bearer gate for the whole server, so no card grants a
        // per-model scope, and a catalog card is never an alias of another one.
        let got = card_json(&ModelCard::new("p", "m"));
        assert_eq!(got["permission"], serde_json::json!([]));
        assert_eq!(got["root"], got["id"]);
    }

    // --- HEAD /v1/models ----------------------------------------------

    #[tokio::test]
    async fn head_models_is_200_with_no_body() {
        let resp = models_head(
            State(state(vec![])),
            axum::extract::OriginalUri("/v1/models".parse().expect("uri")),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()["content-type"],
            "application/json",
            "the probe must still advertise the media type it would have sent"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body reads");
        assert!(body.is_empty(), "HEAD returned {} bytes", body.len());
    }

    // --- GET /v1/models/{model} ---------------------------------------

    #[tokio::test]
    async fn serves_the_card_for_a_known_id() {
        let card = ModelCard::new("openai", "gpt-4o-mini");
        let state = state(vec![card.clone()]);
        let uri = format!("/v1/models/{}", card.id);
        let resp = model(
            State(state),
            axum::extract::OriginalUri(uri.parse().expect("uri")),
            HeaderMap::new(),
            Path(card.id.clone()),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body reads"),
        )
        .expect("single-model body is JSON");
        assert_eq!(body, card_json(&card), "one id, one card, one shape");
    }

    #[tokio::test]
    async fn answers_an_unknown_id_with_model_not_found() {
        let state = state(vec![ModelCard::new("openai", "gpt-4o-mini")]);
        let resp = model(
            State(state),
            axum::extract::OriginalUri("/v1/models/gpt-4o".parse().expect("uri")),
            HeaderMap::new(),
            Path("gpt-4o".to_owned()),
        )
        .await;
        assert_eq!(resp.status(), 404);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "model_not_found");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("gpt-4o"),
            "the miss must name what was asked for: {body}"
        );
    }

    // --- error envelope ------------------------------------------------

    /// Reads a response's body as JSON.
    async fn body_of(resp: Response) -> serde_json::Value {
        serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body reads"),
        )
        .expect("error body is JSON")
    }

    #[tokio::test]
    async fn an_error_carries_a_type_alongside_its_code() {
        let body = body_of(error_because(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "credential_rejected",
            "nope",
        ))
        .await;
        assert_eq!(body["error"]["type"], "authentication_error");
        assert_eq!(body["error"]["code"], "invalid_api_key");
        assert_eq!(body["error"]["reason"], "credential_rejected");
        assert_eq!(body["error"]["message"], "nope");
    }

    #[tokio::test]
    async fn an_error_omits_a_reason_it_does_not_have() {
        // The reference emits `reason: undefined` for the cases it has no finer
        // cause for, which serializes to an absent key. Emitting `null` instead
        // would make every client that does `if ("reason" in error)` take the
        // wrong branch.
        let body = body_of(error(StatusCode::SERVICE_UNAVAILABLE, "no provider")).await;
        assert_eq!(body["error"]["code"], "request_rejected");
        assert!(
            body["error"].get("reason").is_none(),
            "an absent reason must be absent, not null: {body}"
        );
    }

    #[test]
    fn the_type_matches_the_status_it_was_built_for() {
        // One table, not a per-call-site choice: a 404 labelled
        // `invalid_request_error` teaches a client to ignore the field.
        assert_eq!(kind_for(StatusCode::BAD_REQUEST), "invalid_request_error");
        assert_eq!(kind_for(StatusCode::UNAUTHORIZED), "authentication_error");
        assert_eq!(kind_for(StatusCode::NOT_FOUND), "not_found");
        assert_eq!(
            kind_for(StatusCode::UNSUPPORTED_MEDIA_TYPE),
            "invalid_request_error"
        );
        assert_eq!(kind_for(StatusCode::TOO_MANY_REQUESTS), "rate_limit_error");
        assert_eq!(kind_for(StatusCode::BAD_GATEWAY), "server_error");
    }

    // --- unknown-route fallback ----------------------------------------

    #[tokio::test]
    async fn an_unknown_path_is_a_json_404_that_carries_the_path() {
        let req = axum::http::Request::builder()
            .header("host", "127.0.0.1:20128")
            .uri("/v1/chat/completionz")
            .body(axum::body::Body::empty())
            .expect("request builds");
        let resp = not_found(req).await;
        assert_eq!(resp.status(), 404);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "unknown_route");
        assert_eq!(body["error"]["type"], "not_found");
        assert_eq!(
            body["error"]["path"], "/v1/chat/completionz",
            "a client that parsed nothing must at least learn which path failed: {body}"
        );
    }

    // --- 415 guard ------------------------------------------------------

    fn post_with_content_type(value: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = value {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        headers
    }

    #[test]
    fn refuses_a_body_that_is_not_json() {
        let resp = require_json(&post_with_content_type(Some("text/plain")))
            .expect("text/plain is not a JSON route's media type");
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn refuses_a_body_with_no_declared_content_type() {
        // "I did not say" is not "I said JSON", and the 400 this would otherwise
        // become reads as a syntax error rather than a missing header.
        assert!(require_json(&post_with_content_type(None)).is_some());
    }

    #[test]
    fn admits_json_with_a_charset_parameter() {
        // The parameter is not a different media type; refusing it would break
        // every client that sets a charset, which is most of them.
        assert!(
            require_json(&post_with_content_type(Some(
                "application/json; charset=utf-8"
            )))
            .is_none()
        );
    }

    #[test]
    fn admits_json_regardless_of_case() {
        assert!(require_json(&post_with_content_type(Some("Application/JSON"))).is_none());
    }

    // --- Accept forces streaming ---------------------------------------

    fn accept(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_str(value).expect("header value"),
        );
        headers
    }

    #[test]
    fn a_pure_sse_accept_header_opts_into_streaming() {
        assert!(accept_forces_stream(&accept("text/event-stream")));
    }

    #[test]
    fn an_accept_header_naming_both_json_and_sse_stays_json() {
        // The OpenAI and Vercel AI SDK non-stream signature. Streaming this one
        // hands the client a body it cannot parse.
        assert!(!accept_forces_stream(&accept(
            "application/json, text/event-stream"
        )));
    }

    #[test]
    fn a_wildcard_accept_header_does_not_opt_into_streaming() {
        assert!(!accept_forces_stream(&accept("*/*")));
    }

    #[test]
    fn an_explicit_stream_false_is_a_declaration_too() {
        // The body is the client's own statement; `Accept` is only a fallback for
        // a body that said nothing, so the two signals have to be distinguishable
        // and this is the pair that decides it.
        assert!(declares_stream(&Bytes::from_static(
            br#"{"model":"m","stream":false}"#
        )));
    }

    #[test]
    fn a_body_that_says_nothing_about_stream_does_not_declare_it() {
        assert!(!declares_stream(&Bytes::from_static(br#"{"model":"m"}"#)));
    }

    #[test]
    fn an_explicit_null_stream_does_not_declare_it() {
        // What a client that builds its body programmatically sends for "unset".
        // Reading it as a declaration would refuse the `Accept` opt-in to a client
        // that never opted out of it.
        assert!(!declares_stream(&Bytes::from_static(
            br#"{"model":"m","stream":null}"#
        )));
    }

    #[test]
    fn an_explicit_stream_true_is_a_declaration() {
        assert!(declares_stream(&Bytes::from_static(
            br#"{"model":"m","stream":true}"#
        )));
    }

    // --- keepalive frames ----------------------------------------------

    #[test]
    fn the_responses_terminator_is_its_own_completed_event() {
        // Responses discriminates on a `type` inside the payload, so the
        // terminator is the completed event rather than a terminator line.
        assert!(!completion_seen(Dialect::Responses, b"data: [DONE]\n\n"));
        assert!(completion_seen(
            Dialect::Responses,
            b"data: {\"type\":\"response.completed\"}\n\n"
        ));
    }

    #[test]
    fn the_anthropic_terminator_is_its_own_stop_event() {
        // Not `data: [DONE]`, which is the OpenAI spelling: an Anthropic stream
        // that ends on the wrong terminator is a stream that ended wrong.
        assert!(completion_seen(
            Dialect::Anthropic,
            b"event: message_stop\ndata: {}\n\n"
        ));
        assert!(!completion_seen(Dialect::Anthropic, b"data: [DONE]\n\n"));
    }

    #[test]
    fn the_anthropic_keepalive_is_a_real_ping_event() {
        // The load-bearing assertion in the whole keepalive design: an SSE
        // comment is invisible to the client whose watchdog this exists for.
        let frame = Dialect::Anthropic
            .keepalive()
            .expect("anthropic has a keepalive");
        assert!(
            frame.starts_with("event: ping"),
            "not a real event: {frame}"
        );
        assert!(
            !frame.contains(": keepalive"),
            "a comment is not a ping: {frame}"
        );
    }

    #[test]
    fn the_anthropic_truncation_error_is_a_real_error_event() {
        // The same reasoning as the ping: the Anthropic spec defines
        // `event: error`, and a comment carrying an error reaches no client.
        let frame = Dialect::Anthropic
            .stream_error()
            .expect("anthropic names truncation");
        assert!(
            frame.starts_with("event: error"),
            "not a real event: {frame}"
        );
    }

    #[test]
    fn the_openai_chat_frames_never_emit_an_event_line() {
        // A line-based parser drops an unrecognised `event:` line and desyncs on
        // the `data:` after it, which loses the error the client needed. Both
        // frames, not just the keepalive.
        for frame in [
            Dialect::OpenAi.keepalive().expect("openai has a keepalive"),
            Dialect::OpenAi
                .stream_error()
                .expect("openai names truncation"),
        ] {
            assert!(
                !frame.contains("event:"),
                "an OpenAI frame emitted an event: {frame}"
            );
        }
    }

    #[test]
    fn the_responses_keepalive_is_a_real_in_progress_event() {
        // A Responses client discriminates on `type` inside the payload, so
        // "still working" has to be spelled `response.in_progress` rather than a
        // comment the parser skips.
        let frame = Dialect::Responses
            .keepalive()
            .expect("responses has a keepalive");
        assert!(
            frame.contains("\"type\":\"response.in_progress\""),
            "not an in-progress event: {frame}"
        );
        assert!(
            !frame.contains(": keepalive"),
            "a comment is not an event: {frame}"
        );
        // The payload must not look like a finished answer, or a client renders
        // content this server invented.
        assert!(frame.contains("\"status\":\"in_progress\""), "{frame}");
        assert!(frame.contains("\"output\":[]"), "{frame}");
    }

    #[test]
    fn the_openai_keepalive_is_an_inert_chunk() {
        // A comment is invisible to a client that resets its first-token
        // watchdog on decoded frames, which is every OpenAI-compatible client
        // ar is the front door for. So the frame is a real chunk — but one that
        // cannot be mistaken for answer text.
        let frame = Dialect::OpenAi.keepalive().expect("openai has a keepalive");
        assert!(
            frame.starts_with("data: "),
            "an OpenAI keepalive must be a decodable frame: {frame}"
        );
        let payload = frame
            .trim_start_matches("data: ")
            .trim_end()
            .trim_end_matches('\n');
        let chunk: serde_json::Value = serde_json::from_str(payload)
            .unwrap_or_else(|e| panic!("keepalive is not a decodable chunk: {e}"));
        assert_eq!(chunk["object"], "chat.completion.chunk");
        let choice = &chunk["choices"][0];
        assert_eq!(
            choice["delta"],
            serde_json::json!({}),
            "a keepalive that carries a delta would be concatenated into the answer"
        );
        assert!(
            choice["finish_reason"].is_null(),
            "a non-null finish_reason reads as a terminated stream: {frame}"
        );
    }

    #[test]
    fn ollama_gets_no_sse_frames() {
        // Its stream is NDJSON, so a keepalive would have to be a JSON line and
        // inventing one would be inventing a wire format.
        assert!(Dialect::Ollama.keepalive().is_none());
        assert!(Dialect::Ollama.stream_error().is_none());
        assert!(Dialect::Ollama.terminator().is_none());
    }

    #[test]
    fn every_dialect_maps_to_the_path_its_dialect_is_parsed_by() {
        // The route string is both the router's path and the translator's dialect
        // selector, so a mismatch is a 400 on every request to that route.
        for (dialect, path) in [
            (Dialect::OpenAi, "/v1/chat/completions"),
            (Dialect::Anthropic, "/v1/messages"),
            (Dialect::Responses, "/v1/responses"),
            (Dialect::Ollama, "/api/chat"),
        ] {
            assert_eq!(dialect.route(), path);
            // A body this dialect accepts, so the path is proven to name a
            // registered dialect rather than just a plausible-looking string.
            let probe: &[u8] = match dialect {
                Dialect::Responses => br#"{"model":"m","input":"hi"}"#,
                Dialect::Ollama => br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
                Dialect::Anthropic => {
                    br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#
                }
                Dialect::OpenAi => br#"{"model":"m","messages":[]}"#,
            };
            assert!(
                crate::translate::to_canonical_for_route(path, probe).is_ok(),
                "{path} is not a registered dialect"
            );
        }
    }

    #[test]
    fn a_stream_error_frame_never_carries_a_credential() {
        // The frames are synthesised here, not relayed, so they can only ever
        // contain what is written above — asserted so a future frame that quotes
        // an upstream body cannot slip a credential in.
        for dialect in [Dialect::OpenAi, Dialect::Anthropic, Dialect::Responses] {
            let frame = dialect
                .stream_error()
                .expect("sse dialects name truncation");
            assert!(frame.contains("stream_error"), "{dialect:?}: {frame}");
            assert!(!frame.contains("Bearer"), "{dialect:?}: {frame}");
        }
    }

    #[test]
    fn a_done_marker_is_recognised_inside_a_frame() {
        assert!(completion_seen(Dialect::OpenAi, b"data: [DONE]\n\n"));
        assert!(!completion_seen(Dialect::OpenAi, b"data: {}\n\n"));
        // A partial marker is not a completion on its own; the window in the
        // relay is what joins it to the bytes that follow (docs/22, B3).
        assert!(!completion_seen(Dialect::OpenAi, b"data: [DO"));
        // Answer text that spells the marker is preceded by its own text, not a
        // line break, so it is not a frame boundary.
        assert!(!completion_seen(
            Dialect::OpenAi,
            b"data: {\"choices\":[{\"delta\":{\"content\":\"x data: [DONE]\"}}]}\n\n"
        ));
    }

    #[test]
    fn a_finish_reason_string_completes_an_openai_stream() {
        // docs/22, B2: a completed answer that closes without `[DONE]`.
        assert!(completion_seen(
            Dialect::OpenAi,
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
        ));
        // `json.dumps` writes a space after the colon; the marker must not depend
        // on the encoder.
        assert!(completion_seen(
            Dialect::OpenAi,
            b"data: {\"choices\": [{\"finish_reason\": \"length\"}]}\n\n"
        ));
    }

    #[test]
    fn a_gemini_finish_reason_completes_a_raw_relayed_stream() {
        // A Gemini stream is relayed as Google sends it (docs/22, D6); its final
        // frame carries `finishReason` as a string.
        assert!(completion_seen(
            Dialect::OpenAi,
            b"data: {\"candidates\":[{\"finishReason\": \"STOP\"}]}\n\n"
        ));
        assert!(!completion_seen(
            Dialect::OpenAi,
            b"data: {\"candidates\":[{\"finishReason\":null}]}\n\n"
        ));
    }

    #[test]
    fn a_null_finish_reason_is_not_a_completion() {
        assert!(!completion_seen(
            Dialect::OpenAi,
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n"
        ));
    }

    #[test]
    fn an_escaped_finish_reason_in_answer_text_is_not_a_completion() {
        // The answer spells the key; JSON writes the quotes as `\"`, which never
        // matches the unescaped key.
        assert!(!completion_seen(
            Dialect::OpenAi,
            b"data: {\"choices\":[{\"delta\":{\"content\":\"\\\"finish_reason\\\":\\\"stop\\\"\"}}]}\n\n"
        ));
    }

    /// Drains a relay to a `Vec<String>`, one entry per frame.
    async fn drain(keepalive: Keepalive, chunks: Vec<Bytes>) -> Vec<String> {
        use futures::StreamExt;
        let stream: ar_route::ChunkStream = Box::pin(futures::stream::iter(chunks));
        keepalive
            .relay(stream, "p", "m")
            .map(|item| {
                String::from_utf8(item.expect("infallible").to_vec()).expect("frames are UTF-8")
            })
            .collect()
            .await
    }

    #[tokio::test]
    async fn a_finished_answer_without_done_is_not_named_truncated() {
        // docs/22, B2: the upstream sent its finish frame and closed. Nothing is
        // added after the answer.
        let frames = drain(
            Keepalive::new(Dialect::OpenAi, NOWAIT),
            vec![
                Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n"),
                Bytes::from_static(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"),
            ],
        )
        .await;
        assert_eq!(
            frames.len(),
            2,
            "no error after a finished answer: {frames:?}"
        );
        assert!(!frames.iter().any(|f| f.contains("stream_error")));
    }

    #[tokio::test]
    async fn a_done_marker_split_across_chunks_names_nothing() {
        // docs/22, B3: `[DONE]` arrives as two writes. The relay must see the
        // whole marker, so no error follows it.
        let frames = drain(
            Keepalive::new(Dialect::OpenAi, NOWAIT),
            vec![
                Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                ),
                Bytes::from_static(b"data: [DO"),
                Bytes::from_static(b"NE]\n\n"),
            ],
        )
        .await;
        assert_eq!(frames.len(), 3, "{frames:?}");
        assert!(
            !frames.iter().any(|f| f.contains("stream_error")),
            "{frames:?}"
        );
    }

    #[tokio::test]
    async fn a_finish_reason_split_across_chunks_completes_the_stream() {
        let frames = drain(
            Keepalive::new(Dialect::OpenAi, NOWAIT),
            vec![
                Bytes::from_static(b"data: {\"choices\":[{\"finish_reason\":\""),
                Bytes::from_static(b"stop\"}]}\n\n"),
            ],
        )
        .await;
        assert!(
            !frames.iter().any(|f| f.contains("stream_error")),
            "{frames:?}"
        );
    }

    #[tokio::test]
    async fn a_real_cut_mid_answer_is_still_named_in_band() {
        // Nothing marks the end: no `[DONE]`, no finish reason. The client must
        // still learn the stream was cut.
        let frames = drain(
            Keepalive::new(Dialect::OpenAi, NOWAIT),
            vec![Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"part\"}}]}\n\n",
            )],
        )
        .await;
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert!(frames[1].contains("stream_error"), "{frames:?}");
    }

    /// An interval long enough that a relay over an already-complete stream
    /// never waits for one. The idle-path test below uses a short one instead.
    const NOWAIT: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn a_stream_that_ends_without_its_terminator_is_named_in_band() {
        // The client is now waiting on a frame that is never coming, and the
        // connection closed cleanly so no error is visible. Without this frame it
        // waits until its own read times out and reports nothing.
        let frames = drain(
            Keepalive::new(Dialect::OpenAi, NOWAIT),
            vec![Bytes::from_static(b"data: {\"delta\":{}}\n\n")],
        )
        .await;
        assert_eq!(
            frames.len(),
            2,
            "one relayed frame plus the error: {frames:?}"
        );
        assert!(
            frames[1].contains("stream_error"),
            "no in-band error: {frames:?}"
        );
    }

    #[tokio::test]
    async fn a_stream_that_ends_with_its_terminator_gets_no_error_frame() {
        let frames = drain(
            Keepalive::new(Dialect::OpenAi, NOWAIT),
            vec![
                Bytes::from_static(b"data: {\"delta\":{}}\n\n"),
                Bytes::from_static(b"data: [DONE]\n\n"),
            ],
        )
        .await;
        assert_eq!(
            frames.len(),
            2,
            "a terminated stream gained a frame: {frames:?}"
        );
        assert!(
            frames[1].contains("[DONE]"),
            "the terminator was not relayed: {frames:?}"
        );
    }

    /// A chat POST to any dialect's own path — the same shape as [`chat`] with
    /// the route swapped.
    fn post_to(path: &str, body: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .header("host", "127.0.0.1:20128")
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_owned()))
            .expect("request builds")
    }

    /// The single usage row a drained ledger holds, or none.
    ///
    /// Every stream test here records at most one reply, so "the report" is the
    /// clearest way to say "exactly the row this test claims and no more".
    fn only_row(ledger: &Mutex<ar_tokens::Ledger>) -> Option<ar_tokens::LedgerRow> {
        let report = ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .report(10)
            .expect("report");
        let mut rows = report.rows;
        (rows.len() == 1).then(|| rows.remove(0))
    }

    #[tokio::test]
    async fn a_streamed_reply_records_its_usage_when_the_stream_completes() {
        // The half of the wave D split this closes: frames relay untouched —
        // the byte identity below is asserted, not assumed — and the usage the
        // final frame carried lands in the ledger under the same accounting
        // name the non-streaming arm uses.
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let chunks = [
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        ];
        let exec = Arc::new(CannedExec(chunks.to_vec()));
        let router = crate::app::app(routed_with_ledger(exec, Arc::clone(&ledger)));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("stream drains");
        assert_eq!(
            body.as_ref(),
            chunks.concat().as_bytes(),
            "the tee must relay the upstream's exact bytes"
        );
        let row = only_row(&ledger).expect("the streamed usage was recorded");
        // The row stores the sum; the prompt/completion split is pinned by the
        // `usage_from_frames` unit tests below.
        assert_eq!(row.total_tokens, 18);
        assert_eq!(row.key_id, "anonymous");
        assert_eq!(row.provider, "p");
        assert_eq!(row.model, "m");
    }

    #[tokio::test]
    async fn a_stream_without_a_usage_frame_records_nothing() {
        // An OpenAI stream that did not opt into `include_usage` carries no
        // counts in-band; the honest entry is none, not an estimate.
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let exec = Arc::new(CannedExec(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: [DONE]\n\n",
        ]));
        let router = crate::app::app(routed_with_ledger(exec, Arc::clone(&ledger)));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        drain_body(resp).await;
        let report = ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .report(10)
            .expect("report");
        assert!(
            report.rows.is_empty(),
            "a usage-free stream recorded {report:?}"
        );
    }

    #[tokio::test]
    async fn an_anthropic_stream_records_the_usage_its_openai_upstream_reports() {
        // The OpenAI upstream reports its counts on the chunk after the finish
        // reason. The re-frame carries them in `message_delta`, and the tee
        // composes that with the input count, which `message_start` cannot know.
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let exec = Arc::new(CannedExec(vec![
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":21,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        ]));
        let router = crate::app::app(routed_with_ledger(exec, Arc::clone(&ledger)));
        let resp = drive(
            &router,
            post_to(
                "/v1/messages",
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;
        drain_body(resp).await;
        let row = only_row(&ledger).expect("the composed usage was recorded");
        assert_eq!(row.total_tokens, 26);
    }

    #[tokio::test]
    async fn a_responses_stream_records_the_completed_events_usage() {
        // The re-framed `response.completed` carries the usage the OpenAI upstream
        // reported on its last chunk, and that frame is the tail the tee keeps.
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let exec = Arc::new(CannedExec(vec![
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        ]));
        let router = crate::app::app(routed_with_ledger(exec, Arc::clone(&ledger)));
        let resp = drive(
            &router,
            post_to(
                "/v1/responses",
                "{\"model\":\"m\",\"input\":\"hi\",\"stream\":true}",
            ),
        )
        .await;
        drain_body(resp).await;
        let row = only_row(&ledger).expect("the completed response's usage was recorded");
        assert_eq!(row.total_tokens, 5);
    }

    #[tokio::test]
    async fn an_ollama_stream_records_the_final_counts() {
        // The OpenAI upstream's usage chunk becomes the one `done:true` line,
        // which carries both counts at its root; the tee reads that spelling.
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let exec = Arc::new(CannedExec(vec![
            "data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4}}\n\n",
            "data: [DONE]\n\n",
        ]));
        let router = crate::app::app(routed_with_ledger(exec, Arc::clone(&ledger)));
        let resp = drive(
            &router,
            post_to(
                "/api/chat",
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;
        drain_body(resp).await;
        let row = only_row(&ledger).expect("the final counts were recorded");
        assert_eq!(row.total_tokens, 13);
    }

    /// [`routed_under`] with the one provider speaking `wire`, as `zenmux-free`
    /// (Anthropic) and `grok-cli` (Responses) do.
    fn routed_on_wire(exec: Arc<dyn ArExec>, wire: ar_registry::WireFormat) -> AppState {
        let mut config = one_provider_config();
        config.providers = config
            .providers
            .into_iter()
            .map(|p| p.with_wire_format(wire))
            .collect();
        Components {
            exec,
            ..Components::unconfigured(config)
        }
        .into_state()
    }

    #[tokio::test]
    async fn openai_chat_should_answer_in_the_openai_shape_for_an_anthropic_upstream() {
        let exec = Arc::new(CannedExec(vec![
            r#"{"id":"msg_1","type":"message","model":"claude","role":"assistant","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}}"#,
        ]));
        let router = crate::app::app(routed_on_wire(exec, ar_registry::WireFormat::Anthropic));
        let resp = drive(
            &router,
            post_to(
                "/v1/chat/completions",
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;

        let body = body_of(resp).await;
        assert_eq!(body["object"], "chat.completion", "{body}");
        assert_eq!(body["choices"][0]["message"]["content"], "hi", "{body}");
    }

    #[tokio::test]
    async fn openai_stream_should_carry_chat_chunks_for_an_anthropic_upstream() {
        let exec = Arc::new(CannedExec(vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"c\",\"usage\":{\"input_tokens\":2}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]));
        let router = crate::app::app(routed_on_wire(exec, ar_registry::WireFormat::Anthropic));
        let resp = drive(
            &router,
            post_to(
                "/v1/chat/completions",
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;

        let text = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body reads")
                .to_vec(),
        )
        .expect("the stream is UTF-8");
        assert!(text.contains("\"content\":\"hi\""), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
        assert!(!text.contains("message_start"), "{text}");
    }

    #[tokio::test]
    async fn openai_chat_should_answer_in_the_openai_shape_for_a_responses_upstream() {
        let exec = Arc::new(CannedExec(vec![
            r#"{"id":"resp_1","object":"response","model":"grok","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}"#,
        ]));
        let router = crate::app::app(routed_on_wire(
            exec,
            ar_registry::WireFormat::OpenaiResponses,
        ));
        let resp = drive(
            &router,
            post_to(
                "/v1/chat/completions",
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;

        let body = body_of(resp).await;
        assert_eq!(body["object"], "chat.completion", "{body}");
        assert_eq!(body["choices"][0]["message"]["content"], "hi", "{body}");
    }

    #[tokio::test]
    async fn openai_stream_should_carry_chat_chunks_for_a_responses_upstream() {
        let exec = Arc::new(CannedExec(vec![
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"grok\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n",
        ]));
        let router = crate::app::app(routed_on_wire(
            exec,
            ar_registry::WireFormat::OpenaiResponses,
        ));
        let resp = drive(
            &router,
            post_to(
                "/v1/chat/completions",
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;

        let text = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body reads")
                .to_vec(),
        )
        .expect("the stream is UTF-8");
        assert!(text.contains("\"content\":\"hi\""), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
        assert!(!text.contains("response.created"), "{text}");
    }

    #[tokio::test]
    async fn openai_stream_should_carry_chat_chunks_for_a_gemini_upstream() {
        let exec = Arc::new(CannedExec(vec![
            "data: {\"responseId\":\"g1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\r\n\r\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":2,\"totalTokenCount\":4}}\r\n\r\n",
        ]));
        let router = crate::app::app(routed_on_wire(exec, ar_registry::WireFormat::Gemini));
        let resp = drive(
            &router,
            post_to(
                "/v1/chat/completions",
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;

        let text = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body reads")
                .to_vec(),
        )
        .expect("the stream is UTF-8");
        assert!(text.contains("\"content\":\"hi\""), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
        assert!(!text.contains("candidates"), "{text}");
    }

    /// One OpenAI completion, as a non-stream upstream answers it.
    const OPENAI_COMPLETION: &str = r#"{"id":"c1","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#;

    #[tokio::test]
    async fn anthropic_messages_should_answer_in_the_messages_shape_for_an_openai_upstream() {
        // A system turn inside `messages` is the shape Claude Code sends, and it
        // is the request that used to be refused as an unknown role.
        let exec = Arc::new(CannedExec(vec![OPENAI_COMPLETION]));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            post_to(
                "/v1/messages",
                r#"{"model":"m","messages":[{"role":"system","content":"be brief"},{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;

        let status = resp.status();
        let body = body_of(resp).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert_eq!(body["type"], "message", "{body}");
        assert_eq!(body["content"][0]["text"], "hi", "{body}");
    }

    /// A Responses client on a Responses upstream is the native pair: the reply is
    /// relayed as the upstream wrote it, not re-synthesised, so no field is lost.
    #[tokio::test]
    async fn responses_stream_should_relay_a_responses_upstream_unchanged() {
        let frames = [
            "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"model\":\"grok\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"delta\":\"hi\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n",
        ];
        let exec = Arc::new(CannedExec(frames.to_vec()));
        let router = crate::app::app(routed_on_wire(
            exec,
            ar_registry::WireFormat::OpenaiResponses,
        ));
        let resp = drive(
            &router,
            post_to(
                "/v1/responses",
                r#"{"model":"m","stream":true,"input":"hi"}"#,
            ),
        )
        .await;

        let text = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body reads")
                .to_vec(),
        )
        .expect("the stream is UTF-8");
        assert_eq!(text, frames.concat(), "{text}");
    }

    #[tokio::test]
    async fn responses_should_answer_in_the_responses_shape_for_an_openai_upstream() {
        let exec = Arc::new(CannedExec(vec![OPENAI_COMPLETION]));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            post_to("/v1/responses", r#"{"model":"m","input":"hi"}"#),
        )
        .await;

        let body = body_of(resp).await;
        assert_eq!(body["object"], "response", "{body}");
        assert_eq!(body["output"][0]["content"][0]["text"], "hi", "{body}");
    }

    #[tokio::test]
    async fn ollama_chat_should_answer_in_the_ollama_shape_for_an_openai_upstream() {
        let exec = Arc::new(CannedExec(vec![OPENAI_COMPLETION]));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            post_to(
                "/api/chat",
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;

        let body = body_of(resp).await;
        assert_eq!(body["message"]["content"], "hi", "{body}");
        assert_eq!(body["done"], true, "{body}");
    }

    #[tokio::test]
    async fn anthropic_stream_should_end_with_message_stop_and_no_openai_frames() {
        let exec = Arc::new(CannedExec(vec![
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ]));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            post_to(
                "/v1/messages",
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;

        let text = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body reads")
                .to_vec(),
        )
        .expect("the stream is UTF-8");
        assert!(text.contains("event: message_start"), "{text}");
        assert!(text.contains("event: message_stop"), "{text}");
        assert!(!text.contains("chat.completion.chunk"), "{text}");
    }

    #[tokio::test]
    async fn an_ollama_non_stream_reply_counts_its_root_fields() {
        // The wave D bug this wave found while building it: the server passed
        // Ollama's reply root to a normalizer that could not read it, so every
        // non-streaming Ollama reply counted as zero in headers and ledger.
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let reply = "{\"model\":\"m\",\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"prompt_eval_count\":9,\"eval_count\":4,\"done\":true}";
        let exec = Arc::new(CannedExec(vec![reply]));
        let router = crate::app::app(routed_with_ledger(exec, Arc::clone(&ledger)));
        let resp = drive(
            &router,
            post_to(
                "/api/chat",
                "{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}",
            ),
        )
        .await;
        assert_eq!(
            resp.headers()
                .get(TOKENS_IN_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("9"),
            "the prompt count rides the header"
        );
        assert_eq!(
            resp.headers()
                .get(TOKENS_OUT_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("4"),
        );
        drain_body(resp).await;
        let row = only_row(&ledger).expect("the reply was recorded");
        assert_eq!(row.total_tokens, 13);
    }

    #[test]
    fn the_frame_parser_reads_each_dialects_usage_shape() {
        // The ledger row stores only the sum, so the split is pinned here, at
        // the parser, where prompt and completion are still separate numbers.
        let usage = |value: serde_json::Value| ar_tokens::NormalizedUsage::from_usage(&value);

        // OpenAI: usage rides in the last data frame that carries one.
        let tail = [
            Bytes::from_static(
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
            ),
            Bytes::from_static(b"data: [DONE]\n\n"),
        ];
        let got = usage_from_frames(Dialect::OpenAi, None, &tail).expect("openai usage frame");
        assert_eq!(usage(got), ar_tokens::NormalizedUsage::new(11, 7));

        // Anthropic: input in the first frame, output in the last.
        let head = Bytes::from_static(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":21}}}\n\n",
        );
        let tail = [
            Bytes::from_static(
                b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":5}}\n\n",
            ),
            Bytes::from_static(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        ];
        let got =
            usage_from_frames(Dialect::Anthropic, Some(&head), &tail).expect("anthropic compose");
        assert_eq!(usage(got), ar_tokens::NormalizedUsage::new(21, 5));

        // Responses: the completed event carries the response object with
        // its usage inside.
        let tail = [
            Bytes::from_static(
                b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
            ),
        ];
        let got = usage_from_frames(Dialect::Responses, None, &tail).expect("responses completed");
        assert_eq!(usage(got), ar_tokens::NormalizedUsage::new(3, 2));

        // Ollama: the final `done:true` NDJSON line carries both counts at
        // its root.
        let tail = [Bytes::from_static(
            b"{\"model\":\"m\",\"done\":true,\"prompt_eval_count\":9,\"eval_count\":4}\n",
        )];
        let got = usage_from_frames(Dialect::Ollama, None, &tail).expect("ollama final line");
        assert_eq!(usage(got), ar_tokens::NormalizedUsage::new(9, 4));
    }

    #[test]
    fn the_frame_parser_stays_silent_when_no_usage_was_seen() {
        // Every dialect's "no counts arrived" answer is the same: nothing,
        // so the tee records nothing rather than a zero row that would read
        // as a free request.
        let frames = [
            Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"),
            Bytes::from_static(b"data: [DONE]\n\n"),
        ];
        assert!(usage_from_frames(Dialect::OpenAi, None, &frames).is_none());
    }

    /// Polls a stream to its terminal `None`, which is when `UsageTee` records.
    ///
    /// The tee does its accounting on the last poll, so a test that reads the
    /// metrics has to drive it there rather than stopping after the first frame.
    #[cfg(test)]
    fn block_on_stream<S: futures::Stream<Item = Bytes> + Unpin>(stream: &mut S) -> Vec<Bytes> {
        use std::task::Context;

        let mut out = Vec::new();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            match std::pin::Pin::new(&mut *stream).poll_next(&mut cx) {
                std::task::Poll::Ready(Some(chunk)) => out.push(chunk),
                std::task::Poll::Ready(None) => return out,
                std::task::Poll::Pending => panic!("stream stalled in a test"),
            }
        }
    }

    #[tokio::test]
    async fn a_served_request_reaches_the_trace_and_the_audit_ledger() {
        // The write path's whole claim: `ar-obs`'s trace and audit halves are no
        // longer library-only. Both are armed by `Components::obs_dir`, and one
        // served request must leave a trace line naming its provider AND a
        // bounded audit row — the split, where the row is the same width on disk
        // whatever the provider answered.
        let dir = std::env::temp_dir().join(format!("ar-obs-rs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let exec = Arc::new(CannedExec(vec![
            r#"{"id":"r1","usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
        ]));
        let state = Components {
            exec,
            ledger: Some(Arc::new(Mutex::new(
                ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
            ))),
            obs_dir: Some(dir.clone()),
            ..Components::unconfigured(one_provider_config())
        }
        .into_state();
        let router = crate::app::app(state);
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Dropped before reading: `AuditLedger` takes redb's exclusive file
        // lock, so the writer and any reader cannot coexist in one process. That
        // is redb's contract rather than a choice here, and it is the same
        // property that makes two proxies sharing one obs dir impossible.
        drop(router);

        let audit = ar_obs::AuditLedger::open(&dir.join("audit.redb")).expect("audit open");
        let rows = audit.query(0, u64::MAX).expect("audit query");
        assert_eq!(rows.len(), 1, "one served request, one row: {rows:?}");
        assert!(
            rows[0].contains("admit ok routed"),
            "the row is the bounded tuple, not the trace line: {rows:?}"
        );
        drop(audit);

        // Any `ar-trace-<day>.jsonl` in the directory: the day index moves with
        // the wall clock, so pinning `0` would make this test pass for a few
        // hours every day and fail for the rest.
        let trace_file = std::fs::read_dir(&dir)
            .expect("trace dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("ar-trace-"))
            })
            .expect("a trace file was written");
        let trace = std::fs::read_to_string(&trace_file).expect("trace body");
        assert!(
            trace.contains(r#""tokens_in":11"#) && trace.contains(r#""tokens_out":7"#),
            "the trace line carries the full figure: {trace}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_streamed_reply_reaches_the_routing_metrics() {
        // The tee used to be armed only when a ledger was configured, so a
        // streamed request reached neither the ledger nor `metrics.work` and
        // `ar_requests_total` silently under-counted every stream. Pin the half
        // that was missing: with no ledger at all, the observation still lands.
        let state = routed_with_ledger(
            Arc::new(CannedExec(vec![])),
            Arc::new(Mutex::new(
                ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
            )),
        );
        let mut tee = UsageTee {
            inner: Box::pin(futures::stream::iter([
                // The first chunk becomes `head`; OpenAI usage rides in a
                // later frame, so a single-chunk stream carries none and the
                // tee correctly records nothing.
                Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"),
                Bytes::from_static(
                    b"data: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
                ),
            ])),
            head: None,
            tail: Vec::new(),
            state,
            key_id: "anonymous".to_owned(),
            provider: "soak-stub".to_owned(),
            model: Strng::from("m"),
            combo: "combo-under-test".to_owned(),
            dialect: Dialect::OpenAi,
            attempts: 2,
            dispatched: Instant::now(),
        };
        // Drain to the terminal poll, which is when `record` runs.
        block_on_stream(&mut tee);

        let rendered = tee.state.metrics.work.render();
        assert!(
            rendered.contains(r#"ar_requests_total{provider="soak-stub""#),
            "a streamed reply must be observed; got:\n{rendered}"
        );
        assert!(
            rendered.contains(
                r#"ar_tokens_in_total{provider="soak-stub",family="unpriced",decision="primary"} 7"#
            ),
            "streamed input tokens must be counted; got:\n{rendered}"
        );
        assert!(
            rendered.contains(r#"ar_tokens_out_total{provider="soak-stub",family="unpriced",decision="primary"} 3"#),
            "streamed output tokens must be counted; got:\n{rendered}"
        );
    }

    #[test]
    fn a_frame_over_the_cap_is_not_kept_for_accounting() {
        // The cap is what keeps the tee's retention bounded against an
        // upstream that ships its whole reply in one write: a frame that
        // big is payload, not bookkeeping, and the tee must drop it rather
        // than hold it.
        let huge = Bytes::from(vec![b'x'; USAGE_FRAME_CAP + 1]);
        let state = routed_with_ledger(
            Arc::new(CannedExec(vec![])),
            Arc::new(Mutex::new(
                ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
            )),
        );
        let mut tee = UsageTee {
            inner: Box::pin(futures::stream::iter([Bytes::from_static(
                b"data: [DONE]\n\n",
            )])),
            head: None,
            tail: Vec::new(),
            state,
            key_id: "anonymous".to_owned(),
            provider: "p".to_owned(),
            model: Strng::from("m"),
            combo: "combo-under-test".to_owned(),
            dialect: Dialect::OpenAi,
            attempts: 1,
            dispatched: Instant::now(),
        };
        tee.retain(&huge);
        assert!(tee.head.is_none(), "an over-cap frame was kept");
    }

    /// A panel of two that answers distinctly, and (optionally) a judge.
    ///
    /// The judge is provider `b`, which is also a panel member, so `b` is asked
    /// twice; the reply distinguishes panel from judge by request body, which is
    /// also how the judge test proves the panel texts reached it.
    struct ScriptedPanelJudge {
        refuse_judge: bool,
        seen: Mutex<Vec<String>>,
    }

    impl ScriptedPanelJudge {
        fn ok() -> Self {
            Self {
                refuse_judge: false,
                seen: Mutex::new(Vec::new()),
            }
        }

        fn refusing() -> Self {
            Self {
                refuse_judge: true,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl ArExec for ScriptedPanelJudge {
        fn post_chat<'a>(
            &'a self,
            provider: &'a ProviderId,
            canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            let body = String::from_utf8_lossy(&canonical.body).into_owned();
            // The judge directive is the only body that names sources, so it is
            // how a panel request and a judge request are told apart.
            let is_judge = body.contains("model-fusion panel");
            self.seen.lock().expect("recorder").push(body);
            let text = if is_judge {
                if self.refuse_judge {
                    "refused".to_owned()
                } else {
                    "synthesized answer".to_owned()
                }
            } else if provider.as_str() == "a" {
                "from a".to_owned()
            } else {
                "from b".to_owned()
            };
            let refused = is_judge && self.refuse_judge;
            let payload = format!("{{\"choices\":[{{\"message\":{{\"content\":\"{text}\"}}}}]}}");
            Box::pin(async move {
                if refused {
                    Ok(Upstream::failure(
                        StatusCode::SERVICE_UNAVAILABLE,
                        Bytes::from_static(b"judge refused"),
                        None,
                    ))
                } else {
                    Ok(Upstream::success(Box::pin(futures::stream::iter([
                        Bytes::from(payload),
                    ]))))
                }
            })
        }

        fn post_media<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _model: Option<&'a str>,
            _endpoint: &'a str,
            _content_type: &'a str,
            _body: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
            Box::pin(async move { Err(ExecError::Transport("no media here".to_owned())) })
        }
    }

    #[tokio::test]
    async fn a_fusion_panel_without_a_judge_answers_with_the_first_2xx() {
        // The reference's default: no judge configured means the panel's first
        // 2xx IS the answer. A judge must never be invented.
        let exec = Arc::new(ScriptedPanelJudge::ok());
        let router = crate::app::app(routed_fusion(exec, None));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(PROVIDER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("a")
        );
        let body = body_of(resp).await;
        assert_eq!(body["choices"][0]["message"]["content"], "from a");
    }

    #[tokio::test]
    async fn a_fusion_judge_replaces_the_panel_answer_with_its_own() {
        // With a judge named, the synthesis IS the response and the judge is the
        // provider that served it — the panel answers are its input, not the
        // client's answer.
        let exec = Arc::new(ScriptedPanelJudge::ok());
        let router = crate::app::app(routed_fusion(exec, Some("b/m")));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"which?"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(PROVIDER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("b")
        );
        let body = body_of(resp).await;
        assert_eq!(
            body["choices"][0]["message"]["content"],
            "synthesized answer"
        );
    }

    #[tokio::test]
    async fn a_fusion_judge_failure_degrades_to_the_panel_answer() {
        // A refused judge must not throw away a completed fan-out: the panel's
        // first 2xx is still a usable answer.
        let exec = Arc::new(ScriptedPanelJudge::refusing());
        let router = crate::app::app(routed_fusion(exec, Some("b/m")));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(PROVIDER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("a")
        );
        let body = body_of(resp).await;
        assert_eq!(body["choices"][0]["message"]["content"], "from a");
    }

    #[tokio::test]
    async fn a_fusion_panel_from_a_streaming_client_still_has_answers() {
        // The failure this pins: the fan-out buffers each member's answer to read
        // its text, and an SSE body is frames, not one answer — so a streaming
        // client asked with the fan-out passing its `stream: true` through would
        // yield a panel of empty texts and a judge synthesizing prose from
        // nothing. Every panel member must be asked non-streaming, whatever the
        // client asked for (the reference's `panelBody`, `stream: false`).
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(StreamingAwarePanel {
            seen: Arc::clone(&seen),
        });
        let router = crate::app::app(routed_fusion(exec, Some("b/m")));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let requests = seen.lock().expect("recorder").clone();
        let panel_calls = requests
            .iter()
            .filter(|b| !b.contains("model-fusion panel"))
            .count();
        assert_eq!(panel_calls, 2, "both members were asked");
        assert!(
            requests
                .iter()
                .filter(|b| !b.contains("model-fusion panel"))
                .all(|b| !b.contains("\"stream\":true")),
            "a panel member must not be asked for a stream: {requests:?}"
        );
        // The judge's own ask is deliberately non-streaming too, and deliberately
        // asserted rather than left incidental: a fusion answer is one complete
        // answer, and relaying the judge's token stream would hand the client a
        // stream in place of the fusion it requested.
        let judge_bodies: Vec<&String> = requests
            .iter()
            .filter(|b| b.contains("model-fusion panel"))
            .collect();
        assert_eq!(judge_bodies.len(), 1, "one judge dispatch");
        assert!(
            judge_bodies[0].contains("\"stream\":false"),
            "the judge is asked non-streaming: {:?}",
            judge_bodies[0]
        );
    }

    /// A panel that refuses to answer as a stream: an SSE body instead of a
    /// completion, which is what a member would send if the fan-out passed the
    /// client's `stream: true` through.
    struct StreamingAwarePanel {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl ArExec for StreamingAwarePanel {
        fn post_chat<'a>(
            &'a self,
            _provider: &'a ProviderId,
            canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            let body = String::from_utf8_lossy(&canonical.body).into_owned();
            let streaming = canonical.stream;
            let is_judge = body.contains("model-fusion panel");
            self.seen.lock().expect("recorder").push(body);
            Box::pin(async move {
                if streaming {
                    // The frame shape a real streamed member sends: no
                    // `choices[].message.content`, so an extractor reading it
                    // finds nothing.
                    let frame = concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"text\"}}]}\n\n",
                        "data: [DONE]\n\n"
                    );
                    Ok(Upstream::success(Box::pin(futures::stream::iter([
                        Bytes::from(frame),
                    ]))))
                } else {
                    let text = if is_judge {
                        "synthesized answer"
                    } else {
                        "a complete answer"
                    };
                    let payload =
                        format!("{{\"choices\":[{{\"message\":{{\"content\":\"{text}\"}}}}]}}");
                    Ok(Upstream::success(Box::pin(futures::stream::iter([
                        Bytes::from(payload),
                    ]))))
                }
            })
        }

        fn post_media<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _model: Option<&'a str>,
            _endpoint: &'a str,
            _content_type: &'a str,
            _body: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
            Box::pin(async move { Err(ExecError::Transport("no media here".to_owned())) })
        }
    }

    #[tokio::test]
    async fn a_fusion_panel_whose_members_all_refuse_answers_502() {
        let exec = Arc::new(FailingExec(429, "slow down"));
        let router = crate::app::app(routed_fusion(exec, None));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "panel_unavailable");
    }

    /// An upstream that never sends a byte, for the idle-path tests.
    fn stalled() -> ar_route::ChunkStream {
        Box::pin(futures::stream::pending())
    }

    /// Two keepalive frames out of an upstream that never answers.
    ///
    /// A real short interval rather than a paused clock: the property under test
    /// is that an idle upstream produces the *dialect's* frame, and a real timer
    /// is what actually exercises the `timeout` branch. 20ms x 2 is well under any
    /// test timeout and needs no `test-util` feature.
    async fn idle_frames(keepalive: Keepalive) -> Vec<String> {
        use futures::StreamExt;
        keepalive
            .relay(stalled(), "p", "m")
            .take(2)
            .map(|item| {
                String::from_utf8(item.expect("infallible").to_vec()).expect("frames are UTF-8")
            })
            .collect()
            .await
    }

    #[tokio::test]
    async fn an_idle_openai_stream_emits_a_decodable_keepalive() {
        // The hang this guards: a client that resets its first-token watchdog on
        // decoded frames gets nothing at all from a comment, so a long thinking
        // phase looks like a dead socket. OpenAI is the arm that every
        // OpenAI-compatible client lands on, and it had no idle test at all.
        let frames = idle_frames(Keepalive::new(Dialect::OpenAi, Duration::from_millis(20))).await;
        assert_eq!(
            frames.len(),
            2,
            "an idle stream was not kept alive: {frames:?}"
        );
        for frame in &frames {
            assert!(
                frame.starts_with("data: "),
                "an idle OpenAI stream needs a decodable frame, not a comment: {frame}"
            );
        }
    }

    #[tokio::test]
    async fn an_idle_stream_emits_the_dialects_own_keepalive() {
        let keepalive = Keepalive::new(Dialect::Anthropic, Duration::from_millis(20));
        assert!(!keepalive.is_noop());
        let frames = idle_frames(keepalive).await;
        assert_eq!(
            frames.len(),
            2,
            "an idle stream was not kept alive: {frames:?}"
        );
        for frame in &frames {
            assert!(
                frame.contains("event: ping"),
                "not an Anthropic ping: {frame}"
            );
        }
    }

    #[tokio::test]
    async fn an_idle_responses_stream_emits_in_progress_frames() {
        let frames = idle_frames(Keepalive::new(
            Dialect::Responses,
            Duration::from_millis(20),
        ))
        .await;
        assert_eq!(
            frames.len(),
            2,
            "an idle stream was not kept alive: {frames:?}"
        );
        for frame in &frames {
            assert!(
                frame.contains("response.in_progress"),
                "not an in-progress frame: {frame}"
            );
        }
    }

    #[tokio::test]
    async fn a_healthy_stream_is_relayed_untouched() {
        // A keepalive nobody needed is a frame a strict client has to tolerate
        // for nothing, so one must not appear when the upstream is already
        // producing bytes faster than the interval.
        let frames = drain(
            Keepalive::new(Dialect::OpenAi, NOWAIT),
            vec![
                Bytes::from_static(b"data: {\"delta\":{\"content\":\"He\"}}\n\n"),
                Bytes::from_static(b"data: {\"delta\":{\"content\":\"llo\"}}\n\n"),
                Bytes::from_static(b"data: [DONE]\n\n"),
            ],
        )
        .await;
        assert_eq!(frames.len(), 3, "a fast stream gained a frame: {frames:?}");
        assert!(
            !frames.iter().any(|f| f.contains("keepalive")),
            "keepalive on a fast stream: {frames:?}"
        );
    }

    #[tokio::test]
    async fn ollama_is_relayed_verbatim() {
        // NDJSON: any frame this relay invented would be a line the client
        // cannot parse, and a truncated-NDJSON signal is not worth that.
        let keepalive = Keepalive::new(Dialect::Ollama, NOWAIT);
        assert!(keepalive.is_noop());
        // NDJSON, and the relay adds nothing: the frame is the upstream's own
        // bytes, trailing newline included.
        let frames = drain(keepalive, vec![Bytes::from_static(b"{\"done\":false}\n")]).await;
        assert_eq!(frames, ["{\"done\":false}\n"]);
    }

    #[tokio::test]
    async fn an_anthropic_truncated_stream_gets_its_own_error_event() {
        // The per-dialect half of the truncation signal, end to end: a bare
        // `data:` line here would be dropped by the client whose watchdog the
        // keepalive exists for.
        let frames = drain(
            Keepalive::new(Dialect::Anthropic, NOWAIT),
            vec![Bytes::from_static(
                b"event: content_block_delta\ndata: {}\n\n",
            )],
        )
        .await;
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert!(
            frames[1].starts_with("event: error"),
            "not an Anthropic error event: {frames:?}"
        );
    }

    #[tokio::test]
    async fn a_responses_stream_terminated_by_completed_gets_no_error_frame() {
        let frames = drain(
            Keepalive::new(Dialect::Responses, NOWAIT),
            vec![
                Bytes::from_static(b"data: {\"type\":\"response.output_text.delta\"}\n\n"),
                Bytes::from_static(b"data: {\"type\":\"response.completed\"}\n\n"),
            ],
        )
        .await;
        assert_eq!(
            frames.len(),
            2,
            "a terminated Responses stream gained a frame: {frames:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_stream_gets_the_truncation_frame() {
        // An upstream that produced nothing and then closed is a failed answer,
        // and a client that got zero bytes is waiting on a frame that will not
        // come. Named in-band, the same as a cut mid-stream (R3). Before the fix
        // the check was `== Some(false)`, which left this case silent.
        let frames = drain(Keepalive::new(Dialect::OpenAi, NOWAIT), vec![]).await;
        assert_eq!(
            frames.len(),
            1,
            "an empty stream was not named in-band: {frames:?}"
        );
    }

    // --- the 415 guard, through the router ----------------------------

    #[tokio::test]
    async fn a_text_plain_body_is_a_415_and_never_reaches_the_translator() {
        // Letting it through produces a 400 about JSON syntax, which sends an
        // operator to the wrong place entirely.
        let router = crate::app::app(routed());
        let mut req = chat("hello", &[]);
        req.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        let resp = drive(&router, req).await;
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "unsupported_media_type");
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    /// A one-provider server whose `limits:` block is `limits`, with the
    /// in-memory ledger the spend arms read.
    fn limited(limits: &str) -> AppState {
        let mut config = one_provider_config();
        config.limits = ar_config::Config::parse(limits, |_| Ok(None))
            .expect("test limits parse")
            .limits;
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        Components {
            ledger: Some(ledger),
            ..Components::unconfigured(config)
        }
        .into_state()
    }

    #[tokio::test]
    async fn a_key_past_its_requests_per_minute_is_told_when_to_retry() {
        let mut config = one_provider_config();
        config.limits = ar_config::Config::parse("limits:\n  default:\n    rpm: 1\n", |_| Ok(None))
            .expect("test limits parse")
            .limits;
        let router = crate::app::app(Components::unconfigured(config).into_state());
        let body = "{\"model\":\"m\",\"messages\":[]}";
        let first = drive(&router, chat(body, &[])).await;
        assert_ne!(
            first.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the first request is within the bucket"
        );
        let second = drive(&router, chat(body, &[])).await;
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry = second
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        assert_eq!(
            retry,
            Some(61),
            "a full minute plus the rounding-up second, so a client that waits exactly it is admitted"
        );
    }

    #[tokio::test]
    async fn a_key_past_its_token_ceiling_is_refused_before_dispatch() {
        let router = crate::app::app(limited("limits:\n  default:\n    tokens: 1\n"));
        let resp = drive(
            &router,
            chat("{\"model\":\"m\",\"messages\":[],\"max_tokens\":50}", &[]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["type"], "billing_error");
        assert_eq!(body["error"]["code"], "payment_required");
        assert_eq!(body["error"]["reason"], "token_cap");
    }

    #[tokio::test]
    async fn a_key_past_its_dollar_ceiling_is_refused_before_dispatch() {
        let mut config = one_provider_config();
        // $1 per MTok both ways, so a request declaring fifty output tokens
        // projects past a one-micro-dollar ceiling whatever the input counts.
        config.prices.set(
            "p",
            "m",
            ar_tokens::Prices {
                input_micros_per_mtok: 1_000_000,
                output_micros_per_mtok: 1_000_000,
            },
        );
        config.limits =
            ar_config::Config::parse("limits:\n  default:\n    usd_micros: 1\n", |_| Ok(None))
                .expect("test limits parse")
                .limits;
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let router = crate::app::app(
            Components {
                ledger: Some(ledger),
                ..Components::unconfigured(config)
            }
            .into_state(),
        );
        let resp = drive(
            &router,
            chat("{\"model\":\"m\",\"messages\":[],\"max_tokens\":50}", &[]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(body_of(resp).await["error"]["reason"], "usd_cap");
    }

    #[tokio::test]
    async fn a_key_refusing_unpriced_models_gets_a_402_for_a_model_with_no_row() {
        // The one-provider catalog is a provider no registry row covers, so the
        // projection is unpriced and `refuse_unpriced` is the arm that bites.
        let router = crate::app::app(limited("limits:\n  default:\n    refuse_unpriced: true\n"));
        let resp = drive(&router, chat("{\"model\":\"m\",\"messages\":[]}", &[])).await;
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(body_of(resp).await["error"]["reason"], "unpriced");
    }

    #[tokio::test]
    async fn spend_arms_stay_inert_without_a_ledger() {
        // A cap that cannot read its spend is not a cap: the request passes to
        // the pipeline (and fails there, at the null executor, for unrelated
        // reasons) rather than at the policy layer.
        let mut config = one_provider_config();
        config.limits =
            ar_config::Config::parse("limits:\n  default:\n    tokens: 1\n", |_| Ok(None))
                .expect("test limits parse")
                .limits;
        let router = crate::app::app(Components::unconfigured(config).into_state());
        let resp = drive(
            &router,
            chat("{\"model\":\"m\",\"messages\":[],\"max_tokens\":50}", &[]),
        )
        .await;
        assert_ne!(
            resp.status(),
            StatusCode::PAYMENT_REQUIRED,
            "no ledger means the spend arms read nothing and deny nothing"
        );
    }

    #[tokio::test]
    async fn a_key_under_its_token_ceiling_reaches_the_pipeline() {
        // The mirror of the over-cap test: a ceiling the request stays well
        // under must not cost it anything at the policy layer, so the request
        // proceeds to dispatch and fails there, at the null executor, for
        // unrelated reasons.
        let router = crate::app::app(limited("limits:\n  default:\n    tokens: 100000\n"));
        let resp = drive(
            &router,
            chat("{\"model\":\"m\",\"messages\":[],\"max_tokens\":50}", &[]),
        )
        .await;
        assert_ne!(resp.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[tokio::test]
    async fn an_unpriced_model_serves_when_the_key_does_not_refuse_unpriced() {
        // The same provider with no pricing row, but the default posture: the
        // request prices at zero and serves rather than being denied —
        // refuse_unpriced is opt-in, and without it an unknown row is a cost
        // of zero, not a refusal.
        let router = crate::app::app(limited("limits:\n  default:\n    usd_micros: 1000\n"));
        let resp = drive(&router, chat("{\"model\":\"m\",\"messages\":[]}", &[])).await;
        assert_ne!(resp.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[tokio::test]
    async fn the_402_names_its_arm_in_a_header_and_never_its_amounts() {
        let router = crate::app::app(limited("limits:\n  default:\n    tokens: 1\n"));
        let resp = drive(
            &router,
            chat("{\"model\":\"m\",\"messages\":[],\"max_tokens\":50}", &[]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        let deny = resp
            .headers()
            .get(DENY_HEADER)
            .and_then(|v| v.to_str().ok());
        assert_eq!(deny, Some("token_cap"), "the arm, not the amounts");
    }

    #[test]
    fn stamps_the_model_provider_and_version_the_response_carries() {
        let headers = headers_for(ar_tokens::ResponseMeta::default(), 1);
        assert_eq!(
            headers.get(MODEL_HEADER).and_then(|v| v.to_str().ok()),
            Some("m"),
            "the client's own model spelling is the one it can recognize"
        );
        assert_eq!(
            headers.get(PROVIDER_HEADER).and_then(|v| v.to_str().ok()),
            Some("openai"),
            "a succeeded outcome names the provider that answered"
        );
        assert_eq!(
            headers.get(VERSION_HEADER).and_then(|v| v.to_str().ok()),
            Some(super::SERVER_VERSION),
            "the version header and the crate must move together"
        );
    }

    #[tokio::test]
    async fn a_stream_response_carries_the_model_and_version_headers() {
        // The streamed arm shares the builder, so the cheap meta headers ride
        // the first flush rather than appearing only on buffered replies.
        let recorder = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(RecordingExec(Arc::clone(&recorder)));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(
            resp.headers().get(axum::http::header::CONTENT_TYPE),
            Some(&axum::http::HeaderValue::from_static("text/event-stream")),
            "the fixture must actually stream for the arm to be the streamed one"
        );
        assert!(
            resp.headers().contains_key(MODEL_HEADER),
            "no model header on a streamed response"
        );
        assert!(
            resp.headers().contains_key(VERSION_HEADER),
            "no version header on a streamed response"
        );
    }

    #[tokio::test]
    async fn a_non_stream_answer_is_counted_in_the_headers_and_the_ledger() {
        // Wave D's whole chain in one drive: buffer, read the upstream's own
        // `usage`, answer with the counted figures, and persist the same row.
        // The anonymous key id is the accounting name for a request no gate
        // verified — the config in this fixture arms no gate.
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let reply = r#"{"id":"r1","usage":{"prompt_tokens":11,"completion_tokens":7}}"#;
        let exec = Arc::new(CannedExec(vec![reply]));
        let router = crate::app::app(routed_with_ledger(exec, Arc::clone(&ledger)));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;

        assert_eq!(
            resp.headers()
                .get(TOKENS_IN_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("11"),
            "the prompt count rides the header"
        );
        assert_eq!(
            resp.headers()
                .get(TOKENS_OUT_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("7"),
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body reads");
        assert_eq!(
            bytes.as_ref(),
            reply.as_bytes(),
            "the buffered relay is the upstream's own bytes"
        );
        let report = ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .report(10)
            .expect("report");
        assert_eq!(
            report.rows.len(),
            1,
            "exactly one row: {rows:?}",
            rows = report.rows
        );
        assert_eq!(report.rows[0].key_id, "anonymous");
        assert_eq!(report.rows[0].total_tokens, 18);
    }

    #[tokio::test]
    async fn a_streamed_answer_is_relayed_byte_for_byte_and_not_counted() {
        // The other half of wave D's split: no buffering, no usage reading,
        // and the headers say so with zeros rather than with invented counts.
        // Two chunks because a one-chunk stream would not distinguish
        // "streamed" from "buffered then relayed as a single chunk".
        let exec = Arc::new(CannedExec(vec!["data: {\"a\":1}\n\n", "data: [DONE]\n\n"]));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;

        assert_eq!(
            resp.headers()
                .get(TOKENS_IN_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("0"),
            "a streamed answer must not be counted: {:?}",
            resp.headers()
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body reads");
        assert_eq!(
            bytes.as_ref(),
            b"data: {\"a\":1}\n\ndata: [DONE]\n\n".as_ref(),
            "stream bytes must be identical to the upstream's"
        );
    }

    /// [`routed_under`], plus a live 1MB memory cache — the cache tests need a
    /// tier that actually stores, not a bigger fixture.
    fn routed_with_cache(exec: Arc<dyn ArExec>) -> AppState {
        Components {
            exec,
            cache_bytes: Some(Some(1 << 20)),
            ..Components::unconfigured(one_provider_config())
        }
        .into_state()
    }

    /// Consumes a response body so the cache tee's final poll — the one that
    /// stores the entry — actually runs. `drive` returns the response without
    /// polling the body, and an unpollled relay stores nothing.
    async fn drain_body(resp: Response) {
        let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
    }

    /// The canned completion every cache test replays: non-stream, JSON, a
    /// usage block the accounting path can read.
    const CANNED: &str = r#"{"id":"r1","choices":[{"message":{"role":"assistant","content":"hi"}}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;

    #[tokio::test]
    async fn a_failover_the_client_sees_names_the_upstreams_code() {
        // Wave J1: the envelope is unchanged — same fields, same shape — but
        // the code is the provider's own, because the reference projects it
        // through an allowlist the same way rather than swallow it as
        // `upstream_unavailable`. `upstream_details`-style body passthrough is
        // stayed out of: the message stays router-authored, the code is the
        // one identifier a client may branch on, and the body keeps its
        // provider-account identifiers out of client reach.
        let exec = Arc::new(FailingExec(
            503,
            r#"{"error":{"code":"insufficient_quota","message":"over"}}"#,
        ));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers()[axum::http::header::CONTENT_TYPE],
            "application/json",
            "a JSON error must say so (docs/22, B5)"
        );
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "insufficient_quota");
        assert_eq!(body["error"]["type"], "server_error");
    }

    /// One OpenAI chat SSE frame carrying `text` as its content delta.
    fn openai_frame(text: &str) -> Bytes {
        Bytes::from(format!(
            "data: {}\n\n",
            serde_json::json!({ "choices": [{ "delta": { "content": text } }] })
        ))
    }

    /// Answers the one continuation dispatch with a canned stream, and records what
    /// it was asked to send.
    struct ResumeExec {
        second: Mutex<Option<Upstream>>,
        seen: Mutex<Vec<String>>,
    }

    impl ArExec for ResumeExec {
        fn post_chat<'a>(
            &'a self,
            provider: &'a ProviderId,
            canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!(
                    "{}|{}|{}",
                    provider.as_str(),
                    canonical.model,
                    String::from_utf8_lossy(&canonical.body)
                ));
            let next = self
                .second
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            Box::pin(async move { next.ok_or_else(|| ExecError::Transport("no second target".to_owned())) })
        }

        fn post_media<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _model: Option<&'a str>,
            _endpoint: &'a str,
            _content_type: &'a str,
            _body: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
            Box::pin(async move { Err(ExecError::Transport("no media dispatch in this fixture".to_owned())) })
        }
    }

    /// A first stream that sends `frames`, then is cut, and the continuation plan
    /// that would take over from it.
    fn cut_after(
        frames: Vec<Bytes>,
        second: Option<Upstream>,
    ) -> (ChunkStream, CutFlag, Continuation, Arc<ResumeExec>) {
        let exec = Arc::new(ResumeExec {
            second: Mutex::new(second),
            seen: Mutex::new(Vec::new()),
        });
        let plan = Continuation {
            exec: Arc::clone(&exec) as Arc<dyn ArExec>,
            body: Bytes::from(
                r#"{"model":"combo","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
            ),
            target: (ProviderId::new("p2"), Strng::from("m2")),
        };
        let cut: CutFlag = Arc::new(std::sync::atomic::AtomicBool::new(true));
        (Box::pin(futures::stream::iter(frames)), cut, plan, exec)
    }

    #[test]
    fn sent_text_should_read_a_frame_split_across_chunks() {
        let mut sent = SentText::default();
        let frame = openai_frame("Hi");
        let (head, tail) = frame.split_at(frame.len() / 2);
        sent.feed(head);
        sent.feed(tail);
        assert_eq!(sent.resumable(), Some("Hi"));
    }

    #[test]
    fn sent_text_should_refuse_to_resume_when_a_tool_call_was_sent() {
        let mut sent = SentText::default();
        sent.feed(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]}}]}\n\n");
        assert_eq!(sent.resumable(), None);
    }

    #[test]
    fn resume_body_should_append_the_prefill_and_retarget_the_model() {
        let body = resume_body(
            br#"{"model":"combo","messages":[{"role":"user","content":"hi"}]}"#,
            "Hel",
            "m2",
        )
        .expect("an OpenAI body resumes");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(
            v["messages"][1],
            serde_json::json!({"role":"assistant","content":"Hel","prefix":true})
        );
    }

    #[tokio::test]
    async fn resumed_should_splice_the_second_target_when_the_first_is_cut() {
        use futures::StreamExt;
        let second = Upstream::success(Box::pin(futures::stream::iter(vec![
            openai_frame("lo"),
            Bytes::from("data: [DONE]\n\n"),
        ])));
        let (first, cut, plan, _) = cut_after(vec![openai_frame("Hel")], Some(second));
        let (out, _) = resumed(first, cut, plan);
        let joined: String = out
            .map(|c: Bytes| String::from_utf8_lossy(&c).into_owned())
            .collect::<Vec<_>>()
            .await
            .concat();
        assert!(joined.contains("Hel") && joined.contains("lo") && joined.contains("[DONE]"));
    }

    #[tokio::test]
    async fn resumed_should_send_the_partial_text_as_prefill_to_the_second_target() {
        use futures::StreamExt;
        let second = Upstream::success(Box::pin(futures::stream::iter(Vec::<Bytes>::new())));
        let (first, cut, plan, exec) = cut_after(vec![openai_frame("Hel")], Some(second));
        let (out, _) = resumed(first, cut, plan);
        let _ = out.collect::<Vec<_>>().await;
        let seen = exec
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            seen.len() == 1
                && seen[0].starts_with("p2|m2|")
                && seen[0].contains(r#""content":"Hel""#)
        );
    }

    #[tokio::test]
    async fn resumed_should_end_cut_when_no_second_target_answers() {
        use futures::StreamExt;
        let (first, cut, plan, _) = cut_after(vec![openai_frame("Hel")], None);
        let (out, ends_cut) = resumed(first, cut, plan);
        let _ = out.collect::<Vec<_>>().await;
        assert!(ends_cut.load(std::sync::atomic::Ordering::SeqCst));
    }

    fn cut_rows(cut: bool) -> usize {
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let record = CutRecord {
            sink: FailureSink {
                ledger: Some(Arc::clone(&ledger)),
                key_id: "k1".to_owned(),
            },
            provider: "p".to_owned(),
            model: "jev".to_owned(),
            cut: Arc::new(std::sync::atomic::AtomicBool::new(cut)),
        };
        drop(record);
        let filter = ar_tokens::LogFilter {
            days: 30,
            model: None,
            provider: None,
            errors_only: true,
            limit: 10,
            offset: 0,
        };
        ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .log(&filter)
            .expect("log")
            .len()
    }

    #[test]
    fn cut_record_should_log_one_failure_when_the_body_was_cut() {
        assert_eq!(cut_rows(true), 1);
    }

    #[test]
    fn cut_record_should_log_nothing_when_the_body_finished_clean() {
        assert_eq!(cut_rows(false), 0);
    }

    #[tokio::test]
    async fn a_failed_request_names_the_provider_the_chain_tried() {
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let exec = Arc::new(FailingExec(502, r#"{"error":{"message":"down"}}"#));
        let router = crate::app::app(routed_with_ledger(exec, Arc::clone(&ledger)));
        let _ = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        let filter = ar_tokens::LogFilter {
            days: 30,
            model: Some("m"),
            provider: None,
            errors_only: true,
            limit: 10,
            offset: 0,
        };
        let rows = ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .log(&filter)
            .expect("log");
        assert_eq!(rows[0].provider, "p");
    }

    #[tokio::test]
    async fn a_failed_request_lands_in_the_failure_log_with_the_model_asked_for() {
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let exec = Arc::new(FailingExec(502, r#"{"error":{"message":"down"}}"#));
        let router = crate::app::app(routed_with_ledger(exec, Arc::clone(&ledger)));
        let _ = drive(
            &router,
            chat(
                r#"{"model":"jev","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        let filter = ar_tokens::LogFilter {
            days: 30,
            model: Some("jev"),
            provider: None,
            errors_only: true,
            limit: 10,
            offset: 0,
        };
        let rows = ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .log(&filter)
            .expect("log");
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn a_streamed_failure_is_json_not_an_event_stream() {
        // docs/22, B5: an SSE client that gets a JSON 502 must be able to tell it
        // is not a stream, so the content type says what the body is.
        let exec = Arc::new(FailingExec(502, r#"{"error":{"message":"down"}}"#));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            resp.headers()[axum::http::header::CONTENT_TYPE],
            "application/json"
        );
    }

    #[tokio::test]
    async fn an_unknown_upstream_code_collapses_to_the_router_default() {
        // The other half of the projection: a provider-invented identifier
        // reaches the client only after collapsing to the status-derived
        // default, exactly as the reference's allowlist behaves for unknowns.
        let exec = Arc::new(FailingExec(503, r#"{"error":{"code":"weird_error"}}"#));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "upstream_unavailable");
    }

    #[tokio::test]
    async fn a_disconnected_client_takes_the_upstream_stream_with_it() {
        // Wave I1's pin, one half: a client that hangs up aborts the upstream
        // read, by construction rather than by a cancellation channel — the
        // relay chain owns the upstream stream, so dropping the response drops
        // the read. The flag lives in the stream itself so the assertion
        // observes the exact object the upstream connection is, and not a
        // proxy that could outlive it.
        use futures::Stream;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Poll};

        /// A stream that never yields and never ends, and says so when it is
        /// dropped: the shape of an upstream that is between frames.
        struct GuardedStream(Arc<AtomicBool>);

        impl Stream for GuardedStream {
            type Item = Bytes;

            fn poll_next(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Bytes>> {
                Poll::Pending
            }
        }

        impl Drop for GuardedStream {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        struct HangingExec(Arc<AtomicBool>);

        impl ArExec for HangingExec {
            fn post_chat<'a>(
                &'a self,
                _provider: &'a ProviderId,
                _canonical: &'a CanonicalRequest,
            ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>>
            {
                let guarded = GuardedStream(Arc::clone(&self.0));
                Box::pin(async move { Ok(Upstream::success(Box::pin(guarded))) })
            }

            fn post_media<'a>(
                &'a self,
                _provider: &'a ProviderId,
                _model: Option<&'a str>,
                _endpoint: &'a str,
                _content_type: &'a str,
                _body: &'a [u8],
            ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>>
            {
                Box::pin(async move { Err(ExecError::Transport("no media in this fixture".to_owned())) })
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let exec = Arc::new(HangingExec(Arc::clone(&dropped)));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        assert_eq!(
            resp.headers().get(axum::http::header::CONTENT_TYPE),
            Some(&axum::http::HeaderValue::from_static("text/event-stream")),
            "the fixture must stream for the disconnect to mean anything"
        );
        drop(resp);
        assert!(
            dropped.load(Ordering::Relaxed),
            "the upstream stream outlived the client that hung up"
        );
    }

    #[tokio::test]
    async fn a_trailing_chunk_after_the_terminator_does_not_claim_truncation() {
        // Several upstreams close with `[DONE]` and then one more frame — an
        // empty keep-alive, or a usage-only tail. Reading "did the LAST chunk
        // carry the terminator" made ar emit its truncation error *after*
        // `data: [DONE]`, which is what opencode reports as "the upstream
        // stream ended before it was complete".
        let exec = Arc::new(CannedExec(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: [DONE]\n\n",
            "data: {}\n\n",
        ]));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("the relay ends when the upstream does");
        let relayed = String::from_utf8_lossy(&bytes);
        assert!(
            !relayed.contains("stream_error"),
            "a terminated stream was called truncated: {relayed}"
        );
        assert!(
            relayed.contains("[DONE]"),
            "the terminator was not relayed: {relayed}"
        );
    }

    #[tokio::test]
    async fn a_stream_that_dies_mid_flight_is_described_in_band() {
        // Wave I1's pin, other half: an upstream that ends without its
        // terminator is named to the client in the dialect's own error frame
        // — the keepalive's truncation signal, end to end through the relay —
        // so a silent clean close is not the client's only experience of a
        // cut-off generation.
        let exec = Arc::new(CannedExec(vec!["data: {\"delta\":\"x\"}\n\n"]));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ),
        )
        .await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("the relay ends when the upstream does");
        let relayed = String::from_utf8_lossy(&bytes);
        assert!(
            relayed.contains("stream_error"),
            "a truncated stream was not described: {relayed}"
        );
    }

    #[tokio::test]
    async fn a_no_cache_header_kills_both_cache_sides() {
        // The reference's both-sides bypass, strict-"true" grammar: both
        // spellings must answer `bypass` and neither may leave an entry —
        // two dispatches for two requests is the proof nothing was served.
        let recorder = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(RecordingExec(Arc::clone(&recorder)));
        let router = crate::app::app(routed_with_cache(exec));
        let body = r#"{"model":"m","messages":[{"role":"user","content":"once"}]}"#;

        let first = drive(&router, chat(body, &[("x-ar-no-cache", "true")])).await;
        let second = drive(&router, chat(body, &[("x-omniroute-no-cache", "true")])).await;
        assert_eq!(
            first
                .headers()
                .get(CACHE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("bypass")
        );
        assert_eq!(
            second
                .headers()
                .get(CACHE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("bypass")
        );
        assert_eq!(
            recorder.lock().expect("recorder lock").len(),
            2,
            "a no-cache request stored or served an entry"
        );
    }

    #[tokio::test]
    async fn a_no_store_header_reads_but_writes_nothing() {
        // The write-side kill: the first request's lookup answers `miss`, the
        // second (same body, no header) must also answer `miss` — the entry the
        // first would have written is the one `no-store` refused — and the
        // third+fourth pair prove the cache in this fixture does hit when a
        // store was allowed, so the second `miss` is the header's doing.
        let exec = Arc::new(CannedExec(vec![CANNED]));
        let router = crate::app::app(routed_with_cache(exec));
        let body = r#"{"model":"m","messages":[{"role":"user","content":"once"}]}"#;

        let first = drive(&router, chat(body, &[("x-ar-cache-no-store", "true")])).await;
        let first_verdict = first
            .headers()
            .get(CACHE_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // The tee's final poll is where the no-store refusal lives, so the body
        // must actually be read for the request to have *attempted* a store.
        drain_body(first).await;
        let second = drive(&router, chat(body, &[])).await;
        assert_eq!(
            first_verdict.as_deref(),
            Some("miss"),
            "no-store must not kill the lookup"
        );
        assert_eq!(
            second
                .headers()
                .get(CACHE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("miss"),
            "the no-store request left an entry behind"
        );

        let other = r#"{"model":"m","messages":[{"role":"user","content":"twice"}]}"#;
        drain_body(drive(&router, chat(other, &[])).await).await;
        let hit = drive(&router, chat(other, &[])).await;
        assert_eq!(
            hit.headers()
                .get(CACHE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("hit"),
            "the fixture cache never hits, so this test proves nothing"
        );
    }

    #[test]
    fn request_key_should_change_when_the_served_target_changes() {
        use super::request_key;
        // K4: a combo retargeted in config keeps its alias. The key must follow
        // the target that answers, or the old target's reply is replayed.
        let canonical = ar_route::CanonicalRequest::new("combo", Bytes::from_static(b"{}"));
        let before = request_key(
            &canonical,
            "openai/gpt-a",
            "anonymous",
            None,
            Dialect::OpenAi,
        );
        let after = request_key(
            &canonical,
            "anthropic/claude-b",
            "anonymous",
            None,
            Dialect::OpenAi,
        );
        assert_ne!(before, after);
        assert_eq!(
            before,
            request_key(
                &canonical,
                "openai/gpt-a",
                "anonymous",
                None,
                Dialect::OpenAi
            )
        );
    }

    #[test]
    fn request_key_should_differ_by_dialect_when_the_same_reply_is_served() {
        use super::request_key;
        // A re-framed reply is stored in its client's dialect, so an OpenAI
        // client and an Anthropic client sending one request must not share a key.
        let canonical = ar_route::CanonicalRequest::new("combo", Bytes::from_static(b"{}"));
        let openai = request_key(
            &canonical,
            "openai/gpt-a",
            "anonymous",
            None,
            Dialect::OpenAi,
        );
        let anthropic = request_key(
            &canonical,
            "openai/gpt-a",
            "anonymous",
            None,
            Dialect::Anthropic,
        );
        assert_ne!(openai, anthropic);
    }

    #[test]
    fn storable_reply_should_refuse_a_fallback_under_the_winners_key() {
        // K4: the winner is planned, a fallback answers. Its reply must not be
        // stored under the winner's key, where it would outlive the winner's recovery.
        assert!(!super::storable_reply(
            Some("openai/gpt-a"),
            "anthropic/claude-b"
        ));
        assert!(super::storable_reply(Some("openai/gpt-a"), "openai/gpt-a"));
        assert!(super::storable_reply(None, "anthropic/claude-b"));
    }

    #[tokio::test]
    async fn a_cache_key_header_namespaces_entries() {
        // The same body under different caller-key segments is two entries;
        // the same segment under either spelling is one. The alias spelling on
        // the second request and the native on the third is the probe that both
        // fold into the digest identically.
        let exec = Arc::new(CannedExec(vec![CANNED]));
        let router = crate::app::app(routed_with_cache(exec));
        let body = r#"{"model":"m","messages":[{"role":"user","content":"once"}]}"#;

        drain_body(drive(&router, chat(body, &[])).await).await;
        let namespaced = drive(
            &router,
            chat(body, &[("x-omniroute-cache-key", "tenant-b")]),
        )
        .await;
        let namespaced_verdict = namespaced
            .headers()
            .get(CACHE_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        drain_body(namespaced).await;
        assert_eq!(
            namespaced_verdict.as_deref(),
            Some("miss"),
            "the caller-key segment did not enter the digest"
        );
        let same_segment = drive(&router, chat(body, &[("x-ar-cache-key", "tenant-b")])).await;
        assert_eq!(
            same_segment
                .headers()
                .get(CACHE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("hit"),
            "the two spellings of one segment must fold identically"
        );
    }

    #[tokio::test]
    async fn a_cache_ttl_header_shortens_but_never_extends_an_entry() {
        // The ms form (`150000`) asks for 150ms: after it lapses the entry is
        // gone. The seconds form asking for centuries must be clamped to the
        // policy's own lifetime — the entry it writes still answers `hit` here
        // because the clamp is what kept it from being the asked-for one.
        let exec = Arc::new(CannedExec(vec![CANNED]));
        let router = crate::app::app(routed_with_cache(exec));
        let short = r#"{"model":"m","messages":[{"role":"user","content":"short"}]}"#;
        let long = r#"{"model":"m","messages":[{"role":"user","content":"long"}]}"#;

        // The seconds form (`1`) is the only way to ask for a short entry: the
        // reference's heuristic reads anything at or under `100_000` as
        // seconds, so a sub-second TTL is not expressible — the ms form starts
        // at ~100s. After the 1s entry lapses the lookup must miss.
        drain_body(drive(&router, chat(short, &[("x-ar-cache-ttl", "1")])).await).await;
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let expired = drive(&router, chat(short, &[])).await;
        assert_eq!(
            expired
                .headers()
                .get(CACHE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("miss"),
            "the caller TTL did not shorten the entry"
        );

        drain_body(drive(&router, chat(long, &[("x-ar-cache-ttl", "999999999")])).await).await;
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let clamped = drive(&router, chat(long, &[])).await;
        assert_eq!(
            clamped
                .headers()
                .get(CACHE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("hit"),
            "the clamp must hold: a century-long request still answers within the policy lifetime"
        );
    }

    #[tokio::test]
    async fn savings_tokens_count_exactly_what_compression_removed() {
        // The savings header is a counted promise: the exact BPE count of the
        // text compression removed, not a character-based estimate. The
        // guard proves caveman actually rewrote this content — without it,
        // `0 == 0` would pass on a body compression never touched.
        let long = "Please summarize the following text for me. ".repeat(60);
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":{}}}]}}"#,
            serde_json::to_string(&long).expect("message serializes"),
        );
        let recorder = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(RecordingExec(Arc::clone(&recorder)));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(&body, &[("x-ar-compression", "engine:caveman")]),
        )
        .await;
        let forwarded = recorder
            .lock()
            .expect("recorder lock")
            .pop()
            .expect("the request reached dispatch");
        let forwarded_json = serde_json::from_slice::<serde_json::Value>(&forwarded)
            .expect("the forwarded body is JSON");
        let after = forwarded_json["messages"][0]["content"]
            .as_str()
            .expect("the canonical content is a string");
        assert_ne!(after, long, "caveman did not rewrite the fixture content");
        let reported = resp
            .headers()
            .get(SAVINGS_TOKENS_HEADER)
            .expect("savings ride the response")
            .to_str()
            .expect("ASCII")
            .parse::<u32>()
            .expect("the header is an integer");
        assert_eq!(
            reported,
            ar_tokens::count_text(&long) - ar_tokens::count_text(after),
            "the header must equal count(before) - count(after)"
        );
    }

    #[tokio::test]
    async fn an_omniroute_spelled_compression_header_is_honored() {
        // A client configured against OmniRoute sends `x-omniroute-compression`
        // and never learns the native name; the echo answering the engine it
        // named is the alias earning its two lines of routing.
        let recorder = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(RecordingExec(Arc::clone(&recorder)));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"hey there partner"}]}"#,
                &[("x-omniroute-compression", "engine:caveman")],
            ),
        )
        .await;
        let echo = resp
            .headers()
            .get(COMPRESSION_ECHO)
            .expect("a 200 carries the compression echo");
        assert!(
            echo.to_str().expect("ASCII").contains("caveman"),
            "alias spelling was not honored: {echo:?}"
        );
    }

    #[tokio::test]
    async fn the_native_compression_spelling_wins_over_the_alias() {
        // `native > alias` is the precedence the alias resolver pins: a client
        // sending both spellings gets its native word, and the alias cannot
        // override it.
        let recorder = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(RecordingExec(Arc::clone(&recorder)));
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[{"role":"user","content":"hey there partner"}]}"#,
                &[
                    ("x-ar-compression", "engine:caveman"),
                    ("x-omniroute-compression", "off"),
                ],
            ),
        )
        .await;
        let echo = resp
            .headers()
            .get(COMPRESSION_ECHO)
            .expect("a 200 carries the compression echo");
        assert!(
            echo.to_str().expect("ASCII").contains("caveman"),
            "the alias overrode the native spelling: {echo:?}"
        );
    }

    #[tokio::test]
    async fn forwards_an_over_budget_body_to_the_executor_unclamped() {
        // The accepted divergence `docs/audit-notes.md` (d) pins here: the
        // reference warns on an over-budget body rather than truncating it, so
        // dispatch never clamps. A body far past any plausible token budget
        // must reach the executor with the user's text intact — wiring
        // `clamp_to_budget` in with a default-on budget turns this red before
        // it silently shortens a request.
        let long = "the quick brown fox. ".repeat(600);
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":{}}}]}}"#,
            serde_json::to_string(&long).expect("message serializes"),
        );
        let recorder = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(RecordingExec(Arc::clone(&recorder)));
        let router = crate::app::app(routed_under(exec));
        drive(&router, chat(&body, &[])).await;
        let forwarded = recorder
            .lock()
            .expect("recorder lock")
            .pop()
            .expect("the request reached dispatch exactly once");
        assert!(
            String::from_utf8_lossy(&forwarded).contains(&long),
            "the forwarded body lost user text"
        );
    }

    // A JSON body on a chat route that is a valid canonical request reaches the
    // pipeline, where the null executor refuses with a 502. That 502 is the proof
    // the body was translated rather than refused at the edge — and the charset
    // parameter is the interesting half, since refusing it would break every
    // client that sets one.
    #[tokio::test]
    async fn a_json_body_with_a_charset_reaches_the_pipeline() {
        let router = crate::app::app(routed());
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":true,"messages":[]}"#,
                &[("content-type", "application/json; charset=utf-8")],
            ),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "the body was refused at the edge"
        );
    }

    // --- Accept forces streaming, through the router -------------------

    #[tokio::test]
    async fn a_sse_accept_header_reports_a_stream_even_with_no_stream_field() {
        // A client that sends no `stream` at all but names SSE gets a stream, and
        // `x-ar-cache: bypass` is the observable difference: a stream never looks.
        let router = crate::app::app(routed());
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","messages":[]}"#,
                &[("accept", "text/event-stream")],
            ),
        )
        .await;
        assert_eq!(
            resp.headers()
                .get(crate::routes::CACHE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("bypass"),
            "the request was not treated as a stream"
        );
    }

    #[tokio::test]
    async fn an_explicit_stream_false_is_not_overridden_by_a_sse_accept_header() {
        let router = crate::app::app(routed());
        let resp = drive(
            &router,
            chat(
                r#"{"model":"m","stream":false,"messages":[]}"#,
                &[("accept", "text/event-stream")],
            ),
        )
        .await;
        assert_eq!(
            resp.headers()
                .get(crate::routes::CACHE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("miss"),
            "the body said JSON and Accept was only a fallback"
        );
    }

    #[test]
    fn stopped_at_ceiling_should_flag_length_in_each_dialect() {
        assert!(super::stopped_at_ceiling(
            br#"{"choices":[{"finish_reason":"length"}]}"#
        ));
        assert!(super::stopped_at_ceiling(
            br#"{"stop_reason":"max_tokens"}"#
        ));
        assert!(super::stopped_at_ceiling(
            br#"{"candidates":[{"finishReason":"MAX_TOKENS"}]}"#
        ));
    }

    #[test]
    fn stopped_at_ceiling_should_pass_a_complete_reply() {
        assert!(!super::stopped_at_ceiling(
            br#"{"choices":[{"finish_reason":"stop"}]}"#
        ));
        assert!(!super::stopped_at_ceiling(b"not json"));
    }

    #[tokio::test]
    async fn abort_on_cut_should_end_clean_only_when_not_cut() {
        use futures::StreamExt as _;
        use std::sync::atomic::Ordering;
        let chunks = || -> ar_route::ChunkStream {
            Box::pin(futures::stream::iter([Bytes::from_static(b"a")]))
        };
        let clean: Vec<_> = super::abort_on_cut(chunks(), Default::default())
            .collect()
            .await;
        assert!(clean.iter().all(Result::is_ok));

        let cut = ar_route::CutFlag::default();
        cut.store(true, Ordering::SeqCst);
        let cut_items: Vec<_> = super::abort_on_cut(chunks(), cut).collect().await;
        assert!(matches!(cut_items.last(), Some(Err(_))));
    }
}

#[cfg(test)]
mod lane_tests {
    use super::{hold_lease, lane_refusal};
    use ar_keys::{Admission, AdmitError, Lane, LaneSpec};
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::Response;

    fn controller() -> Admission {
        Admission::new(
            [LaneSpec::INTERACTIVE, LaneSpec::BATCH, LaneSpec::HEAVY],
            u32::MAX,
        )
        .expect("the standard lanes build")
    }

    #[tokio::test]
    async fn hold_lease_should_hold_the_permit_while_the_body_lives() {
        let lanes = controller();
        let lease = lanes
            .acquire(Lane::Interactive, "k")
            .await
            .expect("a free lane admits");
        let resp = hold_lease(Response::new(Body::from("x")), Some(lease));
        assert_eq!(lanes.in_flight(Lane::Interactive), 1);
        drop(resp);
    }

    #[tokio::test]
    async fn hold_lease_should_release_the_permit_when_the_body_is_dropped() {
        let lanes = controller();
        let lease = lanes
            .acquire(Lane::Interactive, "k")
            .await
            .expect("a free lane admits");
        let resp = hold_lease(Response::new(Body::from("x")), Some(lease));
        drop(resp);
        assert_eq!(lanes.in_flight(Lane::Interactive), 0);
    }

    #[test]
    fn lane_refusal_should_answer_429_with_the_window_when_the_lane_is_busy() {
        let refused = AdmitError::Busy {
            lane: Lane::Interactive,
            retry_after: 7,
        };
        let resp = lane_refusal(&refused);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn lane_refusal_should_send_the_window_as_retry_after_when_the_lane_is_busy() {
        let refused = AdmitError::Busy {
            lane: Lane::Interactive,
            retry_after: 7,
        };
        let resp = lane_refusal(&refused);
        assert_eq!(resp.headers()["retry-after"], "7");
    }

    #[test]
    fn lane_refusal_should_answer_503_when_the_gateway_is_shutting_down() {
        let resp = lane_refusal(&AdmitError::Shutdown);
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
