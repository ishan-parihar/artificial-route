//! The `limits:` block enforced.
//!
//! Two arms, one module. The 429 arm is requests per minute: a token bucket
//! keyed by the credential the gate authenticated, so one long-lived key is
//! indistinguishable from one abusive one. The 402 arm is the projected cost of
//! *this* request against the spend the usage ledger already holds for that
//! key, checked before dispatch — the only point at which refusing it costs
//! the client nothing.
//!
//! The 402 arm borrows its check from the ledger itself
//! ([`Ledger::admit_with`]) rather than re-reading the spend table, so a
//! ceiling enforced here cannot drift from the ledger the response path
//! records against. Both arms are opt-in: a config that never mentions
//! `limits:` gets [`Policy::disabled`], which answers everything without
//! touching a lock.

use ar_config::Limits;
use ar_limit::{Limiter, Spec};
use ar_tokens::{Cap, Cost, Ledger, PricingTable, Verdict, cost_micros};

/// The enforcer, built once from the config block.
pub struct Policy {
    /// One bucket per distinct `rpm` the block names, keyed by that value.
    ///
    /// Built eagerly rather than on first sight of a value so the request path
    /// reads a slice with no lock on it: the set of rpm values is finite and
    /// known at boot.
    buckets: Vec<(u32, Limiter)>,
    /// The block as parsed, consulted at request time because `keys:` rows
    /// differ per credential.
    limits: Limits,
    /// Whether the spend arms may run at all: a configured usage ledger.
    ///
    /// The spend arms read the ledger's totals, so without one they are not
    /// ceilings — this is where the caller's "no ledger" becomes the policy's
    /// "nothing armed" rather than a cap that pretends. Recorded at
    /// construction, not consulted per request, because the ledger is built
    /// once at boot and is not going to appear mid-flight.
    armed: bool,
}

impl Policy {
    /// The block from config, with one bucket per `rpm` it names.
    ///
    /// `armed` is whether a usage ledger is configured — the spend arms read
    /// its totals, so without one they stay inert rather than pretending a
    /// ceiling they cannot evaluate.
    #[must_use]
    pub fn from_limits(limits: &Limits, armed: bool) -> Self {
        let mut rpms: Vec<u32> = std::iter::once(&limits.default)
            .chain(limits.keys.values())
            .filter_map(|limit| limit.rpm)
            .collect();
        rpms.sort_unstable();
        rpms.dedup();
        Self {
            buckets: rpms
                .into_iter()
                .map(|rpm| (rpm, Limiter::new(Spec::per_minute(rpm))))
                .collect(),
            limits: limits.clone(),
            armed,
        }
    }

    /// Nothing is limited: the state a config without the block comes up in.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            buckets: Vec::new(),
            limits: Limits::default(),
            armed: false,
        }
    }

    /// The 429 arm. `None` when the request passes or the key is unlimited;
    /// `Some(seconds)` is the bucket's own answer to "when".
    ///
    /// The key is the authenticated client key. Anonymous traffic shares the
    /// `default` row, which is what that row is for.
    #[must_use]
    pub fn rate_wait(&self, key_id: &str) -> Option<u64> {
        let rpm = self.limits.for_key(key_id).rpm?;
        let bucket = self.buckets.iter().find(|(value, _)| *value == rpm)?;
        bucket.1.check(key_id).err()
    }

    /// The 402 arm's cap: the ceiling this key's row names, if it arms anything.
    ///
    /// `None` means *no ceiling*, which the caller reports as "nothing to do"
    /// rather than as an error — a row that limits only requests per minute is
    /// a perfectly reasonable row.
    #[must_use]
    pub fn cap(&self, key_id: &str) -> Option<Cap> {
        if !self.armed {
            return None;
        }
        let limit = self.limits.for_key(key_id);
        if limit.usd_micros.is_none() && limit.tokens.is_none() && !limit.refuse_unpriced {
            return None;
        }
        Some(Cap {
            key_id: key_id.to_string(),
            usd_micros: limit.usd_micros,
            tokens: limit.tokens,
            refuse_unpriced: limit.refuse_unpriced,
        })
    }

    /// The 402 arm end-to-end: project the request, check it against the
    /// ledger's spend for this key, verdict.
    ///
    /// Spend the ledger cannot read denies. The caller asked for a ceiling,
    /// and when the ceiling cannot be evaluated the honest answer is not to
    /// serve — a cap that fails open on a disk error is not a cap.
    pub fn spend_verdict(
        &self,
        key_id: &str,
        ledger: &Ledger,
        projection: &Projection<'_>,
    ) -> Verdict {
        let Some(cap) = self.cap(key_id) else {
            return Verdict::Allow;
        };
        let projected = project_cost(projection);
        // The ledger records a response's tokens both ways, so the projection
        // counts both too — an input-only figure is a ceiling that
        // systematically under-counts what the bill will add.
        let projected_tokens = projection.tokens_in.saturating_add(projection.tokens_out);
        ledger
            .admit_with(key_id, &cap, projected, projected_tokens)
            .unwrap_or(Verdict::Deny(ar_tokens::DenyReason::Unauditable))
    }
}

/// What this request is projected to cost, priced against the row that will
/// serve it.
///
/// Reads the same [`PricingTable`] the response path prices against, so an
/// estimate and the bill it will face differ only by what the model actually
/// returns. A model with no row is [`Cost::UNPRICED`]: what `refuse_unpriced`
/// denies, and what the spend arms price at zero.
#[must_use]
fn project_cost(projection: &Projection<'_>) -> Cost {
    let Some(row) = projection.prices.get(projection.provider, projection.model) else {
        return Cost::UNPRICED;
    };
    Cost {
        usd: ar_tokens::Usd {
            micros: cost_micros(projection.tokens_in, row.input_micros_per_mtok).saturating_add(
                cost_micros(projection.tokens_out, row.output_micros_per_mtok),
            ),
        },
        priced: true,
    }
}

/// The request as the projection needs it: the target that will serve it, its
/// counted tokens, and the table that prices it.
pub struct Projection<'a> {
    /// The provider the chain's first target names.
    pub(crate) provider: &'a str,
    /// The model that target serves — its own spelling, not the alias the
    /// request arrived under.
    pub(crate) model: &'a str,
    /// Tokens the request sends, counted rather than estimated.
    pub(crate) tokens_in: u32,
    /// The ceiling the request declares, `0` when it declares none.
    pub(crate) tokens_out: u32,
    /// The table the response path prices against.
    pub(crate) prices: &'a PricingTable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ar_config::Limit;

    fn limits_yaml(yaml: &str) -> Limits {
        let cfg = ar_config::Config::parse(yaml, |_| Ok(None)).expect("valid limits yaml");
        cfg.limits
    }

    fn prices_with(row_micros: u64) -> PricingTable {
        let mut t = PricingTable::default();
        t.set(
            "provider",
            "model",
            ar_tokens::Prices {
                input_micros_per_mtok: row_micros,
                output_micros_per_mtok: row_micros,
            },
        );
        t
    }

    #[test]
    fn disabled_answers_nothing_limited() {
        let p = Policy::disabled();
        assert!(p.rate_wait("anyone").is_none());
        assert!(p.cap("anyone").is_none());
    }

    #[test]
    fn the_429_arm_counts_one_key_independently() {
        let p = Policy::from_limits(&limits_yaml("limits:\n  default:\n    rpm: 1\n"), true);
        assert!(p.rate_wait("k1").is_none(), "first request passes");
        assert!(p.rate_wait("k1").is_some(), "second is told to wait");
        assert!(
            p.rate_wait("k2").is_none(),
            "another key has its own bucket"
        );
    }

    #[test]
    fn a_per_key_row_governs_its_own_ceiling() {
        let p = Policy::from_limits(
            &limits_yaml("limits:\n  default:\n    rpm: 5\n  keys:\n    needy:\n      rpm: 1\n"),
            true,
        );
        assert!(p.rate_wait("needy").is_none());
        assert!(
            p.rate_wait("needy").is_some(),
            "the per-key bucket is its own"
        );
        assert!(p.rate_wait("other").is_none(), "default bucket untouched");
    }

    #[test]
    fn a_response_only_row_arms_no_cap() {
        let p = Policy::from_limits(&limits_yaml("limits:\n  default:\n    rpm: 3\n"), true);
        assert!(p.cap("k").is_none(), "rpm alone is not a spend ceiling");
    }

    #[test]
    fn the_projection_prices_from_the_serving_row() {
        let p = Policy::from_limits(&Limits::default(), true);
        let prices = prices_with(2_000_000); // $2 per MTok
        let projection = Projection {
            provider: "provider",
            model: "model",
            tokens_in: 500_000,
            tokens_out: 10_000,
            prices: &prices,
        };
        let cost = project_cost(&projection);
        assert!(cost.priced);
        // 0.5 MTok in + 0.01 MTok out, both at $2/MTok.
        assert_eq!(cost.usd.micros, 1_000_000 + 20_000);
        let _ = p;
    }

    #[test]
    fn an_unpriced_model_projects_unpriced() {
        let prices = PricingTable::default();
        let projection = Projection {
            provider: "provider",
            model: "no-row",
            tokens_in: 10,
            tokens_out: 0,
            prices: &prices,
        };
        let cost = project_cost(&projection);
        assert!(!cost.priced);
    }

    #[test]
    fn refuse_unpriced_alone_still_arms_a_cap() {
        let p = Policy::from_limits(
            &limits_yaml("limits:\n  default:\n    refuse_unpriced: true\n"),
            true,
        );
        assert!(p.cap("k").is_some());
        assert!(p.cap("k").unwrap().refuse_unpriced);
    }

    #[test]
    fn a_cap_without_a_ledger_is_not_a_cap() {
        let p = Policy::from_limits(&limits_yaml("limits:\n  default:\n    tokens: 10\n"), true);
        assert!(p.cap("k").is_some(), "armed with a ledger behind it");
        let p = Policy::from_limits(&limits_yaml("limits:\n  default:\n    tokens: 10\n"), false);
        assert!(p.cap("k").is_none());
    }

    #[test]
    fn a_cap_row_reads_its_three_arms() {
        let p = Policy::from_limits(
            &limits_yaml(
                "limits:\n  default:\n    tokens: 10\n  keys:\n    k:\n      usd_micros: 7\n      tokens: 9\n      refuse_unpriced: true\n",
            ),
            true,
        );
        let cap = p.cap("k").expect("armed");
        assert_eq!(cap.usd_micros, Some(7));
        assert_eq!(cap.tokens, Some(9));
        assert!(cap.refuse_unpriced);
    }

    #[test]
    fn a_limit_defaults_to_empty() {
        assert!(Limit::default().is_empty());
    }
}
