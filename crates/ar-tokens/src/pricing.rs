//! Cost: USD held as integer micro-dollars, priced against a caller-supplied
//! table.
//!
//! Ported from `../OmniRoute/src/lib/usage/{modelPricingRegistry,costCalculator,
//! flatRateProviders}.ts`.
//!
//! [`PricingTable::from_registry`] loads the rows from the `ar-registry`
//! catalog, so a price has one home: `ar import` writes it, the router reads it,
//! and a model with no upstream row stays [`Cost::UNPRICED`] rather than
//! inheriting a guess. An empty table is the honest default — reporting a
//! made-up price would corrupt every budget decision downstream.

use std::collections::{HashMap, HashSet};

use crate::usage::NormalizedUsage;

/// A USD amount in micro-dollars (1e-6).
///
/// Integer because this is what the ledger stores: a `REAL` column would make
/// every cap comparison a float compare, and float money accumulates drift
/// across a busy month.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Usd {
    /// Micro-dollars. `15_000_000` is $15.
    pub micros: u64,
}

impl Usd {
    /// The zero amount.
    pub const ZERO: Self = Self { micros: 0 };

    /// Builds an amount from a per-million-token dollar price.
    #[must_use]
    pub fn per_mtok(dollars: f64) -> Self {
        Self { micros: (dollars * 1e6).max(0.0) as u64 }
    }

    /// The amount as a float, for display and reporting only.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        self.micros as f64 / 1e6
    }
}

/// Per-token prices for one model, expressed per million tokens in
/// micro-dollars so the whole table stays integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Prices {
    /// Micro-dollars per million prompt tokens.
    pub input_micros_per_mtok: u64,
    /// Micro-dollars per million completion tokens.
    pub output_micros_per_mtok: u64,
}

const fn cost_micros(tokens: u32, micros_per_mtok: u64) -> u64 {
    (tokens as u64 * micros_per_mtok) / 1_000_000
}

/// A priced (or deliberately unpriced) cost.
///
/// `priced` is the distinction OmniRoute added for its #12341: "$0 priced" is a
/// genuinely free flat-rate model, "$0 unpriced" is a routing alias with no
/// catalog row. Collapsing them to a bare `f64` would let an unknown model pass
/// a hard budget cap by looking free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cost {
    /// What this request cost.
    pub usd: Usd,
    /// `false` when no pricing row was found.
    pub priced: bool,
}

impl Cost {
    /// $0 with no pricing row behind it.
    pub const UNPRICED: Self = Self { usd: Usd::ZERO, priced: false };
}

/// Provider → model → prices, plus the flat-rate provider set.
///
/// Lookup is case-insensitive on both halves and falls back to the
/// provider-path-stripped model name, so `openai/gpt-4o` and `gpt-4o` resolve
/// to the same row — the same composite key and the same fallback the TS
/// registry uses.
#[derive(Debug, Default)]
pub struct PricingTable {
    rows: HashMap<String, Prices>,
    flat_rate: HashSet<String>,
}

/// NUL-separated so a provider name can never collide with a model name.
fn row_key(provider: &str, model: &str) -> String {
    format!("{}\0{}", provider.trim().to_ascii_lowercase(), model.trim().to_ascii_lowercase())
}

impl PricingTable {
    /// Registers (or replaces) the price row for one provider/model pair.
    pub fn set(&mut self, provider: &str, model: &str, prices: Prices) {
        self.rows.insert(row_key(provider, model), prices);
    }

    /// Loads every priced row and every flat-rate provider from the registry.
    ///
    /// The only loader that should exist: the catalog is generated, so a table
    /// built by hand here would be a second copy to keep in sync with it. A model
    /// the catalog has no row for stays unpriced, which is what `cost-optimized`
    /// sorts last on and what a budget cap must not read as free.
    #[must_use]
    pub fn from_registry(registry: &ar_registry::Registry) -> Self {
        let mut t = Self::default();
        for (provider, model, price) in registry.prices() {
            t.set(
                provider,
                model,
                Prices {
                    input_micros_per_mtok: Self::to_micros(price.input_usd_per_mtok),
                    output_micros_per_mtok: Self::to_micros(price.output_usd_per_mtok),
                },
            );
        }
        for provider in registry.flat_rate_providers() {
            t.set_flat_rate(provider);
        }
        t
    }

    /// The process-wide registry's table.
    ///
    /// Lazy through `ar-registry`'s own `OnceLock`, so this costs nothing until a
    /// price is actually asked for.
    #[must_use]
    pub fn global() -> Self {
        Self::from_registry(ar_registry::global())
    }

    /// USD per MTok to micro-dollars, saturating a negative rate at zero.
    fn to_micros(dollars: f64) -> u64 {
        (dollars * 1e6).max(0.0) as u64
    }

    /// Marks a provider as billed at a flat rate — a subscription or coding
    /// plan rather than per token.
    ///
    /// Such a provider still needs a per-token row, because the row is what the
    /// quota pre-flight estimates against; only the *charge* collapses to $0.
    ///
    /// [`PricingTable::from_registry`] wires the ids from `registry.json`, where
    /// `ar import` writes them; nothing is hard-coded here.
    pub fn set_flat_rate(&mut self, provider: &str) {
        self.flat_rate.insert(provider.trim().to_ascii_lowercase());
    }

    /// Whether `provider` was declared flat-rate.
    #[must_use]
    pub fn is_flat_rate(&self, provider: &str) -> bool {
        self.flat_rate.contains(provider.trim())
    }

    /// Cost of `usage` for one request.
    ///
    /// A flat-rate provider returns [`Cost`] with `usd: ZERO` and `priced: true`:
    /// a real $0, not an unknown. An unpriced model returns [`Cost::UNPRICED`].
    #[must_use]
    pub fn cost(&self, provider: &str, model: &str, usage: NormalizedUsage) -> Cost {
        if self.is_flat_rate(provider) {
            return Cost { usd: Usd::ZERO, priced: true };
        }
        let Some(prices) = self.prices(provider, model) else {
            return Cost::UNPRICED;
        };
        let micros = cost_micros(usage.prompt, prices.input_micros_per_mtok)
            .saturating_add(cost_micros(usage.completion, prices.output_micros_per_mtok));
        Cost { usd: Usd { micros }, priced: true }
    }

    fn prices(&self, provider: &str, model: &str) -> Option<Prices> {
        if let Some(prices) = self.rows.get(&row_key(provider, model)) {
            return Some(*prices);
        }
        // "accounts/fireworks/models/x" and "fireworks/x" both mean "x".
        self.rows.get(&row_key(provider, strip_provider_path(model))).copied()
    }
}

fn strip_provider_path(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

#[cfg(test)]
mod tests {
    use super::{Cost, Prices, PricingTable, Usd};
    use crate::usage::NormalizedUsage;

    fn table() -> PricingTable {
        let mut t = PricingTable::default();
        t.set("openai", "gpt-4o", Prices { input_micros_per_mtok: 2_500_000, output_micros_per_mtok: 10_000_000 });
        t
    }

    #[test]
    fn prices_known_model_when_row_present() {
        let cost = table().cost("openai", "gpt-4o", NormalizedUsage::new(1_000_000, 1_000_000));
        assert_eq!(cost.usd.micros, 12_500_000);
    }

    #[test]
    fn reports_unpriced_when_no_row_found() {
        assert_eq!(table().cost("openai", "unknown", NormalizedUsage::new(1, 1)), Cost::UNPRICED);
    }

    #[test]
    fn resolves_row_through_provider_path_prefix() {
        let cost = table().cost("openai", "accounts/fireworks/models/gpt-4o", NormalizedUsage::new(1_000_000, 0));
        assert_eq!(cost.usd.micros, 2_500_000);
    }

    #[test]
    fn matches_row_case_insensitively() {
        let cost = table().cost("OpenAI", "GPT-4O", NormalizedUsage::new(1_000_000, 0));
        assert_eq!(cost.usd.micros, 2_500_000);
    }

    #[test]
    fn costs_zero_but_priced_when_flat_rate_provider() {
        let mut t = table();
        t.set_flat_rate("anthropic");
        let cost = t.cost("anthropic", "claude-sonnet-4", NormalizedUsage::new(1_000_000, 1_000_000));
        assert_eq!(cost, Cost { usd: Usd::ZERO, priced: true });
    }

    #[test]
    fn converts_per_mtok_dollars_to_micros() {
        assert_eq!(Usd::per_mtok(15.0).micros, 15_000_000);
    }

    #[test]
    fn prices_known_model_when_loaded_from_registry() {
        let cost = PricingTable::global().cost("openai", "gpt-5.4", NormalizedUsage::new(1_000_000, 0));
        assert!(cost.priced, "the generated catalog carries an openai/gpt-5.4 row");
        assert!(cost.usd.micros > 0, "{cost:?}");
    }

    #[test]
    fn reports_unpriced_when_registry_has_no_row() {
        let cost = PricingTable::global().cost("openai", "definitely-not-a-model", NormalizedUsage::new(1, 1));
        assert_eq!(cost, Cost::UNPRICED);
    }

    #[test]
    fn marks_flat_rate_providers_from_registry() {
        // `claude` is the Claude Code plan: a subscription, so $0 but priced.
        let cost = PricingTable::global().cost("claude", "claude-sonnet-5", NormalizedUsage::new(1_000_000, 1_000_000));
        assert_eq!(cost, Cost { usd: Usd::ZERO, priced: true });
    }

    #[test]
    fn sorts_a_cheap_model_under_a_pricey_one() {
        // The claim `cost-optimized` rests on: the table reports a real
        // difference, not the order rows were inserted in.
        let t = PricingTable::global();
        let cheap = t.cost("openai", "gpt-5.4-nano", NormalizedUsage::new(1_000_000, 0));
        let pricey = t.cost("openai", "gpt-5.4", NormalizedUsage::new(1_000_000, 0));
        assert!(cheap.priced && pricey.priced);
        assert!(cheap.usd.micros < pricey.usd.micros, "cheap {cheap:?} vs pricey {pricey:?}");
    }
}
