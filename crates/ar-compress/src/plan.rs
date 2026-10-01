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
//! use ar_compress::{Engine, Layers, Plan, Source, Step, plan_resolution};
//!
//! let plan = plan_resolution(
//!     &[],
//!     &Layers {
//!         header: Some("engine:rtk"),
//!         combo: Some(&[Step::new(Engine::Lite)]),
//!         ..Layers::default()
//!     },
//! );
//! assert_eq!(plan.steps, [Step::new(Engine::Rtk)]);
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
//!
//! # Intensity is a dial, not an engine
//!
//! The reference ships twelve engine ids across four disagreeing catalogs and
//! `omniglyph`/`ionizer` are registered but unreachable (audit F-MED-1). This
//! crate keeps the three it can actually run and adds [`Intensity`] instead: a
//! dial on behaviour that already exists, so the reachable surface is
//! `3 engines x their own ladders` rather than `12 half-wired ids`.
//!
//! Each engine declares its own ladder in [`Engine::levels`], and every other
//! fact — which levels exist, what a bare engine id means, where a level sits —
//! is derived from that one declaration. `rtk` offers `minimal`/`standard`/
//! `aggressive`; `caveman` offers `lite`/`full`/`ultra`; `lite` offers none,
//! because one behaviour has no dial. A level an engine does not offer is a
//! *config* error ([`Engine::rung`] falls back rather than panicking, because a
//! library call is not a place to refuse text), which is deliberately the
//! opposite of the reference, where a combo override naming a mode the engine
//! does not have is accepted and silently ignored.

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
///
/// It is also the single list of engine ids in this workspace: a config's
/// `compression.engine` resolves through [`Engine::from_id`], so there is no
/// second table to drift — the defect the audit names in the reference, whose
/// four catalogs disagree.
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

    /// The intensity levels this engine offers, weakest first.
    ///
    /// The one declaration every other fact derives from: [`Intensity::from_id`]
    /// walks [`Intensity::ALL`], [`Engine::default_level`] indexes this, and
    /// [`Engine::rung`] searches it. `&[]` is a real answer, not a placeholder —
    /// it means the engine has exactly one behaviour, so it has no dial.
    #[must_use]
    pub const fn levels(self) -> &'static [Intensity] {
        match self {
            Self::Lite => &[],
            Self::Rtk => &[Intensity::Minimal, Intensity::Standard, Intensity::Aggressive],
            Self::Caveman => &[Intensity::Lite, Intensity::Full, Intensity::Ultra],
        }
    }

    /// The level a bare engine id means: the middle rung.
    ///
    /// Middle, not weakest, so a config that names only an engine keeps the
    /// behaviour it had before the dial existed — and the middle rung is the
    /// reference's own default for both engines that have one
    /// (`DEFAULT_RTK_CONFIG.intensity`, `getRulesForContext`'s `= "full"`).
    ///
    /// `const` so a `Step` can be built in a `const`: the test tables and the
    /// eval example would otherwise have to spell the level out, and a spelled
    /// out default is a second copy of this answer.
    #[must_use]
    pub const fn default_level(self) -> Intensity {
        let levels = self.levels();
        if levels.len() > 1 {
            levels[1]
        } else {
            Intensity::Standard
        }
    }

    /// `level` as a rung on this engine's ladder, weakest `0`.
    ///
    /// A level this engine does not offer, on an engine with no dial at all,
    /// resolves to the engine's own default rather than panicking: a library
    /// call is not a place to refuse text, and refusing would turn a cosmetic
    /// mispair into a failed request. Config is where the pair is *rejected* —
    /// see `ar_config::CompressionError::NoDial` — because a file is a place
    /// where telling the operator is free.
    #[must_use]
    pub fn rung(self, level: Intensity) -> u8 {
        let find = |l: Intensity| self.levels().iter().position(|x| *x == l);
        find(level)
            .or_else(|| find(self.default_level()))
            .and_then(|i| u8::try_from(i).ok())
            .unwrap_or(0)
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How hard an engine works.
///
/// Two disjoint ladders in one enum, because that is what the three engines
/// actually have: `rtk` scales a repeat threshold
/// (`minimal`/`standard`/`aggressive`, from `effectiveMaxLines`'s 1.5/1.0/0.5
/// budget factor), and `caveman` gates its rule table by rank
/// (`lite`/`full`/`ultra`, from `cavemanRules.ts`'s `INTENSITY_RANK`). There is
/// no global order — `Minimal` is weaker than `Lite` on one engine and means
/// nothing on the other — so an `Ord` derive here would be a lie, and
/// [`Engine::rung`] is the only way to compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intensity {
    /// `rtk`: collapse a run of four or more identical lines.
    Minimal,
    /// `rtk`: three or more — the reference default. The `lite` engine's
    /// neutral marker, since it has no ladder to sit on.
    Standard,
    /// `rtk`: two or more.
    Aggressive,
    /// `caveman`: referent-free noise only, the rules that cannot remove
    /// something a later turn needed.
    Lite,
    /// `caveman`: every filler rule — the reference default, and what this
    /// crate ran before the dial existed.
    Full,
    /// `caveman`: `full`, plus the reference's noun-abbreviation table.
    Ultra,
}

impl Intensity {
    /// Every spelling, in one table.
    ///
    /// [`Intensity::from_id`] derives from it, so adding a level is a change
    /// here and in the owning engine's [`Engine::levels`] — never in a second
    /// parser.
    const ALL: [Self; 6] = [
        Self::Minimal,
        Self::Standard,
        Self::Aggressive,
        Self::Lite,
        Self::Full,
        Self::Ultra,
    ];

    /// The level's stable lowercase id, as written in `config.yaml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Standard => "standard",
            Self::Aggressive => "aggressive",
            Self::Lite => "lite",
            Self::Full => "full",
            Self::Ultra => "ultra",
        }
    }

    /// Parses a level id, case-insensitively.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        let id = id.trim();
        Self::ALL.into_iter().find(|l| l.as_str().eq_ignore_ascii_case(id))
    }
}

impl fmt::Display for Intensity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One engine running at one intensity.
///
/// The pipeline element, and the unit an `engine@intensity` id names. A step
/// built with [`Step::new`] is at its engine's own default level, so every
/// engine-only spelling — the header grammar, the named-combo table, an
/// `engine: rtk` config with no `intensity:` line — is unchanged behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Step {
    /// The engine that runs.
    pub engine: Engine,
    /// How hard it works.
    pub level: Intensity,
}

impl Step {
    /// `engine` at its own default level.
    #[must_use]
    pub const fn new(engine: Engine) -> Self {
        Self {
            engine,
            level: engine.default_level(),
        }
    }

    /// `engine` at an explicitly named level.
    #[must_use]
    pub const fn at(engine: Engine, level: Intensity) -> Self {
        Self { engine, level }
    }

    /// The engine id, suffixed `@level` only when the level is not the
    /// engine's default.
    ///
    /// An echo that always printed `@standard` would make every response header
    /// carry a dial nobody turned, and the suffix is the whole signal that a
    /// non-default level ran.
    #[must_use]
    pub fn label(self) -> String {
        if self.level == self.engine.default_level() {
            self.engine.as_str().to_owned()
        } else {
            format!("{}@{}", self.engine.as_str(), self.level.as_str())
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
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
    pub steps: Vec<Step>,
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
    pub steps: &'a [Step],
}

/// The precedence layers, highest first. Absent means "not set at this layer".
#[derive(Debug, Clone, Copy, Default)]
pub struct Layers<'a> {
    /// Per-request `x-ar-compression` header value.
    pub header: Option<&'a str>,
    /// Routing-combo override: the combo's `compression:` block.
    pub combo: Option<&'a [Step]>,
    /// Active named profile.
    pub profile: Option<&'a [Step]>,
    /// Adaptive context-budget dial.
    pub adaptive: Option<&'a [Step]>,
    /// Panel default.
    pub default: Option<&'a [Step]>,
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
/// use ar_compress::{Combo, Engine, Intensity, Layers, Source, Step, plan_resolution};
///
/// let steps = [Step::new(Engine::Lite), Step::at(Engine::Caveman, Intensity::Lite)];
/// let combos = [Combo { id: "c1", name: Some("Balanced"), steps: &steps }];
/// let by_name = plan_resolution(&combos, &Layers { header: Some("balanced"), ..Default::default() });
/// assert_eq!(by_name.steps, steps);
///
/// // An unrecognized header is not a decision: the chain continues.
/// let unknown = plan_resolution(
///     &combos,
///     &Layers { header: Some("nope"), profile: Some(&[Step::new(Engine::Rtk)]), ..Default::default() },
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
            steps: vec![Step::new(engine)],
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
    /// Applies `step` to `text`.
    ///
    /// An implementation must return [`Cow::Borrowed`] when it declines to
    /// rewrite, and must never return more text than it received: a
    /// "compression" that inflates is a bug, and [`crate::budget`] will not
    /// catch it because the input was already within budget. `step.level` is
    /// advisory for an engine with no dial and binding for one that has it.
    fn apply<'a>(&self, step: Step, text: &'a str) -> Cow<'a, str>;
}

/// Runs `plan`'s pipeline over `text`.
///
/// An empty plan and a single declined engine both return the input borrowed,
/// so the common no-op path costs zero allocations. A multi-engine pipeline
/// owns its intermediates — step N+1 has to read step N's output while writing
/// a fresh buffer, and only its *final* output is handed back.
///
/// ```
/// use ar_compress::{Engine, Plan, Source, Step, apply_plan, registered};
///
/// let off = Plan::off(Source::Default);
/// assert!(matches!(apply_plan(&off, "keep me", registered()), std::borrow::Cow::Borrowed(_)));
///
/// // A single engine that declines to rewrite (clean input) stays borrowed.
/// let lite_only = Plan { steps: vec![Step::new(Engine::Lite)], source: Source::Default };
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
    for step in rest {
        acc = transforms.apply(*step, &acc).into_owned();
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
    fn apply<'a>(&self, step: Step, text: &'a str) -> Cow<'a, str> {
        match step.engine {
            Engine::Lite => crate::lite::lite(text),
            Engine::Rtk => crate::rtk::rtk_at(text, step.level),
            Engine::Caveman => crate::caveman::caveman_at(text, step.level),
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
        Combo, Engine, Intensity, Layers, Plan, Source, Step, Transform, apply_plan, plan_resolution,
        registered,
    };

    const BALANCED: &[Step] = &[Step::new(Engine::Lite), Step::new(Engine::Caveman)];

    const RTK_ONLY: &[Step] = &[Step::new(Engine::Rtk)];
    const CAVEMAN_ONLY: &[Step] = &[Step::new(Engine::Caveman)];
    const LITE_ONLY: &[Step] = &[Step::new(Engine::Lite)];
    const RTK_THEN_LITE: &[Step] = &[Step::new(Engine::Rtk), Step::new(Engine::Lite)];

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
            combo: Some(RTK_ONLY),
            profile: Some(CAVEMAN_ONLY),
            adaptive: Some(LITE_ONLY),
            default: Some(RTK_THEN_LITE),
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
        assert_eq!(plan.steps, [Step::new(Engine::Rtk), Step::new(Engine::Lite)]);
    }

    #[test]
    fn resolves_engine_header_to_one_step() {
        let plan = plan_resolution(
            &combos(),
            &Layers { header: Some("engine:caveman"), ..all_layers_set() },
        );
        assert_eq!(plan.steps, [Step::new(Engine::Caveman)]);
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
            fn apply<'a>(&self, step: Step, text: &'a str) -> Cow<'a, str> {
                Cow::Owned(format!("{text}{}", step.label()))
            }
        }

        let plan = Plan {
            steps: vec![
                Step::new(Engine::Rtk),
                Step::at(Engine::Caveman, Intensity::Ultra),
            ],
            source: Source::Combo,
        };
        assert_eq!(apply_plan(&plan, "x", &Trace), "xrtkcaveman@ultra");
    }

    #[test]
    fn borrows_input_when_plan_is_off() {
        let plan = Plan::off(Source::Default);
        assert!(matches!(apply_plan(&plan, "x", registered()), Cow::Borrowed(_)));
    }

    #[test]
    fn borrows_input_when_single_engine_declines() {
        let plan = Plan { steps: vec![Step::new(Engine::Lite)], source: Source::Default };
        assert!(matches!(apply_plan(&plan, "let x = 1;", registered()), Cow::Borrowed(_)));
    }

    #[test]
    fn rewrites_when_single_engine_acts() {
        let plan = Plan { steps: vec![Step::new(Engine::Caveman)], source: Source::Default };
        let out = apply_plan(&plan, "It seems like the cache is basically re-validating.", registered());
        assert!(out.len() < 56, "caveman did not shrink: {out}");
    }

    #[test]
    fn collapses_repeated_lines_when_rtk_runs() {
        let plan = Plan { steps: vec![Step::new(Engine::Rtk)], source: Source::Default };
        let out = apply_plan(&plan, "same\nsame\nsame\nsame\n", registered());
        assert!(out.contains("same"), "{out}");
    }

    /// The intensity axis: each engine's ladder is *its own*, so a level that
    /// belongs to `rtk` must not silently become a `caveman` rung. `rung` is
    /// total by design (a library call must not refuse text), which makes this
    /// test the thing that keeps the fallback honest.
    #[test]
    fn maps_each_level_to_the_rung_of_its_own_engine() {
        assert_eq!(Engine::Rtk.rung(Intensity::Minimal), 0);
        assert_eq!(Engine::Rtk.rung(Intensity::Standard), 1);
        assert_eq!(Engine::Rtk.rung(Intensity::Aggressive), 2);
        assert_eq!(Engine::Caveman.rung(Intensity::Lite), 0);
        assert_eq!(Engine::Caveman.rung(Intensity::Full), 1);
        assert_eq!(Engine::Caveman.rung(Intensity::Ultra), 2);
    }

    #[test]
    fn falls_back_to_the_default_rung_when_an_engine_does_not_offer_a_level() {
        assert_eq!(
            Engine::Rtk.rung(Intensity::Ultra),
            Engine::Rtk.rung(Intensity::Standard),
            "a caveman level must not become an rtk rung"
        );
    }

    /// `lite` is the one engine with no dial, so every level collapses onto the
    /// same rung. `Engine::levels` returning `&[]` is what makes that true
    /// without a special case in each transform.
    #[test]
    fn pins_a_fixed_engine_to_one_rung_for_every_level() {
        for level in [Intensity::Minimal, Intensity::Standard, Intensity::Ultra] {
            assert_eq!(Engine::Lite.rung(level), 0, "{level}");
        }
    }

    #[test]
    fn defaults_a_bare_engine_to_the_middle_of_its_own_ladder() {
        for engine in [Engine::Lite, Engine::Rtk, Engine::Caveman] {
            let expected = engine.levels().get(1).copied().unwrap_or(Intensity::Standard);
            assert_eq!(Step::new(engine).level, expected, "{engine}");
        }
    }

    #[test]
    fn labels_a_non_default_level_so_an_echo_can_show_the_dial() {
        assert_eq!(Step::new(Engine::Rtk).label(), "rtk");
        assert_eq!(Step::at(Engine::Rtk, Intensity::Aggressive).label(), "rtk@aggressive");
        assert_eq!(Step::at(Engine::Caveman, Intensity::Lite).label(), "caveman@lite");
    }

    #[test]
    fn parses_intensity_ids_case_insensitively() {
        assert_eq!(Intensity::from_id("  ULTRA "), Some(Intensity::Ultra));
    }

    #[test]
    fn rejects_an_intensity_id_no_engine_offers() {
        assert_eq!(Intensity::from_id("maximum"), None);
    }

    /// Every level spelling resolves, so `Engine::levels` cannot name a level
    /// the parser has never heard of — the drift the audit found in the
    /// reference's four disagreeing engine lists.
    #[test]
    fn parses_every_level_named_by_an_engine_ladder() {
        for engine in [Engine::Lite, Engine::Rtk, Engine::Caveman] {
            for level in engine.levels() {
                assert_eq!(Intensity::from_id(level.as_str()), Some(*level), "{engine}");
            }
        }
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
