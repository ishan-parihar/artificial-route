//! Selection: pool → ranked candidates → one winner plus a fallback tree.
//!
//! Ported from `../OmniRoute/open-sse/services/autoCombo/engine.ts` (tier
//! grouping, L154-177, and selection, L302-312).
//!
//! Two deviations from upstream, both forced by this crate's constraints:
//!
//! * **No `Math.random()`.** Upstream picks a tier by a weighted random draw
//!   and an "exploration" candidate by a coin flip. A dry run that reported a
//!   random winner would report a different winner than the request that
//!   follows it, and `explain_route` would be explaining a fiction. Rotation
//!   over an [`AutoSelector`] cursor keeps the *shape* of upstream's spread
//!   (successive requests do not all land on the same provider) with a result
//!   that is reproducible from the inputs.
//! * **No self-healing exclusion filter.** Upstream drops tripped breakers
//!   before scoring, then re-admits everything if the filter emptied the pool.
//!   Here a tripped breaker scores `health = 0.0`, which is 0.16–0.40 of the
//!   distribution — the same exclusion, expressed as a factor instead of a
//!   filter, and with no second scoring pass to re-rank the survivors.

use crate::auto::engine::AutoCombo;
use crate::auto::scoring::{AutoCandidate, Scored, score_pool};
use crate::error::RouteError;

/// Score gap below which two candidates are a tie for tiering purposes.
const SCORE_EPSILON: f64 = 1e-4;

/// Score spread at which the top tier is a clear enough winner to stop
/// rotating. Matches upstream `CLEAR_WINNER_THRESHOLD`.
const CLEAR_WINNER: f64 = 0.1;

/// Why a provider won. The one field of a trace that is not a number or a name.
///
/// Reported because the answer changes what a score means. A clear winner is
/// "this one is better"; a rotation is "this one, this time" — a pool inside
/// the clear-winner threshold has no meaningful ordering, and a trace that
/// presented it as one would be overstating what the scorer knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WinnerReason {
    /// Largest score, and the gap to the rest cleared [`CLEAR_WINNER`], so no
    /// rotation happened.
    ClearScore,
    /// Won its band on a rotation.
    Rotation,
}

impl WinnerReason {
    /// Canonical spelling for the trace.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClearScore => "clear_score",
            Self::Rotation => "rotation",
        }
    }
}

/// Splits a ranked pool into top / mid / rest bands.
///
/// Ported from `groupIntoTiers`, including its rebalance: when no candidate
/// lands in `mid`, half of `rest` is promoted so a lopsided pool still spreads
/// across bands instead of collapsing to two.
fn group_into_tiers(ranked: &[Scored]) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    let mut top = Vec::new();
    let mut mid = Vec::new();
    let mut rest = Vec::new();

    if ranked.is_empty() {
        return (top, mid, rest);
    }
    let best = ranked[0].score;
    let worst = ranked[ranked.len() - 1].score;
    let range = best - worst;

    for (i, c) in ranked.iter().enumerate() {
        let delta = best - c.score;
        if delta <= SCORE_EPSILON {
            top.push(i);
        } else if range <= SCORE_EPSILON || delta <= range * 0.3 {
            mid.push(i);
        } else {
            rest.push(i);
        }
    }

    if mid.is_empty() && !rest.is_empty() {
        let half = rest.len().div_ceil(2);
        let promoted: Vec<usize> = rest.drain(..half).collect();
        mid.extend(promoted);
    }

    (top, mid, rest)
}

/// Round-robin cursor shared by every `auto/*` selection.
///
/// One `AtomicU64`, no lock, exactly as [`crate::Strategy::RoundRobin`] does it.
/// Sharing one cursor across variants is deliberate: two variants selecting at
/// the same time should not be able to collide on the same index by accident.
#[derive(Debug, Default)]
pub struct AutoSelector {
    cursor: std::sync::atomic::AtomicU64,
}

impl AutoSelector {
    /// Builds a selector at position zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Position of the next selection. Exposed for tests that assert rotation
    /// is *advancing* rather than random.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.cursor.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Scores `pool` and picks a winner. `None` when `pool` is empty.
    ///
    /// `task_fitness` is the caller's model/task hook; the scorer reads it for
    /// the `task_fit` factor. Pass `|_| 0.5` for the neutral value.
    #[must_use]
    pub fn select<F>(
        &self,
        combo: &AutoCombo,
        pool: &[AutoCandidate],
        task_fitness: F,
    ) -> Option<(Scored, WinnerReason)>
    where
        F: Fn(&str) -> f64,
    {
        let ranked = score_pool(pool, &combo.weights(), task_fitness);
        Self::select_ranked(combo, &ranked, &self.cursor)
    }

    /// Picks from an already-ranked pool. Split out so `explain_route` and
    /// `simulate_route` score once and read the winner out of the same ranking
    /// rather than each paying for their own `score_pool`.
    pub(crate) fn select_ranked(
        combo: &AutoCombo,
        ranked: &[Scored],
        cursor: &std::sync::atomic::AtomicU64,
    ) -> Option<(Scored, WinnerReason)> {
        if ranked.is_empty() {
            return None;
        }
        if ranked.len() == 1 {
            return Some((ranked[0].clone(), WinnerReason::ClearScore));
        }

        // `fetch_add` wraps on overflow, which the modulus below absorbs.
        let n = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize;

        let (top, mid, rest) = group_into_tiers(ranked);
        let spread = ranked[0].score - ranked[ranked.len() - 1].score;
        if !top.is_empty() && spread >= CLEAR_WINNER {
            return Some((Self::take(ranked, &top, n), WinnerReason::ClearScore));
        }

        // `exploration_rate` is the band ceiling: 0.0 (`auto/chaos`) is the top
        // band only, anything above it may reach as far as `rest`.
        let ceiling = if combo.exploration_rate > 0.0 { 3 } else { 1 };
        let bands: [&Vec<usize>; 3] = [&top, &mid, &rest];
        let reachable: Vec<&Vec<usize>> = bands
            .iter()
            .take(ceiling)
            .copied()
            .filter(|band| !band.is_empty())
            .collect();
        if reachable.is_empty() {
            return Some((ranked[0].clone(), WinnerReason::ClearScore));
        }
        let reason = WinnerReason::Rotation;
        Some((
            Self::take(ranked, reachable[n % reachable.len()], n),
            reason,
        ))
    }

    /// One index from `band`, rotating.
    fn take(ranked: &[Scored], band: &[usize], n: usize) -> Scored {
        ranked[band[n % band.len()]].clone()
    }
}

/// The ranked pool plus the winner, which is everything both entry points need.
///
/// One `score_pool` call feeds [`crate::simulate_route`] and
/// [`crate::explain_route`]; neither re-scores.
pub(crate) fn rank<F>(
    combo: &AutoCombo,
    pool: &[AutoCandidate],
    selector: &AutoSelector,
    task_fitness: F,
) -> Result<(Vec<Scored>, Scored, WinnerReason), RouteError>
where
    F: Fn(&str) -> f64,
{
    if pool.is_empty() {
        return Err(RouteError::NoCandidates);
    }
    let ranked = score_pool(pool, &combo.weights(), task_fitness);
    let (winner, reason) = AutoSelector::select_ranked(combo, &ranked, &selector.cursor)
        .ok_or(RouteError::NoCandidates)?;
    Ok((ranked, winner, reason))
}

#[cfg(test)]
mod tests {
    use super::{AutoSelector, WinnerReason, group_into_tiers};

    use crate::auto::engine::AutoCombo;
    use crate::auto::scoring::{AutoCandidate, Scored, score_pool};
    use crate::contract::ProviderId;
    use std::sync::atomic::AtomicU64;

    fn scored(provider: &str, score: f64) -> Scored {
        Scored {
            provider: ProviderId::new(provider),
            model: crate::contract::Strng::from("m"),
            score,
            factors: crate::auto::scoring::healthy_factors(),
        }
    }

    #[test]
    fn puts_equal_scores_all_in_top() {
        let ranked = [scored("a", 0.5), scored("b", 0.5), scored("c", 0.5)];
        let (top, mid, rest) = group_into_tiers(&ranked);
        assert_eq!((top.len(), mid.len(), rest.len()), (3, 0, 0));
    }

    #[test]
    fn promotes_half_of_rest_when_mid_is_empty() {
        // A 0.4 spread with one leader: everyone else is >30% below, so `mid`
        // would be empty and the pool would collapse to two bands.
        let ranked = [
            scored("a", 1.0),
            scored("b", 0.4),
            scored("c", 0.4),
            scored("d", 0.4),
        ];
        let (top, mid, rest) = group_into_tiers(&ranked);
        assert_eq!((top.len(), mid.len(), rest.len()), (1, 2, 1));
    }

    #[test]
    fn takes_clear_winner_regardless_of_cursor() {
        let combo = AutoCombo::new(crate::auto::AutoVariant::Balanced);
        let ranked = [scored("a", 0.9), scored("b", 0.5)];
        for n in 0..4 {
            let cursor = AtomicU64::new(n);
            let (got, reason) =
                AutoSelector::select_ranked(&combo, &ranked, &cursor).expect("pool non-empty");
            assert_eq!(got.provider.as_str(), "a");
            assert_eq!(reason, WinnerReason::ClearScore);
        }
    }

    #[test]
    fn restricts_to_top_band_when_exploration_is_zero() {
        let combo = AutoCombo::new(crate::auto::AutoVariant::Chaos);
        // Uniform scores: every candidate is in `top`, so `rest`/`mid` are
        // empty and the ceiling cannot be observed. Drop one far enough to land
        // in `rest` while keeping the spread under CLEAR_WINNER.
        let ranked = [scored("a", 0.8), scored("b", 0.8), scored("c", 0.72)];
        let cursor = AtomicU64::new(0);
        let (got, _) =
            AutoSelector::select_ranked(&combo, &ranked, &cursor).expect("pool non-empty");
        assert_eq!(got.provider.as_str(), "a");
    }

    #[test]
    fn rotates_across_bands_when_exploration_is_positive() {
        let combo = AutoCombo::new(crate::auto::AutoVariant::Balanced);
        let ranked = [scored("a", 0.8), scored("b", 0.8), scored("c", 0.72)];
        let selector = AutoSelector::new();
        let first = AutoSelector::select_ranked(&combo, &ranked, &selector.cursor).map(|(s, _)| s);
        let second = AutoSelector::select_ranked(&combo, &ranked, &selector.cursor).map(|(s, _)| s);
        assert_ne!(
            first.map(|s| s.provider),
            second.map(|s| s.provider),
            "cursor did not advance the selection"
        );
    }

    #[test]
    fn selects_the_only_candidate_when_pool_has_one() {
        let combo = AutoCombo::new(crate::auto::AutoVariant::Balanced);
        let pool = [AutoCandidate::new(ProviderId::new("solo"), "m").with_price(1.0)];
        let ranked = score_pool(&pool, &combo.weights(), |_| 0.5);
        let (got, _) =
            AutoSelector::select_ranked(&combo, &ranked, &AtomicU64::new(0)).expect("non-empty");
        assert_eq!(got.provider.as_str(), "solo");
    }
}
