//! axum 0.8 router and its tower layers.
//!
//! Layer order is outermost-first as written below: CORS runs first so a
//! preflight is answered from the headers alone and never reaches a layer that
//! would want to read a body, trace-id runs next so every later layer's log line
//! carries the id, the body limit runs before the handler reads a byte, and the
//! timeout wraps the handler future.
//!
//! The timeout deliberately bounds the *request*, not the response body.
//! `TimeoutLayer::new` resolves when the handler returns a `Response`, which for
//! a streamed completion is as soon as upstream headers arrive — so a 20-minute
//! generation is fine while a 20-minute *hang* is a 408. Wrapping the body
//! instead (`.map_response_body()`) would cut every long answer off at 120s.
//!
//! It is set to [`ServerConfig::max_deadline`] rather than [`REQUEST_TIMEOUT`]:
//! the layer is a per-router constant, so it has to be the widest deadline the
//! config asks for, and the per-model narrowing happens one layer in
//! ([`crate::routes::handle_chat`]) where the model is known. A model-aware
//! answer that a wider layer cut off before it could fire would be the wrong
//! order — and one narrower than the layer would make the per-model value
//! unreachable, so the layer is never narrower than [`REQUEST_TIMEOUT`] either.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use ar_route::{ArExec, LkgpPins, Resilience};

use crate::config::ServerConfig;
use crate::keys::AuthGate;
use crate::media;
use crate::metrics::{Metrics, Outcome};
use crate::models::{ModelsCache, StaticCatalog};
use crate::routes;

/// Request body ceiling. A chat request is kilobytes; 2MB is a client's runaway
/// loop, and refusing it at the edge is cheaper than buffering it. Checked here
/// rather than by the handler because a handler has already paid the allocation.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Request body ceiling on the media routes. Audio uploads are the whole point
/// of `/v1/audio/transcriptions` and run megabytes by nature, so the chat cap
/// cannot govern them; 25MB is the reference platform's own documented upload
/// limit for the same endpoint, which is the one number a client can already
/// be relying on. Applied on a sub-router merged before the shared layers, so
/// chat keeps its own tighter ceiling.
pub const MEDIA_BODY_BYTES: usize = 25 * 1024 * 1024;

/// Ceiling on time-to-response-headers. Deliberately generous: it is a
/// backstop against a dead upstream, not a latency budget.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Trace id echoed to the client and attached to every span.
pub const TRACE_HEADER: &str = "x-ar-trace-id";

/// Methods a preflight may ask for, from the reference gateway's
/// `Access-Control-Allow-Methods` (`src/shared/utils/cors.ts:12`).
///
/// The full verb set rather than the three this server registers: a preflight
/// names the methods it *intends* to use, and answering 204 to a preflight for
/// a verb this server has no route for is correct — the follow-up request gets
/// the 405. Narrowing the list to the registered verbs instead produces a
/// preflight failure the client reports as a CORS error, which is a worse
/// message about a request that was never going to succeed.
///
/// The reference deliberately does not set `Access-Control-Allow-Origin` here and
/// leaves that to a central allowlist in its middleware. This crate has no such
/// configuration, so [`cors`] echoes the origin instead — see its docs for what
/// that does and does not claim.
pub const CORS_METHODS: &str = "GET, POST, PUT, DELETE, PATCH, OPTIONS";

/// Request headers a preflight may name, from the same file (line 14).
///
/// Every header this server's own credential matrix and dialect handling read,
/// plus the two the reference gateway admits for its own control plane
/// (`x-omniroute-connection` and the lease pair) so a client configured against
/// OmniRoute does not fail its preflight against a header this server ignores.
/// A preflight is a question about what *may* be sent, so over-answering costs
/// nothing and under-answering breaks a browser for no reason.
///
/// `x-goog-api-key` is here for the same reason `x-api-key` is: it is a credential
/// slot [`crate::keys::extract_credential`] reads, and a preflight that omitted it
/// would fail in the browser before the request ever reached the matrix.
pub const CORS_HEADERS: &str = "Content-Type, Authorization, x-api-key, x-goog-api-key, \
     anthropic-version, x-ar-compression, x-omniroute-compression, x-ar-session, accept, \
     user-agent, x-omniroute-connection, X-OmniRoute-Lease-Owner, X-OmniRoute-Lease-Generation, \
     x-internal-test";

/// Shared server state. `Arc`-ed once by `app()` and cloned per request by
/// axum's `State`, so handlers never re-allocate the routing tables.
///
/// `Clone` rather than an `Arc`-of-`Arc`: axum's `State` extractor wants an owned
/// handle per request and a second `Arc` layer would buy nothing.
#[derive(Clone)]
pub struct AppState {
    /// Resolved configuration.
    pub config: Arc<ServerConfig>,
    /// The provider executor. `dyn` because the provider list is heterogeneous
    /// and defined at runtime (ch.6).
    pub exec: Arc<dyn ArExec>,
    /// The single per-key resilience layer.
    pub resilience: Arc<Resilience>,
    /// Session-keyed last-known-good providers.
    pub lkgp: Arc<LkgpPins>,
    /// Round-robin cursor. One atomic, no lock (ch.1/3).
    pub rr: Arc<AtomicU64>,
    /// Counters for `/metrics`.
    pub metrics: Arc<Metrics>,
    /// `/v1/models` cache, revalidated every 60s.
    pub models: Arc<ModelsCache<'static>>,
    /// Exact response cache, when one is configured.
    ///
    /// `None` means no cache at all, which `x-ar-cache` then reports as `miss`
    /// rather than as a `bypass` the client has to interpret.
    pub cache: Option<Arc<ar_cache::Cache>>,
    /// Bearer gate, when one is configured.
    ///
    /// `None` is the default and means every request is anonymous — which is only
    /// safe because [`crate::config::ServerConfig::public`] then holds the bind to
    /// loopback. See [`bind_addr`] for the other half of that pair.
    pub auth: Option<Arc<AuthGate>>,
    /// How the gate treats a request with no usable credential.
    ///
    /// Carried on the state rather than read from `config` at the call site so
    /// the mode and the gate it modifies are always the pair the operator
    /// configured: a gate whose mode lives somewhere else is a mode that can
    /// disagree with it.
    pub auth_mode: crate::config::AuthMode,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("port", &self.config.port)
            .field("strategy", &self.config.strategy)
            .field("providers", &self.config.providers.len())
            .field("combos", &self.config.combos.len())
            .field("cache", &self.cache.is_some())
            .field("auth", &self.auth.is_some())
            .field("auth_mode", &self.auth_mode)
            .field("public", &self.config.public)
            .finish_non_exhaustive()
    }
}

/// Everything needed to serve: config, executor, and the caches that hang off
/// them.
///
/// Constructed with a struct literal so a caller names only what it changes;
/// `..Components::unconfigured(config)` fills the rest with "no cache, no bearer
/// gate", which is the safe direction for both.
pub struct Components {
    /// Resolved configuration.
    pub config: ServerConfig,
    /// The provider executor.
    pub exec: Arc<dyn ArExec>,
    /// Extra models beyond what the provider chain declares, for a test double
    /// or a hand-written catalog entry.
    pub extra_models: Vec<crate::models::ModelCard>,
    /// Byte ceiling for the memory cache tier. `Some(0)` disables the cache.
    ///
    /// `None` means the `ar-cache` default (32 MB). The cap is a `Components`
    /// field rather than a constant because a test needs a cache whose behaviour
    /// it can predict, and 32 MB of headroom changes nothing about one.
    pub cache_bytes: Option<Option<u64>>,
    /// Master key bytes for the bearer gate. `None` disables the gate.
    ///
    /// Overrides [`ServerConfig::http_master_key`] when set, which is what makes
    /// it a `Components` field at all: a caller that already holds key material
    /// (a test minting a token through this very gate) should not have to push
    /// it through a config it does not otherwise care about. A config already
    /// carries the env-derived one for every shipped path, so leaving this
    /// `None` is the ordinary case and not an omission.
    pub master_key: Option<Vec<u8>>,
}
impl Components {
    /// The minimal configuration: no cache, no bearer gate, no extra models, and
    /// a `NullExec` that refuses every dispatch.
    ///
    /// Every field is public so a caller that wants a cache says
    /// `cache_bytes: Some(Some(1 << 20))` rather than reaching for a builder that
    /// exists only to hide this. The gate defaults off because the gate is a
    /// *deployment* decision and the ordinary install has not made one — see
    /// [`crate::config::HTTP_MASTER_KEY_VAR`].
    #[must_use]
    pub fn unconfigured(config: ServerConfig) -> Self {
        Self {
            config,
            exec: Arc::new(NullExec),
            extra_models: Vec::new(),
            cache_bytes: Some(Some(0)),
            master_key: None,
        }
    }

    /// Built with an executor and nothing else: the shape `ar serve` and `ar run`
    /// both use, so both inherit the gate and the per-model deadlines from the
    /// config rather than having to name either themselves.
    #[must_use]
    pub fn with_exec(config: ServerConfig, exec: Arc<dyn ArExec>) -> Self {
        Self {
            exec,
            ..Self::unconfigured(config)
        }
    }
}

impl std::fmt::Debug for Components {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `master_key` is a credential; `ProviderConfig` also carries `api_key`.
        // Neither has a redacting `Debug`, so nothing here prints them.
        f.debug_struct("Components")
            .field("providers", &self.config.providers.len())
            .field("combos", &self.config.combos.len())
            .field("cache_bytes", &self.cache_bytes)
            .field("master_key", &self.master_key.as_ref().map(|_| "set"))
            .finish_non_exhaustive()
    }
}

impl Components {
    /// Builds state from config plus an executor.
    ///
    /// # Panics
    ///
    /// Never. A cache that cannot be built, or a master key `ar-keys` rejects,
    /// disables that subsystem and says so on stderr rather than taking the
    /// process down: a proxy that will not start is harder to diagnose than one
    /// that reports which layer it came up without.
    #[must_use]
    pub fn into_state(self) -> AppState {
        // The master key bytes, however the caller named them.
        //
        // Copied rather than borrowed: the config is about to be moved into an
        // `Arc`, so a borrow of its key would outlive the move.
        // `to_owned_secret` is the one sanctioned copy, and it is 32 bytes, once,
        // at boot.
        //
        // The explicit field wins, then the config's. Both are read here rather
        // than in the caller so `ar serve` — which builds its `Components` with
        // `with_exec` and never mentions a master key — still gets the gate
        // `AR_HTTP_MASTER_KEY` armed. That is the difference between a gate that
        // exists and one nothing can turn on.
        let master: Option<ar_keys::Secret> = self
            .master_key
            .map(ar_keys::Secret::new)
            .or_else(|| self.config.http_master_key.as_ref().map(ar_keys::Secret::to_owned_secret));
        let auth_mode = self.config.auth_mode;
        let config = Arc::new(self.config);
        let mut cards = config.model_cards();
        cards.extend(self.extra_models);
        // The catalog is leaked for `'static` because a `StaticCatalog` is
        // immutable and lives as long as the process. A leaked `Vec` here is a
        // few dozen bytes once, not a leak in the growing sense.
        let catalog: &'static StaticCatalog = Box::leak(Box::new(StaticCatalog::new(cards)));

        AppState {
            config,
            exec: self.exec,
            resilience: Arc::new(Resilience::new()),
            lkgp: Arc::new(LkgpPins::new()),
            rr: Arc::new(AtomicU64::new(0)),
            metrics: Arc::new(Metrics::new()),
            models: Arc::new(ModelsCache::new(catalog)),
            cache: build_cache(self.cache_bytes),
            auth_mode,
            // `Secret` has no `Deref`, so the gate takes the bytes rather than the
            // wrapper; the wrapper stays in `master` so it is zeroized on drop.
            auth: master.as_ref().map(|m| m.as_bytes()).and_then(build_gate),
        }
    }
}

/// Builds the cache when one is configured.
///
/// `Some(Some(0))` is the explicit "no cache" spelling and is distinct from
/// `None` ("cache with the default budget"), because "I want this server not to
/// cache" and "I did not think about the cache" are different instructions. A
/// cache that cannot be built falls back to the library default rather than to no
/// cache: a degraded cache is still a cache, and silently turning one off would
/// change `x-ar-cache` from `miss` to `bypass` for every request.
fn build_cache(bytes: Option<Option<u64>>) -> Option<Arc<ar_cache::Cache>> {
    let cap = bytes??;
    if cap == 0 {
        return None;
    }
    Some(Arc::new(
        ar_cache::Cache::with_config(ar_cache::CacheConfig::memory_only().with_mem_bytes(cap))
            .unwrap_or_else(|e| {
                eprintln!("ar: exact cache disabled ({e})");
                ar_cache::Cache::memory_only()
            }),
    ))
}

/// Builds the bearer gate, reporting a rejected master key on stderr.
///
    /// A gate that cannot be built is a gate that is **off**, and that is the
    /// dangerous direction on a routable bind — so the warning names the
    /// variable an operator would set and says the gate is disabled, rather than
    /// being a line that scrolls past. [`ServerConfig::public`] through
    /// [`bind_addr`] is the other half of the same property: a server with no
    /// gate refuses a routable bind unless the operator declared it public.
fn build_gate(master: &[u8]) -> Option<Arc<AuthGate>> {
    match AuthGate::new(master) {
        Ok(gate) => Some(Arc::new(gate)),
        Err(e) => {
            // stderr, not stdout: stdout is the data channel (`docs/06`).
            eprintln!("ar: bearer gate DISABLED — the master key was rejected ({e})");
            None
        }
    }
}

/// An executor that refuses every dispatch, for a server with no providers.
///
/// Present rather than `Option<Arc<dyn ArExec>>` so `AppState` has one type in
/// that field: a handler that had to unwrap an executor would have two ways to
/// panic on a request path, and a proxy that will not start is harder to diagnose
/// than one that reports which layer came up without.
///
/// `/healthz`, `/metrics` and `/v1/models` must answer on a proxy that has no
/// upstream yet — a proxy that 503s on boot because a key is missing is a proxy
/// an operator cannot debug. Every chat request is refused before this is reached
/// (`has_provider()` is false), so this never dispatches.
struct NullExec;

impl ArExec for NullExec {
    fn post_chat<'a>(
        &'a self,
        provider: &'a ar_route::ProviderId,
        _canonical: &'a ar_route::CanonicalRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ar_route::Upstream, ar_route::ExecError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Err(ar_route::ExecError(format!(
                "no provider is configured; refusing to dispatch to {provider}"
            )))
        })
    }

    fn post_media<'a>(
        &'a self,
        provider: &'a ar_route::ProviderId,
        _endpoint: &'a str,
        _content_type: &'a str,
        _body: &'a [u8],
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ar_route::MediaReply, ar_route::ExecError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Err(ar_route::ExecError(format!(
                "no provider is configured; refusing media dispatch to {provider}"
            )))
        })
    }
}

/// Builds the router with every route and the tower layers.
///
/// Four inbound dialects share one pipeline: `to_canonical_for_route` selects the
/// dialect by path, so each route is a one-line handler rather than a second
/// copy of the guard/cache/compress/attempt sequence.
///
/// The unknown-path fallback is a JSON 404 carrying the path, not axum's empty
/// one: a client that parses every error as JSON gets nothing at all to parse on
/// a typo'd route, which is the whole reason the reference gateway registers a
/// catch-all for `/v1/*` (`[...omnirouteCatchAll]/route.ts:15-31`).
///
/// Every route is covered by the CORS layer, including the fallback and the
/// preflight, which is the point of hand-rolling it: a 404 without CORS headers
/// reaches a browser as an opaque failure and the JSON body never gets read.
pub fn app(state: AppState) -> Router {
    let timeout = state.config.max_deadline();
    // The media family carries bodies the chat ceiling would refuse (audio),
    // so its routes are built on their own sub-router with their own body
    // limit and merged in — `route_layer` would have applied to the chat routes
    // registered before it, and a second global layer would loosen nothing.
    let media = Router::new()
        .route("/v1/embeddings", axum::routing::post(media::embeddings))
        .route(
            "/v1/audio/transcriptions",
            axum::routing::post(media::transcriptions),
        )
        .route(
            "/v1/images/generations",
            axum::routing::post(media::image_generations),
        )
        .route("/v1/ocr", axum::routing::post(media::ocr))
        .layer(RequestBodyLimitLayer::new(MEDIA_BODY_BYTES));
    Router::new()
        .route("/v1/chat/completions", axum::routing::post(routes::chat_completions))
        .route("/v1/messages", axum::routing::post(routes::messages))
        .route("/v1/responses", axum::routing::post(routes::responses))
        .route("/api/chat", axum::routing::post(routes::ollama_chat))
        // The legacy OpenAI alias the reference gateway still serves: a client
        // configured against `/v1/completions` is a client this server would
        // otherwise 404 for no reason. Same handler, so the two routes cannot
        // drift.
        .route("/v1/completions", axum::routing::post(routes::chat_completions))
        .merge(media)
        // `.head()` is spelled out: axum answers an unregistered method with
        // 405, so a `.get()`-only route turns an SDK's HEAD availability probe
        // into a refusal instead of a 200.
        .route(
            "/v1/models",
            axum::routing::get(routes::models).head(routes::models_head),
        )
        // Catch-all rather than `{model}`: every catalog id is `provider/model`,
        // so a single-segment param could never match one.
        .route("/v1/models/{*model}", axum::routing::get(routes::model))
        .route("/healthz", axum::routing::get(routes::healthz))
        .route("/metrics", axum::routing::get(routes::metrics))
        .fallback(routes::not_found)
        // 504, not 408: a stalled upstream is a gateway failure, and the
        // client's own request was fine.
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            timeout,
        ))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(state.clone(), trace_id))
        // Outermost, and last in the chain: outermost so a preflight is answered
        // from the headers alone, and last so it wraps the fallback too.
        .layer(axum::middleware::from_fn(cors))
        .with_state(state)
}

/// Answers a CORS preflight and stamps the headers on every response.
///
/// Outermost layer, so a preflight is answered without the timeout, the body
/// limit or the trace counter ever seeing it — none of which can help a preflight
/// and all of which could answer it with something else.
///
/// A hand-rolled middleware rather than `tower_http::cors::CorsLayer` for two
/// reasons that are both about this server rather than about CORS: the layer
/// takes a *typed* allowlist, so naming one here would mean inventing an origin
/// policy this crate has no configuration for; and it answers a preflight only
/// on a route it recognises, whereas an unknown path has to get CORS headers too
/// or a browser reports the 404 as an opaque CORS failure instead of the JSON
/// body the fallback just built.
///
/// The origin is echoed rather than wildcarded because the credential matrix
/// accepts `Authorization`, and a wildcarded origin is incompatible with a
/// credentialed request in every browser. There is no `Allow-Credentials`: the
/// credential is a header the client sets deliberately, and this server never
/// authenticates a cookie, so advertising credentialed CORS would claim a
/// capability it does not have.
///
/// That echo is a permissive policy and is called one here rather than left to be
/// discovered: any origin may read this server's responses, so a browser on a
/// page an attacker controls can drive it. The mitigations that matter are the
/// two already in place — the bind is loopback-only without
/// [`ServerConfig::public`], and a configured gate still requires a credential —
/// and neither is this layer's to decide.
///
/// Ponytail: an allowlist is the right shape for a routable deployment, and it
/// wants a `cors_origins:` config field; until one exists the echo is documented
/// rather than pretended away.
///
/// An absent `Origin` (a same-origin navigation, or `curl`) gets no
/// `Access-Control-Allow-Origin` at all rather than `*`, which is the honest
/// answer: a header that only means something to a cross-origin caller has
/// nothing to say to one that is not making one.
async fn cors(req: Request, next: Next) -> Response {
    let origin: Option<HeaderValue> = req.headers().get(header::ORIGIN).cloned();
    let is_preflight = req.method() == Method::OPTIONS;

    let mut response = if is_preflight {
        // 204 with no body: a preflight asks "may I?", and the answer is the
        // status line plus the two headers. A body here would be relayed to a
        // client that is not expecting one, and 204 is bodyless by definition so
        // nothing has to be suppressed later.
        axum::http::StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };

    let headers = response.headers_mut();
    if let Some(origin) = origin {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        // The response body varies by origin, so a cache must key on it. Without
        // this a shared cache would hand one origin's response to another.
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(CORS_METHODS),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(CORS_HEADERS),
    );
    // 600s: an LLM answer is not a 5-second request, so the preflight's
    // non-default has to outlast a typical generation or the browser re-asks
    // mid-stream.
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    response
}

/// Assigns a trace id to every request and echoes it back.
///
/// Outermost after `cors`, so a preflight never reaches here — a preflight is not
/// a request to account for, and counting one would put browser probes in the
/// request rate next to real traffic.
///
/// Honours an inbound `x-ar-trace-id` so a retry from an upstream caller stays
/// correlated instead of forking a second id; otherwise mints a v4 uuid. The
/// inbound value is length-capped at 64 because it is echoed into a response
/// header, and a client that sent a megabyte of it should not decide this
/// server's header size.
///
/// The span is entered around `next.run` rather than around the whole function so
/// every later layer's log line carries it.
async fn trace_id(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get(TRACE_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 64)
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);

    let path = req.uri().path().to_owned();
    let span = tracing::info_span!("http.request", trace_id = %id, method = %req.method(), path = %path);
    let _guard = span.enter();

    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        TRACE_HEADER,
        axum::http::HeaderValue::from_str(&id)
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("invalid")),
    );
    state.metrics.observe_request(classify(resp.status()));
    resp
}

/// Buckets a response status for the metrics counter.
///
/// Ranges rather than exact codes so a status this crate has never seen still
/// lands somewhere sensible; the buckets are the ones `/metrics` exposes.
fn classify(status: axum::http::StatusCode) -> Outcome {
    match status.as_u16() {
        200..=299 => Outcome::Ok,
        429 | 503 => Outcome::Throttled,
        400..=499 => Outcome::Client,
        500..=599 => Outcome::Upstream,
        _ => Outcome::Transport,
    }
}

/// Why a listener's address was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    /// A routable address was requested without `server.public`.
    #[error(
        "refusing to bind {host}: this proxy has no credential check by default, so a routable bind would publish it; set server.public (or AR_PUBLIC=1) to allow it"
    )]
    NotPublic {
        /// The address that was refused.
        host: String,
    },
    /// The address could not be turned into a socket address.
    #[error("`{host}` is not a valid bind address: {reason}")]
    Malformed {
        /// The address as written.
        host: String,
        /// Why it could not be parsed.
        reason: String,
    },
}

/// Resolves the address a listener should bind, refusing a routable one unless
/// the operator declared the server public.
///
/// The gate exists because the two halves of "is this safe to expose" are
/// separate settings that can be got wrong independently: a deployer sets
/// `host: 0.0.0.0` to reach a container port and publishes a credentialed LLM
/// proxy to the network. Loopback answers for `localhost`, `127.0.0.0/8` and
/// `::1`; everything else — including `0.0.0.0`, an empty host and a routable
/// interface name — needs `public`.
///
/// The check is on the *host half*, so `127.0.0.1:9000` passes with or without
/// the flag: the port a config file spells next to a loopback host is the common
/// case, and demanding `public` for it would train an operator to set the flag.
///
/// # Errors
///
/// [`BindError::NotPublic`] for a routable address without the flag, and
/// [`BindError::Malformed`] for one `SocketAddr` cannot parse at all.
pub fn bind_addr(host: &str, port: u16, public: bool) -> Result<std::net::SocketAddr, BindError> {
    let trimmed = host.trim();
    // `host:port` as written wins over the `port` argument, so the gate reads
    // the host half of whatever it will actually bind.
    if let Ok(explicit) = trimmed.parse::<std::net::SocketAddr>() {
        return gated(host, explicit.ip(), explicit.port(), public);
    }
    // A bare IP literal is never a `host:port` pair: `::1` splits on `:` into an
    // empty host, and `1` would then look like the port. So the literal is tried
    // first and only a non-IP host may have a port suffix stripped.
    let host_only = match trimmed.parse::<std::net::IpAddr>() {
        Ok(_) => trimmed,
        Err(_) => trimmed
            .rsplit_once(':')
            .filter(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            .map_or(trimmed, |(h, _)| h),
    };
    // `localhost` resolves to loopback on every host this runs on, and a name is
    // what a config file carries. Spelled out here rather than resolved through
    // `to_socket_addrs`, which needs a runtime and a blocking call.
    let ip = if host_only.eq_ignore_ascii_case("localhost") {
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    } else {
        match host_only.parse::<std::net::IpAddr>() {
            Ok(ip) => ip,
            // With the flag set, a bare interface name (`0.0.0.0`, `::`) still
            // has to become a socket address, so it is spelled out rather than
            // handed to the listener as a string.
            Err(_) if host_only == "0.0.0.0" => std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            Err(_) if host_only == "::" => std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
            Err(e) => {
                return Err(BindError::Malformed {
                    host: trimmed.to_owned(),
                    reason: e.to_string(),
                });
            }
        }
    };
    gated(trimmed, ip, port, public)
}

/// Applies the loopback rule to an already-parsed address.
///
/// A separate function so both entry points — a bare `host:port`, and a
/// `host:port` written with the port inline — go through one rule rather than
/// two spellings of it.
fn gated(
    host: &str,
    ip: std::net::IpAddr,
    port: u16,
    public: bool,
) -> Result<std::net::SocketAddr, BindError> {
    let loopback = match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback(),
    };
    // An empty host is deliberately *not* loopback: `bind("")` means "every
    // interface" to the resolver, so treating it as loopback would be the one
    // reading of it that opens the gate.
    if !loopback && !public {
        return Err(BindError::NotPublic { host: host.to_owned() });
    }
    Ok(std::net::SocketAddr::new(ip, port))
}

/// Convenience for `main`: a router plus the state it was built from, so the
/// binary can report the bound port without re-deriving it, and a test can read
/// the counters the same router is serving.
pub struct Server {
    /// The router to hand to `axum::serve`.
    pub router: Router,
    /// The state backing it, kept so tests can read counters.
    pub state: AppState,
}

/// Builds [`Server`] from components.
#[must_use]
pub fn server(components: Components) -> Server {
    let state = components.into_state();
    let router = app(state.clone());
    Server { router, state }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::time::Duration;

    use axum::http::StatusCode;

    use super::{
        BindError, Components, MAX_BODY_BYTES, REQUEST_TIMEOUT, ServerConfig, TRACE_HEADER, app,
        bind_addr, classify,
    };
    use crate::metrics::Outcome;

    #[test]
    fn body_limit_is_2mb() {
        assert_eq!(MAX_BODY_BYTES, 2 * 1024 * 1024);
    }

    /// The fallback deadline, not the layer's: a model with a reason to take
    /// longer says so in `ServerConfig::timeouts`, and the layer rises to the
    /// widest of them.
    #[test]
    fn request_timeout_is_120s() {
        assert_eq!(REQUEST_TIMEOUT.as_secs(), 120);
    }

    #[test]
    fn buckets_429_as_throttled() {
        assert_eq!(
            classify(StatusCode::TOO_MANY_REQUESTS),
            Outcome::Throttled
        );
    }

    #[test]
    fn buckets_503_as_throttled() {
        assert_eq!(
            classify(StatusCode::SERVICE_UNAVAILABLE),
            Outcome::Throttled
        );
    }

    #[test]
    fn buckets_500_as_upstream() {
        assert_eq!(
            classify(StatusCode::INTERNAL_SERVER_ERROR),
            Outcome::Upstream
        );
    }

    #[test]
    fn binds_loopback_without_asking_for_permission() {
        let got = bind_addr("127.0.0.1", 20128, false).expect("loopback always binds");
        assert_eq!(got, SocketAddr::from((Ipv4Addr::LOCALHOST, 20128)));
    }

    #[test]
    fn binds_localhost_without_asking_for_permission() {
        let got = bind_addr("localhost", 20128, false).expect("a name is what a config carries");
        assert_eq!(got.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn binds_ipv6_loopback_without_asking_for_permission() {
        assert!(bind_addr("::1", 20128, false).is_ok());
    }

    #[test]
    fn refuses_a_routable_address_when_not_public() {
        // The gate this whole function exists for.
        assert!(matches!(
            bind_addr("0.0.0.0", 20128, false),
            Err(BindError::NotPublic { .. })
        ));
    }

    #[test]
    fn refuses_a_routable_interface_when_not_public() {
        assert!(bind_addr("10.0.0.5", 20128, false).is_err());
    }

    #[test]
    fn refuses_an_empty_host_when_not_public() {
        // `bind("")` means every interface to the resolver, so it is routable.
        assert!(bind_addr("", 20128, false).is_err());
    }

    #[test]
    fn allows_a_routable_address_when_public() {
        let got = bind_addr("0.0.0.0", 20128, true).expect("explicitly public");
        assert_eq!(got.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn allows_the_ipv6_unspecified_address_when_public() {
        let got = bind_addr("::", 20128, true).expect("explicitly public");
        assert_eq!(got.ip(), IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert_eq!(got.port(), 20128);
    }

    #[test]
    fn reports_a_host_it_cannot_parse_even_when_public() {
        assert!(matches!(
            bind_addr("not a host", 20128, true),
            Err(BindError::Malformed { .. })
        ));
    }

    #[test]
    fn accepts_a_host_port_pair_as_written() {
        let got = bind_addr("127.0.0.1:9999", 20128, false).expect("explicit port wins");
        assert_eq!(got.port(), 9999);
    }

    #[test]
    fn reads_the_loopback_rule_off_a_host_port_pair() {
        // The gate reads the host half, so `10.0.0.5:9000` is refused even
        // though it parses as a socket address.
        assert!(matches!(
            bind_addr("10.0.0.5:9000", 20128, false),
            Err(BindError::NotPublic { .. })
        ));
    }

    // --- catalog routing -----------------------------------------------

    /// The router a test drives the read-only routes through.
    ///
    /// Unconfigured on purpose: none of the tests below need a provider, and
    /// `/healthz` answering on a proxy with no upstream is itself a property
    /// worth a test depending on.
    fn router() -> axum::Router {
        app(
            Components::unconfigured(ServerConfig::single(
                0,
                ar_route::Strategy::Priority,
                Vec::new(),
            ))
            .into_state(),
        )
    }

    /// The status of one request through [`router`].
    async fn status_of(method: &str, path: &str) -> u16 {
        response_of(method, path).await.status().as_u16()
    }

    /// One empty-bodied request through the shared fixture router, every layer
    /// included. The method is a parameter because a preflight is the same request
    /// with a different verb.
    async fn response_of(method: &str, path: &str) -> axum::response::Response {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .body(axum::body::Body::empty())
            .expect("request builds");
        send(router(), req).await
    }

    /// One response's body, read as JSON.
    async fn json_of(resp: axum::response::Response) -> serde_json::Value {
        serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body reads"),
        )
        .expect("body is JSON")
    }

    #[tokio::test]
    async fn a_preflight_admits_the_omniroute_compression_alias() {
        // A browser client configured against OmniRoute names the alias in its
        // preflight; answering it in `CORS_HEADERS` is the difference between a
        // browser client that works and one that never sends its first real
        // request. The 204 is every preflight's answer — the assertion is
        // about the allow-list, which is answered wholesale.
        let req = axum::http::Request::builder()
            .method("OPTIONS")
            .uri("/v1/chat/completions")
            .header(axum::http::header::ORIGIN, "http://localhost:5173")
            .header(
                axum::http::header::ACCESS_CONTROL_REQUEST_METHOD,
                "POST",
            )
            .header(
                axum::http::header::ACCESS_CONTROL_REQUEST_HEADERS,
                "x-omniroute-compression",
            )
            .body(axum::body::Body::empty())
            .expect("request builds");
        let resp = send(router(), req).await;
        assert_eq!(resp.status(), axum::http::StatusCode::NO_CONTENT);
        let allow = resp
            .headers()
            .get(axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS)
            .expect("the preflight answers an allow-list");
        assert!(
            allow
                .to_str()
                .expect("ASCII list")
                .contains("x-omniroute-compression"),
            "alias missing from the allow-list: {allow:?}"
        );
    }

    #[tokio::test]
    async fn head_models_is_routed_rather_than_405() {
        // The reason `.head()` is spelled out: `.get()` alone answers 405 here,
        // which reads to an SDK probe as "this server has no catalog".
        assert_eq!(status_of("HEAD", "/v1/models").await, 200);
        assert_eq!(status_of("GET", "/v1/models").await, 200);
    }

    #[tokio::test]
    async fn a_provider_qualified_id_reaches_the_single_model_route() {
        // A `{model}` segment could not match `provider/model` at all; the
        // catch-all is what makes the route reachable at all.
        assert_eq!(status_of("GET", "/v1/models/openai/gpt-4o-mini").await, 404);
        assert_eq!(status_of("GET", "/v1/models/nope").await, 404);
    }

    // --- the unknown-route fallback ------------------------------------

    #[tokio::test]
    async fn an_unknown_path_answers_with_a_json_envelope() {
        // axum's own fallback is an empty body, and an OpenAI-compatible SDK that
        // hits a typo'd path then reports a JSON *parse* failure rather than the
        // 404 — which is why the reference gateway registers a catch-all.
        let resp = response_of("GET", "/v1/chat/completionz").await;
        assert_eq!(resp.status(), 404);
        let body = json_of(resp).await;
        assert_eq!(body["error"]["code"], "unknown_route");
        assert_eq!(body["error"]["type"], "not_found");
        assert_eq!(body["error"]["path"], "/v1/chat/completionz");
    }

    #[tokio::test]
    async fn an_unknown_path_still_carries_a_trace_id() {
        // The fallback sits inside every layer, so a client still gets the id its
        // logs will be correlated by.
        let resp = response_of("GET", "/nope").await;
        assert!(resp.headers().contains_key(TRACE_HEADER), "no trace id on a 404");
    }

    // --- CORS / OPTIONS ------------------------------------------------

    #[tokio::test]
    async fn a_preflight_does_not_consume_the_route_behind_it() {
        // A browser sends the preflight first and then the real request, so the
        // second one has to still reach its handler.
        assert_eq!(response_of("OPTIONS", "/healthz").await.status(), 204);
        assert_eq!(status_of("GET", "/healthz").await, 200);
    }

    #[tokio::test]
    async fn a_preflight_is_answered_204_with_no_body() {
        let resp = response_of("OPTIONS", "/v1/chat/completions").await;
        assert_eq!(resp.status(), 204);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body reads");
        assert!(body.is_empty(), "a preflight returned {} bytes", body.len());
    }

    #[tokio::test]
    async fn a_preflight_advertises_the_methods_and_the_credential_headers() {
        use axum::http::header;
        let resp = response_of("OPTIONS", "/v1/messages").await;
        let methods = resp
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|v| v.to_str().ok())
            .expect("methods advertised");
        assert!(methods.contains("POST"), "a chat route needs POST: {methods}");
        let allowed = resp
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .and_then(|v| v.to_str().ok())
            .expect("headers advertised")
            .to_ascii_lowercase();
        // The three credential slots the matrix reads, plus the Anthropic version
        // header that gates one of them. Lowercased first because HTTP header
        // names are case-insensitive and the value is the reference gateway's
        // spelling, not this crate's.
        for needed in ["authorization", "x-api-key", "x-goog-api-key", "anthropic-version"] {
            assert!(allowed.contains(needed), "preflight would reject {needed}: {allowed}");
        }
    }

    #[tokio::test]
    async fn a_cross_origin_request_gets_its_origin_echoed_with_vary() {
        use axum::http::header;
        let req = axum::http::Request::builder()
            .uri("/healthz")
            .header(header::ORIGIN, "https://example.test")
            .body(axum::body::Body::empty())
            .expect("request builds");
        let resp = send(router(), req).await;
        assert_eq!(
            resp.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).and_then(|v| v.to_str().ok()),
            Some("https://example.test")
        );
        // Without this a shared cache hands one origin's response to another.
        assert!(resp.headers().contains_key(header::VARY), "no Vary: Origin");
    }

    #[tokio::test]
    async fn a_same_origin_request_gets_no_allow_origin_header() {
        use axum::http::header;
        let resp = response_of("GET", "/healthz").await;
        assert_eq!(resp.status(), 200);
        // `*` on a server that reads `Authorization` is incompatible with a
        // credentialed request in every browser, and a header that only means
        // something to a cross-origin caller has nothing to say to one that is
        // not making one.
        assert!(
            !resp.headers().contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            "a non-browser caller was handed an origin policy"
        );
    }

    #[tokio::test]
    async fn an_error_response_carries_cors_headers_too() {
        // Otherwise a browser reports the 404 as an opaque CORS failure and the
        // JSON body the fallback just built never reaches the client.
        use axum::http::header;
        let req = axum::http::Request::builder()
            .uri("/v1/nope")
            .header(header::ORIGIN, "https://example.test")
            .body(axum::body::Body::empty())
            .expect("request builds");
        let resp = send(router(), req).await;
        assert!(resp.headers().contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
    }

    #[tokio::test]
    async fn a_preflight_advertises_a_max_age_long_enough_for_a_generation() {
        // An LLM answer is not a 5-second request. The default of 5s would have
        // the browser re-ask mid-stream.
        use axum::http::header;
        let resp = response_of("OPTIONS", "/v1/chat/completions").await;
        let max_age: u32 = resp
            .headers()
            .get(header::ACCESS_CONTROL_MAX_AGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .expect("a max age is advertised");
        assert!(max_age >= 60, "a browser would re-ask mid-stream: {max_age}s");
    }

    // --- the model-aware deadline --------------------------------------

    #[test]
    fn an_unconfigured_server_keeps_the_fixed_120s_deadline() {
        let config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        assert_eq!(config.stream_deadline("anything"), REQUEST_TIMEOUT);
        assert_eq!(config.max_deadline(), REQUEST_TIMEOUT);
    }

    #[test]
    fn a_named_model_gets_its_own_deadline() {
        let mut config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        config.timeouts.insert("reasoner".to_owned(), Duration::from_secs(600));
        assert_eq!(config.stream_deadline("reasoner"), Duration::from_secs(600));
        // A model nobody named keeps the old answer, so one config line cannot
        // change the behaviour of every other request.
        assert_eq!(config.stream_deadline("other"), REQUEST_TIMEOUT);
    }

    #[test]
    fn the_layer_cover_is_the_widest_deadline_any_model_asked_for() {
        // The layer is a per-router constant, so if it were narrower than a
        // model's own deadline it would cut that request off before the
        // model-aware timeout could fire — the wrong order, and the client would
        // see the layer's number rather than the model's.
        let mut config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        config.timeouts.insert("reasoner".to_owned(), Duration::from_secs(600));
        assert_eq!(config.max_deadline(), Duration::from_secs(600));
    }

    #[test]
    fn a_narrower_deadline_does_not_narrow_the_layer_cover() {
        // A model asking for *less* is honoured per request, but the layer stays
        // at the default so it cannot pre-empt a different model's wider ask.
        let mut config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        config.timeouts.insert("quick".to_owned(), Duration::from_secs(5));
        assert_eq!(config.stream_deadline("quick"), Duration::from_secs(5));
        assert_eq!(config.max_deadline(), REQUEST_TIMEOUT);
    }

    #[test]
    fn a_wildcard_deadline_widens_the_layer_too() {
        // The `*` entry is a deadline like any other, so a blanket ask has to
        // reach the layer — otherwise the layer would cut every one of those
        // requests off before the per-request timeout could fire, which is the
        // failure the whole two-layer arrangement exists to prevent.
        let mut config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        config.timeouts.insert("*".to_owned(), Duration::from_secs(300));
        assert_eq!(config.stream_deadline("any-model"), Duration::from_secs(300));
        assert_eq!(config.max_deadline(), Duration::from_secs(300));
    }

    #[test]
    fn no_configured_deadline_is_ever_narrower_than_what_the_layer_allows() {
        // The invariant the whole two-layer arrangement rests on, checked over a
        // table rather than in one case: for every model this config can resolve,
        // its own deadline has to fit inside the layer's cover, or the client sees
        // the layer's 504 — which names no model and no timeout.
        let mut config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        config.timeouts.insert("reasoner".to_owned(), Duration::from_secs(600));
        config.timeouts.insert("quick".to_owned(), Duration::from_secs(5));
        config.timeouts.insert("*".to_owned(), Duration::from_secs(90));
        let cover = config.max_deadline();
        for model in ["reasoner", "quick", "anything-else"] {
            assert!(
                config.stream_deadline(model) <= cover,
                "{model} would be cut off by the layer before its own timeout could fire"
            );
        }
    }

    #[tokio::test]
    async fn a_model_whose_own_deadline_expires_is_a_504_naming_that_model() {
        // The per-request half, end to end. A `1s` deadline against an executor
        // that never answers is the only way to make the `timeout` arm fire
        // without a 600-second test, and it is a real one-second wait rather than
        // a mock of the timeout itself.
        let mut config = ServerConfig::single(
            0,
            ar_route::Strategy::Priority,
            vec![crate::exec::ProviderConfig::new(
                ar_route::ProviderId::new("p"),
                "http://127.0.0.1:1/v1",
                "k",
            )
            .with_model("m")],
        );
        config.timeouts.insert("m".to_owned(), Duration::from_secs(1));
        let exec = std::sync::Arc::new(Hangs);
        let resp = send(
            app(Components { exec, ..Components::unconfigured(config) }.into_state()),
            post_chat(r#"{"model":"m","stream":true,"messages":[]}"#, &[]).await,
        )
        .await;
        assert_eq!(resp.status(), 504, "a stalled model was not cut off");
        let body = json_of(resp).await;
        assert_eq!(body["error"]["code"], "upstream_timeout");
        assert_eq!(body["error"]["reason"], "model_deadline");
        assert!(
            body["error"]["message"].as_str().unwrap_or_default().contains("\"m\""),
            "the 504 must name the model, not just the timeout: {body}"
        );
    }

    /// An executor whose dispatch never resolves, for the deadline test.
    ///
    /// `NullExec` refuses immediately, which is the opposite of what a stalled
    /// upstream looks like — a stalled one returns nothing at all, so the attempt
    /// loop waits and the deadline is what has to end it.
    struct Hangs;

    impl ar_route::ArExec for Hangs {
        fn post_chat<'a>(
            &'a self,
            _provider: &'a ar_route::ProviderId,
            _canonical: &'a ar_route::CanonicalRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<ar_route::Upstream, ar_route::ExecError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(std::future::pending())
        }

        fn post_media<'a>(
            &'a self,
            _provider: &'a ar_route::ProviderId,
            _endpoint: &'a str,
            _content_type: &'a str,
            _body: &'a [u8],
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<ar_route::MediaReply, ar_route::ExecError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(std::future::pending())
        }
    }

    // --- a model with a wider deadline is not cut off by the layer -----

    #[tokio::test]
    async fn a_model_with_a_wider_deadline_reaches_dispatch() {
        // The whole point of the two-layer arrangement, asserted through the
        // router rather than through the config: the layer is a constant, so a
        // 600s model the layer still held at 120s would never see its own
        // timeout — it would see the layer's 504, which names no model.
        let mut config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        config.timeouts.insert("m".to_owned(), Duration::from_secs(600));
        let resp = send(
            app(Components::unconfigured(config).into_state()),
            post_chat(r#"{"model":"m","messages":[]}"#, &[]).await,
        )
        .await;
        // 503 from the null executor: the request was not cut off on the way in.
        assert_eq!(resp.status(), 503, "the layer cut the request before dispatch");
    }

    // --- the gate is armable, and off by default ----------------------

    /// 32 bytes, the length `ar-keys` requires of a master key.
    const MASTER: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

    /// A token this gate accepts, minted through the gate itself so the positive
    /// case cannot pass for an unrelated reason.
    fn accepted_token() -> String {
        crate::keys::AuthGate::new(MASTER)
            .expect("master key")
            .issue_for_tests("key-1")
            .expect("token mints")
            .access
    }

    /// A token this gate did *not* sign.
    fn foreign_token() -> String {
        crate::keys::AuthGate::new(b"ffffffffffffffffffffffffffffffff")
            .expect("master key")
            .issue_for_tests("key-1")
            .expect("token mints")
            .access
    }

    /// Drives one request through a router, for the tests that need a bespoke one
    /// rather than the shared fixture.
    async fn send(
        router: axum::Router,
        req: axum::http::Request<axum::body::Body>,
    ) -> axum::response::Response {
        use tower::ServiceExt;
        router.oneshot(req).await.expect("router answers")
    }

    /// A router whose gate is armed and whose mode is `mode`.
    fn gated_with(master: &[u8], mode: crate::config::AuthMode) -> axum::Router {
        let mut config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        config.auth_mode = mode;
        app(
            Components {
                master_key: Some(master.to_vec()),
                ..Components::unconfigured(config)
            }
            .into_state(),
        )
    }

    #[test]
    fn the_auth_mode_reaches_the_state_with_the_gate() {
        // The mode and the gate it modifies travel together, so a state cannot
        // carry a gate whose mode lives somewhere else and can disagree with it.
        let mut config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        config.auth_mode = crate::config::AuthMode::DegradeInvalidToAnon;
        let state = Components::unconfigured(config).into_state();
        assert_eq!(state.auth_mode, crate::config::AuthMode::DegradeInvalidToAnon);
    }

    /// A server whose gate is armed with `master`, in the default mode.
    fn gated(master: &[u8]) -> axum::Router {
        gated_with(master, crate::config::AuthMode::Required)
    }

    /// A chat POST with a JSON body, plus any extra headers.
    async fn post_chat(body: &str, extra: &[(&str, &str)]) -> axum::http::Request<axum::body::Body> {
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_owned()))
            .expect("request builds");
        for (name, value) in extra {
            req.headers_mut().insert(
                axum::http::HeaderName::try_from(*name).expect("header name"),
                axum::http::HeaderValue::from_str(value).expect("header value"),
            );
        }
        req
    }

    #[tokio::test]
    async fn an_armed_gate_answers_401_with_the_invalid_api_key_code() {
        // The code is the OpenAI-compatible spelling a client branches on; the
        // `reason` is what separates "you sent nothing" from "what you sent is
        // wrong" without either reaching the client verbatim.
        let resp = send(gated(MASTER), post_chat(r#"{"model":"m","messages":[]}"#, &[]).await).await;
        assert_eq!(resp.status(), 401);
        let body = json_of(resp).await;
        assert_eq!(body["error"]["code"], "invalid_api_key");
        assert_eq!(body["error"]["type"], "authentication_error");
        assert!(body["error"]["reason"].is_string(), "no reason to branch on: {body}");
    }

    #[tokio::test]
    async fn a_credential_the_gate_accepts_passes_through() {
        let token = accepted_token();
        let resp = send(
            gated(MASTER),
            post_chat(
                r#"{"model":"m","messages":[]}"#,
                &[("authorization", &format!("Bearer {token}"))],
            )
            .await,
        )
        .await;
        // This server has no provider configured, so 503 is what a request that
        // cleared the gate gets. Anything 401 means the credential did not.
        assert_eq!(resp.status(), 503, "a valid credential was refused");
    }

    #[tokio::test]
    async fn a_foreign_credential_is_refused_and_never_echoed() {
        let other = foreign_token();
        let resp = send(
            gated(MASTER),
            post_chat(
                r#"{"model":"m","messages":[]}"#,
                &[("authorization", &format!("Bearer {other}"))],
            )
            .await,
        )
        .await;
        assert_eq!(resp.status(), 401);
        let body = json_of(resp).await;
        // This response is a cacheable 401 a browser may keep; a credential in it
        // would be a credential on disk.
        assert!(
            !body.to_string().contains(&other),
            "the refused token reached the client: {body}"
        );
    }

    #[tokio::test]
    async fn a_refused_credential_is_served_under_the_degrade_mode() {
        // The mode that makes a stale CLI config keep working. Configured
        // explicitly, because the default is `Required` and this is the entire
        // point of the third mode.
        let other = foreign_token();
        let resp = send(
            gated_with(MASTER, crate::config::AuthMode::DegradeInvalidToAnon),
            post_chat(r#"{"model":"m","messages":[]}"#, &[("authorization", &format!("Bearer {other}"))])
                .await,
        )
        .await;
        // 503 rather than 401: the request passed the gate and reached the
        // provider check, which this server has nothing configured for.
        assert_eq!(resp.status(), 503, "a stale key was refused under degrade");
    }

    #[tokio::test]
    async fn an_absent_credential_is_anonymous_under_the_degrade_mode() {
        // The documented cost of that mode: it waives the requirement as well as
        // tolerating a wrong key, which is exactly why it is a mode and not the
        // default. 503 rather than 401 — the request was served, and this server
        // has no provider to serve it from.
        let resp = send(
            gated_with(MASTER, crate::config::AuthMode::DegradeInvalidToAnon),
            post_chat(r#"{"model":"m","messages":[]}"#, &[]).await,
        )
        .await;
        assert_eq!(resp.status(), 503, "degrade refused a credential-less request");
    }

    #[tokio::test]
    async fn a_google_api_key_authenticates_a_gated_request() {
        // The row with no gate on it: `gemini-cli` sends its key here and
        // nowhere else, so there is no version header to condition on.
        let token = accepted_token();
        let resp = send(
            gated(MASTER),
            post_chat(r#"{"model":"m","messages":[]}"#, &[("x-goog-api-key", &token)]).await,
        )
        .await;
        assert_eq!(resp.status(), 503, "the x-goog-api-key matrix row did not authenticate");
    }

    #[tokio::test]
    async fn a_tokenized_alias_path_authenticates_a_gated_request() {
        // The client that cannot attach a header at all. `/vscode/<token>/…` is
        // not a route this server serves, so the request 404s — but only *after*
        // the gate read the path, which is the property under test.
        let token = accepted_token();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/vscode/{token}/chat/completions"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(r#"{"model":"m"}"#))
            .expect("request builds");
        let resp = send(gated(MASTER), req).await;
        assert_ne!(resp.status(), 401, "the path token was not read");
    }

    #[tokio::test]
    async fn an_unarmed_server_serves_an_anonymous_request() {
        // The property every default in this change rests on.
        let resp = send(router(), post_chat(r#"{"model":"m","messages":[]}"#, &[]).await).await;
        assert_ne!(resp.status(), 401);
    }

    #[tokio::test]
    async fn an_anthropic_x_api_key_authenticates_a_gated_request() {
        let token = accepted_token();
        let resp = send(
            gated(MASTER),
            post_chat(
                r#"{"model":"m","messages":[]}"#,
                &[("x-api-key", &token), ("anthropic-version", "2023-06-01")],
            )
            .await,
        )
        .await;
        assert_eq!(resp.status(), 503, "the x-api-key matrix row did not authenticate");
    }

    #[test]
    fn the_config_can_arm_the_gate_the_components_field_would_have() {
        // The wiring that makes the gate reachable from a shipped serve path:
        // `AR_HTTP_MASTER_KEY` lands in the config, and `into_state` reads it when
        // the `Components` field is unset — which is how `ar serve` builds its
        // components, since it never names a master key.
        let mut config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        config.http_master_key = Some(ar_keys::Secret::new(MASTER.to_vec()));
        assert!(Components::unconfigured(config).into_state().auth.is_some());
    }

    #[test]
    fn an_explicit_master_key_wins_over_the_configs() {
        // A caller that already holds the material should not have to push it
        // through a config, and the two must not be able to disagree silently.
        let mut config = ServerConfig::single(0, ar_route::Strategy::Priority, Vec::new());
        config.http_master_key = Some(ar_keys::Secret::new(b"f".repeat(32)));
        let state = Components {
            master_key: Some(MASTER.to_vec()),
            ..Components::unconfigured(config)
        }
        .into_state();
        let token = accepted_token();
        assert!(
            state.auth.as_deref().expect("gate configured").verify(&token).is_ok(),
            "the explicit key did not win"
        );
    }
}
