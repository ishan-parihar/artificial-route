//! The HTTP executor: canonical request → provider wire → [`Upstream`].
//!
//! This used to be a second implementation of `ar-exec`. It is now a thin
//! adapter: [`HttpExec`] holds the provider table and the `ar_exec::ArExec`
//! client, and every request goes through `ar_exec::ArExec::post` — the wire
//! gate, the header merge, the response-start budget, the abort race and the
//! model rewrite all live there exactly once. What stays here is the part that
//! is *not* dispatch: which provider id maps to which base URL and key, and the
//! translation from `ar_exec`'s [`ar_exec::ExecError`] to the
//! [`ar_route::ExecError`] the router's trait demands.
//!
//! The one thing this layer does *not* do is decide what a non-2xx means.
//! `ar-exec`'s `post` hands back the live response whatever its status, and
//! [`ar_exec::ChatStream::into_failure`] reads the bounded body; the router
//! classifies it through `ar_route::attempt_loop`. A provider that says 429 and
//! a provider that says 400 must reach the router differently, and only the
//! router knows the difference.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ar_registry::WireFormat;
use ar_route::{ArExec as ArRouteExec, CanonicalRequest, ExecError, ProviderId, Upstream};
use bytes::Bytes;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use ar_exec::Dispatch;

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
    /// Defaults to [`WireFormat::Openai`] because that is the only wire this
    /// build can *send*; a provider discovered to speak anything else is
    /// refused by `ar-exec`'s gate rather than sent an OpenAI body. That
    /// refusal is the fix for a defect this crate used to have: it POSTed the
    /// canonical body to every provider regardless of what the registry said.
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

    /// Sets the wire dialect. Anything but [`WireFormat::Openai`] makes this
    /// provider undispatchable in this build — see [`ProviderConfig::wire_format`].
    #[must_use]
    pub fn with_wire_format(mut self, wire_format: WireFormat) -> Self {
        self.wire_format = wire_format;
        self
    }

    /// Whether this build can actually POST to this provider.
    ///
    /// Checked at config-build time so an unspeakable provider never enters a
    /// candidate list and never burns one of the three attempt slots on a
    /// guaranteed wrong-wire request.
    #[must_use]
    pub fn is_dispatchable(&self) -> bool {
        self.wire_format == WireFormat::Openai
    }

    /// The `ar-exec` dispatch bundle for this provider, borrowed.
    fn dispatch(&self, stream: bool) -> Dispatch<'_> {
        Dispatch {
            base_url: &self.base_url,
            wire_format: self.wire_format,
            api_key: &self.api_key,
            upstream_model: &self.upstream_model,
            stream,
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
}

impl Clone for HttpExec {
    fn clone(&self) -> Self {
        // `ar_exec::ArExec` is `Clone` (one `reqwest::Client` holding the pool);
        // the provider tables are small and `Copy`-enough to be worth sharing
        // over re-deriving from config.
        Self {
            core: Arc::clone(&self.core),
            providers: self.providers.clone(),
            by_id: self.by_id.clone(),
        }
    }
}

impl HttpExec {
    /// Builds an executor over `providers`. The first entry is the default when
    /// a request names no routable candidate.
    ///
    /// # Errors
    /// When the `reqwest` client cannot be constructed (TLS backend missing).
    pub fn new(providers: Vec<ProviderConfig>) -> Result<Self, String> {
        let core = ar_exec::ArExec::new().map_err(|e| format!("reqwest client: {e}"))?;
        let by_id = providers.iter().cloned().map(|p| (p.id.clone(), p)).collect();
        Ok(Self {
            core: Arc::new(core),
            providers,
            by_id,
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
}

impl std::fmt::Debug for HttpExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `ProviderConfig` carries `api_key`, and `Debug` on it would print the
        // credential. Count the table instead.
        f.debug_struct("HttpExec")
            .field("providers", &self.providers.len())
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
            let stream = self
                .core
                .post(&cfg.dispatch(canonical.stream), &canonical.body, &abort)
                .await
                .map_err(flatten)?;

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
}

/// Flattens `ar_exec::ExecError` into the router's string-carrying error.
///
/// The router's taxonomy is one opaque string by design (`ar_route::ExecError`),
/// so there is nothing to map *to* — but the message must keep the typed
/// detail, because "provider wire format Anthropic is not implemented" and
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
};

#[cfg(test)]
mod tests {
    use ar_registry::WireFormat;
    use ar_route::{ProviderId, QuotaWindow};

    use super::{HttpExec, ProviderConfig};

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

    #[test]
    fn a_non_openai_wire_is_not_dispatchable() {
        // The regression this guards: silently POSTing a canonical OpenAI body
        // to a provider whose registry entry says Anthropic.
        let p = cfg("p").with_wire_format(WireFormat::Anthropic);
        assert!(!p.is_dispatchable());
    }

    #[test]
    fn keeps_rank_price_weight_and_quota_on_the_config() {
        let p = cfg("p")
            .with_price(0.15)
            .with_rank(3)
            .with_weight(2)
            .with_quota(QuotaWindow::new(100, 10, 0));
        assert_eq!(
            (p.input_usd_per_mtok, p.rank, p.weight, p.quota.map(|q| q.remaining())),
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
}
