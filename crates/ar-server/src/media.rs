//! Media routes: embeddings, audio transcriptions, image generations, OCR.
//!
//! The non-chat half of the surface `docs/02` assigns to "P6, separate
//! adapters": one handler per endpoint, one dispatch core shared by all of
//! them. A media request routes exactly like a chat one — model in, provider
//! chain out — so [`dispatch_media`] builds a [`ar_route::CanonicalRequest`]
//! shell around the model and reuses [`crate::routes::resolve`]; what it
//! deliberately does not reuse is the chat pipeline's body translation (these
//! bodies are not chat-shaped), compression (they are not prose), and cache
//! (media is not cached in this build, so reporting a cache verdict would be
//! reporting a decision nothing made).
//!
//! # Model spelling
//!
//! The three JSON endpoints read `model` from the body; transcriptions cannot
//! — the model lives inside a multipart form this build does not parse — so
//! they route by the `?model=` query parameter and the form data must already
//! spell the upstream's model. That is the reference's own Deepgram
//! convention; the divergence (OpenAI-style form-field routing is not ported)
//! is recorded in `docs/audit-notes.md`.
//!
//! # Error shape
//!
//! Every router-authored failure is the typed envelope from the same
//! [`crate::routes::error_because`] vocabulary the chat routes use, so a
//! client switches on one error grammar across the whole surface. The one
//! exception is an upstream non-2xx: when every provider returned a verdict
//! and none was a 2xx, the last verdict is relayed verbatim — status,
//! `Content-Type`, body, `Retry-After` — because "the provider said no" is the
//! provider's answer to show, not ours to paraphrase.

use axum::extract::{OriginalUri, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use std::collections::HashMap;

use ar_keys::Lane;

use crate::app::AppState;
use crate::routes::{
    DECISION_HEADER, DENY_HEADER, RouteReject, SESSION_HEADER, admit, authorize, epoch_now,
    error_because, hold_lease, model_asked, note_failure, require_json, resolve, stamp,
};

/// `POST /v1/embeddings` — text in, vectors out, response re-rendered.
pub async fn embeddings(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    json_route(&state, &headers, uri.path(), "/embeddings", body).await
}

/// `POST /v1/images/generations` — prompt in, image reference out, verbatim.
pub async fn image_generations(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    json_route(&state, &headers, uri.path(), "/images/generations", body).await
}

/// `POST /v1/ocr` — document in, text out, verbatim; vendor-defined wire.
pub async fn ocr(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    json_route(&state, &headers, uri.path(), "/ocr", body).await
}

/// `POST /v1/audio/transcriptions` — multipart audio in, text out, verbatim.
///
/// The query parameter names the route: `?model=<combo|provider/model>`. The
/// body's own `Content-Type` — multipart, with its per-request boundary — is
/// forwarded untouched.
pub async fn transcriptions(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    audio_route(
        &state,
        &headers,
        uri.path(),
        "transcriptions",
        "/audio/transcriptions",
        query,
        body,
    )
    .await
}

/// `POST /v1/audio/translations` — multipart audio in, English text out,
/// verbatim. Route and forwarding are [`transcriptions`]'s, one endpoint over.
pub async fn translations(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    audio_route(
        &state,
        &headers,
        uri.path(),
        "translations",
        "/audio/translations",
        query,
        body,
    )
    .await
}

/// The shared body of the two multipart audio routes. Logs a failed request
/// to the failure log under the `?model=` it asked for, as the chat routes do.
async fn audio_route(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    route_name: &str,
    endpoint: &'static str,
    query: HashMap<String, String>,
    body: Bytes,
) -> Response {
    let asked = query.get("model").cloned().unwrap_or_default();
    let mut seen_key = None;
    let mut lease = None;
    let resp = audio_route_inner(
        state,
        headers,
        path,
        route_name,
        endpoint,
        query,
        body,
        &mut seen_key,
        &mut lease,
    )
    .await;
    note_failure(state, &resp, seen_key.as_deref(), || asked);
    hold_lease(resp, lease)
}

/// [`audio_route`]'s work: auth, `?model=` route input, verbatim forward.
#[expect(
    clippy::too_many_arguments,
    reason = "the inputs are audio_route's own; a struct would exist for one call"
)]
async fn audio_route_inner(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    route_name: &str,
    endpoint: &'static str,
    query: HashMap<String, String>,
    body: Bytes,
    seen_key: &mut Option<String>,
    lease: &mut Option<ar_keys::Lease>,
) -> Response {
    let key_id = match authorize(state, headers, path) {
        Ok(key_id) => key_id,
        Err(reason) => return *reason,
    };
    seen_key.clone_from(&key_id);
    // The policy layer's 429 arm, right after the gate and before any body
    // work — the same order the chat path uses (`routes.rs`). The media
    // family used to consult no policy at all, so the one arm that is NOT
    // gated on a configured ledger was the one that would have bitten first.
    //
    // The scope key is the model the client asked for: the audio routes take
    // it from `?model=`, and a `model:`-scoped row must not rate-limit a
    // request for a model it does not name.
    let policy_key = key_id.as_deref().unwrap_or("anonymous");
    let asked_model = query.get("model").map(String::as_str).unwrap_or_default();
    if let Some(wait) = state.policy.rate_wait(policy_key, asked_model) {
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
    match admit(state, Lane::Heavy, key_id.as_deref().unwrap_or("anonymous")).await {
        Ok(granted) => *lease = granted,
        Err(refusal) => return *refusal,
    }
    // A malformed percent-encoding is no reason to refuse a request whose only
    // route input is this parameter; the missing-model case has its own refusal.
    let model = query.get("model").map(String::as_str).unwrap_or_default();
    if model.trim().is_empty() {
        return error_because(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing_model",
            &format!(
                "{route_name} route by the `?model=` query parameter in this build; \
                 the multipart form's model field is not parsed"
            ),
        );
    }
    // No `require_json`: the body is the client's own multipart payload, and
    // its boundary directive is per-request state the forward must preserve.
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");
    respond_media(
        state,
        headers,
        model,
        endpoint,
        content_type,
        body,
        policy_key,
    )
    .await
}

/// The shared body of the three JSON routes. Logs a failed request to the
/// failure log under the `model` its body asked for, as the chat routes do.
async fn json_route(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    endpoint: &'static str,
    body: Bytes,
) -> Response {
    // `Bytes` clones by refcount; the copy is only parsed on a failure.
    let asked = body.clone();
    let mut seen_key = None;
    let mut lease = None;
    let resp = json_route_inner(
        state,
        headers,
        path,
        endpoint,
        body,
        &mut seen_key,
        &mut lease,
    )
    .await;
    note_failure(state, &resp, seen_key.as_deref(), || model_asked(&asked));
    hold_lease(resp, lease)
}

/// [`json_route`]'s work: auth, 415, model from the body.
async fn json_route_inner(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    endpoint: &'static str,
    body: Bytes,
    seen_key: &mut Option<String>,
    lease: &mut Option<ar_keys::Lease>,
) -> Response {
    let key_id = match authorize(state, headers, path) {
        Ok(key_id) => key_id,
        Err(reason) => return *reason,
    };
    seen_key.clone_from(&key_id);
    // The policy layer's 429 arm, right after the gate and before any body
    // work — the same order the chat path uses (`routes.rs`).
    //
    // The scope key is the model the client asked for, which for these routes
    // is the body's `model` field. It is read here, before the format check
    // and before the full parse, because the arm must be cheap: a
    // rate-limited client should not cost the router a translation.
    let policy_key = key_id.as_deref().unwrap_or("anonymous");
    let asked_model = model_asked(&body);
    if let Some(wait) = state.policy.rate_wait(policy_key, &asked_model) {
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
    if let Some(reason) = require_json(headers) {
        return reason;
    }
    // Embeddings are batch work that may wait; generation and OCR are heavy.
    let lane = if endpoint == "/embeddings" {
        Lane::Batch
    } else {
        Lane::Heavy
    };
    match admit(state, lane, key_id.as_deref().unwrap_or("anonymous")).await {
        Ok(granted) => *lease = granted,
        Err(refusal) => return *refusal,
    }
    let model = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) => match value.get("model").and_then(serde_json::Value::as_str) {
            Some(model) if !model.trim().is_empty() => model.to_owned(),
            _ => {
                return error_because(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "missing_model",
                    "a media request must name a model",
                );
            }
        },
        Err(e) => {
            return error_because(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "unparsable_body",
                &e.to_string(),
            );
        }
    };
    respond_media(
        state,
        headers,
        &model,
        endpoint,
        "application/json",
        body,
        policy_key,
    )
    .await
}

/// The upstream's answer, as the relay hands it to the client.
struct Dispatched {
    /// The winning (2xx) or last-returned verdict.
    status: StatusCode,
    /// Body bytes, verbatim.
    body: Bytes,
    /// The upstream's own `Content-Type`.
    content_type: String,
    /// `Retry-After`, when the upstream sent a usable one.
    retry_after: Option<std::time::Duration>,
    /// The routing verdict, same grammar as the chat routes' `x-ar-decision`.
    decision: String,
}

/// Routes by `model` and walks the provider chain: first 2xx wins, a non-2xx
/// is remembered and the next provider is tried, and the last non-2xx is
/// relayed verbatim when nothing succeeded — the provider's verdict, not
/// ours. Only when no provider was reached at all (transport errors
/// throughout) does the router author its own 502.
async fn respond_media(
    state: &AppState,
    headers: &HeaderMap,
    model: &str,
    endpoint: &'static str,
    content_type: &str,
    body: Bytes,
    policy_key: &str,
) -> Response {
    if !state.config.has_provider() {
        return error_because(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_provider",
            "nothing_configured",
            "no provider is configured; `ar doctor` lists what is missing",
        );
    }
    let session = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(|v| ar_route::Strng::from(v.to_owned()));
    let shell = ar_route::CanonicalRequest::new(model, Bytes::new()).with_session(session);
    let mut plan = match resolve(state, &shell) {
        Ok(plan) => plan,
        Err(RouteReject::UnknownModel(reason)) => {
            return error_because(
                StatusCode::BAD_REQUEST,
                "model_not_found",
                "unknown_model",
                &reason,
            );
        }
    };

    // The spend arms, under the same ceiling the chat path enforces. The
    // media family used to consult no policy at all, which meant a
    // `limits:` block was not a complete statement of what the proxy
    // refuses: a key over its ceiling was refused on `/v1/chat/completions`
    // and spent freely on `/v1/embeddings`.
    //
    // The scope key is the model the client asked for — `model`, which is the
    // combo id the route resolved — so a `model:`-scoped row governs this
    // request exactly as it governs a chat request for the same combo.
    if let Some(ledger) = state.ledger.as_ref() {
        let window = state.policy.spend_window(policy_key);
        let key_window = state.policy.key_spend_window(policy_key, model);
        let (breakdown, key_breakdown) = {
            let guard = ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let target_since = window.map(|w| epoch_now().saturating_sub(w.as_secs() as i64));
            let key_since = key_window.map(|w| epoch_now().saturating_sub(w.as_secs() as i64));
            (
                guard.spend_breakdown(policy_key, target_since),
                guard.spend_breakdown(policy_key, key_since),
            )
        };
        let (breakdown, key_breakdown) = match (breakdown, key_breakdown) {
            (Ok(a), Ok(b)) => (a, b),
            _ => {
                // A ledger that cannot be read cannot evaluate a ceiling, and
                // a cap that fails open is not a cap.
                tracing::warn!("spend ledger could not be read");
                let mut resp = error_because(
                    StatusCode::PAYMENT_REQUIRED,
                    "payment_required",
                    "unauditable",
                    "the spend ledger could not be read, so this key's ceilings cannot be checked",
                );
                stamp(&mut resp, DENY_HEADER, "unauditable");
                return resp;
            }
        };

        // The key-wide arm. The projection is the media request's own shape:
        // the body is the client's payload, so its token count is what the
        // client sent, and there is no declared output ceiling for a media
        // route. An unpriced model projects UNPRICED, which is what
        // `refuse_unpriced` denies.
        if let Some(cap) = state.policy.key_cap(policy_key, model) {
            let spend = if key_window.is_some() {
                key_breakdown.windowed
            } else {
                key_breakdown.total
            };
            let projection = crate::policy::Projection {
                provider: plan.chain.first().map_or("", |t| t.provider.as_str()),
                model: plan.chain.first().map_or("", |t| t.model.as_ref()),
                tokens_in: ar_tokens::count_text(&String::from_utf8_lossy(&body)),
                tokens_out: 0,
                prices: &state.config.prices,
            };
            let verdict = {
                let guard = ledger
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.admit_against(
                    policy_key,
                    &cap,
                    &spend,
                    crate::policy::projected_cost(&projection),
                    projection.tokens_in,
                )
            };
            if let Ok(ar_tokens::Verdict::Deny(reason)) = verdict {
                let mut resp = error_because(
                    reason.status(),
                    "payment_required",
                    reason.code(),
                    &reason.to_string(),
                );
                stamp(&mut resp, DENY_HEADER, reason.code());
                return resp;
            }
        }

        // The per-target arm: a target over its own ceiling is dropped from
        // the chain and the request continues on a sibling with budget left.
        // Only a chain with nothing left is refused.
        //
        // The one decision the chat path's `filter_by_target_budget` also
        // makes, spelled out here because that helper is private to `routes`
        // and the media chain is the same shape.
        let windowed = window.is_some();
        let mut kept = Vec::with_capacity(plan.chain.len());
        let mut refused = Vec::new();
        for target in &plan.chain {
            let name = format!("{}/{}", target.provider.as_str(), target.model);
            let Some(cap) = state.policy.target_cap(policy_key, &name) else {
                kept.push(target.clone());
                continue;
            };
            let spend = breakdown.target(target.provider.as_str(), target.model.as_ref());
            let spent = if windowed {
                spend.windowed.tokens
            } else {
                spend.total.tokens
            };
            if spent >= cap {
                refused.push((target.provider.clone(), target.model.clone()));
            } else {
                kept.push(target.clone());
            }
        }
        if !refused.is_empty() && kept.is_empty() {
            let mut resp = error_because(
                StatusCode::PAYMENT_REQUIRED,
                "payment_required",
                "model_token_cap",
                &format!(
                    "every media target has reached its own ceiling: {}",
                    refused
                        .iter()
                        .map(|(p, m)| format!("{p}/{m}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            stamp(&mut resp, DENY_HEADER, "model_token_cap");
            return resp;
        }
        plan.chain = kept;
    }

    let mut last_reply: Option<ar_route::MediaReply> = None;
    let mut last_error: Option<String> = None;
    let mut tried: u16 = 0;
    let mut winner: Option<ar_route::ProviderId> = None;
    for target in &plan.chain {
        tried += 1;
        match state
            .exec
            .post_media(
                &target.provider,
                Some(target.model.as_ref()),
                endpoint,
                content_type,
                &body,
            )
            .await
        {
            Ok(reply) if reply.status.is_success() => {
                winner = Some(target.provider.clone());
                last_reply = Some(reply);
                break;
            }
            Ok(reply) => last_reply = Some(reply),
            Err(e) => last_error = Some(e.to_string()),
        }
    }

    let Some(reply) = last_reply else {
        // No provider answered with a verdict; nothing to relay but our own
        // refusal, which names the last transport failure so an operator
        // reads what actually broke.
        return error_because(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "media_dispatch",
            last_error
                .as_deref()
                .unwrap_or("every provider refused the dispatch"),
        );
    };

    let decision = format!(
        "strategy={};outcome={};provider={};attempts={tried}",
        plan.strategy,
        if winner.is_some() { "ok" } else { "failover" },
        winner.as_ref().map_or("-", ar_route::ProviderId::as_str),
    );
    let dispatched = Dispatched {
        status: reply.status,
        body: reply.body,
        content_type: reply.content_type,
        retry_after: reply.retry_after,
        decision,
    };

    // Embeddings are the one reply this build re-renders: providers order
    // their vectors however they like, and `create_embedding_response` is the
    // port of the reference's normalizer, so the client sees one shape. A
    // reply that does not parse is the provider's verdict relayed, not a
    // router-authored 502 — the bytes reached the client either way.
    if endpoint == "/embeddings"
        && dispatched.status == StatusCode::OK
        && let Ok(upstream) =
            serde_json::from_slice::<ar_translate::EmbeddingUpstream>(&dispatched.body)
    {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header(DECISION_HEADER, dispatched.decision)
            .body(axum::body::Body::from(
                serde_json::to_vec(&ar_translate::create_embedding_response(&upstream))
                    .unwrap_or_else(|_| b"{}".to_vec()),
            ))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    }

    let mut builder = Response::builder()
        .status(dispatched.status)
        .header(header::CONTENT_TYPE, dispatched.content_type)
        .header(DECISION_HEADER, dispatched.decision);
    if let Some(retry_after) = dispatched.retry_after {
        builder = builder.header(header::RETRY_AFTER, retry_after.as_secs());
    }
    builder
        .body(axum::body::Body::from(dispatched.body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    use ar_route::{
        ArExec, CanonicalRequest, ExecError, MediaReply, ProviderId, Strategy, Upstream,
    };

    use super::*;

    /// One recorded media dispatch: provider, model, endpoint, content type,
    /// bytes. The model is recorded because `post_media` receives the routed
    /// target's model as a separate parameter — there is no `CanonicalRequest` in
    /// that call for a rebind to ride on, so nothing else observes it.
    type Seen = Arc<Mutex<Vec<(String, String, String, String, Vec<u8>)>>>;

    /// Records each media dispatch and answers with the canned reply, or the
    /// canned refusal when `error` is set.
    struct MediaExec {
        seen: Seen,
        reply: &'static str,
        status: StatusCode,
        error: bool,
        /// Provider id that refuses, so a chain walks past it to the next target.
        refuse: Option<&'static str>,
    }

    impl ArExec for MediaExec {
        fn post_chat<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            Box::pin(async move {
                Err(ExecError::Transport(
                    "no chat dispatch in a media test".to_owned(),
                ))
            })
        }

        fn post_media<'a>(
            &'a self,
            provider: &'a ProviderId,
            model: Option<&'a str>,
            endpoint: &'a str,
            content_type: &'a str,
            body: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
            self.seen.lock().expect("recorder lock").push((
                provider.as_str().to_owned(),
                model.unwrap_or_default().to_owned(),
                endpoint.to_owned(),
                content_type.to_owned(),
                body.to_vec(),
            ));
            let status = self.status;
            let reply = self.reply;
            let error = self.error || self.refuse == Some(provider.as_str());
            Box::pin(async move {
                if error {
                    return Err(ExecError::Transport("connection refused".to_owned()));
                }
                Ok(MediaReply {
                    status,
                    body: Bytes::from(reply),
                    content_type: "application/json".to_owned(),
                    retry_after: None,
                })
            })
        }
    }

    /// One dispatchable provider serving model `m`, with `exec` swapped in.
    fn routed_under(exec: Arc<dyn ArExec>) -> AppState {
        routed_over(exec, 1)
    }

    /// `width` targets on the chain, all sharing the combo alias `m`. Width > 1
    /// is what makes per-target model dispatch observable: each target carries
    /// its own model spelling, and the recorder sees exactly what the chain
    /// asked for.
    fn routed_over(exec: Arc<dyn ArExec>, width: usize) -> AppState {
        let providers = (0..width)
            .map(|i| {
                crate::exec::ProviderConfig::new(
                    ProviderId::new(format!("p{i}")),
                    format!("http://127.0.0.1:1/v{i}"),
                    "k",
                )
                .with_model("row-model")
            })
            .collect();
        let mut config = crate::config::ServerConfig::single(0, Strategy::Priority, providers);
        config.combos = vec![crate::config::RouteCombo::new(
            "m",
            Strategy::Priority,
            (0..width)
                .map(|i| {
                    crate::config::ComboTarget::new(ProviderId::new(format!("p{i}")), "t-model")
                })
                .collect(),
        )];
        crate::app::Components {
            exec,
            ..crate::app::Components::unconfigured(config)
        }
        .into_state()
    }

    async fn drive(app: &axum::Router, req: axum::http::Request<axum::body::Body>) -> Response {
        use tower::ServiceExt;
        app.clone().oneshot(req).await.expect("router answers")
    }

    /// The same server, with a `limits:` block and the in-memory ledger the
    /// spend arms read.
    ///
    /// The ledger is what arms the spend arms: without one they are inert by
    /// construction, so a test that wants them to bite has to configure one.
    fn limited_over(
        limits: &str,
        exec: Arc<dyn ArExec>,
        width: usize,
    ) -> (AppState, Arc<Mutex<ar_tokens::Ledger>>) {
        let providers = (0..width)
            .map(|i| {
                crate::exec::ProviderConfig::new(
                    ProviderId::new(format!("p{i}")),
                    format!("http://127.0.0.1:1/v{i}"),
                    "k",
                )
                .with_model("row-model")
            })
            .collect();
        let mut config = crate::config::ServerConfig::single(0, Strategy::Priority, providers);
        config.combos = vec![crate::config::RouteCombo::new(
            "m",
            Strategy::Priority,
            (0..width)
                .map(|i| {
                    crate::config::ComboTarget::new(ProviderId::new(format!("p{i}")), "t-model")
                })
                .collect(),
        )];
        config.limits = ar_config::Config::parse(limits, |_| Ok(None))
            .expect("test limits parse")
            .limits;
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let state = crate::app::Components {
            exec,
            ledger: Some(ledger.clone()),
            ..crate::app::Components::unconfigured(config)
        }
        .into_state();
        (state, ledger)
    }

    fn post(path: &str, body: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .header("host", "127.0.0.1:20128")
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_owned()))
            .expect("request builds")
    }

    async fn body_of(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body reads");
        serde_json::from_slice(&bytes).expect("body is JSON")
    }

    #[tokio::test]
    async fn embeddings_rebuild_the_reply_in_index_order() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: r#"{"data":[{"embedding":[0.1,0.2],"index":1},
                {"embedding":[0.3],"index":0}],
                "model":"m","usage":{"prompt_tokens":3,"total_tokens":3}}"#,
            status: StatusCode::OK,
            error: false,
            refuse: None,
        });
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            post("/v1/embeddings", r#"{"model":"m","input":["a"]}"#),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_of(resp).await;
        assert_eq!(body["data"][0]["index"], 0, "vectors are re-sorted: {body}");
    }

    #[tokio::test]
    async fn each_chain_target_is_dispatched_under_its_own_model() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: r#"{"data":[{"embedding":[0.1],"index":0}],"model":"m"}"#,
            status: StatusCode::OK,
            error: false,
            refuse: Some("p0"),
        });
        let router = crate::app::app(routed_over(exec, 2));
        let resp = drive(
            &router,
            post("/v1/embeddings", r#"{"model":"m","input":["a"]}"#),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let recorded = seen.lock().expect("recorder");
        assert_eq!(recorded.len(), 2, "both targets were tried: {recorded:?}");
        assert_eq!(recorded[0].0, "p0", "first target: {recorded:?}");
        assert_eq!(recorded[1].0, "p1", "second target: {recorded:?}");
        assert_eq!(
            recorded[0].1, "t-model",
            "target p0 asked under the wrong model: {recorded:?}"
        );
        assert_eq!(
            recorded[1].1, "t-model",
            "target p1 asked under the wrong model: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn a_media_dispatch_never_falls_back_to_the_provider_rows_model() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: r#"{"data":[{"embedding":[0.1],"index":0}],"model":"m"}"#,
            status: StatusCode::OK,
            error: false,
            refuse: None,
        });
        // `routed_over` gives every provider row the model `row-model`; the chain
        // target says `t-model`. The chain is authoritative, so the row must never
        // be what reaches the executor.
        let router = crate::app::app(routed_over(exec, 1));
        let resp = drive(
            &router,
            post("/v1/embeddings", r#"{"model":"m","input":["a"]}"#),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let recorded = seen.lock().expect("recorder");
        assert_ne!(
            recorded[0].1, "row-model",
            "the dispatch row's model beat the routed target: {recorded:?}"
        );
        assert_eq!(recorded[0].1, "t-model", "{recorded:?}");
    }

    #[tokio::test]
    async fn an_unknown_model_is_a_typed_400_that_never_dispatches() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: "{}",
            status: StatusCode::OK,
            error: false,
            refuse: None,
        });
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            post("/v1/ocr", r#"{"model":"nope","document":"x"}"#),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "model_not_found");
        assert!(seen.lock().expect("recorder").is_empty());
    }

    #[tokio::test]
    async fn a_media_route_over_its_keys_ceiling_is_refused_before_any_dispatch() {
        // `docs/40` P3: every route that can spend is under the same arms. The
        // media family used to consult no policy at all, so a key past its
        // ceiling was refused on `/v1/chat/completions` and spent freely here.
        let (exec, seen) = recording_media();
        let (state, ledger) = limited_over("limits:\n  default:\n    tokens: 1\n", exec, 1);
        // Spend a token against the anonymous key so the ceiling is reached.
        ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(&ar_tokens::Entry {
                key_id: "anonymous",
                provider: "p0",
                model: "t-model",
                combo: "m",
                usage: ar_tokens::NormalizedUsage::new(1, 0),
                cost: ar_tokens::Cost::UNPRICED,
                created_at: 1_700_000_000,
            })
            .expect("seed the spend");
        let router = crate::app::app(state);
        let resp = drive(
            &router,
            post("/v1/embeddings", r#"{"model":"m","input":"soak"}"#),
        )
        .await;

        assert_eq!(
            resp.status(),
            StatusCode::PAYMENT_REQUIRED,
            "a media route must refuse a key past its ceiling: {}",
            body_of(resp).await
        );
        let seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(seen.is_empty(), "dispatched past the ceiling: {seen:?}");
    }

    #[tokio::test]
    async fn a_media_route_with_no_limits_block_behaves_as_before() {
        // The inert case, which is the deployed system's state on both hosts:
        // no `limits:` block, so the arms must change NOTHING.
        let (exec, seen) = recording_media();
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            post("/v1/embeddings", r#"{"model":"m","input":"soak"}"#),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK, "{}", body_of(resp).await);
        let seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(seen.len(), 1, "the request still dispatched: {seen:?}");
    }

    #[tokio::test]
    async fn a_media_request_inside_its_keys_ceiling_is_dispatched() {
        // The companion to the refusal: the arm is a ceiling, not a blanket
        // refusal. A key with a ceiling it is under still gets served.
        let (exec, seen) = recording_media();
        let (state, _ledger) = limited_over("limits:\n  default:\n    tokens: 10000\n", exec, 1);
        let router = crate::app::app(state);
        let resp = drive(
            &router,
            post("/v1/embeddings", r#"{"model":"m","input":"soak"}"#),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK, "{}", body_of(resp).await);
        let seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(seen.len(), 1, "the request dispatched: {seen:?}");
    }

    #[tokio::test]
    async fn a_media_request_past_its_keys_rpm_is_answered_with_retry_after() {
        // The 429 arm, and the one that matters most here: it is the single
        // arm NOT gated on a configured ledger, so without this a media route
        // is uncapped the moment any `limits:` block exists. A ledger is not
        // even needed to prove it, because the bucket is built from the rows.
        let (exec, _seen) = recording_media();
        let (state, _ledger) = limited_over("limits:\n  default:\n    rpm: 1\n", exec, 1);
        let router = crate::app::app(state);

        // The bucket is per credential, and anonymous traffic shares it, so the
        // first request spends the one token and the second is told to wait.
        let first = drive(
            &router,
            post("/v1/embeddings", r#"{"model":"m","input":"a"}"#),
        )
        .await;
        assert_eq!(
            first.status(),
            StatusCode::OK,
            "the first request is inside its bucket: {}",
            body_of(first).await
        );
        let second = drive(
            &router,
            post("/v1/embeddings", r#"{"model":"m","input":"b"}"#),
        )
        .await;
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry = second
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(!retry.is_empty(), "the 429 names when the next token lands");
    }

    /// The recorder the media-tests above share, so each one only states its
    /// own limits and its own assertions.
    fn recording_media() -> (Arc<MediaExec>, Seen) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: r#"{"object":"list","data":[]}"#,
            status: StatusCode::OK,
            error: false,
            refuse: None,
        });
        (exec, seen)
    }

    #[tokio::test]
    async fn transcriptions_require_a_model_query_parameter() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: "{}",
            status: StatusCode::OK,
            error: false,
            refuse: None,
        });
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            axum::http::Request::builder()
                .header("host", "127.0.0.1:20128")
                .method("POST")
                .uri("/v1/audio/transcriptions")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=b")
                .body(axum::body::Body::from("--b\r\n\r\n"))
                .expect("request builds"),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "invalid_request");
        assert!(seen.lock().expect("recorder").is_empty());
    }

    #[tokio::test]
    async fn transcriptions_forward_the_multipart_body_verbatim() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: r#"{"text":"hello"}"#,
            status: StatusCode::OK,
            error: false,
            refuse: None,
        });
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            axum::http::Request::builder()
                .header("host", "127.0.0.1:20128")
                .method("POST")
                .uri("/v1/audio/transcriptions?model=m")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=b")
                .body(axum::body::Body::from("--b\r\n\r\nfile-bytes"))
                .expect("request builds"),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let recorded = seen.lock().expect("recorder");
        assert_eq!(
            recorded[0].2, "/audio/transcriptions",
            "wrong endpoint: {:?}",
            recorded[0]
        );
        assert_eq!(
            recorded[0].4, b"--b\r\n\r\nfile-bytes",
            "multipart bytes were not forwarded verbatim"
        );
    }

    #[tokio::test]
    async fn translations_require_a_model_query_parameter() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: "{}",
            status: StatusCode::OK,
            error: false,
            refuse: None,
        });
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            axum::http::Request::builder()
                .header("host", "127.0.0.1:20128")
                .method("POST")
                .uri("/v1/audio/translations")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=b")
                .body(axum::body::Body::from("--b\r\n\r\n"))
                .expect("request builds"),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "invalid_request");
        assert!(seen.lock().expect("recorder").is_empty());
    }

    #[tokio::test]
    async fn translations_forward_the_multipart_body_verbatim() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: r#"{"text":"bonjour"}"#,
            status: StatusCode::OK,
            error: false,
            refuse: None,
        });
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            axum::http::Request::builder()
                .header("host", "127.0.0.1:20128")
                .method("POST")
                .uri("/v1/audio/translations?model=m")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=b")
                .body(axum::body::Body::from("--b\r\n\r\nfile-bytes"))
                .expect("request builds"),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let recorded = seen.lock().expect("recorder");
        assert_eq!(
            recorded[0].2, "/audio/translations",
            "wrong endpoint: {:?}",
            recorded[0]
        );
        assert_eq!(
            recorded[0].4, b"--b\r\n\r\nfile-bytes",
            "multipart bytes were not forwarded verbatim"
        );
    }

    #[tokio::test]
    async fn image_generations_relay_the_reply_verbatim() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: r#"{"data":[{"url":"http://x/i.png"}]}"#,
            status: StatusCode::OK,
            error: false,
            refuse: None,
        });
        let router = crate::app::app(routed_under(exec));
        let resp = drive(
            &router,
            post(
                "/v1/images/generations",
                r#"{"model":"m","prompt":"a cat"}"#,
            ),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_of(resp).await;
        assert_eq!(body["data"][0]["url"], "http://x/i.png");
    }

    #[tokio::test]
    async fn an_upstream_non_2xx_is_relayed_verbatim() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: r#"{"error":{"message":"bad document","code":"payload_too_large"}}"#,
            status: StatusCode::UNPROCESSABLE_ENTITY,
            error: false,
            refuse: None,
        });
        let router = crate::app::app(routed_under(exec));
        let resp = drive(&router, post("/v1/ocr", r#"{"model":"m","document":"x"}"#)).await;

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "payload_too_large");
    }

    #[tokio::test]
    async fn a_failed_media_request_lands_in_the_failure_log_with_the_model_asked_for() {
        let ledger = Arc::new(Mutex::new(
            ar_tokens::Ledger::open_in_memory().expect("in-memory ledger"),
        ));
        let exec = Arc::new(MediaExec {
            seen: Arc::new(Mutex::new(Vec::new())),
            reply: r#"{"error":{"message":"bad document","code":"payload_too_large"}}"#,
            status: StatusCode::UNPROCESSABLE_ENTITY,
            error: false,
            refuse: None,
        });
        let mut state = routed_under(exec);
        state.ledger = Some(Arc::clone(&ledger));
        let router = crate::app::app(state);
        let _ = drive(&router, post("/v1/ocr", r#"{"model":"m","document":"x"}"#)).await;

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
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn every_transport_refusal_is_a_typed_502() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(MediaExec {
            seen: Arc::clone(&seen),
            reply: "{}",
            status: StatusCode::OK,
            error: true,
            refuse: None,
        });
        let router = crate::app::app(routed_under(exec));
        let resp = drive(&router, post("/v1/ocr", r#"{"model":"m","document":"x"}"#)).await;

        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = body_of(resp).await;
        assert_eq!(body["error"]["code"], "upstream_error");
    }
}
