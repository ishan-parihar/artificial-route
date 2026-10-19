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
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use http::StatusCode;

use crate::resilience::{BreakerClass, Resilience};

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
    /// The model this request is dispatched as.
    ///
    /// A client-facing name on the way in (a combo alias, `auto/*`, or a bare
    /// `provider/model`), and the *target's own* model once a dispatcher has
    /// picked one — the attempt loop rewrites it per chain entry, so what
    /// reaches the executor is what that target serves. Empty means the caller
    /// named no model of its own and the executor's configured model serves it.
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

    /// The same request, asked for as `model`.
    ///
    /// The attempt loop builds one of these per chain entry: the body is a
    /// `Bytes` (a refcount bump) and the only field that changes is the model,
    /// so rewriting it per attempt costs one allocation and leaves every other
    /// byte the router was handed untouched.
    #[must_use]
    pub fn with_model(mut self, model: impl AsRef<str>) -> Self {
        self.model = Strng::from(model.as_ref());
        self
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

/// Set by the executor when the upstream body fails after its headers.
///
/// `ChunkStream` yields bare bytes, so a body error cannot travel in the stream
/// itself. The relay reads this flag once the stream ends, to tell a finished
/// reply from a cut one.
pub type CutFlag = Arc<std::sync::atomic::AtomicBool>;

/// Settles a streamed attempt's breaker verdict when its body is over.
///
/// The breaker is not credited at the first byte. `record_provider_success`
/// takes one failure off the count, so a provider that cut every stream after
/// its first byte would be credited and charged once per request and never
/// reach its threshold. The verdict is taken here instead: a cut is one
/// failure, and any other end (a clean finish, or the client going away) is a
/// success. Dropping the stream is the settle point, so a disconnect settles too.
pub fn settle_stream(
    stream: ChunkStream,
    resilience: Arc<Resilience>,
    provider: &str,
    cut: CutFlag,
) -> ChunkStream {
    Box::pin(Settled {
        inner: stream,
        _verdict: Verdict {
            resilience,
            provider: Strng::from(provider),
            cut,
        },
    })
}

/// The breaker verdict for one streamed attempt, taken on drop.
struct Verdict {
    resilience: Arc<Resilience>,
    provider: Strng,
    cut: CutFlag,
}

impl Drop for Verdict {
    fn drop(&mut self) {
        if self.cut.load(std::sync::atomic::Ordering::SeqCst) {
            self.resilience
                .record_provider_failure(&self.provider, BreakerClass::default());
        } else {
            self.resilience.record_provider_success(&self.provider);
        }
    }
}

/// A chunk stream that carries its [`Verdict`] until it is dropped.
struct Settled {
    inner: ChunkStream,
    // Read only by its `Drop`, which runs when this stream is dropped.
    _verdict: Verdict,
}

impl Stream for Settled {
    type Item = Bytes;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Bytes>> {
        self.get_mut().inner.as_mut().poll_next(cx)
    }
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
    /// Whether `stream` ended because the upstream body failed. See [`CutFlag`].
    pub cut: CutFlag,
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
            cut: CutFlag::default(),
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
            cut: CutFlag::default(),
        }
    }
}

/// Failure that is not an HTTP status — transport error, TLS failure, or a
/// provider whose body never arrived. Always fail-over-able: no provider ever
/// produced a verdict on the request.
///
/// The abort is a **separate variant**, not another string, because it is the
/// one transport-shaped failure that is **not the provider's fault**: the
/// client hung up. Charging it to the provider cools a healthy endpoint and
/// can trip its breaker, so a burst of cancelled requests would take a
/// provider out of rotation for every other client. The reference makes the
/// same distinction — `isLocalStreamLifecycleError` treats an `AbortError` and
/// a client disconnect as *not* a provider failure
/// (`src/shared/utils/circuitBreaker.ts`).
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The client went away before the provider answered.
    ///
    /// The attempt loop stops here rather than failing over: there is nobody
    /// left to receive the failover's answer, and the provider did nothing
    /// wrong.
    #[error("upstream call aborted by caller")]
    Aborted,
    /// A transport-level failure the provider is responsible for.
    #[error("upstream transport error: {0}")]
    Transport(String),
}

impl From<String> for ExecError {
    fn from(message: String) -> Self {
        Self::Transport(message)
    }
}

impl ExecError {
    /// The message without the variant's prefix.
    ///
    /// The `Display` impl names the *kind* ("upstream transport error: …"),
    /// which is right for a log line and wrong for a message a client reads:
    /// the client's own text is the provider's reason, not our taxonomy. This
    /// is what the surfaces that echo the reason use.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Aborted => "upstream call aborted by caller",
            Self::Transport(message) => message,
        }
    }
}

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
///
/// Both entry points take the model to ask for rather than reading it from the
/// provider's configuration. A chain entry names a provider *and* a model, and a
/// provider serving three models from one endpoint is the ordinary case: reading
/// the model off the provider would make every target in a pool ask for the same
/// one, which is the defect `ChainTarget` exists to prevent. `post_chat` gets it
/// already applied to `canonical`; `post_media` takes it separately because a
/// media body carries the caller's spelling, not the chain's.
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
        model: Option<&'a str>,
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
        assert_eq!(
            (
                c.weight,
                c.in_flight,
                c.quota,
                c.context_window,
                c.cached_prefix_tokens
            ),
            (1, 0, None, 0, 0)
        );
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

    #[test]
    fn settle_stream_should_open_the_breaker_when_every_body_is_cut() {
        use super::{CutFlag, settle_stream};
        use crate::resilience::{BreakerClass, BreakerState, Resilience};
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        let resilience = Arc::new(Resilience::new());
        for _ in 0..BreakerClass::default().threshold() {
            let cut = CutFlag::default();
            cut.store(true, Ordering::SeqCst);
            let stream = settle_stream(
                Box::pin(futures::stream::empty()),
                Arc::clone(&resilience),
                "p1",
                cut,
            );
            drop(stream);
        }
        assert_eq!(resilience.breaker_state("p1"), BreakerState::Open);
    }

    #[test]
    fn settle_stream_should_credit_nothing_until_the_body_is_dropped() {
        use super::{CutFlag, settle_stream};
        use crate::resilience::{BreakerClass, BreakerState, Resilience};
        use std::sync::Arc;
        let resilience = Arc::new(Resilience::new());
        for _ in 0..BreakerClass::default().threshold() {
            resilience.record_provider_failure("p1", BreakerClass::default());
        }
        assert_eq!(resilience.breaker_state("p1"), BreakerState::Open);
        let open_stream = settle_stream(
            Box::pin(futures::stream::empty()),
            Arc::clone(&resilience),
            "p1",
            CutFlag::default(),
        );
        assert_eq!(resilience.breaker_state("p1"), BreakerState::Open);
        drop(open_stream);
    }
}
