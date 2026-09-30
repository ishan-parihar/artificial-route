//! axum 0.8 router and its tower layers.
//!
//! Layer order is outermost-first as written below: trace-id runs first so every
//! later layer's log line carries the id, the body limit runs before the
//! handler reads a byte, and the timeout wraps the handler future.
//!
//! The timeout deliberately bounds the *request*, not the response body.
//! `TimeoutLayer::new` resolves when the handler returns a `Response`, which for
//! a streamed completion is as soon as upstream headers arrive — so a 20-minute
//! generation is fine while a 20-minute *hang* is a 408. Wrapping the body
//! instead (`.map_response_body()`) would cut every long answer off at 120s.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use ar_route::{ArExec, LkgpPins, Resilience};

use crate::config::ServerConfig;
use crate::keys::AuthGate;
use crate::metrics::{Metrics, Outcome};
use crate::models::{ModelsCache, StaticCatalog};
use crate::routes;

/// Request body ceiling. A chat request is kilobytes; 2MB is a client's runaway
/// loop, and refusing it at the edge is cheaper than buffering it.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Ceiling on time-to-response-headers. Deliberately generous: it is a
/// backstop against a dead upstream, not a latency budget.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Trace id echoed to the client and attached to every span.
pub const TRACE_HEADER: &str = "x-ar-trace-id";

/// Shared server state. `Arc`-ed once by `app()` and cloned per request by
/// axum's `State`, so handlers never re-allocate the routing tables.
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
    pub auth: Option<Arc<AuthGate>>,
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
            .field("public", &self.config.public)
            .finish_non_exhaustive()
    }
}

/// Everything needed to serve: config, executor, and the caches that hang off
/// them.
///
/// Constructed with a struct literal so a caller names only what it changes;
/// `..Components::unconfigured(config)` fills the rest with "no cache, no
/// bearer gate", which is the safe direction for both.
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
    pub master_key: Option<Vec<u8>>,
}

impl Components {
    /// The minimal configuration: no cache, no bearer gate, no extra models.
    ///
    /// Every field is public so a caller that wants a cache says
    /// `cache_bytes: Some(Some(1 << 20))` rather than reaching for a builder
    /// that exists only to hide this.
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

    /// Builds components with an executor and nothing else.
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
            auth: self.master_key.as_deref().and_then(build_gate),
        }
    }
}

/// Builds the cache when one is configured.
///
/// `Some(Some(0))` is the explicit "no cache" spelling and is distinct from
/// `None` ("cache with the default budget"), because "I want this server not to
/// cache" and "I did not think about the cache" are different instructions.
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

/// An executor for a server with no providers configured.
///
/// `/healthz`, `/metrics` and `/v1/models` must answer on a proxy that has no
/// upstream yet — a proxy that 503s on boot because a key is missing is a proxy
/// an operator cannot debug. Every chat request is refused before this is
/// reached (`has_provider()` is false), so this never dispatches.
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
}

/// Builds the router with every route and the three tower layers.
///
/// Four inbound dialects share one pipeline: `to_canonical_for_route` selects the
/// dialect by path, so each route is a one-line handler rather than a second
/// copy of the guard/cache/compress/attempt sequence.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", axum::routing::post(routes::chat_completions))
        .route("/v1/messages", axum::routing::post(routes::messages))
        .route("/v1/responses", axum::routing::post(routes::responses))
        .route("/api/chat", axum::routing::post(routes::ollama_chat))
        .route("/v1/models", axum::routing::get(routes::models))
        .route("/healthz", axum::routing::get(routes::healthz))
        .route("/metrics", axum::routing::get(routes::metrics))
        // 504, not 408: a stalled upstream is a gateway failure, and the
        // client's own request was fine.
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(state.clone(), trace_id))
        .with_state(state)
}

/// Assigns a trace id to every request and echoes it back.
///
/// Honours an inbound `x-ar-trace-id` so a retry from an upstream caller stays
/// correlated instead of forking a second id; otherwise mints a v4 uuid.
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
fn classify(status: axum::http::StatusCode) -> Outcome {
    match status.as_u16() {
        200..=299 => Outcome::Ok,
        429 | 503 => Outcome::Throttled,
        400..=499 => Outcome::Client,
        500..=599 => Outcome::Upstream,
        _ => Outcome::Transport,
    }
}

/// Why a bind address was refused.
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
/// binary can report the bound port without re-deriving it.
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

    use axum::http::StatusCode;

    use super::{BindError, MAX_BODY_BYTES, REQUEST_TIMEOUT, bind_addr, classify};
    use crate::metrics::Outcome;

    #[test]
    fn body_limit_is_2mb() {
        assert_eq!(MAX_BODY_BYTES, 2 * 1024 * 1024);
    }

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
}
