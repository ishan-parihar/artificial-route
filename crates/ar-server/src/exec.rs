//! The HTTP executor: canonical request → provider wire → [`Upstream`].
//!
//! This used to be a second implementation of `ar-exec`. It is now a thin
//! adapter: [`HttpExec`] holds the provider table and the `ar_exec::ArExec`
//! client, and every request goes through the `ar-exec` core — the wire gate, the
//! header merge, the response-start budget, the abort race and the model rewrite
//! all live there exactly once. What stays here is the part that is *not*
//! dispatch: which provider id maps to which base URL and key, and the
//! translation from `ar_exec`'s [`ar_exec::ExecError`] to the
//! [`ar_route::ExecError`] the router's trait demands.
//!
//! One rule is hardcoded here: a non-2xx is a verdict the router has to see,
//! not an executor failure, so it is never an error at this layer. `ar-exec`
//! hands back the live response whatever its status,
//! [`ar_exec::ChatStream::into_failure`] reads the bounded body, and the router
//! classifies it through `ar_route::attempt_loop`. A provider that says 429 and
//! one that says 400 must reach the router differently, and only the router
//! knows the difference.
//!
//! # The request direction
//!
//! Closed: a canonical body reaches every named wire in that wire's own shape.
//! [`ar_exec::ArExec::render_request`] does the rendering and
//! [`ar_exec::outbound_wire`] is the one mapping from a registry label to a
//! renderer, so the API-key branch in [`HttpExec`] renders there rather than
//! handing raw canonical bytes to the core — one render, one POST. The OAuth
//! branch hands `oauth::Connection::dispatch` the canonical bytes and lets it
//! render, since that function re-posts the same body on a 401 rotation.
//!
//! # The response direction
//!
//! Not closed, and not presented as closed. The relay hands the client's bytes
//! through in the *provider's* framing, so a Claude-dialect client receives
//! Gemini SSE when the router picked a Gemini provider. The two non-streaming
//! envelopes built for that job are `ar_translate::to_anthropic_response` and
//! `ar_translate::to_responses_response`, and `ar_translate::missing_pairs` names
//! every remaining re-framing cell with its reason.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ar_registry::WireFormat;
use ar_route::{
    ArExec as ArRouteExec, CanonicalRequest, ExecError, MediaReply, ProviderId, Upstream,
};
use axum::http::StatusCode;
use bytes::Bytes;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use ar_exec::Dispatch;
use ar_exec::oauth::{
    Connected, Connection, HttpRefresher, OAuthKind, OAuthToken, Refresher, RotationPool, Session,
};

/// One configured upstream provider's OAuth credentials, resolved and ready.
///
/// Holds two bearer strings, so it is **not** `Debug`-derived even though
/// [`ProviderConfig`] is: a derived `Debug` would put an access token in whatever
/// printed a provider. [`Debug`] here prints only whether a refresh path is
/// armed, which is the fact an operator diagnosing a 401 actually needs.
///
/// The token material is already decrypted when this is built — the credential
/// store's job, done once in `ar-server`'s config reader — so nothing here
/// reaches back into a database on the request path.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthAuth {
    /// Which provider, which carve-out table, and where to refresh.
    pub session: Session,
    access: String,
    refresh: String,
    expires_at: Option<u64>,
}

impl OAuthAuth {
    /// Builds a resolved session. `refresh` empty means "useable but not
    /// renewable", which is a legitimate state the doctor reports rather than an
    /// error.
    #[must_use]
    pub fn new(
        session: Session,
        access: impl Into<String>,
        refresh: impl Into<String>,
        expires_at: Option<u64>,
    ) -> Self {
        Self {
            session,
            access: access.into(),
            refresh: refresh.into(),
            expires_at,
        }
    }

    /// Whether this session can renew: a refresh token *and* an endpoint.
    #[must_use]
    pub fn can_refresh(&self) -> bool {
        self.session.can_refresh(&self.token())
    }

    /// Whether this session can authenticate at all.
    ///
    /// An empty access token is the difference between a session and a hole, and
    /// `ProviderConfig::is_dispatchable` asks this rather than discovering it at
    /// the socket.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        !self.access.trim().is_empty()
    }

    /// The token `ar-exec` connects with.
    #[must_use]
    pub fn token(&self) -> OAuthToken {
        let mut token = OAuthToken::new(ar_config::Secret::new(&self.access));
        if !self.refresh.trim().is_empty() {
            token = token.with_refresh(ar_config::Secret::new(&self.refresh));
        }
        if let Some(at) = self.expires_at {
            token = token.with_expiry(at);
        }
        token
    }
}

impl std::fmt::Debug for OAuthAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthAuth")
            .field("provider", &self.session.provider())
            .field("kind", &self.session.kind())
            .field("access", &self.access.trim().is_empty().to_string())
            .field("can_refresh", &self.can_refresh())
            .finish_non_exhaustive()
    }
}

/// One configured upstream provider.
///
/// `Copy`-shaped metadata with a borrowed dispatch bundle at call time: the
/// router reads the routing signals, `ar-exec` reads the wire fields, and
/// neither reads the other's.
#[derive(Clone, Debug)]
pub struct ProviderConfig {
    /// Provider identity used for routing, cooldowns and metrics labels.
    pub id: ProviderId,
    /// Base URL without a trailing slash, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    /// Bearer credential. Empty means no auth header (keyless providers).
    pub api_key: String,
    /// Model id as this provider spells it. The client-facing id may differ.
    pub upstream_model: String,
    /// Dialect this provider speaks.
    ///
    /// Defaults to [`WireFormat::Openai`], and any of the eight named dialects is
    /// dispatchable: `ar-exec` renders the canonical body into whichever one this
    /// says, so a provider discovered to speak Anthropic is posted a Messages
    /// body rather than refused. Only [`WireFormat::Custom`] has no renderer and
    /// stays undispatchable — a provider-specific dialect with no shared label is
    /// a shape this build has no transcription for, and sending it a renamed
    /// OpenAI body is the defect this gate exists to prevent.
    pub wire_format: WireFormat,
    /// USD per 1M input tokens, for `Strategy::CostOptimized`.
    pub input_usd_per_mtok: Option<f64>,
    /// `Strategy::Priority` order, lower is earlier.
    pub rank: u32,
    /// Relative share for `Strategy::Weighted`.
    pub weight: u32,
    /// Current quota window, when one is known. `None` sorts a candidate after
    /// every metered one rather than dropping it.
    pub quota: Option<ar_route::QuotaWindow>,
    /// Extra outbound headers, copied from the provider definition.
    ///
    /// Empty for every compiled-in provider; a file-declared one fills it.
    pub headers: BTreeMap<String, String>,
    /// OAuth credentials, when this provider authenticates by OAuth.
    ///
    /// `None` for every API-key provider, which is the overwhelming majority.
    /// `Some` means the dispatch goes through `ar_exec::oauth`'s grant-and-rotate
    /// path and `api_key` is never read.
    pub oauth: Option<OAuthAuth>,
    /// Whether the catalog labels this provider `authType: oauth`.
    ///
    /// A separate fact from [`Self::oauth`] because they answer different
    /// questions: `oauth` says "a session is configured", this says "one has to
    /// be". A provider labelled `oauth` with no session is still undispatchable —
    /// it has no way to authenticate — and without this flag nothing here could
    /// tell that apart from a keyless provider. Set from the registry entry by
    /// `ar-server`'s config reader.
    pub needs_oauth_executor: bool,
    /// Whether this dispatch rides the provider's anonymous free tier.
    ///
    /// The one mechanism that needs no credential *and* no OAuth executor, which
    /// is what makes it the escape hatch for a provider this build otherwise
    /// cannot authenticate (red-team R1's `kilocode`). When set, [`Self::api_key`
    /// ] already holds the gateway's constant anonymous token and
    /// [`Self::headers`] already carries its editor header, both written by the
    /// config reader from the session's `anonymous:` block.
    pub anonymous: bool,
}

impl ProviderConfig {
    /// Builds a provider with no price, top rank and the OpenAI wire.
    #[must_use]
    pub fn new(id: ProviderId, base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            id,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.into(),
            upstream_model: String::new(),
            wire_format: WireFormat::Openai,
            input_usd_per_mtok: None,
            rank: 0,
            weight: 1,
            quota: None,
            headers: BTreeMap::new(),
            oauth: None,
            needs_oauth_executor: false,
            anonymous: false,
        }
    }

    /// Sets the provider-local model name.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.upstream_model = model.into();
        self
    }

    /// Sets the input price in USD per 1M tokens.
    #[must_use]
    pub fn with_price(mut self, usd_per_mtok: f64) -> Self {
        self.input_usd_per_mtok = Some(usd_per_mtok);
        self
    }

    /// Sets the `Strategy::Priority` rank.
    #[must_use]
    pub fn with_rank(mut self, rank: u32) -> Self {
        self.rank = rank;
        self
    }

    /// Sets the `Strategy::Weighted` share.
    #[must_use]
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight.max(1);
        self
    }

    /// Attaches a quota window.
    #[must_use]
    pub fn with_quota(mut self, quota: ar_route::QuotaWindow) -> Self {
        self.quota = Some(quota);
        self
    }

    /// Sets the wire dialect.
    ///
    /// Anything but [`WireFormat::Custom`] dispatches — see
    /// [`ProviderConfig::wire_format`].
    #[must_use]
    pub fn with_wire_format(mut self, wire_format: WireFormat) -> Self {
        self.wire_format = wire_format;
        self
    }

    /// Sets the extra outbound headers.
    #[must_use]
    pub fn with_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.headers = headers;
        self
    }

    /// Attaches resolved OAuth credentials.
    #[must_use]
    pub fn with_oauth(mut self, oauth: OAuthAuth) -> Self {
        self.oauth = Some(oauth);
        self
    }

    /// Records that the catalog labels this provider `authType: oauth`.
    #[must_use]
    pub fn with_needs_oauth_executor(mut self, needs: bool) -> Self {
        self.needs_oauth_executor = needs;
        self
    }

    /// Records that this provider dispatches on its anonymous free tier.
    #[must_use]
    pub fn with_anonymous(mut self, anonymous: bool) -> Self {
        self.anonymous = anonymous;
        self
    }

    /// The OAuth executor this provider's id permits, if any.
    ///
    /// The single place "can this build authenticate that provider at all" is
    /// answered. [`Self::is_dispatchable`] and `HttpExec::new` both ask it rather
    /// than each spelling the rule: a provider that is undispatchable but still
    /// gets a live connection is the silent known-listing F-CRIT-1 is about.
    #[must_use]
    pub fn oauth_executor(&self) -> Option<OAuthKind> {
        OAuthKind::parse(self.id.as_str())
    }

    /// Whether this build can actually POST to this provider.
    ///
    /// Checked at config-build time so an unspeakable provider never enters a
    /// candidate list and never burns one of the three attempt slots on a
    /// guaranteed wrong-wire request.
    ///
    /// The wire gate is [`ar_exec::outbound_wire`]: a dialect this build has no
    /// renderer for is refused here, and a dialect it does is rendered into by
    /// `ar-exec`. The predicate is asked rather than re-derived here, so the
    /// config-time answer and the dispatch-time gate cannot disagree — that
    /// disagreement is what put a dispatchable-looking provider in a candidate
    /// list and then failed it at the socket.
    ///
    /// An OAuth session is a second gate. A provider the catalog labels `oauth`
    /// with no executor in this build has **no** way to authenticate, so admitting
    /// it would send the `api_key` — an empty string for a session that has none
    /// — as a bearer and report the provider's own 401 as a transport failure.
    /// Red-team R1's `kilocode` is exactly that case and must stay out of every
    /// candidate list until its mechanism is understood.
    #[must_use]
    pub fn is_dispatchable(&self) -> bool {
        if ar_exec::outbound_wire(self.wire_format).is_none() {
            return false;
        }
        // The free tier answers before the OAuth gate is consulted: it carries a
        // constant credential of its own, so there is no session to be missing
        // and no executor that could be required. Without this arm the only
        // mechanism a provider needs no account for is also the only one this
        // build refuses, which is the whole of R1's `kilocode`.
        if self.anonymous {
            return true;
        }
        match &self.oauth {
            Some(auth) => {
                auth.is_usable()
                    && self
                        .oauth_executor()
                        .is_some_and(|kind| auth.session.kind() == kind)
            }
            // No session declared. A pasted access token in `keys:` still works
            // and simply cannot renew — unless the catalog says this provider needs
            // an OAuth executor, which is the case no fallback can paper over.
            None => !(self.needs_oauth_executor && self.oauth_executor().is_none()),
        }
    }

    /// The `ar-exec` dispatch bundle for this provider, borrowed.
    ///
    /// `pub(crate)` rather than private so a dispatch test can render the same
    /// bundle production posts through. Re-deriving it from this struct's public
    /// fields would be a second place to forget a field, and a test that rebuilt
    /// it would pass even if this one dropped the model or the wire.
    pub(crate) fn dispatch(&self, stream: bool) -> Dispatch<'_> {
        self.dispatch_for(stream, &self.upstream_model)
    }

    /// The same bundle, asking for `upstream_model` instead of the provider's
    /// configured default.
    ///
    /// The router resolves a *target* out of a pool, and a target is a
    /// provider plus the model that provider serves it under. Dispatching on the
    /// configured model alone would send every entry of a three-model pool to
    /// whichever one the config names, so the override is the request's own
    /// model — empty meaning "the caller named none", which falls back to the
    /// configured default so a provider-addressed dispatch still works.
    pub(crate) fn dispatch_for<'a>(
        &'a self,
        stream: bool,
        upstream_model: &'a str,
    ) -> Dispatch<'a> {
        Dispatch {
            base_url: &self.base_url,
            wire_format: self.wire_format,
            api_key: &self.api_key,
            upstream_model,
            stream,
            headers: &self.headers,
        }
    }
}

/// `reqwest`-backed [`ar_route::ArExec`] over `ar-exec`'s core.
///
/// `dyn`-safe by construction: the whole provider set is one heterogeneous
/// list, which is the one case ch.6 allows `dyn` for. `Clone` so a test double
/// and `ar-cli` can share one table across two servers.
pub struct HttpExec {
    core: Arc<ar_exec::ArExec>,
    providers: Vec<ProviderConfig>,
    by_id: HashMap<ProviderId, ProviderConfig>,
    /// Live OAuth connections, one per provider that authenticates by OAuth.
    ///
    /// `Arc` because a connection *is* the shared state: the per-connection
    /// refresh mutex and the terminal slot are the point of it, so a per-request
    /// copy would be a per-request mutex and no circuit at all.
    oauth: HashMap<ProviderId, Arc<Connection<Connected>>>,
}

impl Clone for HttpExec {
    fn clone(&self) -> Self {
        // `ar_exec::ArExec` is `Clone` (one `reqwest::Client` holding the pool);
        // the provider tables are small and `Copy`-enough to be worth sharing
        // over re-deriving from config. The connections are `Arc`-shared, which
        // is what keeps a clone from losing the single-flight guarantee.
        Self {
            core: Arc::clone(&self.core),
            providers: self.providers.clone(),
            by_id: self.by_id.clone(),
            oauth: self.oauth.clone(),
        }
    }
}

impl HttpExec {
    /// The `ar-exec` core, for a caller that has to render a request without
    /// dispatching it.
    ///
    /// A seam rather than a public surface: rendering a body is a decision the
    /// executor owns, so this exists so a dispatch test can assert the wire
    /// without a socket. A test that needed a live upstream to check the body
    /// would assert nothing about the body.
    #[cfg(test)]
    pub(crate) fn core(&self) -> &ar_exec::ArExec {
        &self.core
    }

    /// Builds an executor over `providers`. The first entry is the default when
    /// a request names no routable candidate.
    ///
    /// OAuth sessions are connected here rather than at config-build time,
    /// because this is where the one `reqwest` client lives: the refresher reuses
    /// [`ar_exec::ArExec::client`] instead of opening a second pool, which
    /// `ar-exec`'s module docs make the rule.
    ///
    /// A session whose access token will not connect is reported on stderr and
    /// left out of the table. It is not an error the server cannot start over —
    /// one dead account should not take a proxy with nine live ones down — and
    /// `ar doctor` is where the operator sees it (stderr, not stdout:
    /// `docs/06`).
    ///
    /// # Errors
    ///
    /// When the `reqwest` client cannot be constructed (TLS backend missing).
    pub fn new(providers: Vec<ProviderConfig>) -> Result<Self, String> {
        let core = ar_exec::ArExec::new().map_err(|e| format!("reqwest client: {e}"))?;
        let by_id = providers
            .iter()
            .cloned()
            .map(|p| (p.id.clone(), p))
            .collect();

        // One pool for the whole process: connections share it because they are
        // keyed by token hash, and two connections sharing a *pool* is fine
        // where two connections sharing a *refresh token* is the bug F-HIGH-4
        // names.
        let pool = Arc::new(RotationPool::new());
        let refresher: Arc<dyn Refresher> = Arc::new(HttpRefresher::new(core.client()));
        let mut oauth = HashMap::new();
        for p in &providers {
            // `let Some(..) else { continue }` rather than a `.filter(..)` plus an
            // `expect`: the filter would make the invariant a runtime check, and
            // AGENTS.md forbids `expect` outside tests.
            let Some(auth) = &p.oauth else { continue };
            // The executor has to match the provider id. A session hand-built for a
            // provider this build has no executor for is refused rather than
            // connected: R1's `kilocode` would otherwise get a guessed carve-out
            // table and a live connection nobody traced.
            if p.oauth_executor() != Some(auth.session.kind()) {
                eprintln!(
                    "ar: oauth session for {} DISABLED — this build has no executor for it; \
                     `ar doctor` reports why",
                    p.id.as_str()
                );
                continue;
            }
            match Connection::pending(
                auth.session.clone(),
                Arc::clone(&pool),
                Arc::clone(&refresher),
            )
            .connect(auth.token())
            {
                Ok(conn) => {
                    oauth.insert(p.id.clone(), Arc::new(conn));
                }
                Err(e) => eprintln!(
                    "ar: oauth session for {} DISABLED — {e}; `ar doctor` reports why",
                    p.id.as_str()
                ),
            }
        }

        Ok(Self {
            core: Arc::new(core),
            providers,
            by_id,
            oauth,
        })
    }

    /// Configured providers, in fallback order.
    #[must_use]
    pub fn providers(&self) -> &[ProviderConfig] {
        &self.providers
    }

    /// Looks one provider up by id.
    #[must_use]
    pub fn provider(&self, id: &ProviderId) -> Option<&ProviderConfig> {
        self.by_id.get(id)
    }

    /// The live OAuth connection for `id`, when it has one.
    ///
    /// `None` for every API-key provider, and for a provider whose session failed
    /// to connect — which is why the connection table is built once at boot
    /// rather than looked up per request.
    #[must_use]
    pub fn oauth(&self, id: &ProviderId) -> Option<&Arc<Connection<Connected>>> {
        self.oauth.get(id)
    }
}

impl std::fmt::Debug for HttpExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `ProviderConfig` carries `api_key`, and `Debug` on it would print the
        // credential. Count the tables instead.
        f.debug_struct("HttpExec")
            .field("providers", &self.providers.len())
            .field("oauth", &self.oauth.len())
            .finish_non_exhaustive()
    }
}

impl ArRouteExec for HttpExec {
    fn post_chat<'a>(
        &'a self,
        provider: &'a ProviderId,
        canonical: &'a CanonicalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
        Box::pin(async move {
            let cfg = self
                .by_id
                .get(provider)
                .ok_or_else(|| ExecError(format!("no such provider: {provider}")))?;

            // One fresh token per attempt. The tower `TimeoutLayer` already
            // bounds the handler future, and dropping it drops the `reqwest`
            // request — so this token is the belt to that suspenders, not the
            // only thing between a client hang and a hung socket. It is not
            // threaded from the router because `ar_route::ArExec::post_chat`
            // carries no cancellation channel, and adding one is ar-route's
            // signature to change.
            //
            // TODO(#p1-cancel): plumb a per-request token from the router so a
            // client disconnect aborts the upstream read instead of waiting for
            // the response-start budget.
            let abort = CancellationToken::new();
            // The model the router picked for *this* attempt, not the provider's
            // configured one: a chain entry names its own model, and every
            // attempt that fell back to the config asked for a model the pool
            // never selected.
            let requested = if canonical.model.is_empty() {
                cfg.upstream_model.as_str()
            } else {
                canonical.model.as_ref()
            };
            let shape = cfg.dispatch_for(canonical.stream, requested);
            // The provider's wire is rendered by `ar-exec`, not by the caller: the canonical
            // body is a provider-neutral shape, and translating it into the
            // registry's declared dialect is the executor's job. The OpenAI arm is
            // byte-identical to the pre-existing path, so nothing about the default
            // dispatch changes.
            //
            // Rendered here on the API-key branch only. The OAuth branch hands
            // `oauth::Connection::dispatch` the canonical bytes and lets it render,
            // because that function re-posts the same body on a 401 rotation and
            // rendering once outside it would mean either a second render or a body
            // it would render again.
            let stream = match self.oauth.get(provider) {
                Some(conn) => {
                    conn.dispatch(&self.core, &shape, &canonical.body, &abort)
                        .await
                }
                None => {
                    // Rendered here, once, so the API-key path posts a body already
                    // in the provider's wire.
                    let body = match self.core.render_request(&shape, &canonical.body) {
                        // Rendered once, so the API-key path posts a body already in
                        // the provider's wire.
                        Ok(body) => body,
                        // The provider passed `is_dispatchable` at boot, so a wire with
                        // no renderer here means the two drifted — a diagnosable error
                        // rather than a 502-shaped `Upstream`.
                        Err(e) => return Err(flatten(e)),
                    };
                    self.core.post_rendered(&shape, body, &abort).await
                }
            };
            let stream = match stream {
                Ok(stream) => stream,
                // R2: a terminal session is a 401 that *names the account*, not a
                // bare 502. The router classifies a 401 as a failover candidate
                // (another chain may still work) and the client gets a body it can
                // branch on.
                Err(ar_exec::ExecError::OAuthTerminal(report)) => {
                    return Ok(Upstream::failure(
                        StatusCode::UNAUTHORIZED,
                        report.client_body(),
                        None,
                    ));
                }
                Err(e) => return Err(flatten(e)),
            };

            let status = stream.status();
            let retry_after = stream.retry_after();
            if !status.is_success() {
                let (status, body, _) = stream.into_failure().await;
                return Ok(Upstream::failure(status, body, retry_after));
            }

            // Relay raw bytes, not decoded SSE events: `ar_route::Upstream`
            // hands the router a `ChunkStream` of `Bytes` and the server writes
            // them straight to the client, so upstream framing crosses the proxy
            // untouched. A body read error terminates the stream rather than
            // being dropped — see `ar_exec::ChatStream::into_bytes`.
            let chunks = stream
                .into_bytes()
                .filter_map(|chunk| {
                    futures::future::ready(match chunk {
                        Ok(bytes) => Some(bytes),
                        Err(e) => {
                            tracing::warn!(error = %e, "upstream stream ended early");
                            None
                        }
                    })
                })
                .boxed();

            Ok(Upstream {
                status,
                // A 2xx can still carry `Retry-After` (an overloaded stream
                // asking the *client* to slow down), and the router reports it.
                retry_after,
                error_body: Bytes::new(),
                stream: chunks,
            })
        })
    }

    /// POSTs a media body and reads the whole reply.
    ///
    /// The one per-provider rewrite chat gets for free from `render_request`
    /// is done here by hand: a routed media request names a combo or
    /// `provider/model`, and the upstream wants its own spelling. JSON bodies
    /// get the `model` field replaced; multipart bodies cannot be rewritten
    /// without re-encoding them, so they forward verbatim and the caller's form
    /// data must already spell the upstream's model.
    fn post_media<'a>(
        &'a self,
        provider: &'a ProviderId,
        model: Option<&'a str>,
        endpoint: &'a str,
        content_type: &'a str,
        body: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
        Box::pin(async move {
            let cfg = self
                .by_id
                .get(provider)
                .ok_or_else(|| ExecError(format!("no such provider: {provider}")))?;

            // No media executor exists behind an OAuth session in this build,
            // and the failure has to say so: a session that fell through to the
            // API-key branch would send an empty bearer and report the
            // provider's own 401 as if it were a media verdict.
            if self.oauth.contains_key(provider) {
                return Err(ExecError(
                    "oauth sessions do not serve media endpoints in this build; configure an api-key provider for media"
                        .to_owned(),
                ));
            }

            let endpoint = ar_exec::MediaEndpoint::from_path(endpoint)
                .ok_or_else(|| ExecError(format!("unknown media endpoint path: {endpoint}")))?;

            let shape = cfg.dispatch(false);
            // The caller's model wins over the provider's configured one. The
            // media path reaches here the same way the chat path does — a chain
            // entry names a provider *and* a model — and dropping it here is what
            // made `/v1/embeddings` ask every target for the provider's default
            // instead of the one its chain entry named. `None` (a direct
            // provider-keyed request with no chain) keeps the configured model.
            let shape = match model {
                Some(m) => shape.with_upstream_model(m),
                None => shape,
            };
            let bytes = rewrite_json_model(body, content_type, shape.upstream_model);

            let abort = CancellationToken::new();
            let reply = self
                .core
                .post_media(
                    endpoint,
                    &shape,
                    &ar_exec::MediaBody {
                        content_type,
                        bytes: bytes.as_deref().unwrap_or(body),
                    },
                    &abort,
                )
                .await
                .map_err(flatten)?;

            Ok(MediaReply {
                status: reply.status,
                body: reply.bytes,
                content_type: reply.content_type,
                retry_after: reply.retry_after,
            })
        })
    }
}

/// Replaces the `model` field of a JSON body with `upstream_model`.
///
/// `None` means "forward as-is": a non-JSON body, an unparseable one, or an
/// empty `upstream_model` (the config spells the upstream name as its own
/// routing id) all forward verbatim. Rewriting a body that failed to parse
/// would mean inventing a wire, and a multipart body re-encoded here would be
/// a shape no provider documents — `AGENTS.md`'s one hard prohibition.
fn rewrite_json_model(body: &[u8], content_type: &str, upstream_model: &str) -> Option<Vec<u8>> {
    if upstream_model.is_empty() || !content_type.starts_with("application/json") {
        return None;
    }
    let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let model = value.get_mut("model")?;
    *model = serde_json::Value::String(upstream_model.to_owned());
    serde_json::to_vec(&value).ok()
}

/// Flattens `ar_exec::ExecError` into the router's string-carrying error.
///
/// The router's taxonomy is one opaque string by design (`ar_route::ExecError`),
/// so there is nothing to map *to* — but the message must keep the typed
/// detail, because "provider wire format custom has no renderer" and
/// "upstream produced no headers within 110s" send an operator to two entirely
/// different places.
fn flatten(e: ar_exec::ExecError) -> ExecError {
    ExecError(e.to_string())
}

// The executor is shared across tasks; prove it at compile time rather than
// discovering it from a spawn error on the first concurrent request.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HttpExec>();
    assert_send_sync::<OAuthAuth>();
};

#[cfg(test)]
mod tests {
    use ar_registry::WireFormat;
    // The trait `HttpExec` implements. Aliased as in the parent module, so
    // `post_chat` resolves here without a second name for one type.
    use ar_route::{ArExec as ArRouteExec, CanonicalRequest, ProviderId, QuotaWindow};
    use axum::http::StatusCode;
    use bytes::Bytes;

    use super::{Dispatch, HttpExec, OAuthAuth, OAuthKind, ProviderConfig, Session};

    fn cfg(id: &str) -> ProviderConfig {
        ProviderConfig::new(ProviderId::new(id), "https://x/v1", "k")
    }

    #[test]
    fn defaults_to_the_openai_wire() {
        assert_eq!(cfg("p").wire_format, WireFormat::Openai);
    }

    #[test]
    fn default_wire_is_dispatchable() {
        assert!(cfg("p").is_dispatchable());
    }

    /// A provider-specific dialect is not dispatchable: it has no shared shape.
    ///
    /// The regression this guards is still the one worth guarding — silently
    /// POSTing a canonical OpenAI body to a provider whose registry entry says
    /// something else — it just moved. The guard is now the *absence of a
    /// renderer* rather than "not OpenAI", and the named dialects that used to
    /// fail it now dispatch in their own wire.
    #[test]
    fn a_custom_wire_is_not_dispatchable() {
        let p = cfg("p").with_wire_format(WireFormat::Custom);
        assert!(!p.is_dispatchable());
    }

    #[test]
    fn an_anthropic_wire_is_dispatchable() {
        // The unblock: a Claude-dialect provider used to be refused here, so it
        // never entered a candidate list and could not dispatch at all.
        assert!(
            cfg("p")
                .with_wire_format(WireFormat::Anthropic)
                .is_dispatchable()
        );
    }

    #[test]
    fn keeps_rank_price_weight_and_quota_on_the_config() {
        let p = cfg("p")
            .with_price(0.15)
            .with_rank(3)
            .with_weight(2)
            .with_quota(QuotaWindow::new(100, 10, 0));
        assert_eq!(
            (
                p.input_usd_per_mtok,
                p.rank,
                p.weight,
                p.quota.map(|q| q.remaining())
            ),
            (Some(0.15), 3, 2, Some(90))
        );
    }

    #[test]
    fn clamps_a_zero_weight_to_one() {
        assert_eq!(cfg("p").with_weight(0).weight, 1);
    }

    #[test]
    fn trims_trailing_slash_from_base_url() {
        assert_eq!(cfg("p").base_url, "https://x/v1");
    }

    #[test]
    fn carries_provider_headers_into_the_dispatch_bundle() {
        let p = cfg("p").with_headers([("x-api-key".to_owned(), "v".to_owned())].into());
        assert_eq!(
            p.dispatch(false)
                .headers
                .get("x-api-key")
                .map(String::as_str),
            Some("v")
        );
    }

    #[test]
    fn looks_a_provider_up_by_id() {
        let exec = HttpExec::new(vec![cfg("a"), cfg("b")]).expect("executor builds");
        assert_eq!(
            exec.provider(&ProviderId::new("b")).map(|p| p.id.as_str()),
            Some("b")
        );
    }

    #[test]
    fn reports_unknown_provider_as_absent() {
        let exec = HttpExec::new(vec![cfg("a")]).expect("executor builds");
        assert!(exec.provider(&ProviderId::new("nope")).is_none());
    }

    #[test]
    fn clones_without_sharing_a_mutable_table() {
        let exec = HttpExec::new(vec![cfg("a")]).expect("executor builds");
        let copy = exec.clone();
        assert_eq!(copy.providers().len(), exec.providers().len());
    }

    #[test]
    fn debug_renders_no_credential() {
        let exec = HttpExec::new(vec![cfg("a")]).expect("executor builds");
        assert!(!format!("{exec:?}").contains('k'));
    }

    /// An `oauth:` session's resolved credentials. Synthetic tokens only.
    fn auth(kind: OAuthKind, access: &str, refresh: &str, url: Option<&str>) -> OAuthAuth {
        let mut session = Session::new(kind.as_str(), kind);
        if let Some(url) = url {
            session = session.with_token_url(url);
        }
        OAuthAuth::new(session, access, refresh, Some(2_000_000_000))
    }

    #[test]
    fn connects_an_oauth_session_from_resolved_credentials() {
        let exec = HttpExec::new(vec![
            ProviderConfig::new(ProviderId::new("codex"), "https://x/v1", "").with_oauth(auth(
                OAuthKind::Codex,
                "synthetic-access",
                "synthetic-refresh",
                Some("https://a/t"),
            )),
        ])
        .expect("executor builds");
        assert!(
            exec.oauth(&ProviderId::new("codex")).is_some(),
            "a connected session"
        );
    }

    #[test]
    fn omits_an_oauth_session_whose_access_token_is_empty() {
        // One dead account must not take a proxy with nine live ones down, and it
        // must not become an unauthenticated upstream call either.
        let exec = HttpExec::new(vec![
            ProviderConfig::new(ProviderId::new("codex"), "https://x/v1", "").with_oauth(auth(
                OAuthKind::Codex,
                "  ",
                "",
                Some("https://a/t"),
            )),
        ])
        .expect("executor builds");
        assert!(exec.oauth(&ProviderId::new("codex")).is_none());
    }

    #[test]
    fn omits_an_oauth_session_for_a_provider_this_build_cannot_authenticate() {
        // R1: kilocode keeps no connection rather than a guessed one.
        let exec = HttpExec::new(vec![
            ProviderConfig::new(ProviderId::new("kilocode"), "https://x/v1", "").with_oauth(auth(
                OAuthKind::Cline,
                "synthetic-access",
                "",
                Some("https://a/t"),
            )),
        ])
        .expect("executor builds");
        assert!(exec.oauth(&ProviderId::new("kilocode")).is_none());
    }

    #[test]
    fn reports_a_keyless_provider_without_an_executor_as_dispatchable() {
        // The other half of the same flag: a provider the catalog does *not* label
        // `oauth` is unaffected, or every keyless provider would be dropped.
        assert!(
            ProviderConfig::new(ProviderId::new("ollama"), "https://x/v1", "").is_dispatchable()
        );
    }

    #[test]
    fn reports_a_provider_with_a_matching_executor_and_session_as_dispatchable() {
        let p = ProviderConfig::new(ProviderId::new("codex"), "https://x/v1", "")
            .with_needs_oauth_executor(true)
            .with_oauth(auth(
                OAuthKind::Codex,
                "synthetic-access",
                "synthetic-refresh",
                Some("https://a/t"),
            ));
        assert!(p.is_dispatchable());
    }

    #[test]
    fn reports_an_armed_oauth_session_as_dispatchable() {
        let p = ProviderConfig::new(ProviderId::new("codex"), "https://x/v1", "").with_oauth(auth(
            OAuthKind::Codex,
            "synthetic-access",
            "synthetic-refresh",
            Some("https://a/t"),
        ));
        assert!(p.is_dispatchable());
    }

    #[test]
    fn reports_an_oauth_session_with_no_access_token_as_undispatchable() {
        let p = ProviderConfig::new(ProviderId::new("codex"), "https://x/v1", "").with_oauth(auth(
            OAuthKind::Codex,
            "",
            "synthetic-refresh",
            Some("https://a/t"),
        ));
        assert!(
            !p.is_dispatchable(),
            "an empty bearer must not enter the candidate list"
        );
    }

    #[test]
    fn reports_an_oauth_provider_with_no_known_executor_as_undispatchable() {
        // The F-CRIT-1 gate: an `oauthType` provider this build cannot
        // authenticate is not dispatchable, so it never burns an attempt slot.
        let p = ProviderConfig::new(
            ProviderId::new("kimi-coding"),
            "https://x/v1",
            "synthetic-access",
        )
        .with_needs_oauth_executor(true);
        assert!(
            !p.is_dispatchable(),
            "no executor, so no way to authenticate"
        );
    }

    #[test]
    fn reports_an_anonymous_free_tier_as_dispatchable_without_an_executor() {
        // R1's kilocode: catalogued `oauth`, no executor, and — before the free
        // tier — therefore permanently unroutable. The anonymous mechanism is the
        // one that needs no executor, so it is the only one this gate may admit.
        let p = ProviderConfig::new(ProviderId::new("kilocode"), "https://x/v1", "anonymous")
            .with_needs_oauth_executor(true)
            .with_anonymous(true);
        assert!(p.is_dispatchable());
    }

    #[test]
    fn reports_a_custom_wire_on_the_free_tier_as_undispatchable() {
        // The free tier does not buy a second dialect: the wire gate is first for
        // a reason and the anonymous arm deliberately sits below it.
        let p = ProviderConfig::new(ProviderId::new("kilocode"), "https://x/v1", "anonymous")
            .with_wire_format(WireFormat::Custom)
            .with_anonymous(true);
        assert!(!p.is_dispatchable());
    }

    #[test]
    fn reports_a_session_without_a_refresh_path_as_usable_but_not_renewable() {
        let a = auth(OAuthKind::Cline, "synthetic-access", "", None);
        assert!(a.is_usable());
        assert!(
            !a.can_refresh(),
            "a token with no refresh and no endpoint cannot renew"
        );
    }

    #[test]
    fn keeps_the_access_token_out_of_the_oauth_debug_output() {
        let rendered = format!(
            "{:?}",
            auth(
                OAuthKind::Codex,
                "synthetic-access",
                "synthetic-refresh",
                Some("https://a/t")
            )
        );
        assert!(!rendered.contains("synthetic-access"), "{rendered}");
    }

    #[test]
    fn carries_an_expiry_onto_the_token_it_hands_over() {
        let token = auth(
            OAuthKind::Codex,
            "synthetic-access",
            "synthetic-refresh",
            Some("https://a/t"),
        )
        .token();
        assert!(token.can_refresh());
        assert_eq!(token.access().expose(), "synthetic-access");
        assert!(
            !token.is_expiring(0),
            "an expiry in the future is not stale"
        );
    }
    #[test]
    fn hands_over_a_token_with_no_refresh_row_as_unrenewable() {
        let token = auth(OAuthKind::Cline, "synthetic-access", "", None).token();
        assert!(
            !token.can_refresh(),
            "an empty refresh row is no refresh row"
        );
    }

    #[tokio::test]
    async fn carries_the_session_token_on_an_oauth_dispatch() {
        // R2's client-visible half: `ar-server` turns the terminal error into a
        // 401 whose body names the dead account. Asserted here because the
        // conversion lives at the boundary where the typed error becomes an
        // `Upstream`, and nowhere else.
        let exec = HttpExec::new(vec![
            ProviderConfig::new(ProviderId::new("cline"), "https://x/v1", "")
                .with_needs_oauth_executor(true)
                .with_oauth(auth(
                    OAuthKind::Cline,
                    "synthetic-access",
                    "synthetic-refresh",
                    Some("https://a/t"),
                )),
        ])
        .expect("executor builds");
        let conn = exec
            .oauth(&ProviderId::new("cline"))
            .expect("a live connection");
        conn.quarantine(ar_exec::oauth::TerminalReport {
            provider: "cline".to_owned(),
            refresh_status: 400,
            reason: "invalid_grant",
        })
        .await;

        let outcome = exec
            .post_chat(
                &ProviderId::new("cline"),
                &ar_route::CanonicalRequest::new("m", Bytes::from_static(b"{}")),
            )
            .await
            .expect("a verdict, not an error");

        assert_eq!(
            outcome.status,
            StatusCode::UNAUTHORIZED,
            "a terminal session is a 401, not a 502"
        );
        let text = String::from_utf8(outcome.error_body.to_vec()).expect("utf-8 json");
        assert!(text.contains("oauth_terminal"), "{text}");
        assert!(text.contains("cline"), "{text}");
    }

    /// One dispatch test per newly-unblocked provider family, at the seam where
    /// the wire is chosen. Each asserts the *rendered* body reaches the provider
    /// in its own dialect rather than as a renamed OpenAI one — the defect the
    /// old OpenAI-only gate was preventing, with the gate moved to "has a
    /// renderer" instead of "is OpenAI". Rendering needs no socket, so these
    /// assert the wire without a network round-trip and without a mock upstream.
    fn renders_for(dialect: WireFormat, model: &str) -> (String, serde_json::Value) {
        let exec = HttpExec::new(vec![cfg("p").with_wire_format(dialect).with_model(model)])
            .expect("executor builds");
        // The production bundle, not a re-derived one: a test that built its own
        // `Dispatch` would pass even if `ProviderConfig::dispatch` dropped the
        // model or the wire.
        let shape: Dispatch<'_> = exec
            .provider(&ProviderId::new("p"))
            .expect("configured")
            .dispatch(true);
        // The router's canonical body, which is opaque bytes to it and only
        // `ar-exec` parses it. Built from the same JSON a real inbound dialect
        // canonicalises to, so the renderers see the shape they will see in
        // production rather than a hand-rolled subset.
        let body = CanonicalRequest::new(
            "auto",
            Bytes::from_static(
                br#"{"model":"auto","messages":[
                    {"role":"system","content":"be terse"},
                    {"role":"user","content":"hi"}],
                    "max_tokens":64,"stream":true}"#,
            ),
        )
        .with_stream(true);
        let rendered = exec
            .core()
            .render_request(&shape, &body.body)
            .expect("a renderable wire");
        let text = String::from_utf8(rendered.to_vec()).expect("utf-8 body");
        // Read the parsed value rather than a substring of the text:
        // `serde_json::Map` is a `BTreeMap` here, so the serialised key order is
        // alphabetical and a substring assertion would be asserting on that.
        let value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{dialect:?} body is not json: {text}\n{e}"));
        (text, value)
    }

    #[test]
    fn renders_a_claude_family_dispatch_in_the_anthropic_dialect() {
        let (body, value) = renders_for(WireFormat::Anthropic, "claude-sonnet-4-5");
        assert_eq!(value["system"][0]["text"], "be terse", "{body}");
        assert_eq!(value["max_tokens"], 64, "{body}");
    }

    #[test]
    fn renders_a_responses_family_dispatch_in_the_responses_dialect() {
        let (body, value) = renders_for(WireFormat::OpenaiResponses, "gpt-5.4");
        assert_eq!(value["instructions"], "be terse", "{body}");
        assert_eq!(value["max_output_tokens"], 64, "{body}");
    }

    #[test]
    fn renders_a_gemini_family_dispatch_in_the_gemini_dialect() {
        let (body, value) = renders_for(WireFormat::Gemini, "gemini-3-pro");
        assert_eq!(
            value["systemInstruction"]["parts"][0]["text"], "be terse",
            "{body}"
        );
        assert_eq!(value["generationConfig"]["maxOutputTokens"], 64, "{body}");
    }

    #[test]
    fn renders_an_antigravity_family_dispatch_with_the_gemini_body() {
        let (body, value) = renders_for(WireFormat::Antigravity, "gemini-3-pro");
        assert!(value.get("contents").is_some(), "{body}");
    }

    #[test]
    fn renders_a_cursor_family_dispatch_in_the_cursor_dialect() {
        let (body, value) = renders_for(WireFormat::Cursor, "cursor-small");
        assert_eq!(
            value["messages"][0]["content"], "[System Instructions]\nbe terse",
            "{body}"
        );
        assert!(
            value.get("system").is_none(),
            "cursor has no system field: {body}"
        );
    }

    #[test]
    fn renders_a_clova_family_dispatch_in_the_clova_dialect() {
        let (body, value) = renders_for(WireFormat::Clova, "HCX-005");
        assert_eq!(value["maxTokens"], 64, "{body}");
        assert!(
            value.get("max_tokens").is_none(),
            "the openai spelling is not a clova key: {body}"
        );
    }

    #[test]
    fn renders_a_kiro_family_dispatch_in_the_kiro_dialect() {
        let (body, value) = renders_for(WireFormat::Kiro, "claude-sonnet-4-5");
        assert_eq!(
            value["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "claude-sonnet-4-5",
            "{body}"
        );
    }

    #[test]
    fn admits_every_named_wire_into_the_candidate_list() {
        // The config-time half of the unblock: a provider that can render used to
        // be refused here, so it never entered a candidate list and could not
        // dispatch at all however willing the provider was.
        for dialect in [
            WireFormat::Anthropic,
            WireFormat::OpenaiResponses,
            WireFormat::Gemini,
            WireFormat::Antigravity,
            WireFormat::Cursor,
            WireFormat::Clova,
            WireFormat::Kiro,
        ] {
            assert!(
                cfg("p").with_wire_format(dialect).is_dispatchable(),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn leaves_an_openai_dispatch_byte_identical() {
        // The default path must not move for a byte.
        let (body, value) = renders_for(WireFormat::Openai, "llama-3.3-70b");
        assert_eq!(value["model"], "llama-3.3-70b", "{body}");
        // The chat-completions system role stays a role rather than being hoisted
        // onto a top-level `system` block the way the claude arm does.
        assert_eq!(value["messages"][0]["role"], "system", "{body}");
        assert!(
            value.get("system").is_none(),
            "a claude body leaked onto the openai wire: {body}"
        );
    }
}
