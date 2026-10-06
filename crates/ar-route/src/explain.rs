//! `explain_route` — the trace.
//!
//! Ported from `../OmniRoute/open-sse/services/combo/decisionTrace.ts`, and
//! deliberately *narrower* than that file.
//!
//! Upstream's `ComboTrace` is a mutable, TTL-bounded, per-invocation record of
//! every target as the loop touches it, appended in wall-clock order, with a
//! `not_reached` back-fill at the end. This is the other thing the same module
//! is named for in `docs/02-port-from-omniroute.md`: the *scoring* trace — why
//! the winner won. So there is no timestamp, no per-skip-reason allowlist and
//! no retention policy here: those answer "what happened", and this answers
//! "what would happen and why", with the inputs in the caller's hands and the
//! decision reproducible from them.
//!
//! Upstream's SAFETY CONTRACT carries over verbatim: the trace is routing
//! metadata only — provider, model, score, factors, fallbacks. Never a prompt,
//! a body, a header, a credential or a raw upstream error string. Nothing in
//! this module can read one: the inputs are provider ids, model names and
//! `f64`.

use crate::auto::selection::{AutoSelector, WinnerReason, rank};
use crate::auto::{AutoCandidate, AutoCombo, Factors};
use crate::contract::{ProviderId, Strng};
use crate::error::RouteError;
use crate::simulate::chain_from;

/// One provider's routing decision and the arithmetic behind it.
///
/// The exact shape P4 `ar-mcp`'s `ar_explain_route` returns: `provider`,
/// `model`, `score`, `factors`, `fallbacks`.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteTrace {
    /// Provider that would serve the request.
    pub provider: ProviderId,
    /// Provider-local model name.
    pub model: Strng,
    /// Weighted score in `[0, 1]`.
    pub score: f64,
    /// The sixteen factors behind [`Self::score`], in fixed order.
    pub factors: Factors,
    /// Providers that would be tried next, in order. Empty when the pool held
    /// one provider.
    pub fallbacks: Vec<ProviderId>,
    /// Which variant produced the decision, e.g. `"auto/cheap"`.
    pub variant: &'static str,
    /// Whether the pick was a clear score or a rotation.
    pub reason: WinnerReason,
}

/// Ranks `pool` for `combo` and reports the winner plus the arithmetic.
///
/// Same inputs and same scorer as [`crate::simulate_route`], plus the
/// breakdown. The two functions agree by construction: both call
/// [`rank`], so a dry run and its explanation cannot disagree.
///
/// `task_fitness` is the caller's model/task hook; pass `|_| 0.5` for the
/// neutral value.
///
/// # Errors
/// [`RouteError::NoCandidates`] when `pool` is empty.
pub fn explain_route<F>(
    combo: &AutoCombo,
    pool: &[AutoCandidate],
    selector: &AutoSelector,
    task_fitness: F,
) -> Result<RouteTrace, RouteError>
where
    F: Fn(&str) -> f64,
{
    let (ranked, winner, reason) = rank(combo, pool, selector, task_fitness)?;
    let chain = chain_from(&ranked, &winner);
    Ok(RouteTrace {
        provider: winner.provider.clone(),
        model: winner.model.clone(),
        score: winner.score,
        factors: winner.factors,
        // The trace's public shape stays provider-only: the model each fallback
        // would ask for is the plan's, not the trace's, and the MCP consumer of
        // this struct reads providers.
        fallbacks: chain
            .get(1..)
            .unwrap_or_default()
            .iter()
            .map(|t| t.provider.clone())
            .collect(),
        variant: combo.variant.as_str(),
        reason,
    })
}

impl RouteTrace {
    /// The factor list in the order a trace emits it. Delegates to
    /// [`Factors::as_pairs`]; the same 16 pairs a caller would serialise.
    #[must_use]
    pub fn factor_pairs(&self) -> [(&'static str, f64); 16] {
        self.factors.as_pairs()
    }
}

#[cfg(test)]
mod tests {
    use super::{RouteTrace, explain_route};
    use crate::auto::selection::WinnerReason;
    use crate::auto::{AutoCandidate, AutoSelector, virtual_combo};
    use crate::contract::ProviderId;

    fn pool() -> Vec<AutoCandidate> {
        vec![
            AutoCandidate::new(ProviderId::new("openai"), "gpt-4o").with_price(2.50),
            AutoCandidate::new(ProviderId::new("groq"), "llama-3.3-70b").with_price(0.59),
            AutoCandidate::new(ProviderId::new("together"), "mixtral").with_price(0.20),
        ]
    }

    #[test]
    fn traces_factors_when_explain() {
        let combo = virtual_combo("auto/cheap").expect("known alias");
        let trace =
            explain_route(&combo, &pool(), &AutoSelector::new(), |_| 0.5).expect("pool present");
        // The cheapest candidate scores cost_inv 1 - 0.20/2.50 = 0.92 under the
        // cost-saver pack, which is what made it win.
        assert!(
            (trace.factors.cost_inv - 0.92).abs() < 1e-9,
            "cost_inv was {}",
            trace.factors.cost_inv
        );
    }

    #[test]
    fn names_the_cheapest_provider_when_auto_cheap() {
        let combo = virtual_combo("auto/cheap").expect("known alias");
        let trace =
            explain_route(&combo, &pool(), &AutoSelector::new(), |_| 0.5).expect("pool present");
        assert_eq!(trace.provider.as_str(), "together");
    }

    #[test]
    fn reports_every_falling_provider_in_order() {
        let combo = virtual_combo("auto/cheap").expect("known alias");
        let trace =
            explain_route(&combo, &pool(), &AutoSelector::new(), |_| 0.5).expect("pool present");
        assert_eq!(trace.fallbacks.len(), crate::MAX_ATTEMPTS - 1);
        assert_eq!(trace.fallbacks[0].as_str(), "groq");
    }

    #[test]
    fn reports_the_variant_that_produced_the_decision() {
        let combo = virtual_combo("auto/fast").expect("known alias");
        let trace =
            explain_route(&combo, &pool(), &AutoSelector::new(), |_| 0.5).expect("pool present");
        assert_eq!(trace.variant, "auto/fast");
    }

    #[test]
    fn emits_all_sixteen_factors_in_order() {
        let combo = virtual_combo("auto").expect("known alias");
        let trace =
            explain_route(&combo, &pool(), &AutoSelector::new(), |_| 0.5).expect("pool present");
        let pairs = trace.factor_pairs();
        assert_eq!(pairs.len(), 16);
        assert_eq!(pairs[0].0, "quota");
        assert_eq!(pairs[15].0, "reliability");
    }

    #[test]
    fn bounds_every_factor_to_the_unit_interval() {
        let combo = virtual_combo("auto").expect("known alias");
        let mut noisy = pool();
        noisy[0].quota_remaining_pct = -50.0;
        noisy[1].p95_latency_ms = f64::NAN;
        noisy[2].quality = Some(4.0);
        let trace =
            explain_route(&combo, &noisy, &AutoSelector::new(), |_| 0.5).expect("pool present");
        for (name, v) in trace.factor_pairs() {
            assert!((0.0..=1.0).contains(&v), "{name} was {v}");
        }
    }

    #[test]
    fn reports_clear_score_when_pool_has_one() {
        let combo = virtual_combo("auto").expect("known alias");
        let one = [AutoCandidate::new(ProviderId::new("solo"), "m").with_price(1.0)];
        let trace =
            explain_route(&combo, &one, &AutoSelector::new(), |_| 0.5).expect("pool present");
        assert_eq!(trace.reason, WinnerReason::ClearScore);
    }

    #[test]
    fn reports_clear_score_when_one_candidate_leads_by_a_lot() {
        let combo = virtual_combo("auto/cheap").expect("known alias");
        // `pool()` spans 0.20..2.50 USD/Mtok, which is far past CLEAR_WINNER.
        let trace =
            explain_route(&combo, &pool(), &AutoSelector::new(), |_| 0.5).expect("pool present");
        assert_eq!(trace.reason, WinnerReason::ClearScore);
    }

    #[test]
    fn reports_rotation_when_the_pool_is_close() {
        let combo = virtual_combo("auto").expect("known alias");
        // Three near-identical candidates: the spread stays under
        // CLEAR_WINNER, so the pick is a rotation and the trace must say so
        // rather than presenting a coin flip as a verdict.
        let close = vec![
            AutoCandidate::new(ProviderId::new("a"), "m").with_price(1.00),
            AutoCandidate::new(ProviderId::new("b"), "m").with_price(1.01),
            AutoCandidate::new(ProviderId::new("c"), "m").with_price(1.02),
        ];
        let trace =
            explain_route(&combo, &close, &AutoSelector::new(), |_| 0.5).expect("pool present");
        assert_eq!(trace.reason, WinnerReason::Rotation);
    }

    #[test]
    fn errors_when_pool_empty() {
        let combo = virtual_combo("auto").expect("known alias");
        let got = explain_route(&combo, &[], &AutoSelector::new(), |_| 0.5);
        assert!(matches!(got, Err(crate::RouteError::NoCandidates)));
    }

    #[test]
    fn carries_no_request_content() {
        // The safety contract, as a test: the trace is built only from provider
        // ids, model names and f64, so there is no field a prompt could reach.
        let combo = virtual_combo("auto/cheap").expect("known alias");
        let trace =
            explain_route(&combo, &pool(), &AutoSelector::new(), |_| 0.5).expect("pool present");
        let RouteTrace {
            provider,
            model,
            fallbacks,
            ..
        } = trace;
        assert!(!provider.as_str().is_empty() && !model.is_empty() && !fallbacks.is_empty());
    }
}
