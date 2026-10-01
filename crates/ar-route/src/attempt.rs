//! The attempt loop and its status classification.
//!
//! Ported from `../OmniRoute/open-sse/services/combo/comboAttemptLoop.ts` +
//! `executeTargetAttempt.ts` + `statusDecisionTable.ts`. P0 keeps the decision
//! rules and drops the machinery around them: no hedging, no per-model timeout
//! tasks, no set-retry rounds, no quota accounting. What survives decides
//! *retry, fail over, or stop* — see [`classify_status`].

use std::time::Duration;

use http::StatusCode;

use crate::contract::{CanonicalRequest, ExecError, Executor, ProviderId, Upstream};
use crate::error::RouteError;
use crate::resilience::Resilience;

/// Upper bound on providers tried for one request.
///
/// Three is the P0 number: the first candidate plus two fallbacks. Long enough
/// to ride out one provider's quota window, short enough that a request never
/// spends its whole 120s budget being failed over.
pub const MAX_ATTEMPTS: usize = 3;

/// How a request finished, from the router's point of view.
#[derive(Debug)]
pub enum AttemptOutcome {
    /// A provider answered 2xx. The upstream response travels with the verdict
    /// because the loop is the only place that knows *which* provider won.
    Succeeded {
        /// Provider that served the request.
        provider: ProviderId,
        /// How many providers were tried before it succeeded.
        attempts: u16,
        /// The upstream response, ready to relay.
        upstream: Upstream,
    },
    /// Every provider tried was rate-limited or overloaded. The client should
    /// come back after `after` — the aggregate window, i.e. the soonest any of
    /// the keys frees up.
    Retry {
        /// Aggregate wait before the client should retry.
        after: Duration,
        /// Provider that produced the last rate-limit verdict.
        provider: ProviderId,
        /// How many providers were tried.
        ///
        /// Carried because a throttled chain *did* spend attempts: reporting zero
        /// here made `ar_upstream_attempts_total` add nothing for exactly the
        /// requests that made the most upstream calls (audit F-MED-3).
        tried: u16,
    },
    /// The chain ran out after a non-rate-limit failure (5xx, transport, or an
    /// auth refusal on every key). A different chain might still succeed, so
    /// this is a gateway problem, not a client one.
    Failover {
        /// Status of the last upstream verdict. `502` when the failure was
        /// transport-level and produced no status at all.
        status: u16,
        /// Provider that produced it.
        provider: ProviderId,
        /// How many providers were tried.
        tried: u16,
    },
    /// Terminal: the request itself is bad, or there was nothing to route to.
    /// Retrying the same body anywhere produces the same answer.
    Abort(AbortReport),
}

impl AttemptOutcome {
    /// Status to return to the client.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Succeeded { upstream, .. } => upstream.status,
            Self::Retry { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::Failover { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            Self::Abort(report) => report.status,
        }
    }

    /// Provider that served or last refused the request, for the
    /// `x-ar-decision` header. `None` when the loop never dispatched.
    #[must_use]
    pub fn provider(&self) -> Option<&ProviderId> {
        match self {
            Self::Succeeded { provider, .. }
            | Self::Retry { provider, .. }
            | Self::Failover { provider, .. } => Some(provider),
            Self::Abort(_) => None,
        }
    }

    /// Attempts spent, for `x-ar-usage` and `ar_upstream_attempts_total`.
    ///
    /// Every terminal arm reports what the loop actually dispatched, the
    /// throttled one included.
    #[must_use]
    pub fn attempts(&self) -> u16 {
        match self {
            Self::Succeeded { attempts, .. } => *attempts,
            Self::Failover { tried, .. }
            | Self::Retry { tried, .. }
            | Self::Abort(AbortReport { tried, .. }) => *tried,
        }
    }
}

/// Why the loop stopped for good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbortReport {
    /// Status to return to the client.
    pub status: StatusCode,
    /// Short human-readable reason. Never contains the request body.
    pub reason: String,
    /// How many providers were tried before stopping.
    pub tried: u16,
}

/// One attempt's classification. Internal to the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Upstream answered 2xx.
    Success,
    /// Move to the next provider in the chain.
    Failover { status: u16 },
    /// Stop the whole request.
    Abort { status: StatusCode },
}

/// 400 bodies that mean "this request is broken", not "this provider cannot
/// serve it".
///
/// Ported verbatim from `COMBO_400_STOP_ROWS`. The upstream comment is why
/// the list is this narrow: a blanket `contains("invalid")` rule turns every
/// model-scoped rejection into a hard stop and a one-line provider-side fix
/// into a 400 at the edge.
const STOP_400_ROWS: &[&str] =
    &["invalid message format", "malformed", "context", "prompt", "token"];

/// 400 bodies that are a *provider capability* limit, which another provider
/// may well not share. `comboPredicates.ts` advances on these before the stop
/// rows are consulted, and the ordering is load-bearing: without it
/// "maximum context length" trips the `context` stop row and a 200k prompt
/// becomes a hard 400 instead of a failover to a bigger model.
const ADVANCE_400_ROWS: &[&str] = &[
    "context length",
    "maximum context",
    "too many tokens",
    "unsupported parameter",
    "unrecognized request argument",
    "does not support",
];

/// Classifies one upstream status into the loop's next move.
///
/// Pure: no clock, no state, no I/O. Everything the decision needs is in the
/// arguments, which is what makes the whole table unit-testable.
///
/// Timing is *not* decided here. `Retry-After` handling lives in
/// [`Resilience::record_failure`], next to the backoff it has to beat — the
/// same split as the upstream's `connectionCooldown.ts`.
///
/// * **2xx** → `Success`.
/// * **400 matching a stop row** → `Abort`. The body is wrong for every
///   provider, and replaying it is pure latency.
/// * **everything else** → `Failover`. 429, 5xx, 401/403, 408/499 and other
///   4xx are all key- or model-scoped: another provider may accept the request.
#[must_use]
fn classify_status(status: u16, body: &str) -> Step {
    if (200..300).contains(&status) {
        return Step::Success;
    }
    if status == 400 && is_stop_400(body) {
        return Step::Abort { status: StatusCode::BAD_REQUEST };
    }
    Step::Failover { status }
}

/// Whether a 400 body is a client-shape failure rather than a model refusal.
fn is_stop_400(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if ADVANCE_400_ROWS.iter().any(|row| lower.contains(row)) {
        return false;
    }
    STOP_400_ROWS.iter().any(|row| lower.contains(row))
}

/// Runs `canonical` against `fallback_chain` until one provider answers.
///
/// `fallback_chain[0]` is where [`crate::pick`] put the winner; the rest are
/// fallbacks. A provider whose key is already cooling is skipped without
/// spending an attempt, and no provider is tried twice.
///
/// # Errors
/// Never in P0: the loop has no fallible setup. It returns `Result` because
/// the P0 contract fixes that signature and because the first version that
/// consults a `Router` (P2, `auto/*`) will need it.
pub async fn attempt_loop<E: Executor + ?Sized>(
    canonical: &CanonicalRequest,
    fallback_chain: &[ProviderId],
    exec: &E,
    resilience: &Resilience,
) -> Result<AttemptOutcome, RouteError> {
    if fallback_chain.is_empty() {
        return Ok(AttemptOutcome::Abort(AbortReport {
            status: StatusCode::SERVICE_UNAVAILABLE,
            reason: "empty fallback chain".to_owned(),
            tried: 0,
        }));
    }

    let mut tried: u16 = 0;
    // Last verdict: (status, provider, cooldown, was_rate_limit). Transport
    // failures record 502 so the Failover arm never has to invent a status.
    let mut last: Option<(u16, ProviderId, Duration, bool)> = None;
    // Only *throttled* attempts contribute to the aggregate window. A transport
    // failure gets our own 3s guess, and min()-ing that into the client's
    // deadline would advertise "come back in 3 seconds" when a provider has
    // explicitly said 30. Our guess is not information about availability.
    let mut earliest_throttled: Option<Duration> = None;
    // Tracked separately from `last`: a 429 on p1 followed by a dead socket on
    // p2 is still "come back in 30s", not "the gateway is broken".
    let mut throttled_by: Option<ProviderId> = None;

    for provider in fallback_chain {
        if tried as usize >= MAX_ATTEMPTS {
            break;
        }
        // A key already cooling is a known-dead key: skipping it costs one loop
        // iteration, spending an attempt on it costs a round trip and a second
        // 429. `is_cooling` also sweeps, so this is the self-cleaning path.
        if resilience.is_cooling(provider.as_str()) {
            tracing::debug!(provider = %provider, "skipping cooling provider");
            continue;
        }

        tried += 1;
        let verdict = match exec.call(provider, canonical).await {
            Ok(upstream) => match classify_status(upstream.status.as_u16(), &body_text(&upstream)) {
                Step::Success => {
                    // Success clears all error state for this key
                    // (`connectionCooldown.ts`), so the next failure of an
                    // otherwise-healthy provider starts from `base` again.
                    resilience.record_success(provider.as_str());
                    return Ok(AttemptOutcome::Succeeded {
                        provider: provider.clone(),
                        attempts: tried,
                        upstream,
                    });
                }
                Step::Abort { status } => {
                    return Ok(AttemptOutcome::Abort(AbortReport {
                        status,
                        reason: abort_reason(status),
                        tried,
                    }));
                }
                Step::Failover { status } => {
                    let cooldown =
                        resilience.record_failure(provider.as_str(), upstream.retry_after);
                    tracing::warn!(
                        provider = %provider,
                        status,
                        cooldown_ms = cooldown.as_millis() as u64,
                        "attempt failed over"
                    );
                    (status, provider.clone(), cooldown, status == 429)
                }
            },
            Err(ExecError(msg)) => {
                // No verdict from the provider at all: charge a cooldown so the
                // next request does not walk into the same dead socket.
                let cooldown = resilience.record_failure(provider.as_str(), None);
                tracing::warn!(provider = %provider, error = %msg, "transport failure, failing over");
                (502, provider.clone(), cooldown, false)
            }
        };
        if verdict.3 {
            earliest_throttled =
                Some(earliest_throttled.map_or(verdict.2, |prev: Duration| prev.min(verdict.2)));
            throttled_by = Some(verdict.1.clone());
        }
        last = Some(verdict);
    }

    Ok(terminate(last, throttled_by, earliest_throttled, tried))
}

/// Collapses the chain's verdicts into the loop's terminal outcome.
///
/// A throttle *anywhere* in the chain outranks a later transport failure. The
/// alternative — reporting only the last verdict — turns "your key is busy
/// until 08:14" into a 502, which reads as "this gateway is broken" and sends
/// the caller to the wrong dashboard.
///
/// `earliest` is the soonest *throttled* key, never a transport-failure
/// backoff: a dead socket's 3s guess is our own, and advertising it would invite
/// the client back before the one provider that actually said "not before 30s".
fn terminate(
    last: Option<(u16, ProviderId, Duration, bool)>,
    throttled_by: Option<ProviderId>,
    earliest: Option<Duration>,
    tried: u16,
) -> AttemptOutcome {
    let Some((status, provider, cooldown, _)) = last else {
        // Nothing was tried: every key was already cooling.
        return AttemptOutcome::Abort(AbortReport {
            status: StatusCode::SERVICE_UNAVAILABLE,
            reason: "every provider in the chain is cooling down".to_owned(),
            tried,
        });
    };
    if let Some(throttled) = throttled_by {
        let after = earliest
            .or(Some(cooldown))
            .unwrap_or(Duration::from_secs(1))
            .max(Duration::from_millis(1));
        return AttemptOutcome::Retry { after, provider: throttled, tried };
    }
    AttemptOutcome::Failover { status, provider, tried }
}

/// UTF-8 body of a failed upstream, lossily decoded and capped.
///
/// The cap is not a size optimisation: a 400 body is scanned by
/// [`is_stop_400`], and a megabyte of echoed prompt is both a pointless scan
/// and a way to get user content into a log line.
fn body_text(upstream: &Upstream) -> String {
    const MAX_BODY_SCAN: usize = 2048;
    let end = upstream.error_body.len().min(MAX_BODY_SCAN);
    String::from_utf8_lossy(&upstream.error_body[..end]).into_owned()
}

/// Client-safe reason for an abort. Never echoes the upstream body.
fn abort_reason(status: StatusCode) -> String {
    match status {
        StatusCode::BAD_REQUEST => "upstream rejected the request shape".to_owned(),
        other => format!("upstream returned {other}"),
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use bytes::Bytes;
    use http::StatusCode;

    use super::{MAX_ATTEMPTS, attempt_loop, classify_status, Step};
    use crate::contract::{CanonicalRequest, ExecError, Executor, ProviderId, Upstream};
    use crate::resilience::Resilience;
    use crate::AttemptOutcome;

    /// A cloneable verdict description; `Upstream` is not `Clone` because it
    /// owns a boxed stream, so the script stores this instead and builds a
    /// fresh `Upstream` per call.
    #[derive(Clone, Debug)]
    enum Verdict {
        Ok,
        RateLimited(Option<Duration>),
        Status(u16, &'static str),
        Transport(&'static str),
    }

    impl Verdict {
        fn build(&self) -> Result<Upstream, ExecError> {
            match *self {
                Self::Ok => Ok(Upstream::success(Box::pin(futures::stream::empty()))),
                Self::RateLimited(retry_after) => Ok(Upstream::failure(
                    StatusCode::TOO_MANY_REQUESTS,
                    Bytes::from_static(b"slow down"),
                    retry_after,
                )),
                Self::Status(code, body) => Ok(Upstream::failure(
                    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                    Bytes::from_static(body.as_bytes()),
                    None,
                )),
                Self::Transport(msg) => Err(ExecError(msg.to_owned())),
            }
        }
    }

    /// Replays a fixed verdict script and records which providers it saw.
    struct Scripted {
        script: Vec<Verdict>,
        calls: AtomicUsize,
        seen: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(script: Vec<Verdict>) -> Self {
            Self {
                script,
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().map(|s| s.clone()).unwrap_or_default()
        }
    }

    impl Executor for Scripted {
        fn call<'a>(
            &'a self,
            provider: &'a ProviderId,
            _canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            if let Ok(mut s) = self.seen.lock() {
                s.push(provider.as_str().to_owned());
            }
            let idx = self.calls.fetch_add(1, Ordering::Relaxed);
            let verdict = self.script.get(idx).cloned().unwrap_or(Verdict::Ok);
            Box::pin(async move { verdict.build() })
        }
    }

    const MALFORMED: Verdict = Verdict::Status(400, r#"{"error":"invalid message format"}"#);
    const OVERFLOW: Verdict = Verdict::Status(400, "maximum context length is 8192 tokens");
    const PROMPT_SHAPE: Verdict = Verdict::Status(400, "prompt is malformed");

    fn req() -> CanonicalRequest {
        CanonicalRequest::new("m", Bytes::from_static(b"{}"))
    }

    fn chain(names: &[&str]) -> Vec<ProviderId> {
        names.iter().map(|n| ProviderId::new(*n)).collect()
    }

    /// The loop only awaits ready futures, and the crate has no async runtime
    /// dependency; `futures::executor::block_on` is enough and costs no new dep.
    fn block<F: Future>(f: F) -> F::Output {
        futures::executor::block_on(f)
    }

    #[test]
    fn fails_over_when_provider_429() {
        // p1 is rate-limited, p2 answers 200: the loop must report p2 served it.
        let exec = Scripted::new(vec![Verdict::RateLimited(Some(Duration::from_secs(2))), Verdict::Ok]);
        let r = Resilience::new();
        let got = block(attempt_loop(&req(), &chain(&["p1", "p2"]), &exec, &r));
        let Ok(AttemptOutcome::Succeeded { provider, .. }) = got else {
            panic!("expected success, got {got:?}");
        };
        assert_eq!(provider.as_str(), "p2");
    }

    #[test]
    fn puts_rate_limited_provider_in_cooldown() {
        let exec = Scripted::new(vec![Verdict::RateLimited(Some(Duration::from_secs(30))), Verdict::Ok]);
        let r = Resilience::new();
        let _ = block(attempt_loop(&req(), &chain(&["p1", "p2"]), &exec, &r));
        assert!(r.is_cooling("p1"));
    }

    #[test]
    fn retries_with_soonest_window_when_all_rate_limited() {
        let exec = Scripted::new(vec![
            Verdict::RateLimited(Some(Duration::from_secs(9))),
            Verdict::RateLimited(Some(Duration::from_secs(4))),
        ]);
        let r = Resilience::new();
        let got = block(attempt_loop(&req(), &chain(&["p1", "p2"]), &exec, &r));
        let Ok(AttemptOutcome::Retry { after, provider, tried }) = got else {
            panic!("expected retry, got {got:?}");
        };
        // Backoff is per key and p2 has no prior failure, so p1 cools for
        // max(base=3s, 9s)=9s and p2 for max(3s, 4s)=4s. The client is told
        // the soonest, not the latest — and both were tried, so `tried` is 2.
        assert_eq!((after, provider.as_str(), tried), (Duration::from_secs(4), "p2", 2));
    }

    #[test]
    fn retries_when_a_throttle_precedes_a_transport_error() {
        // p1 throttles, p2 is unreachable. Reporting 502 would blame the
        // gateway for the client's own rate-limit window.
        let exec = Scripted::new(vec![
            Verdict::RateLimited(Some(Duration::from_secs(30))),
            Verdict::Transport("reset"),
        ]);
        let r = Resilience::new();
        let got = block(attempt_loop(&req(), &chain(&["p1", "p2"]), &exec, &r));
        let Ok(AttemptOutcome::Retry { after, provider, tried }) = got else {
            panic!("expected retry, got {got:?}");
        };
        // p1 said 30s; p2 only got our own 3s guess, which must not shorten it.
        assert_eq!((after, provider.as_str(), tried), (Duration::from_secs(30), "p1", 2));
    }

    #[test]
    fn stops_on_malformed_400() {
        let exec = Scripted::new(vec![MALFORMED]);
        let r = Resilience::new();
        let got = block(attempt_loop(&req(), &chain(&["p1", "p2"]), &exec, &r));
        let Ok(AttemptOutcome::Abort(report)) = got else {
            panic!("expected abort, got {got:?}");
        };
        assert_eq!((report.status, report.tried), (StatusCode::BAD_REQUEST, 1));
    }

    #[test]
    fn never_dispatches_second_provider_after_client_fault() {
        let exec = Scripted::new(vec![PROMPT_SHAPE]);
        let _ = block(attempt_loop(&req(), &chain(&["p1", "p2"]), &exec, &Resilience::new()));
        assert_eq!(exec.seen(), ["p1"]);
    }

    #[test]
    fn fails_over_on_context_overflow_400() {
        let exec = Scripted::new(vec![OVERFLOW, Verdict::Transport("also down")]);
        let got = block(attempt_loop(&req(), &chain(&["p1", "p2"]), &exec, &Resilience::new()));
        assert!(matches!(got, Ok(AttemptOutcome::Failover { .. })));
    }

    #[test]
    fn fails_over_on_transport_error() {
        let exec = Scripted::new(vec![Verdict::Transport("dns"), Verdict::Ok]);
        let r = Resilience::new();
        let got = block(attempt_loop(&req(), &chain(&["p1", "p2"]), &exec, &r));
        let Ok(AttemptOutcome::Succeeded { provider, .. }) = got else {
            panic!("expected success, got {got:?}");
        };
        assert_eq!(provider.as_str(), "p2");
    }

    #[test]
    fn reports_502_when_only_transport_errors() {
        let exec = Scripted::new(vec![Verdict::Transport("dns"), Verdict::Transport("reset")]);
        let got = block(attempt_loop(&req(), &chain(&["p1", "p2"]), &exec, &Resilience::new()));
        let Ok(AttemptOutcome::Failover { status, tried, .. }) = got else {
            panic!("expected failover, got {got:?}");
        };
        assert_eq!((status, tried), (502, 2));
    }

    #[test]
    fn aborts_when_chain_empty() {
        let exec = Scripted::new(vec![]);
        let got = block(attempt_loop(&req(), &[], &exec, &Resilience::new()));
        let Ok(AttemptOutcome::Abort(report)) = got else {
            panic!("expected abort, got {got:?}");
        };
        assert_eq!(report.tried, 0);
    }

    #[test]
    fn aborts_when_every_key_cooling() {
        let exec = Scripted::new(vec![]);
        let r = Resilience::new();
        r.record_failure("p1", None);
        r.record_failure("p2", None);
        let got = block(attempt_loop(&req(), &chain(&["p1", "p2"]), &exec, &r));
        let Ok(AttemptOutcome::Abort(report)) = got else {
            panic!("expected abort, got {got:?}");
        };
        assert_eq!(report.tried, 0);
    }

    #[test]
    fn spends_at_most_max_attempts() {
        let exec = Scripted::new(vec![
            Verdict::Transport("a"),
            Verdict::Transport("b"),
            Verdict::Transport("c"),
        ]);
        let names: Vec<String> = (0..10).map(|i| format!("p{i}")).collect();
        let chain: Vec<ProviderId> = names.iter().map(ProviderId::new).collect();
        let got = block(attempt_loop(&req(), &chain, &exec, &Resilience::new()));
        assert_eq!(exec.seen().len(), MAX_ATTEMPTS);
        assert!(matches!(got, Ok(AttemptOutcome::Failover { tried, .. }) if tried as usize == MAX_ATTEMPTS));
    }

    #[test]
    fn skips_cooling_provider_without_spending_attempt() {
        let exec = Scripted::new(vec![]);
        let r = Resilience::new();
        r.record_failure("p1", None);
        let _ = block(attempt_loop(&req(), &chain(&["p1"]), &exec, &r));
        assert!(exec.seen().is_empty());
    }

    #[test]
    fn classifies_2xx_as_success() {
        assert_eq!(classify_status(200, ""), Step::Success);
    }

    #[test]
    fn classifies_429_as_failover() {
        assert_eq!(classify_status(429, "slow down"), Step::Failover { status: 429 });
    }

    #[test]
    fn classifies_500_as_failover() {
        assert_eq!(classify_status(500, "boom"), Step::Failover { status: 500 });
    }

    #[test]
    fn classifies_auth_refusal_as_failover() {
        assert_eq!(classify_status(401, "bad key"), Step::Failover { status: 401 });
    }

    #[test]
    fn classifies_400_prompt_shape_as_abort() {
        assert_eq!(
            classify_status(400, "prompt is malformed"),
            Step::Abort { status: StatusCode::BAD_REQUEST }
        );
    }
}
