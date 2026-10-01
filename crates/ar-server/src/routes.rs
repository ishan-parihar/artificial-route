//! The HTTP routes.
//!
//! One inbound chat pipeline shared by four dialects, plus the read-only
//! endpoints. Per request, in order:
//!
//! 1. **auth** — a bearer token when keys are configured; nothing when they are
//!    not, and then the bind is loopback-only (see [`crate::app::bind_addr`]).
//! 2. **translate** — [`to_canonical_for_route`], which refuses a body that is not
//!    the dialect the route claims.
//! 3. **guard** — `ar-guard` stage 1 redacts credentials out of the prompt,
//!    stage 2 refuses prompt injection. The *rewritten* body is what continues.
//! 4. **route** — the request `model` selects a combo; an unknown name is a 400
//!    that names the ones that exist. `auto/*` resolves through `ar_route`'s
//!    virtual factory and `simulate_route`.
//! 5. **compress** — `x-ar-compression` resolves a plan through `ar-compress`
//!    and the pipeline that ran is echoed back.
//! 6. **cache** — a non-streaming request is looked up in `ar-cache`; a stream
//!    bypasses, and says so.
//! 7. **attempt loop** — `ar_route::attempt_loop`, then `decorate`.
//!
//! A combo's `pool:` bench rides at the tail of its chain, so step 7 walks the
//! bench only once every target has refused.
//!
//! Route shape follows `../OmniRoute/src/app/api/v1/chat/completions/route.ts`:
//! validate, translate, resolve a provider, run the attempt loop, relay, stamp
//! decision headers.

use std::sync::Arc;

use ar_cache::{Cache, CacheKey, CacheState};
use ar_compress::Step;
use ar_route::{
    AttemptOutcome, AutoCandidate, AutoSelector, CanonicalRequest, Candidate, ProviderId, RouteError,
    Strng, Strategy, attempt_loop, pick, simulate_route, virtual_combo,
};
use axum::body::Body;
use axum::extract::State;
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

/// Ceiling on the response body the cache will buffer for storage.
///
/// A non-streaming completion is one JSON chunk and is nearly always well under
/// this. A provider that answers a non-streaming request with a multi-megabyte
/// body is relayed anyway — the cap decides only whether the cache *keeps* a
/// copy, never whether the client gets one.
pub const CACHE_MAX_BODY: usize = 1024 * 1024;

/// `POST /v1/chat/completions` — OpenAI-compatible, SSE pass-through.
pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_chat(&state, &headers, "/v1/chat/completions", body).await
}

/// `POST /v1/messages` — Anthropic Messages inbound.
pub async fn messages(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_chat(&state, &headers, "/v1/messages", body).await
}

/// `POST /v1/responses` — OpenAI Responses inbound.
pub async fn responses(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_chat(&state, &headers, "/v1/responses", body).await
}

/// `POST /api/chat` — Ollama chat inbound.
pub async fn ollama_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_chat(&state, &headers, "/api/chat", body).await
}

/// The one chat pipeline, for every inbound dialect.
async fn handle_chat(
    state: &AppState,
    headers: &HeaderMap,
    route: &str,
    body: Bytes,
) -> Response {
    if let Err(reason) = authorize(state, headers) {
        return error(StatusCode::UNAUTHORIZED, &reason);
    }
    if !state.config.has_provider() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no provider is configured; `ar doctor` lists what is missing",
        );
    }

    let mut canonical = match to_canonical_for_route(route, &body) {
        Ok(c) => c,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
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
            return error(StatusCode::BAD_REQUEST, &reason)
                .with_guard(GuardVerdict::Deny);
        }
    };
    canonical.body = Bytes::from(guarded);

    let plan = match resolve(state, &canonical) {
        Ok(p) => p,
        Err(RouteReject::UnknownModel(reason)) => {
            return error(StatusCode::BAD_REQUEST, &reason).with_guard(guard_verdict);
        }
    };

    // Compression runs after the guard, so a rewrite cannot un-redact anything,
    // and before the cache lookup, so the key covers exactly what is dispatched.
    let compression = compression_plan(
        headers.get(COMPRESSION_HEADER).and_then(|v| v.to_str().ok()),
        plan.compression.as_ref().map(std::slice::from_ref),
    );
    if let Some(rewritten) = compress_body(&canonical.body, &compression) {
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

    let outcome = match attempt_loop(
        &canonical,
        &plan.chain,
        state.exec.as_ref(),
        &state.resilience,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return route_error(e).with_guard(guard_verdict),
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
        &canonical,
        plan.strategy,
        compression_echo(&compression),
        cache_state,
        cache_key,
        cache,
    )
    .with_guard(guard_verdict)
}

/// The cache key for a canonical request: `blake3(tenant | model | body)`.
///
/// `ar_cache::key::request_key` is the `docs/04` key. The tenant is the constant
/// `"default"` because there is exactly one tenant — a single-operator install —
/// and inventing a second one would only change the digest.
fn request_key(canonical: &CanonicalRequest) -> Option<CacheKey> {
    let body = serde_json::from_slice::<serde_json::Value>(&canonical.body).ok()?;
    Some(ar_cache::key::request_key("default", &canonical.model, &body))
}

/// Why a request could not be routed.
///
/// Its own enum rather than a `Response` so [`resolve`] can hand back a
/// `Result<RoutePlan, RouteReject>` without a 128-byte error arm; the caller
/// turns the reason into a 400.
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
#[derive(Clone, Debug, PartialEq)]
struct RoutePlan {
    chain: Vec<ProviderId>,
    strategy: Strategy,
    /// The resolved combo's compression setting, if it declared one.
    compression: Option<Step>,
}

/// Checks the bearer credential when an [`crate::keys::AuthGate`] is configured.
///
/// No gate means no check, which is only safe because the same configuration
/// binds loopback-only — see [`crate::app::bind_addr`]. The two decisions live in
/// one struct on purpose: "no auth" and "public bind" must never be settable
/// independently.
///
/// Returns the reason rather than a `Response`: a `Response` in the `Err` arm
/// makes the whole `Result` 128 bytes wide, which clippy's perf gate (rightly)
/// refuses for a function whose happy path returns `()`. A `String` is 24 bytes
/// and is owned by whoever renders the 400, so an attacker sending a million bad
/// tokens allocates a million short-lived strings rather than leaking a million
/// of anything.
fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), String> {
    let Some(gate) = state.auth.as_deref() else {
        return Ok(());
    };
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or(
            "this server requires an access token: send `Authorization: Bearer <token>`",
        )?;
    gate.verify(token)
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
    canonical: &CanonicalRequest,
    strategy: Strategy,
    compression: String,
    cache_state: CacheState,
    cache_key: Option<CacheKey>,
    cache: Option<Arc<Cache>>,
) -> Response {
    let decision = format!(
        "strategy={strategy};outcome={};provider={};attempts={}",
        outcome_label(&outcome),
        outcome.provider().map_or("-", ProviderId::as_str),
        outcome.attempts()
    );

    let builder = Response::builder()
        .header(DECISION_HEADER, decision)
        .header(USAGE_HEADER, format!("attempts={}", outcome.attempts()))
        .header(CACHE_HEADER, cache_state.as_header())
        .header(COMPRESSION_ECHO, compression);

    let resp = match outcome {
        AttemptOutcome::Succeeded { upstream, .. } => {
            // Pass upstream framing through untouched. A `stream: true` request
            // gets `text/event-stream`; a non-stream completion is one JSON
            // chunk and gets `application/json`.
            let ct = if canonical.stream {
                "text/event-stream"
            } else {
                "application/json"
            };
            let status = upstream.status;
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
                _ => builder
                    .header(header::CONTENT_TYPE, ct)
                    .status(status)
                    .body(Body::from_stream(
                        upstream.stream.map(Ok::<Bytes, std::convert::Infallible>),
                    )),
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
                    "rate_limited",
                    "every provider in the chain is throttled",
                ))
        }
        AttemptOutcome::Failover { status, provider, .. } => builder
            .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY))
            .body(json_body(
                "upstream_unavailable",
                &format!("all providers failed; last was {provider}"),
            )),
        AttemptOutcome::Abort(report) => builder
            .status(report.status)
            .body(json_body("aborted", &report.reason)),
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
/// The store happens on the generator's final poll, i.e. after the last byte has
/// already reached the client, so a cache write never delays a relay. Past
/// [`CACHE_MAX_BODY`] the buffer is dropped rather than grown: a partial answer
/// replayed from cache looks like a complete one, and `ar_cache` has a named
/// `store_truncated` for exactly that decision.
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

/// Short outcome word for the decision header. One token, no spaces, so an
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
fn json_body(code: &str, message: &str) -> Body {
    let payload = ErrorEnvelope { error: ErrorBody { code, message } };
    Body::from(serde_json::to_vec(&payload).unwrap_or_else(|_| {
        br#"{"error":{"code":"internal","message":"serialization failed"}}"#.to_vec()
    }))
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

/// Builds a bare error response stamped with a null decision.
fn error(status: StatusCode, message: &str) -> Response {
    let mut resp = json_body("request_rejected", message).into_response();
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        HeaderName::from_static(DECISION_HEADER),
        HeaderValue::from_static("strategy=none"),
    );
    resp
}

/// Stamps the guard verdict onto a response.
trait WithGuard {
    /// Adds `x-ar-guard` so a client can see what the guard did without the
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
fn stamp(resp: &mut Response, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
        resp.headers_mut().insert(n, v);
    }
}

/// Maps a router error onto a response.
fn route_error(e: RouteError) -> Response {
    error(e.status(), &e.to_string())
}

/// `GET /healthz` — liveness only.
///
/// Readiness is P1: there is nothing to be un-ready about until a provider
/// chain exists, and a proxy that 503s on boot because a key is missing is a
/// proxy an operator cannot debug.
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

/// `GET /v1/models` — OpenAI-compatible catalog, stale-while-revalidate 60s.
pub async fn models(State(state): State<AppState>) -> Response {
    let cached = state.models.get();
    // Count revalidations, not reads: a counter that tracks traffic cannot tell a
    // revalidating catalog from an idle one.
    if cached.revalidated {
        state.metrics.observe_models_refresh();
    }
    let payload = serde_json::json!({
        "object": "list",
        "data": cached
            .cards
            .iter()
            .map(|c| serde_json::json!({
                "id": c.id,
                "object": "model",
                "owned_by": c.provider,
                "ar_upstream_model": c.upstream_model,
            }))
            .collect::<Vec<_>>(),
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

#[cfg(test)]
mod tests {
    use ar_route::{AbortReport, AttemptOutcome, ProviderId, Upstream};

    use super::{build_chain, outcome_label, unknown_model};
    use crate::config::ComboTarget;

    fn candidate(provider: &str) -> ar_route::Candidate {
        ar_route::Candidate::new(ProviderId::new(provider), "m")
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

    #[test]
    fn a_combo_target_carries_neutral_signals_by_default() {
        let t = ComboTarget::new(ProviderId::new("p"), "m");
        assert_eq!((t.input_usd_per_mtok, t.weight, t.quota), (None, 1, None));
    }
}
