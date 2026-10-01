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
use ar_tokens::ResponseMeta;
use ar_route::{
    AttemptOutcome, AutoCandidate, AutoSelector, CanonicalRequest, Candidate, ProviderId, RouteError,
    Strng, Strategy, attempt_loop, pick, simulate_route, virtual_combo,
};
use axum::body::Body;
use axum::extract::{OriginalUri, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use serde::Serialize;

use crate::app::AppState;
use crate::config::{DefaultChain, RouteCombo};
use crate::text::{
    COMPRESSION_ECHO, COMPRESSION_HEADER, GUARD_HEADER, GuardVerdict, compression_echo,
    compression_plan, compress_body, guard_body,
};
use crate::translate::to_canonical_for_route;

/// Sticky-session header consumed by `Strategy::Lkgp`.
pub const SESSION_HEADER: &str = "x-ar-session";
/// Routing verdict, for the client and for support.
pub const DECISION_HEADER: &str = "x-ar-decision";
/// Attempt accounting. P0 counts attempts; token accounting is `ar-tokens` (P1).
pub const USAGE_HEADER: &str = "x-ar-usage";
/// Cache verdict: `hit`, `miss` or `bypass`.
pub const CACHE_HEADER: &str = "x-ar-cache";
/// Prompt tokens the upstream reported. `0` when it reported none.
///
/// ponytail: `0` today, because the emit point has no body to read. The
/// upstream's `usage` object is in the payload [`ar_route::Upstream`] relays
/// opaquely, and reading it would mean buffering that payload — a change to how
/// responses are delivered, which this wave does not make. Ceiling: these three
/// report `0` on every response while [`TOKENS_PER_SECOND_HEADER`] is derived
/// from them. Upgrade path, in order of cost: have `AttemptOutcome::Succeeded`
/// carry the parsed `usage` beside the stream (ar-route, no delivery change), or
/// collect the single-chunk non-stream body in [`decorate`] (this file, but it
/// delays the client's first byte). Both are strictly more work than the header
/// they enable; until one lands, `ar-tokens`' [`ar_tokens::Ledger::record_response`]
/// is where a real upstream `usage` object gets priced and persisted.
pub const TOKENS_IN_HEADER: &str = "x-ar-tokens-in";
/// Completion tokens the upstream reported. `0` when it reported none.
///
/// See [`TOKENS_IN_HEADER`] for why it reads `0` today.
pub const TOKENS_OUT_HEADER: &str = "x-ar-tokens-out";
/// What the request cost, in USD with six decimals.
///
/// Six decimals because [`ar_tokens::Usd`] is integer micro-dollars and the
/// ledger prints the same width: a header at a different precision would read
/// as a different amount. See [`TOKENS_IN_HEADER`] for why it reads `0.000000`
/// today.
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

/// The inbound wire dialect, which decides the keepalive frame and the
/// `Accept`-forces-stream rule.
///
/// One enum rather than a `&str` route because three of the four are only
/// reachable as a match arm: the frames are not interchangeable and a `&str`
/// route parameter would let a call site pass a path that has no frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    /// `POST /v1/chat/completions`.
    OpenAi,
    /// `POST /v1/messages`. Its keepalive is a real `ping` *event*, not a
    /// comment — see [`Dialect::keepalive`].
    Anthropic,
    /// `POST /v1/responses`.
    Responses,
    /// `POST /api/chat`. NDJSON, not SSE: a keepalive would have to be a JSON
    /// line, and inventing one would be inventing a wire format.
    Ollama,}

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
    /// Only the OpenAI chat arm is a comment. Its stream has no event names at
    /// all, and a line-based parser can drop an unrecognised `event:` line and
    /// desync on the `data:` line after it — which loses the error the client
    /// needed to see. A comment is skipped by every SSE parser by definition, so
    /// it costs nothing and cannot desync anything.
    fn keepalive(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("event: ping\ndata: {\"type\":\"ping\"}\n\n"),
            Self::Responses => Some(RESPONSES_IN_PROGRESS),
            Self::OpenAi => Some(": keepalive\n\n"),
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

/// The one chat pipeline, for every inbound dialect.
///
/// Order is the module doc's seven steps, and each is where it is because the
/// next one depends on the previous: the credential is read before the body (a
/// tokenized-alias URL carries its key in the path), the media type before the
/// body is parsed, the translation before the guard so the guard sees canonical
/// turns, the route resolution before the cache so the key covers what is
/// dispatched, and the deadline only around the attempt loop because everything
/// before it is local work that cannot hang on a socket.
async fn handle_chat(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    body: Bytes,
    dialect: Dialect,
) -> Response {
    if let Some(reason) = authorize(state, headers, path) {
        return reason;
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

    // A body that is not the dialect this route claims — including a body that
    // is not JSON at all — reaches the translator and comes back as the
    // unparsable-body 400, which is where a client that ignored the 415 lands.
    let mut canonical = match to_canonical_for_route(dialect.route(), &body) {
        Ok(c) => c,
        Err(e) => {
            return error_because(StatusCode::BAD_REQUEST, "invalid_request", "unparsable_body", &e);
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
    let compression = compression_plan(
        headers.get(COMPRESSION_HEADER).and_then(|v| v.to_str().ok()),
        plan.compression.as_ref().map(std::slice::from_ref),
    );
    let mut savings_tokens = 0;
    if let Some(rewritten) = compress_body(&canonical.body, &compression) {
        // `Stats` estimates from character counts, not BPE: a savings *figure* on
        // the header is not a billing figure, and paying two tiktoken passes per
        // request for one would buy precision nothing here bills against.
        savings_tokens = ar_compress::Stats::between(
            &String::from_utf8_lossy(&canonical.body),
            &String::from_utf8_lossy(&rewritten),
        )
        .saved_tokens
        .try_into()
        .unwrap_or(u32::MAX);
        canonical.body = Bytes::from(rewritten);
    }

    // A streaming request cannot be replayed from cache — frames arrive
    // incrementally and the client is already consuming them — so it reports
    // `bypass`. Anything else gets a real lookup.
    let cache = state.cache.clone();
    let cache_key = if canonical.stream {
        None
    } else {
        request_key(&canonical)
    };

    if let (Some(key), Some(cache)) = (cache_key.as_ref(), cache.as_ref())
        && let Some(hit) = cache_hit(cache, key, plan.chain.first(), plan.strategy)
    {
        return hit.with_guard(guard_verdict);
    }

    // Started here so the latency the response reports is the provider's time and
    // not this server's own compression pass — the same span the attempt
    // accounting on `x-ar-decision` describes.
    let dispatched = Instant::now();
    let outcome = match tokio::time::timeout(
        state.config.stream_deadline(canonical.model.as_ref()),
        attempt_loop(
            &canonical,
            &plan.chain,
            state.exec.as_ref(),
            &state.resilience,
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

    state.metrics.observe_attempts(u64::from(outcome.attempts()));
    if matches!(outcome, AttemptOutcome::Failover { .. }) {
        state.metrics.observe_failover();
    }
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
    let cache_state = if canonical.stream {
        CacheState::Bypass
    } else {
        CacheState::Miss
    };

    decorate(
        outcome,
        canonical.stream,
        dialect,
        plan.strategy,
        Stages {
            compression: compression_echo(&compression),
            cache_state,
            cache_key,
            cache,
            meta: ResponseMeta::default().with_latency(dispatched.elapsed()).with_savings_tokens(savings_tokens),
        },
    )
    .with_guard(guard_verdict)
}

/// What the cache and compression stages decided, for the response to carry.
///
/// One struct because these four are one decision, made in one order and read in
/// one place: the compression plan resolved, then the cache consulted, then
/// whatever the cache said about this request. Threading them as four parameters
/// is what let [`decorate`] reach eight arguments.
struct Stages {
    /// The applied `x-ar-compression` value, echoed on the response.
    compression: String,
    /// The cache verdict this request earned, header and all.
    cache_state: CacheState,
    /// The key a non-streaming request was looked up under, when it had one.
    cache_key: Option<CacheKey>,
    /// The cache itself, when one is configured.
    cache: Option<Arc<Cache>>,
    /// What the request spent and how long it took, for the `x-ar-*` accounting
    /// headers.
    ///
    /// Carried here rather than recomputed in [`decorate`] because the emit
    /// point has no clock and no body: the arithmetic happens once, where the
    /// measurement is taken.
    meta: ResponseMeta,
}

/// The cache key for a canonical request: `blake3(tenant | model | body)`.
///
/// `ar_cache::key::request_key` is the `docs/04` key. The tenant is the constant
/// `"default"` because there is exactly one tenant — a single-operator install —
/// and inventing a second one would only change the digest.
/// `None` for a body that is not JSON, which the translator has already refused
/// before this runs, so the arm is unreachable rather than a guess.
fn request_key(canonical: &CanonicalRequest) -> Option<CacheKey> {
    let body = serde_json::from_slice::<serde_json::Value>(&canonical.body).ok()?;
    Some(ar_cache::key::request_key("default", &canonical.model, &body))
}

/// Why a request could not be routed.
///
/// Its own enum rather than a `Response` so [`resolve`] can hand back a
/// `Result<RoutePlan, RouteReject>` without a 128-byte error arm; the caller turns
/// the reason into a 400. One enum rather than a `String` so a new rejection
/// cannot be written by accident as a bare sentence — and there is exactly one
/// today, which is the point.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RouteReject {
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
/// them a `Vec` of small `ProviderId`s.
#[derive(Clone, Debug, PartialEq)]
struct RoutePlan {
    chain: Vec<ProviderId>,
    strategy: Strategy,
    /// The resolved combo's compression setting, if it declared one.
    compression: Option<Step>,
}

/// Checks the credential when an [`crate::keys::AuthGate`] is configured.
///
/// No gate means no check, which is only safe because the same configuration
/// binds loopback-only — see [`crate::app::bind_addr`]. The two decisions live in
/// one struct on purpose: "no auth" and "public bind" must never be settable
/// independently.
///
/// Returns the 401 to send, or `None` to serve the request. The body is built
/// here rather than at the call site so there is one place that knows a refused
/// credential is `code: invalid_api_key` — the OpenAI-compatible spelling a
/// client branches on — with a `reason` that separates "you sent nothing" from
/// "what you sent is wrong" without either reaching the client verbatim. That
/// separation is the reason this returns a `Response` and not a `String`: a
/// `Result<_, String>` arm would make the whole thing 128 bytes wide, which
/// clippy's perf gate rightly refuses for a function whose happy path is `None`.
///
/// The reason is never the token, for the reason in [`crate::keys`]: this
/// response is a cacheable 401 a browser may keep.
fn authorize(state: &AppState, headers: &HeaderMap, path: &str) -> Option<Response> {
    // `path` is only read when a gate exists, which is the only case where a
    // tokenized-alias URL can carry a credential at all.
    let gate = state.auth.as_deref()?;
    // Resolved and checked once, and the mode decides what a refusal means — so
    // the matrix and the policy cannot drift into two answers for one request.
    gate.authorize((headers, path), state.auth_mode)
        .err()
        .map(|reason| {
            error_because(StatusCode::UNAUTHORIZED, "invalid_api_key", "credential_rejected", &reason)
        })
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
fn require_json(headers: &HeaderMap) -> Option<Response> {
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
fn resolve(state: &AppState, canonical: &CanonicalRequest) -> Result<RoutePlan, RouteReject> {
    let model = canonical.model.as_ref();

    if let Some(chain) = auto_chain(state, model) {
        return Ok(RoutePlan {
            chain,
            strategy: state.config.strategy,
            compression: state.config.default_compression(),
        });
    }

    match state.config.default_combo() {
        // A combo table is authoritative: a model that is not one of its ids is
        // a client error, not a hint.
        DefaultChain::Combo(_) => match state.config.combo(model) {
            Some(combo) => Ok(RoutePlan {
                chain: chain(state, combo),
                strategy: combo.strategy,
                compression: combo.compression,
            }),
            None => Err(RouteReject::UnknownModel(unknown_model(model, &state.config.combo_ids()))),
        },
        DefaultChain::Flat => Ok(RoutePlan {
            chain: order(state, state.config.candidates(None)),
            strategy: state.config.strategy,
            compression: None,
        }),
    }
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
fn chain(state: &AppState, combo: &RouteCombo) -> Vec<ProviderId> {
    let mut chain = order(state, state.config.candidates(Some(combo)));
    for target in state.config.pool(combo) {
        if !chain.contains(&target.provider) {
            chain.push(target.provider.clone());
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
fn order(state: &AppState, candidates: Vec<Candidate>) -> Vec<ProviderId> {
    if candidates.is_empty() {
        return Vec::new();
    }
    match pick(
        state.config.strategy,
        None,
        &candidates,
        &state.rr,
        Some(&state.lkgp),
    ) {
        Ok(picked) => build_chain(&picked, &candidates),
        Err(e) => {
            // `pick` refuses an empty list, which is checked above; anything else
            // is a strategy this build has not implemented yet. Config order is
            // the documented fallback rather than a 500.
            tracing::warn!(error = %e, "no strategy winner; falling back to config order");
            candidates.iter().map(|c| c.provider.clone()).collect()
        }
    }
}

/// Winner-first fallback chain.
/// Winner-first fallback chain.
fn build_chain(picked: &ProviderId, candidates: &[Candidate]) -> Vec<ProviderId> {
    let mut chain = vec![picked.clone()];
    chain.extend(
        candidates
            .iter()
            .map(|c| &c.provider)
            .filter(|p| **p != *picked)
            .cloned(),
    );
    chain.dedup();
    chain
}

/// Resolves an `auto/*` alias into a scored chain, or `None` for a concrete name.
///
/// The expected entry point is
/// `ar_route::auto::engine::auto_variant_for_model(name) -> Option<AutoVariant>`.
/// That function does not exist in this build, and `ar_route::auto` is a private
/// module — the same resolver is reached through the crate's public factory
/// [`virtual_combo`], which is what `ar_route`'s own `VirtualFactory` calls. When
/// the engine-level spelling lands, this function is the only thing that changes.
fn auto_chain(state: &AppState, model: &str) -> Option<Vec<ProviderId>> {
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
    provider: Option<&ProviderId>,
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
            format!(
                "strategy={strategy};outcome=cache;provider={};attempts=0",
                provider.map_or("-", ProviderId::as_str)
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
    stream: bool,
    dialect: Dialect,
    strategy: Strategy,
    stages: Stages,
) -> Response {
    let decision = format!(
        "strategy={strategy};outcome={};provider={};attempts={}",
        outcome_label(&outcome),
        outcome.provider().map_or("-", ProviderId::as_str),
        outcome.attempts()
    );

    let Stages { compression, cache_state, cache_key, cache, meta } = stages;

    // Every accounting number the response reports, from one [`ResponseMeta`].
    // Cost is priced in `ar-tokens` against the same row the ledger stores, so a
    // client reading the header and an operator reading `ar cost-report` are
    // looking at the same arithmetic rather than two that can drift.
    let mut builder = Response::builder()
        .header(DECISION_HEADER, decision)
        .header(USAGE_HEADER, format!("attempts={}", outcome.attempts()))
        .header(CACHE_HEADER, cache_state.as_header())
        .header(COMPRESSION_ECHO, compression)
        .header(TOKENS_IN_HEADER, meta.tokens_in().to_string())
        .header(TOKENS_OUT_HEADER, meta.tokens_out().to_string())
        .header(RESPONSE_COST_HEADER, meta.cost().usd.as_decimal_string())
        .header(SAVINGS_TOKENS_HEADER, meta.savings_tokens().to_string())
        .header(LATENCY_MS_HEADER, meta.latency_ms().to_string())
        .header(FALLBACK_ATTEMPTS_HEADER, outcome.attempts().to_string());

    // Conditional, not a zero: an elapsed time too short to divide by would make
    // every downstream division nonsense, and an absent header reads as unknown.
    if let Some(tps) = meta.tokens_per_second() {
        builder = builder.header(TOKENS_PER_SECOND_HEADER, format!("{tps:.3}"));
    }

    let resp = match outcome {
        AttemptOutcome::Succeeded { upstream, .. } => {
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
            match (cache_key, cache) {
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
                    ))),
                _ => {
                    let stream = upstream.stream;
                    let body = match keepalive.filter(|k| !k.is_noop()) {
                        Some(k) => Body::from_stream(k.relay(stream)),
                        None => Body::from_stream(stream.map(Ok::<Bytes, std::convert::Infallible>)),
                    };
                    builder.header(header::CONTENT_TYPE, ct).status(status).body(body)
                }
            }
        }
        AttemptOutcome::Retry { after, .. } => {
            // Honour the window we just advertised, in whole seconds. A
            // sub-second window rounds up to 1s rather than to 0.
            let secs = after.as_secs().max(1);
            builder
                .header(header::RETRY_AFTER, secs.to_string())
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
        AttemptOutcome::Failover { status, provider, .. } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let message = format!("all providers failed; last was {provider}");
            builder.status(status).body(json_body(ErrorSpec::of(
                status,
                "upstream_unavailable",
                &message,
            )))
        }
        AttemptOutcome::Abort(report) => builder
            .status(report.status)
            .body(json_body(ErrorSpec::of(report.status, "aborted", &report.reason))),
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
/// Past [`CACHE_MAX_BODY`] the buffer is dropped rather than grown: a partial
/// answer replayed from cache looks like a complete one, and `ar_cache` has a
/// named `store_truncated` for exactly that decision.
fn tee_and_store(
    mut stream: ar_route::ChunkStream,
    cache: Arc<Cache>,
    key: CacheKey,
    status: u16,
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
        if keep && status < 300 {
            cache.store(&key, status, "application/json", Bytes::from(buf));
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
    /// The terminator whose absence means "truncated". `None` for the same
    /// dialect as `frame`.
    terminator: Option<&'static [u8]>,
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
            terminator: dialect.terminator(),
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
    /// truncation error if the stream ends without its terminator.
    ///
    /// The terminator check is on the *last* frame only rather than accumulated
    /// over the whole stream: a stream that sent `[DONE]` and then held the
    /// connection open is terminated, and scanning every chunk to prove a
    /// negative is work proportional to the answer for a case that cannot happen
    /// on a well-behaved upstream. A client that reconnects mid-stream gets a
    /// fresh request anyway.
    fn relay(
        self,
        mut stream: ar_route::ChunkStream,
    ) -> impl futures::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + use<> {
        let mut keepalive = self;
        async_stream::stream! {
            if keepalive.is_noop() {
                while let Some(chunk) = stream.next().await {
                    yield Ok(chunk);
                }
                return;
            }
            loop {
                let next = tokio::time::timeout(keepalive.interval, stream.next()).await;
                match next {
                    Ok(Some(chunk)) => {
                        keepalive.last_terminated =
                            Some(terminator_seen(&chunk, keepalive.terminator));
                        yield Ok(chunk);
                    }
                    // The upstream finished. If its last frame was not a
                    // terminator, the client is now waiting on a frame that
                    // will never arrive, so name that in-band.
                    Ok(None) => {
                        if keepalive.last_terminated == Some(false)
                            && let Some(frame) = keepalive.truncated
                        {
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

/// Whether `chunk` carries the stream's terminator.
///
/// `false` also covers a dialect with no terminator to look for, which is the
/// answer the caller wants anyway: there is nothing to assert, so nothing is
/// claimed.
fn terminator_seen(chunk: &Bytes, terminator: Option<&'static [u8]>) -> bool {
    terminator.is_some_and(|t| chunk.windows(t.len()).any(|w| w == t))
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
    path: Option<&'a str>,}

impl<'a> ErrorSpec<'a> {
    /// Builds a spec for a status and code, with no finer reason.
    fn of(status: StatusCode, code: &'a str, message: &'a str) -> Self {
        Self { kind: kind_for(status), code, reason: None, message, path: None }
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
fn error_because(
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
             /v1/models, /healthz and /metrics",
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

/// `GET /metrics` — Prometheus text exposition.
pub async fn metrics(State(state): State<AppState>) -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            ),
        ],
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
/// have.
fn card_json(c: &crate::models::ModelCard) -> serde_json::Value {
    serde_json::json!({
        "id": c.id,
        "object": "model",
        "created": ar_registry::BUILT_AT,
        "owned_by": c.provider,
        "permission": Vec::<String>::new(),
        "root": c.id,
        "ar_upstream_model": c.upstream_model,
    })
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
pub async fn models(State(state): State<AppState>) -> Response {
    let cached = cached_models(&state);
    let payload = serde_json::json!({
        "object": "list",
        "data": cached.cards.iter().map(card_json).collect::<Vec<_>>(),
    });
    let age = state.models.age().map_or(0, |a| a.as_secs());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        // `stale` is how an operator sees that a revalidation is pending without
        // reading the body.
        .header(
            CACHE_HEADER,
            if cached.stale { "stale" } else { "fresh" },
        )
        .header("x-ar-models-age-seconds", age.to_string())
        .body(Body::from(
            serde_json::to_vec(&payload)
                .unwrap_or_else(|_| br#"{"object":"list","data":[]}"#.to_vec()),
        ))
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
pub async fn models_head() -> Response {    Response::builder()
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
pub async fn model(State(state): State<AppState>, Path(model): Path<String>) -> Response {
    let cached = cached_models(&state);
    let found = cached
        .cards
        .iter()
        .find(|c| c.id == model)
        .or_else(|| cached.cards.iter().find(|c| c.id.eq_ignore_ascii_case(&model)));

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
    use std::time::Duration;

    use ar_route::{AbortReport, AttemptOutcome, ProviderId, Strategy, Upstream};
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::Response;
    use bytes::Bytes;
    use super::{
        Dialect, FALLBACK_ATTEMPTS_HEADER, Keepalive, LATENCY_MS_HEADER, RESPONSE_COST_HEADER,
        SAVINGS_TOKENS_HEADER, Stages, TOKENS_IN_HEADER, TOKENS_OUT_HEADER,
        TOKENS_PER_SECOND_HEADER, accept_forces_stream, build_chain, card_json, declares_stream,
        decorate, error, error_because, kind_for, model, models_head, not_found, outcome_label,
        require_json, terminator_seen, unknown_model,
    };
    use crate::app::{AppState, Components};
    use crate::config::{ComboTarget, ServerConfig};
    use crate::models::ModelCard;

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
        let mut config = ServerConfig::single(
            0,
            Strategy::Priority,
            vec![crate::exec::ProviderConfig::new(
                ProviderId::new("p"),
                "http://127.0.0.1:1/v1",
                "k",
            )
            .with_model("m")],
        );
        config.combos = vec![crate::config::RouteCombo::new(
            "m",
            Strategy::Priority,
            vec![ComboTarget::new(ProviderId::new("p"), "m")],
        )];
        Components::unconfigured(config).into_state()
    }

    /// Drives one request through the real router, every layer included.
    async fn drive(app: &axum::Router, req: axum::http::Request<axum::body::Body>) -> Response {
        use tower::ServiceExt;
        app.clone().oneshot(req).await.expect("router answers")
    }

    /// A chat POST with a JSON body, plus any extra headers.
    fn chat(body: &str, extra: &[(&str, &str)]) -> axum::http::Request<axum::body::Body> {
        let mut req = axum::http::Request::builder()
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
        assert_eq!(chain[0].as_str(), "b");
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
    fn labels_a_succeeded_outcome_ok() {
        assert_eq!(
            outcome_label(&AttemptOutcome::Succeeded {
                provider: ProviderId::new("p"),
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
        };
        assert_eq!(outcome_label(&AttemptOutcome::Abort(report)), "abort");
    }

    // --- response-header emission ----------------------------------------

    fn succeeded(attempts: u16) -> AttemptOutcome {
        AttemptOutcome::Succeeded {
            provider: ProviderId::new("openai"),
            attempts,
            upstream: Upstream::success(Box::pin(futures::stream::empty())),
        }
    }

    fn headers_for(meta: ar_tokens::ResponseMeta, attempts: u16) -> HeaderMap {
        let resp = decorate(
            succeeded(attempts),
            false,
            Dialect::OpenAi,
            Strategy::Priority,
            Stages {
                compression: "default;engines=-".to_owned(),
                cache_state: ar_cache::CacheState::Miss,
                cache_key: None,
                cache: None,
                meta,
            },
        );
        resp.headers().clone()
    }

    fn header_of(map: &HeaderMap, name: &str) -> String {
        map.get(name).unwrap_or_else(|| panic!("missing {name}")).to_str().expect("header text").to_owned()
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
        assert_eq!((header_of(&headers, TOKENS_IN_HEADER).as_str(), header_of(&headers, TOKENS_OUT_HEADER).as_str()), ("120", "34"));
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
        let headers = headers_for(ar_tokens::ResponseMeta::default().with_savings_tokens(128), 1);
        assert_eq!(header_of(&headers, SAVINGS_TOKENS_HEADER), "128");
    }

    #[test]
    fn stamps_the_latency_in_whole_milliseconds() {
        let headers = headers_for(ar_tokens::ResponseMeta::default().with_latency(Duration::from_millis(42)), 1);
        assert_eq!(header_of(&headers, LATENCY_MS_HEADER), "42");
    }

    #[test]
    fn stamps_the_fallback_attempts_the_attempt_loop_spent() {
        let headers = headers_for(ar_tokens::ResponseMeta::default(), 3);
        assert_eq!(header_of(&headers, FALLBACK_ATTEMPTS_HEADER), "3");
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
        assert_eq!(header_of(&headers_for(meta, 1), RESPONSE_COST_HEADER), row.cost_usd.as_decimal_string());
    }

    #[test]
    fn stamps_a_token_count_that_matches_the_ledger_row() {
        // Completion-only usage, so the ledger's `total_tokens` *is* the prompt
        // count and the two can be compared without widening `LedgerRow`.
        let ledger = ar_tokens::Ledger::open_in_memory().expect("ledger");
        let meta = ledger
            .record_response("k1", "openai", "gpt-4o", &serde_json::json!({ "prompt_tokens": 1_000_000 }), 1_700_000_000, &priced_table())
            .expect("record");
        let mut report = ledger.report(1).expect("report");
        let row = report.rows.remove(0);
        assert_eq!(header_of(&headers_for(meta, 1), TOKENS_IN_HEADER), row.total_tokens.to_string());
    }

    fn priced_table() -> ar_tokens::PricingTable {
        let mut t = ar_tokens::PricingTable::default();
        t.set("openai", "gpt-4o", ar_tokens::Prices { input_micros_per_mtok: 2_500_000, output_micros_per_mtok: 10_000_000 });
        t
    }

    fn record_million_token_completion(ledger: &ar_tokens::Ledger) -> ar_tokens::ResponseMeta {
        ledger
            .record_response(
                "k1",
                "openai",
                "gpt-4o",
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
        let resp = models_head().await;
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
        let resp = model(State(state), Path(card.id.clone())).await;
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
        let resp = model(State(state), Path("gpt-4o".to_owned())).await;
        assert_eq!(resp.status(), 404);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "model_not_found");
        assert!(
            body["error"]["message"].as_str().unwrap_or_default().contains("gpt-4o"),
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
        assert_eq!(kind_for(StatusCode::UNSUPPORTED_MEDIA_TYPE), "invalid_request_error");
        assert_eq!(kind_for(StatusCode::TOO_MANY_REQUESTS), "rate_limit_error");
        assert_eq!(kind_for(StatusCode::BAD_GATEWAY), "server_error");
    }

    // --- unknown-route fallback ----------------------------------------

    #[tokio::test]
    async fn an_unknown_path_is_a_json_404_that_carries_the_path() {
        let req = axum::http::Request::builder()
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
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(value).expect("header value"));
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
        assert!(require_json(&post_with_content_type(Some("application/json; charset=utf-8"))).is_none());
    }

    #[test]
    fn admits_json_regardless_of_case() {
        assert!(require_json(&post_with_content_type(Some("Application/JSON"))).is_none());
    }

    // --- Accept forces streaming ---------------------------------------

    fn accept(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_str(value).expect("header value"));
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
        assert!(!accept_forces_stream(&accept("application/json, text/event-stream")));
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
        assert!(declares_stream(&Bytes::from_static(br#"{"model":"m","stream":false}"#)));
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
        assert!(!declares_stream(&Bytes::from_static(br#"{"model":"m","stream":null}"#)));
    }

    #[test]
    fn an_explicit_stream_true_is_a_declaration() {
        assert!(declares_stream(&Bytes::from_static(br#"{"model":"m","stream":true}"#)));
    }

    // --- keepalive frames ----------------------------------------------

    #[test]
    fn the_responses_terminator_is_its_own_completed_event() {
        // Responses discriminates on a `type` inside the payload, so the
        // terminator is the completed event rather than a terminator line.
        let terminator = Dialect::Responses.terminator().expect("responses has one");
        assert!(!terminator_seen(&Bytes::from_static(b"data: [DONE]\n\n"), Some(terminator)));
        assert!(terminator_seen(
            &Bytes::from_static(b"data: {\"type\":\"response.completed\"}\n\n"),
            Some(terminator)
        ));
    }

    #[test]
    fn the_anthropic_terminator_is_its_own_stop_event() {
        // Not `data: [DONE]`, which is the OpenAI spelling: an Anthropic stream
        // that ends on the wrong terminator is a stream that ended wrong.
        let terminator = Dialect::Anthropic.terminator().expect("anthropic has one");
        assert!(terminator_seen(&Bytes::from_static(b"event: message_stop\ndata: {}\n\n"), Some(terminator)));
        assert!(!terminator_seen(&Bytes::from_static(b"data: [DONE]\n\n"), Some(terminator)));
    }

    #[test]
    fn the_anthropic_keepalive_is_a_real_ping_event() {
        // The load-bearing assertion in the whole keepalive design: an SSE
        // comment is invisible to the client whose watchdog this exists for.
        let frame = Dialect::Anthropic.keepalive().expect("anthropic has a keepalive");
        assert!(frame.starts_with("event: ping"), "not a real event: {frame}");
        assert!(!frame.contains(": keepalive"), "a comment is not a ping: {frame}");
    }

    #[test]
    fn the_anthropic_truncation_error_is_a_real_error_event() {
        // The same reasoning as the ping: the Anthropic spec defines
        // `event: error`, and a comment carrying an error reaches no client.
        let frame = Dialect::Anthropic.stream_error().expect("anthropic names truncation");
        assert!(frame.starts_with("event: error"), "not a real event: {frame}");
    }

    #[test]
    fn the_openai_chat_frames_never_emit_an_event_line() {
        // A line-based parser drops an unrecognised `event:` line and desyncs on
        // the `data:` after it, which loses the error the client needed. Both
        // frames, not just the keepalive.
        for frame in [
            Dialect::OpenAi.keepalive().expect("openai has a keepalive"),
            Dialect::OpenAi.stream_error().expect("openai names truncation"),
        ] {
            assert!(!frame.contains("event:"), "an OpenAI frame emitted an event: {frame}");
        }
    }

    #[test]
    fn the_responses_keepalive_is_a_real_in_progress_event() {
        // A Responses client discriminates on `type` inside the payload, so
        // "still working" has to be spelled `response.in_progress` rather than a
        // comment the parser skips.
        let frame = Dialect::Responses.keepalive().expect("responses has a keepalive");
        assert!(
            frame.contains("\"type\":\"response.in_progress\""),
            "not an in-progress event: {frame}"
        );
        assert!(!frame.contains(": keepalive"), "a comment is not an event: {frame}");
        // The payload must not look like a finished answer, or a client renders
        // content this server invented.
        assert!(frame.contains("\"status\":\"in_progress\""), "{frame}");
        assert!(frame.contains("\"output\":[]"), "{frame}");
    }

    #[test]
    fn the_openai_keepalive_is_a_comment() {
        // Every SSE parser skips a comment by definition, so this is the one frame
        // shape that is safe for a client with no event names.
        let frame = Dialect::OpenAi.keepalive().expect("openai has a keepalive");
        assert!(frame.starts_with(':'), "an OpenAI keepalive must be a comment: {frame}");
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
                Dialect::Anthropic => br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
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
            let frame = dialect.stream_error().expect("sse dialects name truncation");
            assert!(frame.contains("stream_error"), "{dialect:?}: {frame}");
            assert!(!frame.contains("Bearer"), "{dialect:?}: {frame}");
        }
    }

    #[test]
    fn a_terminator_is_recognised_inside_a_frame() {
        assert!(terminator_seen(&Bytes::from_static(b"data: [DONE]\n\n"), Dialect::OpenAi.terminator()));
        assert!(!terminator_seen(&Bytes::from_static(b"data: {}\n\n"), Dialect::OpenAi.terminator()));
        // Split across chunks is not claimed either way: the relay checks the
        // last frame, so a partial frame is a frame without a terminator.
        assert!(!terminator_seen(&Bytes::from_static(b"data: [DO"), Dialect::OpenAi.terminator()));
    }

    /// Drains a relay to a `Vec<String>`, one entry per frame.
    async fn drain(keepalive: Keepalive, chunks: Vec<Bytes>) -> Vec<String> {
        use futures::StreamExt;
        let stream: ar_route::ChunkStream = Box::pin(futures::stream::iter(chunks));
        keepalive
            .relay(stream)
            .map(|item| {
                String::from_utf8(item.expect("infallible").to_vec()).expect("frames are UTF-8")
            })
            .collect()
            .await
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
        assert_eq!(frames.len(), 2, "one relayed frame plus the error: {frames:?}");
        assert!(frames[1].contains("stream_error"), "no in-band error: {frames:?}");
    }

    #[tokio::test]
    async fn a_stream_that_ends_with_its_terminator_gets_no_error_frame() {
        let frames = drain(
            Keepalive::new(Dialect::OpenAi, NOWAIT),
            vec![Bytes::from_static(b"data: {\"delta\":{}}\n\n"), Bytes::from_static(b"data: [DONE]\n\n")],
        )
        .await;
        assert_eq!(frames.len(), 2, "a terminated stream gained a frame: {frames:?}");
        assert!(frames[1].contains("[DONE]"), "the terminator was not relayed: {frames:?}");
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
            .relay(stalled())
            .take(2)
            .map(|item| {
                String::from_utf8(item.expect("infallible").to_vec()).expect("frames are UTF-8")
            })
            .collect()
            .await
    }

    #[tokio::test]
    async fn an_idle_stream_emits_the_dialects_own_keepalive() {
        let keepalive = Keepalive::new(Dialect::Anthropic, Duration::from_millis(20));
        assert!(!keepalive.is_noop());
        let frames = idle_frames(keepalive).await;
        assert_eq!(frames.len(), 2, "an idle stream was not kept alive: {frames:?}");
        for frame in &frames {
            assert!(frame.contains("event: ping"), "not an Anthropic ping: {frame}");
        }
    }

    #[tokio::test]
    async fn an_idle_responses_stream_emits_in_progress_frames() {
        let frames = idle_frames(Keepalive::new(Dialect::Responses, Duration::from_millis(20))).await;
        assert_eq!(frames.len(), 2, "an idle stream was not kept alive: {frames:?}");
        for frame in &frames {
            assert!(frame.contains("response.in_progress"), "not an in-progress frame: {frame}");
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
        assert!(!frames.iter().any(|f| f.contains("keepalive")), "keepalive on a fast stream: {frames:?}");
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
            vec![Bytes::from_static(b"event: content_block_delta\ndata: {}\n\n")],
        )
        .await;
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert!(frames[1].starts_with("event: error"), "not an Anthropic error event: {frames:?}");
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
        assert_eq!(frames.len(), 2, "a terminated Responses stream gained a frame: {frames:?}");
    }

    #[tokio::test]
    async fn an_empty_stream_gets_no_truncation_frame() {
        // `None` rather than `Some(false)`: the upstream produced nothing at all,
        // so there is no half-finished answer to describe and a client that got
        // zero bytes is not waiting on a frame.
        let frames = drain(Keepalive::new(Dialect::OpenAi, NOWAIT), vec![]).await;
        assert!(frames.is_empty(), "an empty stream was described: {frames:?}");
    }

    // --- the 415 guard, through the router ----------------------------

    #[tokio::test]
    async fn a_text_plain_body_is_a_415_and_never_reaches_the_translator() {
        // Letting it through produces a 400 about JSON syntax, which sends an
        // operator to the wrong place entirely.
        let router = crate::app::app(routed());
        let mut req = chat("hello", &[]);
        req.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        let resp = drive(&router, req).await;
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "unsupported_media_type");
        assert_eq!(body["error"]["type"], "invalid_request_error");
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
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY, "the body was refused at the edge");
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
            resp.headers().get(crate::routes::CACHE_HEADER).and_then(|v| v.to_str().ok()),
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
            resp.headers().get(crate::routes::CACHE_HEADER).and_then(|v| v.to_str().ok()),
            Some("miss"),
            "the body said JSON and Accept was only a fallback"
        );
    }
}
