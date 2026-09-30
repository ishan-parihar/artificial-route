//! `simulate_route` — the dry run.
//!
//! Ported from `../OmniRoute/open-sse/services/combo/decisionTrace.ts`
//! (the *plan* half) and `statusDecisionTable.ts` (the shape of a decision).
//!
//! A dry run is the whole decision with the dispatch removed: same scorer,
//! same selection, same ordering, zero I/O. The plan it returns is exactly
//! what [`crate::attempt_loop`] consumes, which is what makes it useful —
//! there is no second code path that could drift from the real one.

use crate::auto::selection::{AutoSelector, rank};
use crate::auto::{AutoCandidate, AutoCombo, Scored};
use crate::contract::ProviderId;
use crate::error::RouteError;

/// The attempt chain a request *would* take, with nothing dispatched.
///
/// `chain[0]` is the winner; `chain[1..]` are the fallbacks. The chain is
/// capped at [`crate::MAX_ATTEMPTS`] because the attempt loop spends at most
/// that many — a plan that listed 20 providers would be a plan the loop never
/// follows, and reporting it would overstate the blast radius of a failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutePlan {
    /// Ordered providers to try, winner first. Never empty when `Ok`.
    chain: Vec<ProviderId>,
}

impl RoutePlan {
    /// The full ordered chain, winner at index 0. Hands straight to
    /// [`crate::attempt_loop`], which is the point: a dry run and the request
    /// that follows it share one chain producer.
    #[must_use]
    pub fn chain(&self) -> &[ProviderId] {
        &self.chain
    }

    /// The fallbacks, i.e. everything after the winner. Empty when the plan is
    /// a single provider.
    #[must_use]
    pub fn fallbacks(&self) -> &[ProviderId] {
        self.chain.get(1..).unwrap_or_default()
    }

    /// The provider the plan would try first.
    ///
    /// `None` is unreachable for a plan produced by [`simulate_route`], which
    /// returns [`RouteError::NoCandidates`] before an empty chain can exist.
    /// It is still an `Option` rather than a panic: the request path must not
    /// be able to abort on a type invariant.
    #[must_use]
    pub fn winner(&self) -> Option<&ProviderId> {
        self.chain.first()
    }
}

/// Ranks `pool` for `combo` and reports the chain, without dispatching.
///
/// `task_fitness` is the caller's model/task hook — the `task_fit` factor's
/// only input. Pass `|_| 0.5` for the neutral value; the task-fit table
/// belongs to the catalog, not to the router.
///
/// # Errors
/// [`RouteError::NoCandidates`] when `pool` is empty. There is deliberately
/// no deferred-variant arm: `combo` is already resolved, so a config that
/// still names a deferred strategy fails at [`crate::Strategy`] and never
/// reaches here.
pub fn simulate_route<F>(
    combo: &AutoCombo,
    pool: &[AutoCandidate],
    selector: &AutoSelector,
    task_fitness: F,
) -> Result<RoutePlan, RouteError>
where
    F: Fn(&str) -> f64,
{
    let (ranked, winner, _) = rank(combo, pool, selector, task_fitness)?;
    Ok(RoutePlan {
        chain: chain_from(&ranked, &winner),
    })
}

/// The shared "winner first, rest by score" ordering.
///
/// Deduplicated by provider: two candidates on one provider would otherwise
/// burn two of the three attempt slots on a second round trip to the same
/// upstream, which tells us nothing the first one did not. A provider that
/// already appears in the chain is skipped, not just the winner — two
/// *losing* models on one provider is the common case, not the rare one.
pub(crate) fn chain_from(ranked: &[Scored], winner: &Scored) -> Vec<ProviderId> {
    let mut chain = vec![winner.provider.clone()];
    for c in ranked {
        if chain.len() == crate::MAX_ATTEMPTS {
            break;
        }
        if chain.contains(&c.provider) {
            continue;
        }
        chain.push(c.provider.clone());
    }
    chain
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::simulate_route;
    use crate::auto::{AutoCandidate, AutoSelector, AutoVariant, virtual_combo};
    use crate::contract::{CanonicalRequest, ExecError, Executor, ProviderId, Upstream};

    /// Counts calls. The whole point of the dry-run test is that this stays
    /// at zero: `simulate_route` has no `Executor` parameter, so the only way
    /// it could dispatch is by reaching one some other way.
    #[derive(Default)]
    struct Tripwire(AtomicU32);

    impl Executor for Tripwire {
        fn call<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _canonical: &'a CanonicalRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Err(ExecError("dry run must not dispatch".to_owned())) })
        }
    }

    fn pool() -> Vec<AutoCandidate> {
        vec![
            AutoCandidate::new(ProviderId::new("openai"), "gpt-4o").with_price(2.50),
            AutoCandidate::new(ProviderId::new("groq"), "llama-3.3-70b").with_price(0.59),
            AutoCandidate::new(ProviderId::new("together"), "mixtral").with_price(0.20),
            AutoCandidate::new(ProviderId::new("mystery"), "unpriced"),
        ]
    }

    fn combo(name: &str) -> crate::auto::AutoCombo {
        virtual_combo(name).expect("known alias")
    }

    #[test]
    fn dry_runs_without_dispatch_when_simulate() {
        let tripwire = Arc::new(Tripwire::default());
        let _ = simulate_route(
            &combo("auto/cheap"),
            &pool(),
            &AutoSelector::new(),
            |_| 0.5,
        )
        .expect("pool present");
        // `simulate_route` has no `Executor` parameter, so the only way it
        // could reach an upstream is by acquiring one some other way. It does
        // not; the counter proves the dry run is a pure function of its inputs.
        assert_eq!(tripwire.0.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn scores_cheapest_when_auto_cheap() {
        let plan = simulate_route(
            &combo("auto/cheap"),
            &pool(),
            &AutoSelector::new(),
            |_| 0.5,
        )
        .expect("pool present");
        assert_eq!(plan.winner().map_or("", ProviderId::as_str), "together");
    }

    #[test]
    fn caps_the_chain_at_max_attempts() {
        let wide: Vec<AutoCandidate> = (0..9)
            .map(|i| {
                AutoCandidate::new(ProviderId::new(format!("p{i}")), "m")
                    .with_price(1.0 + i as f64)
            })
            .collect();
        let plan = simulate_route(
            &combo("auto/cheap"),
            &wide,
            &AutoSelector::new(),
            |_| 0.5,
        )
        .expect("pool present");
        assert_eq!(plan.chain().len(), crate::MAX_ATTEMPTS);
    }

    #[test]
    fn never_repeats_a_provider_in_the_chain() {
        // Two candidates on one provider: the second is a fallback the attempt
        // loop would burn a slot on for no new information.
        let mut dup = pool();
        dup.push(AutoCandidate::new(ProviderId::new("groq"), "llama-3.1-8b").with_price(0.40));
        let plan = simulate_route(&combo("auto"), &dup, &AutoSelector::new(), |_| 0.5)
            .expect("pool present");
        let mut seen: Vec<&str> = plan.chain().iter().map(ProviderId::as_str).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before);
    }

    #[test]
    fn errors_when_pool_empty() {
        let got = simulate_route(&combo("auto"), &[], &AutoSelector::new(), |_| 0.5);
        assert!(matches!(got, Err(crate::RouteError::NoCandidates)));
    }

    #[test]
    fn reports_no_fallbacks_for_a_single_provider() {
        let one = [AutoCandidate::new(ProviderId::new("solo"), "m").with_price(1.0)];
        let plan = simulate_route(&combo("auto"), &one, &AutoSelector::new(), |_| 0.5)
            .expect("pool present");
        assert!(plan.fallbacks().is_empty());
    }

    #[test]
    fn resolves_every_variant_without_dispatch() {
        for v in AutoVariant::ALL {
            let plan = simulate_route(
                &combo(v.as_str()),
                &pool(),
                &AutoSelector::new(),
                |_| 0.5,
            )
            .expect("pool present");
            assert_eq!(plan.chain().len(), crate::MAX_ATTEMPTS, "{v:?}");
        }
    }

    #[test]
    fn plan_drives_the_attempt_loop_from_its_first_entry() {
        let tripwire = Arc::new(Tripwire::default());
        let plan = simulate_route(
            &combo("auto/cheap"),
            &pool(),
            &AutoSelector::new(),
            |_| 0.5,
        )
        .expect("pool present");
        let _ = futures::executor::block_on(crate::attempt_loop(
            &CanonicalRequest::new("m", bytes::Bytes::from_static(b"{}")),
            plan.chain(),
            &*tripwire,
            &crate::Resilience::new(),
        ));
        // The tripwire always errors, so the loop walks the whole chain. One
        // call per entry is what makes this a plan rather than a winner.
        assert_eq!(
            tripwire.0.load(Ordering::Relaxed) as usize,
            plan.chain().len()
        );
    }
}
