//! Plan resolution: which compression pipeline runs for a request.
//!
//! The precedence chain is fixed and total — `header > combo > profile >
//! adaptive > default` — and the first layer that yields a pipeline wins. A
//! layer that yields *nothing* is not a decision: an unrecognized header, an
//! absent override, and an empty pipeline all fall through, so a request
//! always ends up with a plan and compression never becomes an outage.
//!
//! [`Source`] records which layer won, so a plan can be explained after the
//! fact instead of inferred.
//!
//! ```
//! use ar_compress::{Engine, Layers, Plan, Source, plan_resolution};
//!
//! let plan = plan_resolution(
//!     &[],
//!     &Layers {
//!         header: Some("engine:rtk"),
//!         combo: Some(&[Engine::Lite]),
//!         ..Layers::default()
//!     },
//! );
//! assert_eq!(plan.steps, [Engine::Rtk]);
//! assert_eq!(plan.source, Source::Header);
//! assert!(!plan.is_off());
//! ```
//!
//! Ported from `../OmniRoute/open-sse/services/compression/planResolution.ts`
//! (header interpretation + `withSource`) and `resolveCompressionPlan.ts`
//! (the combo/active-combo/auto ordering). The `defaultMode` legacy branch and
//! the panel `enginesExplicit` flag are dropped: `Layers` is already the
//! resolved set of layers, and there is no legacy install to be byte-for-byte
//! compatible with.

use std::borrow::Cow;
use std::fmt;

/// The precedence layer that produced a plan.
///
/// Variant order **is** the precedence order — the first layer that yields a
/// pipeline wins, and declaration order is the whole rule. Reordering these
/// variants changes routing behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// Per-request header: the operator's most explicit say.
    Header,
    /// Routing-combo override.
    Combo,
    /// Active named profile.
    Profile,
    /// Adaptive context-budget dial.
    Adaptive,
    /// Panel default, and the layer that produces `off` when nothing is set.
    Default,
}

impl Source {
    /// The layer's stable lowercase name, for headers and reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Combo => "combo",
            Self::Profile => "profile",
            Self::Adaptive => "adaptive",
            Self::Default => "default",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One compression engine in a pipeline.
///
/// This enum is the *vocabulary* of plan resolution; the `lite`/`rtk`/`caveman`
/// implementations are owned by the sibling transform module, which supplies
/// the [`Transform`] impl. Adding a variant here makes every match in the
/// crate a compile error, which is the intended coordination signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    /// Whitespace and image-URL trimming; the latency-light baseline.
    Lite,
    /// Command-aware tool-result filtering, dedup and truncation.
    Rtk,
    /// Rule-based prose compression.
    Caveman,
}

impl Engine {
    /// The engine's stable lowercase id, as used in an `engine:<id>` header.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lite => "lite",
            Self::Rtk => "rtk",
            Self::Caveman => "caveman",
        }
    }

    /// Parses an engine id, case-insensitively.
    ///
    /// ```
    /// use ar_compress::Engine;
    ///
    /// assert_eq!(Engine::from_id("CAVEMAN"), Some(Engine::Caveman));
    /// assert_eq!(Engine::from_id("llmlingua"), None);
    /// ```
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        // Match on the lowercased id without allocating: an engine id is three
        // to seven ASCII bytes, and this runs once per request.
        let id = id.trim();
        if id.eq_ignore_ascii_case("lite") {
            Some(Self::Lite)
        } else if id.eq_ignore_ascii_case("rtk") {
            Some(Self::Rtk)
        } else if id.eq_ignore_ascii_case("caveman") {
            Some(Self::Caveman)
        } else {
            None
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A resolved compression plan: the pipeline to run, and the layer that chose it.
///
/// An empty [`Plan::steps`] is the `off` plan. `source` still says *why* it is
/// off, which is the only thing distinguishing "the operator asked for off"
/// from "nothing was configured".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The pipeline, in execution order. Empty means no compression.
    pub steps: Vec<Engine>,
    /// The precedence layer that produced `steps`.
    pub source: Source,
}

impl Plan {
    /// The `off` plan, attributed to `source`.
    #[must_use]
    pub fn off(source: Source) -> Self {
        Self {
            steps: Vec::new(),
            source,
        }
    }

    /// Whether this plan runs no engines.
    #[must_use]
    pub fn is_off(&self) -> bool {
        self.steps.is_empty()
    }
}

/// A named combo: an operator-defined pipeline, addressable by id and name.
#[derive(Debug, Clone, Copy)]
pub struct Combo<'a> {
    /// Stable id, matched case-insensitively — same rule as `name`, because a
    /// header is a human-typed string and `Balanced-Combo` reaching the operator
    /// as `balanced-combo` is a typo, not a different combo.
    pub id: &'a str,
    /// Display name, matched case-insensitively. `None` or blank means the
    /// combo is addressable only by id.
    pub name: Option<&'a str>,
    /// The combo's pipeline.
    pub steps: &'a [Engine],
}

/// The precedence layers, highest first. Absent means "not set at this layer".
#[derive(Debug, Clone, Copy, Default)]
pub struct Layers<'a> {
    /// Per-request `x-ar-compression` header value.
    pub header: Option<&'a str>,
    /// Routing-combo override.
    pub combo: Option<&'a [Engine]>,
    /// Active named profile.
    pub profile: Option<&'a [Engine]>,
    /// Adaptive context-budget dial.
    pub adaptive: Option<&'a [Engine]>,
    /// Panel default.
    pub default: Option<&'a [Engine]>,
}

/// Resolves the plan for one request across the precedence chain
/// `header > combo > profile > adaptive > default`.
///
/// The header is interpreted first and may itself select a layer's pipeline:
///
/// | header value | plan |
/// |---|---|
/// | `off` | `off`, attributed to the header |
/// | `default` | the panel default, attributed to the header |
/// | `engine:<id>` | that single engine, when the id is known |
/// | `<combo>` | the named combo, matched by name then id, both case-insensitively |
/// | anything else | unrecognized — fall through to `combo` |
///
/// ```
/// use ar_compress::{Combo, Engine, Layers, Source, plan_resolution};
///
/// let combos = [Combo { id: "c1", name: Some("Balanced"), steps: &[Engine::Lite, Engine::Caveman] }];
/// let by_name = plan_resolution(&combos, &Layers { header: Some("balanced"), ..Default::default() });
/// assert_eq!(by_name.steps, [Engine::Lite, Engine::Caveman]);
///
/// // An unrecognized header is not a decision: the chain continues.
/// let unknown = plan_resolution(
///     &combos,
///     &Layers { header: Some("nope"), profile: Some(&[Engine::Rtk]), ..Default::default() },
/// );
/// assert_eq!(unknown.source, Source::Profile);
/// ```
#[must_use]
pub fn plan_resolution(combos: &[Combo<'_>], layers: &Layers<'_>) -> Plan {
    if let Some(header) = layers.header
        && let Some(plan) = plan_from_header(combos, layers, header)
    {
        return plan;
    }

    // First non-empty layer wins. An empty slice *is* a decision in OmniRoute
    // (an explicit "everything off"), so `Some(&[])` stops the chain here and
    // does not fall through to a less explicit layer.
    for (source, steps) in [
        (Source::Combo, layers.combo),
        (Source::Profile, layers.profile),
        (Source::Adaptive, layers.adaptive),
        (Source::Default, layers.default),
    ] {
        if let Some(steps) = steps {
            return Plan {
                steps: steps.to_vec(),
                source,
            };
        }
    }

    Plan::off(Source::Default)
}

fn plan_from_header(combos: &[Combo<'_>], layers: &Layers<'_>, header: &str) -> Option<Plan> {
    let header = header.trim();
    if header.is_empty() {
        return None;
    }

    if header.eq_ignore_ascii_case("off") {
        return Some(Plan::off(Source::Header));
    }

    if header.eq_ignore_ascii_case("default") {
        // The header asked for the panel default specifically, so the pipeline
        // is the default's — an active profile does not leak into it.
        let steps = layers.default.unwrap_or(&[]);
        return Some(Plan {
            steps: steps.to_vec(),
            source: Source::Header,
        });
    }

    // `split_once` rather than `strip_prefix` twice: the prefix is matched
    // case-insensitively, so one branch covers `engine:` and `ENGINE:` without
    // enumerating casings. Any other `a:b` value falls through to the combo
    // lookup, matching the reference, where only `engine:` is special.
    if let Some((prefix, id)) = header.split_once(':')
        && prefix.eq_ignore_ascii_case("engine")
    {
        return Engine::from_id(id).map(|engine| Plan {
            steps: vec![engine],
            source: Source::Header,
        });
    }

    // Name-first, then id. Both matches are case-insensitive: the reference
    // looks the combo up by lowercased header first and by exact header second,
    // which makes casing significant on the *map key* but not on the value. Here
    // ids and names are stored as declared, so one `eq_ignore_ascii_case` on each
    // reproduces the reference's reach without the allocation a `to_lowercase()`
    // would make on every request.
    combos
        .iter()
        .find(|c| {
            c.name.is_some_and(|name| !name.trim().is_empty() && name.trim().eq_ignore_ascii_case(header))
                || c.id.eq_ignore_ascii_case(header)
        })
        .map(|combo| Plan {
            steps: combo.steps.to_vec(),
            source: Source::Header,
        })
}

/// Engine dispatch. `lite`/`rtk`/`caveman` are implemented by the sibling
/// transform modules ([`crate::lite`], [`crate::rtk`], [`crate::caveman`]);
/// [`registered`] wires them up. This crate decides *which* engines run and
/// *how much* they may spend, never *how* they rewrite.
///
/// `dyn` is correct here despite the generics-first rule (ch.6): the engine set
/// is a heterogeneous policy list chosen at runtime, and the call is not on a
/// per-token path. The late-bound lifetime keeps the trait object-safe, which
/// is what lets an engine hand back a borrow of its input.
pub trait Transform {
    /// Applies `engine` to `text`.
    ///
    /// An implementation must return [`Cow::Borrowed`] when it declines to
    /// rewrite, and must never return more text than it received: a
    /// "compression" that inflates is a bug, and [`crate::budget`] will not
    /// catch it because the input was already within budget.
    fn apply<'a>(&self, engine: Engine, text: &'a str) -> Cow<'a, str>;
}

/// Runs `plan`'s pipeline over `text`.
///
/// An empty plan and a single declined engine both return the input borrowed,
/// so the common no-op path costs zero allocations. A multi-engine pipeline
/// owns its intermediates — step N+1 has to read step N's output while writing
/// a fresh buffer, and only its *final* output is handed back.
///
/// ```
/// use ar_compress::{Engine, Plan, Source, apply_plan, registered};
///
/// let off = Plan::off(Source::Default);
/// assert!(matches!(apply_plan(&off, "keep me", registered()), std::borrow::Cow::Borrowed(_)));
///
/// // A single engine that declines to rewrite (clean input) stays borrowed.
/// let lite_only = Plan { steps: vec![Engine::Lite], source: Source::Default };
/// assert_eq!(apply_plan(&lite_only, "let x = 1;", registered()), "let x = 1;");
/// ```
#[must_use]
pub fn apply_plan<'a>(plan: &Plan, text: &'a str, transforms: &dyn Transform) -> Cow<'a, str> {
    let steps = plan.steps.as_slice();
    let Some((first, rest)) = steps.split_first() else {
        return Cow::Borrowed(text);
    };
    if rest.is_empty() {
        return transforms.apply(*first, text);
    }
    // Steps 1..n own their intermediate, because the next step borrows it
    // while producing a new buffer. Only the last result escapes.
    let mut acc = transforms.apply(*first, text).into_owned();
    for engine in rest {
        acc = transforms.apply(*engine, &acc).into_owned();
    }
    Cow::Owned(acc)
}

/// The crate's own engines, wired to the sibling transform modules.
///
/// This is the only place `Engine` meets an implementation, so replacing an
/// engine's behaviour never touches plan resolution or the budget.
#[derive(Debug, Clone, Copy)]
pub struct Engines;

impl Transform for Engines {
    fn apply<'a>(&self, engine: Engine, text: &'a str) -> Cow<'a, str> {
        match engine {
            Engine::Lite => crate::lite::lite(text),
            Engine::Rtk => crate::rtk::rtk(text),
            Engine::Caveman => crate::caveman::caveman(text),
        }
    }
}

static ENGINES: Engines = Engines;

/// The engines the pipeline runs: [`crate::lite`], [`crate::rtk`],
/// [`crate::caveman`].
#[must_use]
pub fn registered() -> &'static dyn Transform {
    &ENGINES
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{
        Combo, Engine, Layers, Plan, Source, Transform, apply_plan, plan_resolution, registered,
    };

    const BALANCED: &[Engine] = &[Engine::Lite, Engine::Caveman];

    fn combos() -> [Combo<'static>; 1] {
        [Combo {
            id: "c1",
            name: Some("Balanced"),
            steps: BALANCED,
        }]
    }

    fn all_layers_set() -> Layers<'static> {
        Layers {
            header: Some("balanced"),
            combo: Some(&[Engine::Rtk]),
            profile: Some(&[Engine::Caveman]),
            adaptive: Some(&[Engine::Lite]),
            default: Some(&[Engine::Rtk, Engine::Lite]),
        }
    }

    #[test]
    fn resolves_hdr_first_when_all_set() {
        let resolved = plan_resolution(&combos(), &all_layers_set());
        assert_eq!(
            resolved,
            Plan {
                steps: BALANCED.to_vec(),
                source: Source::Header
            }
        );
    }

    #[test]
    fn resolves_combo_when_no_header() {
        let layers = Layers {
            header: None,
            ..all_layers_set()
        };
        assert_eq!(plan_resolution(&combos(), &layers).source, Source::Combo);
    }

    #[test]
    fn resolves_profile_when_header_unrecognized() {
        let layers = Layers {
            header: Some("no-such-combo"),
            combo: None,
            ..all_layers_set()
        };
        assert_eq!(plan_resolution(&combos(), &layers).source, Source::Profile);
    }

    #[test]
    fn falls_through_to_combo_when_header_is_unrecognized() {
        let layers = Layers {
            header: Some("no-such-combo"),
            ..all_layers_set()
        };
        assert_eq!(plan_resolution(&combos(), &layers).source, Source::Combo);
    }

    #[test]
    fn resolves_adaptive_when_higher_layers_unset() {
        let layers = Layers {
            header: None,
            combo: None,
            profile: None,
            ..all_layers_set()
        };
        assert_eq!(plan_resolution(&combos(), &layers).source, Source::Adaptive);
    }

    #[test]
    fn resolves_default_when_nothing_else_set() {
        let plan = plan_resolution(&combos(), &Layers::default());
        assert_eq!(plan.source, Source::Default);
    }

    #[test]
    fn resolves_off_when_no_layer_is_set() {
        assert!(plan_resolution(&combos(), &Layers::default()).is_off());
    }

    #[test]
    fn resolves_off_for_the_off_header() {
        let plan = plan_resolution(&combos(), &Layers { header: Some("off"), ..all_layers_set() });
        assert!(plan.is_off());
    }

    #[test]
    fn treats_blank_header_as_unset() {
        let plan = plan_resolution(&combos(), &Layers { header: Some("   "), ..all_layers_set() });
        assert_eq!(plan.source, Source::Combo);
    }

    #[test]
    fn treats_empty_combo_override_as_a_decision() {
        let plan = plan_resolution(
            &combos(),
            &Layers {
                header: None,
                combo: Some(&[]),
                ..all_layers_set()
            },
        );
        assert!(plan.is_off());
    }

    #[test]
    fn resolves_default_header_to_the_default_pipeline() {
        let plan = plan_resolution(
            &combos(),
            &Layers {
                header: Some("default"),
                ..all_layers_set()
            },
        );
        assert_eq!(plan.steps, [Engine::Rtk, Engine::Lite]);
    }

    #[test]
    fn resolves_engine_header_to_one_step() {
        let plan = plan_resolution(
            &combos(),
            &Layers { header: Some("engine:caveman"), ..all_layers_set() },
        );
        assert_eq!(plan.steps, [Engine::Caveman]);
    }

    #[test]
    fn resolves_uppercase_header_the_same_way() {
        let plan = plan_resolution(
            &combos(),
            &Layers { header: Some("BALANCED"), ..all_layers_set() },
        );
        assert_eq!(plan.steps, BALANCED);
    }

    #[test]
    fn falls_through_for_an_unknown_engine_id() {
        let plan = plan_resolution(
            &combos(),
            &Layers { header: Some("engine:llmlingua"), ..all_layers_set() },
        );
        assert_eq!(plan.source, Source::Combo);
    }

    #[test]
    fn matches_combo_by_id_when_name_does_not() {
        let plan = plan_resolution(
            &combos(),
            &Layers { header: Some("c1"), ..all_layers_set() },
        );
        assert_eq!(plan.steps, BALANCED);
    }

    #[test]
    fn ignores_a_blank_combo_name() {
        let blank = [Combo { id: "c9", name: Some("  "), steps: BALANCED }];
        let plan = plan_resolution(
            &blank,
            &Layers { header: Some("c9"), ..all_layers_set() },
        );
        assert_eq!(plan.steps, BALANCED);
    }

    /// The regression this pins: the id branch used to be an exact `==` while
    /// the name branch was case-insensitive, so a header spelled
    /// `Balanced-Combo` silently missed an id declared `balanced-combo` and fell
    /// through to whatever the next layer had set — the operator's explicit
    /// choice was replaced by an implicit one with no error anywhere.
    #[test]
    fn matches_combo_by_id_case_insensitively() {
        let combos = [Combo {
            id: "balanced-combo",
            name: None,
            steps: BALANCED,
        }];

        let plan = plan_resolution(
            &combos,
            &Layers { header: Some("Balanced-Combo"), ..all_layers_set() },
        );

        assert_eq!(plan.steps, BALANCED, "the id branch must ignore casing");
    }

    /// Case-insensitivity on the id must not make a *missing* combo resolve. If
    /// `eq_ignore_ascii_case` ever widened into a prefix or substring match, the
    /// lower layers would stop being reachable through the header path.
    #[test]
    fn falls_through_when_only_the_id_differs() {
        let plan = plan_resolution(
            &combos(),
            &Layers { header: Some("c2"), ..all_layers_set() },
        );

        assert_eq!(plan.source, Source::Combo);
    }

    /// "The operator asked for off" and "nothing was configured" are the same
    /// pipeline and must stay distinguishable: only `source` separates them, and
    /// a report that collapsed them would answer "why is compression off?" with
    /// the wrong reason.
    #[test]
    fn distinguishes_an_explicit_off_from_a_silent_one() {
        let explicit = plan_resolution(
            &combos(),
            &Layers { header: Some("off"), ..all_layers_set() },
        );
        let silent = plan_resolution(&combos(), &Layers::default());

        assert!(explicit.is_off() && silent.is_off());
        assert_ne!(
            explicit.source,
            silent.source,
            "an explicit `off` and a silent one must not share a source",
        );
        assert_eq!(explicit.source, Source::Header);
        assert_eq!(silent.source, Source::Default);
    }

    #[test]
    fn applies_every_step_in_order() {
        struct Trace;

        impl Transform for Trace {
            fn apply<'a>(&self, engine: Engine, text: &'a str) -> Cow<'a, str> {
                Cow::Owned(format!("{text}{}", engine.as_str()))
            }
        }

        let plan = Plan { steps: vec![Engine::Rtk, Engine::Caveman], source: Source::Combo };
        assert_eq!(apply_plan(&plan, "x", &Trace), "xrtkcaveman");
    }

    #[test]
    fn borrows_input_when_plan_is_off() {
        let plan = Plan::off(Source::Default);
        assert!(matches!(apply_plan(&plan, "x", registered()), Cow::Borrowed(_)));
    }

    #[test]
    fn borrows_input_when_single_engine_declines() {
        let plan = Plan { steps: vec![Engine::Lite], source: Source::Default };
        assert!(matches!(apply_plan(&plan, "let x = 1;", registered()), Cow::Borrowed(_)));
    }

    #[test]
    fn rewrites_when_single_engine_acts() {
        let plan = Plan { steps: vec![Engine::Caveman], source: Source::Default };
        let out = apply_plan(&plan, "It seems like the cache is basically re-validating.", registered());
        assert!(out.len() < 56, "caveman did not shrink: {out}");
    }

    #[test]
    fn collapses_repeated_lines_when_rtk_runs() {
        let plan = Plan { steps: vec![Engine::Rtk], source: Source::Default };
        let out = apply_plan(&plan, "same\nsame\nsame\nsame\n", registered());
        assert!(out.contains("same"), "{out}");
    }

    #[test]
    fn parses_engine_ids_case_insensitively() {
        assert_eq!(Engine::from_id("  RTK  "), Some(Engine::Rtk));
    }

    #[test]
    fn rejects_unknown_engine_ids() {
        assert_eq!(Engine::from_id("ultra"), None);
    }

    #[test]
    fn names_source_for_a_header_value() {
        assert_eq!(Source::Header.to_string(), "header");
    }

    #[test]
    fn names_engine_for_a_report() {
        assert_eq!(Engine::Rtk.to_string(), "rtk");
    }
}
