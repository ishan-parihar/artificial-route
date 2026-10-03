//! Shared wire + provider contracts that `ar-translate`, `ar-exec` and
//! `ar-route` agree on.
//!
//! P0 scope (`docs/05-roadmap.md`): OpenAI chat inbound, one provider wire.
//! The two translator/executor traits below are **stubs** — the real
//! `ar-translate` / `ar-exec` crates fill them in without changing this
//! signature (AGENTS.md §3: coordinate via shared trait signatures).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use http::StatusCode;

/// Interned provider/model/session name.
///
/// This is the `ar-core` `Strng` (8-byte `Arc<str>`) pending the P0 vendoring
/// of `agentgateway/crates/core` (`docs/01-copy-from-agentgateway.md`). The
/// alias is the seam: swapping in the vendored type is a one-line change here,
/// not a change at every call site.
pub type Strng = Arc<str>;

/// A provider identity, e.g. `"openai"` or `"groq-prod-a"`.
///
/// Distinct from [`Strng`] on purpose: a [`ProviderId`] is never a session key
/// and a session key is never a provider. A newtype costs ~20 lines and removes
/// a whole class of argument-order bug in `pick` / `attempt_loop`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderId(Strng);

impl ProviderId {
    /// Wraps an owned or borrowed name, interning it once.
    pub fn new(name: impl AsRef<str>) -> Self {
        Self(Strng::from(name.as_ref()))
    }

    /// Borrows the underlying name. Cheap: no refcount traffic.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProviderId({})", self.as_str())
    }
}

impl From<&str> for ProviderId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for ProviderId {
    fn from(s: String) -> Self {
        Self(Strng::from(s))
    }
}

impl AsRef<str> for ProviderId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// One provider's current allowance, as the quota store reports it.
///
/// The rollover is the caller's, not the sorter's. `docs/02` §route ports
/// `QuotaStore` + `accountBuckets` as a *store* that normalises a window before
/// the strategy sees it, and the reference keeps that split: every quota
/// strategy there is a pure comparator over an already-rolled snapshot.
///
/// So [`QuotaWindow`] carries no clock, and neither does the quota arm of
/// [`crate::pick`]. That is not a simplification — it is what lets the whole
/// table be unit-tested with no time source, and it is why adding `now` to
/// `pick` would have been a behaviour change disguised as a parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct QuotaWindow {
    /// Total allowance for the current window. `0` means "unmetered", which
    /// every quota strategy treats as infinite headroom rather than as an
    /// exhausted pool.
    pub limit: u64,
    /// Consumed so far. Callers reset this to `0` when `reset_at_secs` has
    /// passed, so a rolled window is simply a fresh snapshot.
    pub used: u64,
    /// Unix seconds at which the window rolls over. `0` = unknown.
    pub reset_at_secs: u64,
}

impl QuotaWindow {
    /// Builds a window from its three fields.
    #[must_use]
    pub fn new(limit: u64, used: u64, reset_at_secs: u64) -> Self {
        Self {
            limit,
            used,
            reset_at_secs,
        }
    }

    /// Units left in this window. Saturating: a snapshot that over-reports
    /// usage is fully drained, not a `u64` wrap into "almost all free".
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }

    /// Fraction of the window still free, in `0.0..=1.0`.
    ///
    /// Unmetered (`limit == 0`) is `1.0`: "no ceiling" is not "no capacity",
    /// and returning `0.0` would make an unmetered provider look exhausted and
    /// route away from exactly the providers that have the most headroom.
    #[must_use]
    pub fn headroom(&self) -> f64 {
        if self.limit == 0 {
            return 1.0;
        }
        self.remaining() as f64 / self.limit as f64
    }
}

/// One routable model on one provider, plus the signals the twenty
/// strategies rank on.
///
/// Every added field is *optional metadata with a defined neutral value*, and
/// the neutral value is chosen so an unset signal never makes a candidate look
/// like a bad one. `ar-server` builds candidates from a dozen environment
/// variables and sets only the four original fields; the other strategies still
/// run against that set, degraded to a documented default rather than to an
/// error. That is the same contract `input_usd_per_mtok: None` already had, and
/// it is why the P2 strategies landed without changing `pick`'s
/// signature — `crates/ar-server` is untouched by this work.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Provider that serves this model.
    pub provider: ProviderId,
    /// Provider-local model name, e.g. `"gpt-4o-mini"`.
    pub model: Strng,
    /// USD per 1M input tokens. `None` = price unknown, which sorts **last**:
    /// routing blind to price is worse than routing to a known-cheap tier.
    pub input_usd_per_mtok: Option<f64>,
    /// Lower sorts first. Every strategy falls back to this as its final
    /// tiebreak, so it is the crate's one total order.
    pub rank: u32,
    /// Relative share for `weighted`, and the fill budget for `fill-first`.
    /// Defaults to `1`, which makes an unweighted candidate participate as an
    /// equal — never as weight `0`, which would silently drop it from rotation.
    pub weight: u32,
    /// Requests currently dispatched to this provider, not yet finished.
    /// Read by `p2c`, `least-used`, `fill-first` and `quota-share-fair`.
    pub in_flight: u32,
    /// Current allowance. `None` = no quota record, which the quota strategies
    /// treat as "unknown" and sort after every metered candidate.
    pub quota: Option<QuotaWindow>,
    /// Usable prompt size in tokens. `0` = unknown. `context-optimized`
    /// prefers the largest, so a long prompt routes to a big model instead of
    /// discovering the limit as a 400.
    pub context_window: u32,
    /// Prompt tokens already cached for this conversation on this provider.
    /// The prefix-pin payoff, measured: a cached prefix is a free hit, so
    /// `cache-optimized` prefers the provider already holding the prefix over
    /// one that would re-bill it.
    pub cached_prefix_tokens: u32,
}

impl Candidate {
    /// Build a candidate with every optional signal at its neutral value.
    ///
    /// Neutral means "unknown", never "worst": no price, rank `0`, weight `1`,
    /// nothing in flight, no quota record, unknown context window, nothing
    /// cached. `Priority` and `RoundRobin` read none of it.
    #[must_use]
    pub fn new(provider: ProviderId, model: impl AsRef<str>) -> Self {
        Self {
            provider,
            model: Strng::from(model.as_ref()),
            input_usd_per_mtok: None,
            rank: 0,
            weight: 1,
            in_flight: 0,
            quota: None,
            context_window: 0,
            cached_prefix_tokens: 0,
        }
    }

    /// Sets the input price in USD per 1M tokens.
    #[must_use]
    pub fn with_price(mut self, usd_per_mtok: f64) -> Self {
        self.input_usd_per_mtok = Some(usd_per_mtok);
        self
    }

    /// Sets the `Strategy::Priority` ordering rank (lower = earlier).
    #[must_use]
    pub fn with_rank(mut self, rank: u32) -> Self {
        self.rank = rank;
        self
    }

    /// Sets the relative share for `weighted` and the fill budget for
    /// `fill-first`.
    ///
    /// `0` is clamped to `1`: a zero weight would remove the candidate from
    /// weighted rotation entirely, which is a different decision (disable the
    /// provider) expressed through a field that reads like a share.
    #[must_use]
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight.max(1);
        self
    }

    /// Sets the in-flight request count.
    #[must_use]
    pub fn with_in_flight(mut self, in_flight: u32) -> Self {
        self.in_flight = in_flight;
        self
    }

    /// Attaches a current quota window.
    #[must_use]
    pub fn with_quota(mut self, quota: QuotaWindow) -> Self {
        self.quota = Some(quota);
        self
    }

    /// Sets the usable prompt size in tokens.
    #[must_use]
    pub fn with_context_window(mut self, tokens: u32) -> Self {
        self.context_window = tokens;
        self
    }

    /// Sets how many prompt tokens are already cached for this conversation.
    #[must_use]
    pub fn with_cached_prefix(mut self, tokens: u32) -> Self {
        self.cached_prefix_tokens = tokens;
        self
    }
}

/// A request in the provider-independent shape the router reasons about.
///
/// `body` is already canonical JSON: `ArTranslate::to_canonical` owns that
/// translation and the router never inspects it. That is deliberate — routing
/// must not be able to change the request it is routing.
#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalRequest {
    /// Requested model, possibly a combo alias resolved upstream of the router.
    pub model: Strng,
    /// Canonical JSON request body.
    pub body: Bytes,
    /// Whether the client asked for an SSE stream.
    pub stream: bool,
    /// Opaque client-supplied stickiness key for `Strategy::Lkgp`
    /// (`x-ar-session`). `None` disables pinning for this request.
    pub session: Option<Strng>,
}

impl CanonicalRequest {
    /// Builds a non-streaming request with no session pin.
    #[must_use]
    pub fn new(model: impl AsRef<str>, body: Bytes) -> Self {
        Self {
            model: Strng::from(model.as_ref()),
            body,
            stream: false,
            session: None,
        }
    }

    /// Marks the request as an SSE stream.
    #[must_use]
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Attaches the stickiness key used by `Strategy::Lkgp`.
    #[must_use]
    pub fn with_session(mut self, session: Option<Strng>) -> Self {
        self.session = session;
        self
    }
}

/// Byte stream of a successful upstream response.
///
/// SSE responses arrive as many small `Bytes`; a non-stream completion arrives
/// as exactly one. The router relays it opaquely — the whole point of the
/// `ar-llm` passthrough path is that framing survives untouched.
pub type ChunkStream = Pin<Box<dyn Stream<Item = Bytes> + Send>>;

/// A completed media exchange: the whole reply, read eagerly.
///
/// The non-streaming sibling of [`Upstream`]. None of the media endpoints
/// stream, so there is no [`ChunkStream`] to hand over — the executor reads the
/// full body before returning, and `status` + `error-shaped or not` are the
/// same verdict the chat loop gets from an [`Upstream`].
pub struct MediaReply {
    /// HTTP status the provider returned, 2xx on success.
    pub status: StatusCode,
    /// The reply body, verbatim, successful or not.
    pub body: Bytes,
    /// The upstream's own `Content-Type`, so a verbatim relay does not have to
    /// guess the label for bytes it did not produce.
    pub content_type: String,
    /// Parsed `Retry-After` on a 429/503, if the provider sent a usable one.
    pub retry_after: Option<Duration>,
}

/// A completed upstream exchange, successful or not.
pub struct Upstream {
    /// HTTP status the provider returned.
    pub status: StatusCode,
    /// Parsed `Retry-After` on a 429/503, if the provider sent a usable one.
    pub retry_after: Option<Duration>,
    /// Full body for non-2xx. Empty for 2xx. The router reads this to apply
    /// the 400 stop-row rule (`classify_status`), so it must be complete even
    /// when the success path is streaming.
    pub error_body: Bytes,
    /// Success payload. Empty for non-2xx, which never has one.
    pub stream: ChunkStream,
}

impl fmt::Debug for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hand-written: `ChunkStream` is a boxed trait object with no `Debug`,
        // and printing the payload is exactly the PII leak `docs/04-obs`
        // forbids. Status + body length is the whole useful summary.
        f.debug_struct("Upstream")
            .field("status", &self.status)
            .field("retry_after", &self.retry_after)
            .field("error_body_len", &self.error_body.len())
            .finish_non_exhaustive()
    }
}

impl Upstream {
    /// Builds a success with no payload, for executors that stream lazily.
    #[must_use]
    pub fn success(stream: ChunkStream) -> Self {
        Self {
            status: StatusCode::OK,
            retry_after: None,
            error_body: Bytes::new(),
            stream,
        }
    }

    /// Builds a non-2xx outcome. `retry_after` wins over backoff when present.
    #[must_use]
    pub fn failure(status: StatusCode, error_body: Bytes, retry_after: Option<Duration>) -> Self {
        Self {
            status,
            retry_after,
            error_body,
            stream: Box::pin(futures::stream::empty()),
        }
    }
}

/// Failure that is not an HTTP status — transport error, TLS failure, or a
/// provider whose body never arrived. Always fail-over-able: no provider ever
/// produced a verdict on the request.
#[derive(Debug, thiserror::Error)]
#[error("upstream transport error: {0}")]
pub struct ExecError(pub String);

/// Inbound-wire → canonical request.
///
/// Implemented by `ar-translate`. P0 ships the OpenAI-chat identity adapter.
pub trait ArTranslate {
    /// Normalises one inbound body into the router's canonical shape.
    ///
    /// # Errors
    /// Returns a message describing the shape mismatch when the inbound body
    /// cannot be represented canonically (bad JSON, missing `model`, ...).
    fn to_canonical(&self, inbound: &[u8]) -> Result<CanonicalRequest, String>;
}

/// Canonical request → provider wire, and the POST itself.
///
/// Implemented by `ar-exec`.
pub trait ArExec: Send + Sync {
    /// POSTs `canonical` to `provider` and returns the raw exchange.
    ///
    /// # Errors
    /// Returns [`ExecError`] only for transport-level failures. An HTTP status
    /// — including 4xx and 5xx — is an [`Upstream`], not an error: the router
    /// has to see it to decide retry vs fail-over.
    fn post_chat<'a>(
        &'a self,
        provider: &'a ProviderId,
        canonical: &'a CanonicalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>>;

    /// POSTs a non-chat media `body` to the wire path `endpoint` names and
    /// returns the whole reply.
    ///
    /// `endpoint` is a path, not an enum, because the media vocabulary belongs
    /// to the executor crate; this contract only needs to name a place to POST
    /// to, which a route's own path already spells (`"/embeddings"`, …). An
    /// unknown path is an [`ExecError`], not a fall-through to chat.
    ///
    /// A media exchange is eager where a chat one streams: [`MediaReply`]
    /// carries the decoded body because none of these endpoints frame their
    /// replies incrementally.
    ///
    /// # Errors
    /// Returns [`ExecError`] for transport-level failures, an unknown provider,
    /// an unknown endpoint path, and — in this build — a provider whose
    /// credentials are an OAuth session, which no media executor transcribes.
    /// A non-2xx HTTP status is **not** an error here, for the same reason it
    /// is not one on [`Self::post_chat`]: the caller must see the verdict.
    fn post_media<'a>(
        &'a self,
        provider: &'a ProviderId,
        endpoint: &'a str,
        content_type: &'a str,
        body: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>>;
}

/// What [`Router::attempt_loop`](crate::Router::attempt_loop) executes against.
///
/// Narrower than [`ArExec`] on purpose: the loop only needs "send this
/// canonical body to this provider". [`ArExec`] is the same call plus the
/// provider-specific URL/auth/param rewrite, so an `ArExec` satisfies this
/// blanket impl and `ar-server` does not need a second trait.
impl<T: ArExec + ?Sized> Executor for T {
    fn call<'a>(
        &'a self,
        provider: &'a ProviderId,
        canonical: &'a CanonicalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
        self.post_chat(provider, canonical)
    }
}

/// Object-safe execution seam for the attempt loop.
///
/// Returns a boxed future rather than using an `async fn` in trait so the
/// server can hold one `Arc<dyn Executor>` behind a heterogeneous config
/// (ch.6: `dyn` only for heterogeneous lists). The box is allocated once per
/// attempt, at most a handful per request — not on the SSE byte path, which is
/// where the latency actually lives.
pub trait Executor: Send + Sync {
    /// Sends one attempt and resolves with the provider's verdict.
    ///
    /// # Errors
    /// Transport-level failure only; see [`ArExec::post_chat`].
    fn call<'a>(
        &'a self,
        provider: &'a ProviderId,
        canonical: &'a CanonicalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>>;
}

#[cfg(test)]
mod tests {
    use super::{Candidate, ProviderId, QuotaWindow, Strng};

    #[test]
    fn provider_id_borrows_without_cloning() {
        let id = ProviderId::new("openai");
        assert_eq!(id.as_str(), "openai");
    }

    #[test]
    fn candidate_defaults_to_unpriced_and_unranked() {
        let c = Candidate::new(ProviderId::new("groq"), "llama-3.3-70b");
        assert_eq!(c.input_usd_per_mtok, None);
    }

    #[test]
    fn session_key_interns_once() {
        let a: Strng = Strng::from("s1");
        let b: Strng = Strng::from("s1");
        assert_eq!(a, b);
    }

    #[test]
    fn candidate_defaults_every_added_signal_to_neutral() {
        let c = Candidate::new(ProviderId::new("groq"), "m");
        assert_eq!((c.weight, c.in_flight, c.quota, c.context_window, c.cached_prefix_tokens), (1, 0, None, 0, 0));
    }

    #[test]
    fn clamps_a_zero_weight_to_one() {
        // Weight 0 would silently drop the provider from weighted rotation, which
        // is "disable this provider" wearing the costume of "a share".
        let c = Candidate::new(ProviderId::new("groq"), "m").with_weight(0);
        assert_eq!(c.weight, 1);
    }

    #[test]
    fn remaining_never_wraps_past_zero() {
        let q = QuotaWindow::new(10, 40, 0);
        assert_eq!(q.remaining(), 0);
    }

    #[test]
    fn an_unmetered_window_is_all_headroom() {
        let q = QuotaWindow::new(0, 0, 0);
        assert_eq!(q.headroom(), 1.0);
    }

    #[test]
    fn headroom_is_the_free_fraction_of_the_window() {
        let q = QuotaWindow::new(200, 50, 0);
        assert!((q.headroom() - 0.75).abs() < f64::EPSILON);
    }
}
