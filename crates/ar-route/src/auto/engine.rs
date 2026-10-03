//! The `auto/*` variant table and the virtual factory that resolves it.
//!
//! Ported from `../OmniRoute/open-sse/services/autoCombo/virtualFactory.ts`
//! (the `variant → weights` switch, L1097-1127) and `modePacks.ts`.
//!
//! Upstream reaches this through ~50k of candidate filtering — category/tier
//! specs, free-tier access quota, model-family matching, subscription ladder,
//! chaos panel assembly. None of that is in scope here: the caller owns the
//! pool and hands it over. What is in scope is the part that actually decides
//! *which* candidate wins, and the part an operator can see — the variant
//! table is what `ar_explain_route` names back, so it has to be the real one,
//! not a placeholder.

use crate::auto::scoring::Weights;

/// How many `auto/*` variants this build carries. Every spelling in a
/// `/v1/models` listing that starts with `auto` is one of these.
pub const AUTO_VARIANTS: usize = 8;

/// The `auto/*` variants this build carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AutoVariant {
    /// `auto` — balanced, stick to the last good provider.
    Balanced,
    /// `auto/coding` — quality-first weights for code generation.
    Coding,
    /// `auto/fast` — lowest latency first.
    Fast,
    /// `auto/cheap` — cheapest per token first.
    Cheap,
    /// `auto/smart` — quality-first plus exploration.
    Smart,
    /// `auto/chaos` — most stable panel, no exploration.
    Chaos,
    /// `auto/offline` — most quota / rate-limit headroom first.
    ///
    /// `quota` nearly triples against the balanced pack while `taskFit` goes to
    /// zero, which is the point: the provider with quota left is the one that
    /// can still answer.
    Offline,
    /// `auto/lkgp` — explicit last-known-good-provider stickiness.
    ///
    /// Not a different weighting: upstream every `auto` variant scores with the
    /// balanced pack and applies stickiness separately
    /// (`routerStrategy = "lkgp"`). This variant is the explicit spelling of
    /// that, so a config can ask for it by name.
    Lkgp,
}

impl AutoVariant {
    /// Every variant, in the upstream README's order. For `ar combo` and for
    /// iterating a pool in a test.
    pub const ALL: [Self; 8] = [
        Self::Balanced,
        Self::Coding,
        Self::Fast,
        Self::Cheap,
        Self::Smart,
        Self::Chaos,
        Self::Offline,
        Self::Lkgp,
    ];

    /// The model name a client sends. This is the string that has to survive
    /// a round trip through `/v1/models` and back, so it is the canonical
    /// spelling and not a display label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Balanced => "auto",
            Self::Coding => "auto/coding",
            Self::Fast => "auto/fast",
            Self::Cheap => "auto/cheap",
            Self::Smart => "auto/smart",
            Self::Chaos => "auto/chaos",
            Self::Offline => "auto/offline",
            Self::Lkgp => "auto/lkgp",
        }
    }

    /// Parses an `auto/*` model name. `None` for anything else, so a caller
    /// can branch on "is this an auto alias" without a second convention.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|v| v.as_str().eq_ignore_ascii_case(name))
    }

    /// The weight pack this variant scores with.
    ///
    /// Upstream applies one override on top of the switch: a category/tier
    /// spec (`auto/<category>:<tier>`) re-weights by tier. That is a
    /// `virtualFactory` filter concern, not a scorer one, and this build
    /// carries no category/tier names, so the switch alone is the whole table.
    #[must_use]
    pub fn weights(self) -> Weights {
        match self {
            Self::Balanced => Weights::balanced(),
            Self::Coding | Self::Smart => Weights::quality_first(),
            Self::Fast => Weights::ship_fast(),
            Self::Cheap => Weights::cost_saver(),
            Self::Chaos => Weights::chaos_mode(),
            Self::Offline => Weights::offline_friendly(),
            // Balanced weights plus the sticky pin upstream gives every `auto`
            // variant; naming the pin is the whole difference.
            Self::Lkgp => Weights::balanced(),
        }
    }

    /// How often to try a candidate outside the top tier, in `[0, 1]`.
    ///
    /// Upstream spends this rate on a `Math.random()` coin. This port does
    /// not: a scoring decision that is not reproducible is a scoring decision
    /// that cannot be explained, and `explain_route` has to be able to say why
    /// a provider won. The rate is kept because it is a real knob on the
    /// variant, and it drives the tier spread in [`selection::pick`].
    ///
    /// ponytail: uniform rotation instead of probabilistic sampling. Upgrade
    /// to a seeded PRNG per invocation if load spreading ever matters more
    /// than reproducibility.
    #[must_use]
    pub fn exploration_rate(self) -> f64 {
        match self {
            Self::Smart => 0.10,
            Self::Chaos => 0.0,
            _ => 0.05,
        }
    }
}

/// A resolved `auto/*` combo: which variant, how to weight, how much to spread.
///
/// `Copy` and 40 bytes, built once per request and borrowed everywhere else.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoCombo {
    /// The variant this combo came from.
    pub variant: AutoVariant,
    /// Whether to pin to the last provider that succeeded for this session.
    ///
    /// Every `auto` variant is LKGP in upstream (`routerStrategy = "lkgp"` for
    /// all of them); the sticky path is [`crate::Strategy::Lkgp`], which this
    /// crate already implements. Carrying the flag here is what lets
    /// `explain_route` report that the winner came from a pin rather than from
    /// the score.
    pub sticky: bool,
    /// Tier-spread ceiling, from [`AutoVariant::exploration_rate`].
    pub exploration_rate: f64,
}

impl AutoCombo {
    /// The variant's default combo.
    #[must_use]
    pub fn new(variant: AutoVariant) -> Self {
        Self {
            variant,
            sticky: true,
            exploration_rate: variant.exploration_rate(),
        }
    }

    /// The weight pack to score the pool with.
    #[must_use]
    pub fn weights(&self) -> Weights {
        self.variant.weights()
    }
}

/// The `auto/*` variant a requested model names, or `None` for a concrete model.
///
/// The exact contract the server stream calls: it holds the requested model
/// string from an inbound request and needs to know, *before* it builds a
/// candidate pool, whether that model is an alias or a real provider model. It is
/// pure, allocates nothing, and answers `None` rather than erroring — a concrete
/// model name is the normal case, not a failure, so a caller branches on it
/// instead of unwrapping.
///
/// Accepts every spelling the build carries, including `auto/balanced` as a
/// synonym for `auto`: the README and the UI both use the long form, and
/// rejecting it would make an operator-facing name a 404 for no reason. Matching
/// is case-insensitive, as [`AutoVariant::parse`] is.
#[must_use]
pub fn auto_variant_for_model(model: &str) -> Option<AutoVariant> {
    if model.eq_ignore_ascii_case(AutoVariant::Balanced.as_str())
        || model.eq_ignore_ascii_case("auto/balanced")
    {
        return Some(AutoVariant::Balanced);
    }
    AutoVariant::parse(model)
}

/// Resolves an `auto/*` model name into a combo.
///
/// # Errors
/// [`RouteError::UnknownAutoVariant`] when `name` is not one of
/// [`AutoVariant::ALL`]. A concrete model name is *not* an error here — the
/// caller resolves `auto/*` before the router ever sees a request, so this is
/// only ever called on a name that already looks like an alias.
pub fn virtual_combo(name: &str) -> Result<AutoCombo, crate::error::RouteError> {
    AutoVariant::parse(name).map(AutoCombo::new).ok_or_else(|| {
        crate::error::RouteError::UnknownAutoVariant {
            name: name.to_owned(),
        }
    })
}

/// The factory seam: name in, candidate pool out.
///
/// This exists as a type so the caller can hold one across requests and swap
/// the pool underneath, rather than re-deriving the alias match per request.
/// It holds no state and allocates nothing; the only reason it is a struct is
/// that `ar-mcp` (P4) wants to hang a `simulate_route` tool off it.
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtualFactory;

impl VirtualFactory {
    /// Builds a factory. Const, so a caller can use a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Resolves one `auto/*` model name.
    ///
    /// # Errors
    /// As [`virtual_combo`].
    pub fn combo(&self, name: &str) -> Result<AutoCombo, crate::error::RouteError> {
        virtual_combo(name)
    }

    /// Resolves every variant at once, for a `models` listing.
    ///
    /// # Errors
    /// Never in practice: the names come from [`AutoVariant::ALL`], which
    /// `parse` accepts by construction. The signature stays fallible so this
    /// does not have to be revisited when a variant gains a parse rule.
    pub fn all_combos(&self) -> Result<Vec<(AutoVariant, AutoCombo)>, crate::error::RouteError> {
        AutoVariant::ALL
            .into_iter()
            .map(|v| Ok((v, AutoCombo::new(v))))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AUTO_VARIANTS, AutoCombo, AutoVariant, VirtualFactory, auto_variant_for_model,
        virtual_combo,
    };
    use crate::Weights;
    use crate::error::RouteError;

    #[test]
    fn resolves_every_variant_when_named() {
        for v in AutoVariant::ALL {
            assert_eq!(virtual_combo(v.as_str()).expect("known alias").variant, v);
        }
    }

    #[test]
    fn round_trips_every_variant_name() {
        for v in AutoVariant::ALL {
            assert_eq!(AutoVariant::parse(v.as_str()), Some(v));
        }
    }

    #[test]
    fn rejects_unknown_variant() {
        let got = virtual_combo("auto/turbo");
        assert!(matches!(got, Err(RouteError::UnknownAutoVariant { .. })));
    }

    #[test]
    fn rejects_concrete_model_name() {
        let got = virtual_combo("gpt-4o");
        assert!(matches!(got, Err(RouteError::UnknownAutoVariant { .. })));
    }

    #[test]
    fn gives_cheap_the_cost_saver_pack() {
        let combo = virtual_combo("auto/cheap").expect("known alias");
        assert_eq!(combo.weights().cost_inv, 0.3324);
    }

    /// `auto/offline` is in the reference's advertised variant table
    /// (`README.md:356`) and resolves to its own mode pack, so "the provider with
    /// quota left wins" is reachable rather than a deferred name.
    #[test]
    fn gives_offline_the_quota_first_pack() {
        let combo = virtual_combo("auto/offline").expect("known alias");
        let w = combo.weights();
        assert_eq!(w.quota, Weights::offline_friendly().quota);
        assert!(
            w.quota > AutoVariant::Balanced.weights().quota,
            "offline must out-weigh the balanced pack on quota, {} vs {}",
            w.quota,
            AutoVariant::Balanced.weights().quota
        );
        assert_eq!(w.task_fit, 0.0, "offline scores no task fit at all");
    }

    #[test]
    fn gives_fast_the_ship_fast_pack() {
        let combo = virtual_combo("auto/fast").expect("known alias");
        let w = combo.weights();
        assert!(w.latency_inv > w.cost_inv);
    }

    #[test]
    fn makes_smart_explore_more_than_balanced() {
        let smart = AutoCombo::new(AutoVariant::Smart);
        let balanced = AutoCombo::new(AutoVariant::Balanced);
        assert!(smart.exploration_rate > balanced.exploration_rate);
    }

    #[test]
    fn pins_every_variant_to_the_sticky_path() {
        for v in AutoVariant::ALL {
            assert!(AutoCombo::new(v).sticky, "{v:?} is not sticky");
        }
    }

    #[test]
    fn lists_every_variant_from_the_factory() {
        let got = VirtualFactory::new()
            .all_combos()
            .expect("own variants parse");
        assert_eq!(got.len(), AUTO_VARIANTS);
    }

    #[test]
    fn parses_variant_name_case_insensitively() {
        assert_eq!(AutoVariant::parse("AUTO/CHEAP"), Some(AutoVariant::Cheap));
    }

    #[test]
    fn resolves_every_auto_spelling_when_asked_for_a_variant() {
        // The contract the server stream depends on: all seven names, one
        // `Option`, no error path to unwrap.
        for (name, want) in [
            ("auto", AutoVariant::Balanced),
            ("auto/coding", AutoVariant::Coding),
            ("auto/fast", AutoVariant::Fast),
            ("auto/cheap", AutoVariant::Cheap),
            ("auto/smart", AutoVariant::Smart),
            ("auto/balanced", AutoVariant::Balanced),
            ("auto/chaos", AutoVariant::Chaos),
            ("auto/offline", AutoVariant::Offline),
            ("auto/lkgp", AutoVariant::Lkgp),
        ] {
            assert_eq!(auto_variant_for_model(name), Some(want), "{name}");
        }
    }

    #[test]
    fn reports_no_variant_when_the_model_is_concrete() {
        assert_eq!(auto_variant_for_model("openai/gpt-4o"), None);
    }

    #[test]
    fn accepts_the_auto_prefix_case_insensitively() {
        assert_eq!(
            auto_variant_for_model("AUTO/CHAOS"),
            Some(AutoVariant::Chaos)
        );
    }

    #[test]
    fn reports_no_variant_for_an_unknown_auto_alias() {
        // `auto/turbo` is not a typo to be normalised to `auto`; it is an alias
        // this build does not carry, and the caller has to say so.
        assert_eq!(auto_variant_for_model("auto/turbo"), None);
    }
}
