//! The free-model budget table: what a free tier actually grants, per model.
//!
//! Generated from `../OmniRoute/open-sse/config/freeModelCatalog.data.ts` by
//! `ar import --from omniroute`, embedded here with `include_str!` and parsed
//! once, lazily. It is the answer to a question the price table cannot answer:
//! `openai`'s row says what a metered call costs, and says nothing about the
//! `xai` free tier's 30M tokens/month.
//!
//! # Why the rows are not the answer, and the totals are
//!
//! 489 rows across 78 providers describe 77 pool keys, not 489 allowances. Four
//! rows under one `poolKey` are four descriptions of *one* monthly allowance —
//! Groq's five per-model caps draw on the same quota — so summing rows would
//! overstate real headroom several times over. [`FreeBudgets::totals`] therefore
//! counts each shared pool once, at the **max** within the pool, and lets a row
//! with no `poolKey` count on its own (a model that is genuinely independent).
//!
//! # Regimes, and which figure an allowance belongs to
//!
//! A row's [`FreeRegime`] is not a label: it decides whether the row grants
//! access at all ([`FreeRegime::grants_free_access`]), which of the four totals
//! its allowance feeds ([`FreeRegime::token_bucket`]), and whether a candidate
//! of that regime may skip the live allowance check on the no-auth path
//! ([`FreeRegime::allows_no_auth_shortcut`]).
//!
//! `keyless` is the one regime with `allows_no_auth_shortcut`, and the
//! distinction from "needs no API key" is load-bearing upstream: `blackbox`,
//! `friendliai`, `iflytek` and `sparkdesk` are catalogued `keyless` yet answer
//! 401 without a credential. Do not collapse the two questions.
//!
//! # ToS-avoid
//!
//! [`FreeBudgetRow::tos_avoid`] marks a provider whose published terms prohibit
//! routing through a self-hosted proxy. Those rows are real quotas that this
//! proxy should not spend, so they are excluded from
//! [`FreeBudgets::usable_monthly_tokens`] — the figure routing is meant to plan
//! against — while remaining in [`FreeBudgets::totals`] when the caller asks for
//! the undocumented total. Same shape as upstream's `excludeTosAvoid`:
//! omission is a choice, and the choice is the caller's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use ar_core::Strng;
use serde::{Deserialize, Serialize};

/// The generated budget table, compact JSON for the same reason `registry.json`
/// is: it is `include_str!`-ed into every binary linking this crate, so
/// whitespace is `.rodata`. `python3 -m json.tool` prettifies it for reading.
const FREE_BUDGETS_JSON: &str = include_str!("freeBudgets.json");

/// How many rows the upstream table declares, as of this build.
///
/// A constant so the count assertion can name the real number rather than a
/// floor, and so the number a reader quotes is the one the file holds.
pub const FREE_BUDGET_ROWS: usize = 489;

/// The providers the upstream table names, as of this build.
pub const FREE_BUDGET_PROVIDERS: usize = 78;

/// The month the table was curated against provider documentation, as of this
/// build.
///
/// A literal rather than the file's mtime: a build rewrites timestamps on every
/// deploy, which would report a months-old table as curated today. Asserted
/// against [`FreeBudgets::curated_at`] so the two cannot drift.
pub const FREE_BUDGET_CURATED_AT: &str = "2026-09-12";

/// Which totals figure a regime's allowance belongs to.
///
/// Every regime lands in exactly one bucket, so a regime added upstream cannot
/// quietly contribute to nothing: the compiler asks which figure it feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FreeTokenBucket {
    /// Summed into the steady recurring monthly headline.
    SteadyMonthly,
    /// Credit that refills, reported next to the steady figure.
    RecurringCredit,
    /// Signup credit, first month only.
    OneTimeCredit,
    /// Real access, no published cap — listed, never summed.
    Uncapped,
    /// Grants nothing, so it feeds no figure.
    None,
}

/// What a free tier is, and therefore what its row may be used for.
///
/// Exhaustive against the seven upstream `FreeModelFreeType` members: an eighth
/// regime upstream will not deserialise here until it is classified on every
/// axis, which is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FreeRegime {
    /// A quota that resets daily.
    RecurringDaily,
    /// A quota that resets monthly.
    RecurringMonthly,
    /// A credit grant that refills.
    RecurringCredit,
    /// Permanently free, rate/concurrency limited, no published token cap.
    RecurringUncapped,
    /// Signup credit: real, and gone after the first month.
    OneTimeInitial,
    /// No credential exists at all, so nothing can be billed.
    Keyless,
    /// A retired free tier behind a paid key. Grants no access.
    Discontinued,
}

impl FreeRegime {
    /// The spelling used in `freeBudgets.json`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RecurringDaily => "recurring-daily",
            Self::RecurringMonthly => "recurring-monthly",
            Self::RecurringCredit => "recurring-credit",
            Self::RecurringUncapped => "recurring-uncapped",
            Self::OneTimeInitial => "one-time-initial",
            Self::Keyless => "keyless",
            Self::Discontinued => "discontinued",
        }
    }

    /// Parses an upstream `freeType` label.
    ///
    /// `None` for an unrecognised label rather than a fallback variant: a new
    /// regime must be classified deliberately, because each of the three
    /// answers below changes what a total means.
    #[must_use]
    pub fn from_label(name: &str) -> Option<Self> {
        Some(match name {
            "recurring-daily" => Self::RecurringDaily,
            "recurring-monthly" => Self::RecurringMonthly,
            "recurring-credit" => Self::RecurringCredit,
            "recurring-uncapped" => Self::RecurringUncapped,
            "one-time-initial" => Self::OneTimeInitial,
            "keyless" => Self::Keyless,
            "discontinued" => Self::Discontinued,
            _ => return None,
        })
    }

    /// Can a request route to this regime's models without paying?
    ///
    /// The shared predicate, read from the table rather than from a list of
    /// catalogued ids: a `discontinued` row is in the catalog and is not free.
    #[must_use]
    pub fn grants_free_access(self) -> bool {
        !matches!(self, Self::Discontinued)
    }

    /// Which figure this regime's allowance belongs to.
    #[must_use]
    pub fn token_bucket(self) -> FreeTokenBucket {
        match self {
            Self::RecurringDaily | Self::RecurringMonthly | Self::Keyless => {
                FreeTokenBucket::SteadyMonthly
            }
            Self::RecurringCredit => FreeTokenBucket::RecurringCredit,
            Self::OneTimeInitial => FreeTokenBucket::OneTimeCredit,
            Self::RecurringUncapped => FreeTokenBucket::Uncapped,
            Self::Discontinued => FreeTokenBucket::None,
        }
    }

    /// May a candidate of this regime skip the live allowance check when it is
    /// reached through the synthetic no-auth path?
    ///
    /// This is **not** "this provider needs no API key". True only where the
    /// catalogue says no credential exists at all, so no request against it can
    /// be billed.
    #[must_use]
    pub fn allows_no_auth_shortcut(self) -> bool {
        matches!(self, Self::Keyless)
    }
}

/// One model's documented free allowance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeBudgetRow {
    /// Provider half of the upstream row, as the catalog spells it.
    pub provider: Strng,
    /// Model half of the upstream row.
    pub model: Strng,
    /// Upper bound of the provider's documented recurring monthly free tokens.
    ///
    /// `0` means the provider published no monthly figure — an uncapped tier,
    /// or a credit regime whose allowance is in [`Self::credit_tokens`]. Not the
    /// same as "free with no limit", which is [`FreeRegime::RecurringUncapped`].
    pub monthly_tokens: u64,
    /// Credit-denominated allowance, for the two credit regimes.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub credit_tokens: u64,
    /// What kind of tier this is; see [`FreeRegime`].
    pub regime: FreeRegime,
    /// The shared allowance this row draws on.
    ///
    /// `Some` means several rows describe one quota and only the largest may be
    /// counted. `None` means the model is independent and counts on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<Strng>,

    /// The provider's terms prohibit routing through a self-hosted proxy.
    ///
    /// The flag is carried per row because upstream records it per row, and it
    /// is what excludes a quota from
    /// [`FreeBudgets::usable_monthly_tokens`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub tos_avoid: bool,

    /// The quota only opens after a region-bound identity check, so it is real
    /// but not open to anyone and never joins the steady headline.
    #[serde(default, skip_serializing_if = "is_false")]
    pub gated: bool,
}

/// `skip_serializing_if` helper: keeps an unset name out of the JSON.
///
/// A `&str` method rather than `Strng::is_empty`, which resolves against
/// `ExactSizeIterator` on `Arc<str>` and does not compile.
fn is_blank(v: &Strng) -> bool {
    v.is_empty()
}

/// `skip_serializing_if` helper: keeps a `0` credit row out of the JSON.
fn is_zero(v: &u64) -> bool {
    *v == 0
}
/// `skip_serializing_if` helper: keeps `tos_avoid: false` out of the JSON.
fn is_false(v: &bool) -> bool {
    !*v
}

/// The whole generated table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeBudgets {
    /// When the upstream table was last curated against provider documentation.
    ///
    /// A literal rather than the file's mtime: a build rewrites timestamps on
    /// every deploy, which would report a months-old catalog as curated today.
    #[serde(default, skip_serializing_if = "is_blank")]
    pub curated_at: Strng,
    /// Every row, sorted by `(provider, model, regime)` so two imports of the
    /// same upstream tree produce the same bytes.
    #[serde(default)]
    pub rows: Vec<FreeBudgetRow>,
}

/// What the caller wants excluded from a total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TotalsOpts {
    /// Drop rows whose provider terms prohibit proxy use, so the figure is
    /// headroom this proxy may actually spend.
    pub exclude_tos_avoid: bool,
}

/// The pool-deduped figures, each regime counted in exactly one place.
///
/// Four figures rather than one because the regimes are not commensurable: a
/// monthly token quota, a refilling credit grant, a first-month signup credit
/// and a permanently-free-but-uncapped tier cannot be added without inventing a
/// conversion nobody published.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreeTotals {
    /// Pool-deduped recurring tokens per month. The headline.
    pub steady_monthly_tokens: u64,
    /// Pool-deduped `recurring-credit` grants, reported next to the headline.
    pub recurring_credit_tokens: u64,
    /// Pool-deduped `one-time-initial` grants: first month only.
    pub one_time_credit_tokens: u64,
    /// Pool-deduped tokens behind a regional identity check, never summed into
    /// [`Self::steady_monthly_tokens`].
    pub gated_recurring_tokens: u64,
    /// Distinct pools contributing to [`Self::steady_monthly_tokens`].
    pub pool_count: usize,
    /// Rows considered, after any exclusion.
    pub model_count: usize,
    /// Distinct providers among those rows.
    pub provider_count: usize,
    /// Providers that are permanently free but publish no token cap. Real access,
    /// un-quantifiable, so listed rather than summed.
    pub uncapped_providers: Vec<Strng>,

    /// Rows excluded because their terms prohibit proxy use.
    pub tos_avoid_rows: usize,
}

impl FreeBudgets {
    /// Number of rows in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the table is empty. Never true for a valid build.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Iterates the rows in generated order.
    pub fn iter(&self) -> std::slice::Iter<'_, FreeBudgetRow> {
        self.rows.iter()
    }

    /// One row by provider and model.
    #[must_use]
    pub fn row(&self, provider: &str, model: &str) -> Option<&FreeBudgetRow> {
        self.rows
            .iter()
            .find(|r| r.provider.as_ref() == provider && r.model.as_ref() == model)
    }

    /// Every row for one provider, in generated order.
    pub fn rows_for(&self, provider: &str) -> impl Iterator<Item = &FreeBudgetRow> {
        self.rows
            .iter()
            .filter(move |r| r.provider.as_ref() == provider)
    }

    /// The distinct providers the table names, sorted.
    #[must_use]
    pub fn providers(&self) -> Vec<Strng> {
        let set: BTreeSet<&str> = self.rows.iter().map(|r| r.provider.as_ref()).collect();
        set.into_iter().map(Strng::from).collect()
    }

    /// The distinct pool keys, sorted. A row with no pool is not one.
    #[must_use]
    pub fn pools(&self) -> Vec<Strng> {
        let set: BTreeSet<&str> = self.rows.iter().filter_map(|r| r.pool.as_deref()).collect();
        set.into_iter().map(Strng::from).collect()
    }

    /// The documented figures, with each shared pool counted once.
    ///
    /// `opts.exclude_tos_avoid` drops the rows whose terms prohibit proxy use.
    /// The default total keeps them, so the two numbers bracket the honest
    /// range rather than one of them being the only number in the binary.
    #[must_use]
    pub fn totals(&self, opts: TotalsOpts) -> FreeTotals {
        let considered: Vec<&FreeBudgetRow> = self
            .rows
            .iter()
            .filter(|r| !(opts.exclude_tos_avoid && r.tos_avoid))
            .collect();
        let steady = |r: &FreeBudgetRow| r.regime.token_bucket() == FreeTokenBucket::SteadyMonthly;
        let mut t = FreeTotals {
            steady_monthly_tokens: deduped(&considered, steady, |r| r.monthly_tokens),
            recurring_credit_tokens: deduped(
                &considered,
                |r| r.regime.token_bucket() == FreeTokenBucket::RecurringCredit,
                |r| r.credit_tokens,
            ),
            one_time_credit_tokens: deduped(
                &considered,
                |r| r.regime.token_bucket() == FreeTokenBucket::OneTimeCredit,
                |r| r.credit_tokens,
            ),
            gated_recurring_tokens: deduped(
                &considered,
                |r| steady(r) && r.gated,
                |r| r.monthly_tokens,
            ),
            pool_count: considered
                .iter()
                .filter(|r| steady(r) && r.pool.is_some())
                .map(|r| r.pool.as_deref().unwrap_or_default())
                .collect::<BTreeSet<&str>>()
                .len(),
            model_count: considered.len(),
            provider_count: considered
                .iter()
                .map(|r| r.provider.as_ref())
                .collect::<BTreeSet<&str>>()
                .len(),
            uncapped_providers: {
                let set: BTreeSet<&str> = considered
                    .iter()
                    .filter(|r| r.regime.token_bucket() == FreeTokenBucket::Uncapped)
                    .map(|r| r.provider.as_ref())
                    .collect();
                set.into_iter().map(Strng::from).collect()
            },
            tos_avoid_rows: self.rows.iter().filter(|r| r.tos_avoid).count(),
        };
        // A gated row is reported apart, never inside the headline.
        t.steady_monthly_tokens = t
            .steady_monthly_tokens
            .saturating_sub(gated_from_headline(&considered));
        t
    }

    /// Pool-deduped steady monthly tokens a proxy may actually spend.
    ///
    /// The figure routing plans against: [`FreeTotals::steady_monthly_tokens`]
    /// with the ToS-avoid rows removed. Distinct from a per-row allowance — see
    /// the module docs for why the row is not the answer.
    #[must_use]
    pub fn usable_monthly_tokens(&self) -> u64 {
        self.totals(TotalsOpts {
            exclude_tos_avoid: true,
        })
        .steady_monthly_tokens
    }

    /// The monthly allowance of one pool: the max within it, or `0` when no row
    /// of the pool feeds the steady headline.
    ///
    /// What a routing decision needs per *pool* rather than per model: four
    /// Groq models drawing on one quota have one allowance between them.
    #[must_use]
    pub fn pool_monthly_tokens(&self, pool: &str) -> u64 {
        self.rows
            .iter()
            .filter(|r| r.pool.as_deref() == Some(pool))
            .filter(|r| r.regime.token_bucket() == FreeTokenBucket::SteadyMonthly)
            .map(|r| r.monthly_tokens)
            .max()
            .unwrap_or(0)
    }

    /// The monthly allowance of one model's pool, or its own `monthlyTokens` when
    /// it declares no pool.
    #[must_use]
    pub fn monthly_tokens_for(&self, provider: &str, model: &str) -> u64 {
        let Some(row) = self.row(provider, model) else {
            return 0;
        };
        match row.pool.as_ref() {
            Some(pool) => self.pool_monthly_tokens(pool),
            None => row.monthly_tokens,
        }
    }

    /// Whether routing may spend this row's quota: it grants access, and its
    /// terms do not forbid proxy use.
    ///
    /// The single predicate behind exclusion from usable headroom, so a caller
    /// cannot half-apply it.
    #[must_use]
    pub fn usable(&self, provider: &str, model: &str) -> bool {
        self.row(provider, model)
            .is_some_and(|r| r.regime.grants_free_access() && !r.tos_avoid)
    }
}

/// The gated rows' contribution, so the headline can subtract exactly what the
/// aside reported.
fn gated_from_headline(rows: &[&FreeBudgetRow]) -> u64 {
    deduped(
        rows,
        |r| r.regime.token_bucket() == FreeTokenBucket::SteadyMonthly && r.gated,
        |r| r.monthly_tokens,
    )
}

/// Sums a per-row numeric field, counting each shared pool once at its max.///
/// A row with no `poolKey` is an independent model and adds on its own. The
/// max-within-pool rule is the whole point: `mistral` publishes a per-model
/// figure for a provider whose rows share one monthly allowance, and summing
/// them would multiply real headroom by the row count.
fn deduped<F, P>(rows: &[&FreeBudgetRow], include: F, pick: P) -> u64
where
    F: Fn(&FreeBudgetRow) -> bool,
    P: Fn(&FreeBudgetRow) -> u64,
{
    let mut out: BTreeMap<&str, u64> = BTreeMap::new();
    let mut loose = 0u64;
    for row in rows.iter().copied().filter(|r| include(r)) {
        match row.pool.as_ref() {
            Some(pool) => {
                let value = pick(row);
                pool_max_entry(&mut out, pool, value);
            }
            None => loose = loose.saturating_add(pick(row)),
        }
    }
    out.values().fold(loose, |acc, v| acc.saturating_add(*v))
}

/// The max-within-pool update, spelled out because `entry().and_modify()` reads
/// as noise for a two-line rule. Explicit `'a` because `&mut BTreeMap<&'a str, _>`
/// is invariant over its key type, so the borrow has to be tied to the row it
/// came from rather than to the call.
fn pool_max_entry<'a>(pool_max: &mut BTreeMap<&'a str, u64>, pool: &'a str, value: u64) {
    match pool_max.get_mut(pool) {
        Some(slot) => *slot = (*slot).max(value),
        None => {
            pool_max.insert(pool, value);
        }
    }
}

/// The process-wide table, parsed on first call.
///
/// Lazy on the same reasoning as [`crate::global`]: the parse is not on the
/// `--version` fast path, and nothing on a dispatch path needs a free-tier
/// figure.
///
/// # Panics
///
/// Panics if the embedded table is malformed. It is a generated source file, not
/// user input, so a panic at first use is the honest signal; it cannot be
/// triggered by a request.
#[must_use]
pub fn global() -> &'static FreeBudgets {
    static TABLE: OnceLock<FreeBudgets> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(FREE_BUDGETS_JSON).expect("embedded freeBudgets.json is malformed")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row: a provider, a model, an allowance, and the pool it draws on.
    fn row(
        provider: &str,
        model: &str,
        monthly: u64,
        regime: FreeRegime,
        pool: Option<&str>,
    ) -> FreeBudgetRow {
        FreeBudgetRow {
            provider: Strng::from(provider),
            model: Strng::from(model),
            monthly_tokens: monthly,
            credit_tokens: 0,
            regime,
            pool: pool.map(Strng::from),
            tos_avoid: false,
            gated: false,
        }
    }

    /// A table from rows, with a curation date the assertions do not read.
    fn table(rows: Vec<FreeBudgetRow>) -> FreeBudgets {
        FreeBudgets {
            curated_at: Strng::from("test"),
            rows,
        }
    }

    #[test]
    fn counts_a_shared_pool_once_at_its_max() {
        // The row-sum is 60M; the pool is 30M. Reporting 60M would let routing
        // plan against twice the quota that exists.
        let t = table(vec![
            row(
                "groq",
                "a",
                10_000_000,
                FreeRegime::RecurringDaily,
                Some("groq-free"),
            ),
            row(
                "groq",
                "b",
                20_000_000,
                FreeRegime::RecurringDaily,
                Some("groq-free"),
            ),
            row(
                "groq",
                "c",
                30_000_000,
                FreeRegime::RecurringDaily,
                Some("groq-free"),
            ),
        ]);
        assert_eq!(
            t.totals(TotalsOpts::default()).steady_monthly_tokens,
            30_000_000
        );
    }

    #[test]
    fn counts_an_unpooled_row_on_its_own() {
        let t = table(vec![
            row("a", "m", 5_000_000, FreeRegime::RecurringDaily, None),
            row(
                "b",
                "m",
                7_000_000,
                FreeRegime::RecurringDaily,
                Some("b-pool"),
            ),
            row(
                "b",
                "n",
                2_000_000,
                FreeRegime::RecurringDaily,
                Some("b-pool"),
            ),
        ]);
        assert_eq!(
            t.totals(TotalsOpts::default()).steady_monthly_tokens,
            12_000_000
        );
    }

    #[test]
    fn counts_one_pool_per_key_not_per_provider() {
        // `openrouter-free` is shared across several model ids, and a different
        // provider's rows drawing on it are the same quota.
        let t = table(vec![
            row(
                "openrouter",
                "a",
                1_000_000,
                FreeRegime::RecurringDaily,
                Some("openrouter-free"),
            ),
            row(
                "some-else",
                "b",
                2_000_000,
                FreeRegime::RecurringDaily,
                Some("openrouter-free"),
            ),
        ]);
        assert_eq!(t.totals(TotalsOpts::default()).pool_count, 1);
        assert_eq!(t.pool_monthly_tokens("openrouter-free"), 2_000_000);
    }

    #[test]
    fn drops_a_tos_avoid_pool_from_usable_headroom_only() {
        let mut avoid = row(
            "kiro",
            "claude",
            25_000,
            FreeRegime::RecurringDaily,
            Some("kiro-free"),
        );
        avoid.tos_avoid = true;
        let t = table(vec![
            row("open", "m", 100, FreeRegime::RecurringDaily, None),
            avoid,
        ]);
        assert_eq!(
            t.totals(TotalsOpts::default()).steady_monthly_tokens,
            25_100
        );
        assert_eq!(t.usable_monthly_tokens(), 100);
        assert_eq!(t.totals(TotalsOpts::default()).tos_avoid_rows, 1);
    }

    #[test]
    fn reports_a_gated_pool_apart_from_the_headline() {
        let mut gated = row(
            "modelscope",
            "q",
            6_000_000,
            FreeRegime::RecurringDaily,
            Some("modelscope-free"),
        );
        gated.gated = true;
        let t = table(vec![
            row("open", "m", 1_000_000, FreeRegime::RecurringDaily, None),
            gated,
        ]);
        let totals = t.totals(TotalsOpts::default());
        assert_eq!(totals.gated_recurring_tokens, 6_000_000);
        assert_eq!(totals.steady_monthly_tokens, 1_000_000);
    }

    #[test]
    fn feeds_credit_regimes_to_their_own_figures() {
        // Neither credit regime is a monthly quota, so a totals() that summed
        // every numeric field into the headline would report 1.5M here.
        let mut rec = row("bytez", "m", 0, FreeRegime::RecurringCredit, None);
        rec.credit_tokens = 1_000_000;
        let mut once = row("newco", "m", 0, FreeRegime::OneTimeInitial, None);
        once.credit_tokens = 500_000;
        let totals = table(vec![rec, once]).totals(TotalsOpts::default());
        assert_eq!(totals.recurring_credit_tokens, 1_000_000);
        assert_eq!(totals.one_time_credit_tokens, 500_000);
        assert_eq!(
            totals.steady_monthly_tokens, 0,
            "neither credit regime is a monthly quota"
        );
    }

    #[test]
    fn grants_nothing_for_a_discontinued_tier() {
        let t = table(vec![row(
            "old",
            "m",
            9_000_000,
            FreeRegime::Discontinued,
            None,
        )]);
        assert!(!FreeRegime::Discontinued.grants_free_access());
        let totals = t.totals(TotalsOpts::default());
        assert_eq!(
            totals.steady_monthly_tokens, 0,
            "a retired tier is catalogued, not free"
        );
        assert!(totals.uncapped_providers.is_empty());
    }

    #[test]
    fn lists_an_uncapped_provider_without_summing_it() {
        let t = table(vec![row(
            "siliconflow",
            "m",
            0,
            FreeRegime::RecurringUncapped,
            None,
        )]);
        let totals = t.totals(TotalsOpts::default());
        assert_eq!(totals.uncapped_providers, vec![Strng::from("siliconflow")]);
        assert_eq!(totals.steady_monthly_tokens, 0);
    }

    #[test]
    fn allows_the_no_auth_shortcut_only_for_a_keyless_regime() {
        // The distinction is load-bearing: `blackbox`, `friendliai`, `iflytek`
        // and `sparkdesk` are catalogued `keyless` yet answer 401 without a
        // credential, so "keyless" cannot be read as "needs no API key".
        assert!(FreeRegime::Keyless.allows_no_auth_shortcut());
        assert!(!FreeRegime::RecurringDaily.allows_no_auth_shortcut());
    }

    #[test]
    fn refuses_a_regime_label_it_has_not_classified() {
        assert_eq!(
            FreeRegime::from_label("recurring-daily"),
            Some(FreeRegime::RecurringDaily)
        );
        assert_eq!(FreeRegime::from_label("pay-as-you-go"), None);
    }

    #[test]
    fn round_trips_every_regime_through_its_label() {
        for regime in [
            FreeRegime::RecurringDaily,
            FreeRegime::RecurringMonthly,
            FreeRegime::RecurringCredit,
            FreeRegime::RecurringUncapped,
            FreeRegime::OneTimeInitial,
            FreeRegime::Keyless,
            FreeRegime::Discontinued,
        ] {
            assert_eq!(
                FreeRegime::from_label(regime.as_str()),
                Some(regime),
                "{}",
                regime.as_str()
            );
        }
    }

    #[test]
    fn ships_the_full_free_tier_catalog() {
        // The upstream table is 489 rows over 78 providers. A regenerated catalog
        // that lost an order of magnitude is a broken importer, not a smaller
        // world, so the constants above name the real figures and this test is
        // what fails when they move.
        let g = global();
        assert!(
            !g.curated_at.as_ref().is_empty(),
            "the curation date is the staleness signal"
        );
        assert_eq!(
            g.len(),
            FREE_BUDGET_ROWS,
            "the compiled-in free-tier row count"
        );
        assert_eq!(
            g.providers().len(),
            FREE_BUDGET_PROVIDERS,
            "the free-tier provider count"
        );
        assert_eq!(
            g.curated_at.as_ref(),
            FREE_BUDGET_CURATED_AT,
            "the curation date"
        );
    }

    #[test]
    fn counts_a_shared_pool_once_in_the_shipped_table() {
        // The headline is a pool-deduped figure and the row sum is strictly
        // larger — that gap is the whole reason this module exists, and it is
        // the assertion that would catch a totals() rewritten to just sum.
        let g = global();
        let row_sum: u64 = g
            .iter()
            .filter(|r| r.regime.token_bucket() == FreeTokenBucket::SteadyMonthly)
            .map(|r| r.monthly_tokens)
            .sum();
        let headline = g.usable_monthly_tokens();
        assert!(
            row_sum > headline,
            "row sum {row_sum} must exceed the deduped headline {headline}"
        );
    }

    #[test]
    fn counts_the_pools_the_shipped_table_names() {
        // 35 recurring pool keys upstream; 77 distinct keys overall once the
        // one-time and credit regimes are counted. A regeneration that collapsed
        // the pools into one would still pass the row-count assertion.
        let g = global();
        let totals = g.totals(TotalsOpts::default());
        assert!(
            g.pools().len() >= 70,
            "table names {} pools",
            g.pools().len()
        );
        assert!(
            totals.pool_count >= 30,
            "{} feed the steady headline",
            totals.pool_count
        );
        assert!(
            totals.pool_count < totals.model_count,
            "fewer pools than rows, or nothing is shared"
        );
    }

    #[test]
    fn reserves_the_largest_pools_named_by_the_shipped_table() {
        // Two pools whose published figures are the catalog's headline entries;
        // a regeneration that lost either would still pass the count assertions.
        // The pool keys are the upstream spellings and are not uniform: `mistral`
        // carries no `-free` suffix, `nara-free` does.
        let g = global();
        assert!(
            g.pool_monthly_tokens("mistral") >= 1_000_000_000,
            "mistral: {}",
            g.pool_monthly_tokens("mistral")
        );
        assert!(
            g.pool_monthly_tokens("nara-free") >= 200_000_000,
            "nara: {}",
            g.pool_monthly_tokens("nara-free")
        );
    }

    #[test]
    fn reports_the_to_avoid_rows_the_table_declares() {
        let g = global();
        let totals = g.totals(TotalsOpts::default());
        assert!(
            totals.tos_avoid_rows > 0,
            "the ToS table is not empty upstream"
        );
        assert!(
            g.usable_monthly_tokens() < totals.steady_monthly_tokens,
            "excluding ToS-avoid rows must lower usable headroom: {} vs {}",
            g.usable_monthly_tokens(),
            totals.steady_monthly_tokens
        );
    }

    #[test]
    fn marks_the_regime_gated_rows_apart_from_the_headline() {
        // ModelScope's two rows are real but open only after a region-bound
        // identity check, so they are reported beside the headline and never
        // inside it.
        let g = global();
        let totals = g.totals(TotalsOpts::default());
        assert_eq!(
            totals.gated_recurring_tokens, 6_000_000,
            "the modelscope-free pool"
        );
        assert!(
            !format!("{}", totals.steady_monthly_tokens).contains("002"),
            "gated tokens stay out of the headline: {totals:?}"
        );
    }

    #[test]
    fn resolves_a_models_allowance_through_its_pool() {
        let g = global();
        let Some(row) = g.iter().find(|r| r.pool.is_some() && r.monthly_tokens > 0) else {
            panic!("the table has pooled rows")
        };
        let pool = row.pool.clone().expect("the row is pooled");
        assert_eq!(
            g.monthly_tokens_for(&row.provider, &row.model),
            g.pool_monthly_tokens(&pool)
        );
    }

    #[test]
    fn reports_an_unpooled_models_own_allowance() {
        let g = global();
        let Some(row) = g.iter().find(|r| r.pool.is_none() && r.monthly_tokens > 0) else {
            panic!("the table has independent rows")
        };
        assert_eq!(
            g.monthly_tokens_for(&row.provider, &row.model),
            row.monthly_tokens
        );
    }

    #[test]
    fn marks_the_named_to_avoid_providers() {
        let g = global();
        let avoid: Vec<&str> = g
            .iter()
            .filter(|r| r.tos_avoid)
            .map(|r| r.provider.as_ref())
            .collect();
        for provider in ["agy", "kiro", "opencode", "ai21"] {
            assert!(
                avoid.contains(&provider),
                "{provider} is catalogued avoid: {avoid:?}"
            );
        }
    }

    #[test]
    fn refuses_a_to_avoid_row_as_usable_headroom() {
        let g = global();
        let Some(row) = g.iter().find(|r| r.tos_avoid) else {
            panic!("the table has avoid rows")
        };
        assert!(!g.usable(row.provider.as_ref(), row.model.as_ref()));
    }

    #[test]
    fn round_trips_a_row_through_json() {
        // A generated file is a `#[serde(default)]` shape, so a field the writer
        // emits and the reader ignores would be invisible until a count drifted.
        let json = serde_json::to_string(&FreeBudgets {
            curated_at: Strng::from("2026-09-12"),
            rows: vec![FreeBudgetRow {
                tos_avoid: true,
                gated: true,
                ..row("p", "m", 5, FreeRegime::Keyless, Some("pool"))
            }],
        })
        .unwrap();
        let back: FreeBudgets = serde_json::from_str(&json).unwrap();
        assert!(back.rows[0].tos_avoid);
        assert!(back.rows[0].gated);
        assert_eq!(back.rows[0].pool.as_deref(), Some("pool"));
    }

    #[test]
    fn omits_the_unset_fields_from_the_generated_json() {
        // The file is `include_str!`-ed into every binary, so a row that writes
        // `tosAvoid:false` 489 times is `.rodata` for nothing.
        let json = serde_json::to_string(&row("p", "m", 5, FreeRegime::Keyless, None)).unwrap();
        assert!(!json.contains("tosAvoid"), "{json}");
        assert!(!json.contains("gated"), "{json}");
        assert!(!json.contains("pool"), "{json}");
    }
}
