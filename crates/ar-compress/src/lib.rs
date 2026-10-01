//! `ar-compress` — the transforms, the plan that selects them, the budget that
//! bounds them, and the eval that scores them.
//!
//! Two halves that meet at one seam:
//!
//! * **Transforms** — [`lite`], [`rtk`], [`caveman`]. Each is `&str` in, text
//!   out, with no state, no I/O, and no clock. Each returns the input *borrowed*
//!   when it declines, so the no-op path allocates nothing. [`Stats`] reports
//!   what a transform saved.
//! * **The plan** — [`plan`] decides *which* transforms run, [`budget`] decides
//!   *how much* they may spend, and [`eval`] reports *what that cost*.
//!
//! The seam is [`Transform`]: the transforms never learn about plans, and the
//! plan never learns how a transform rewrites. [`registered`] wires the three
//! together, and it is the only place an [`Engine`] meets an implementation.
//!
//! ```
//! use ar_compress::{Engine, Layers, Plan, Source, Step, clamp_to_budget, eval, plan_resolution, registered};
//!
//! // 1. The transforms, borrowed when they decline.
//! assert_eq!(ar_compress::lite("let x = 1;"), "let x = 1;");
//!
//! // 2. Resolution: a header outranks everything set below it.
//! let plan = plan_resolution(
//!     &[],
//!     &Layers {
//!         header: Some("engine:caveman"),
//!         combo: Some(&[Step::new(Engine::Lite)]),
//!         ..Layers::default()
//!     },
//! );
//! assert_eq!(plan.source, Source::Header);
//!
//! // 3. Budget: enforced after the transforms, and never exceeded.
//! let clamp = clamp_to_budget(&"prose line. ".repeat(200), 24);
//! assert!(clamp.after <= 24);
//!
//! // 4. Eval: three numbers per case, deterministic — no model, no RNG.
//! let report = eval::run(eval::SEED_CORPUS, &plan, registered(), Some(200)).unwrap();
//! assert!(eval::report_table(&report).contains("fidelity"));
//! ```
//!
//! ## Ported from `../OmniRoute`
//!
//! | Rust | OmniRoute |
//! |---|---|
//! | [`lite`] | `services/compression/lite.ts::normalizeMessageWhitespace` + the fence guard on `engines/rtk/index.ts`'s `stripCode` pass |
//! | [`rtk`] | `services/compression/engines/rtk/deduplicator.ts::deduplicateRepeatedLines` |
//! | [`caveman`] | `services/compression/caveman.ts` — `RULE_KEYWORDS`, `applyRulesToText`, `cleanupArtifacts`, `isCodeDominantText` |
//! | [`Stats`] | `services/compression/stats.ts::estimateCompressionTokens` (its `charTokensOf` path) |
//! | [`plan_resolution`] | `services/compression/planResolution.ts`, `resolveCompressionPlan.ts` |
//! | [`clamp_to_budget`], [`is_anchor`] | `services/compression/hardBudget.ts` |
//! | [`eval::run`], [`eval::report_table`] | `services/compression/eval/{runner,aggregate,report}.ts` |
//! | [`eval::SEED_CORPUS`], [`eval::load_corpus`] | `services/compression/eval/{seedCorpus,corpus}.ts` |
//!
//! ## Dropped on purpose
//!
//! The reference pipeline also carries an ONNX/MobileBERT semantic pruner, an
//! `omniglyph` image codec, a `quantumLock` state machine, a worker-thread
//! pool, and 14 dashboard migrations. None is a text transform, and the model
//! runtime is a dependency tree this crate has no business linking. The
//! LLM-judge tier of the eval goes too: a paid, non-reproducible scorer is the
//! wrong instrument for a release gate, so [`eval::fidelity`] is mechanical and
//! deterministic instead.
//!
//! Two counting authorities, deliberately. [`Stats`] uses the cheap `chars/4`
//! estimate for a per-transform *ratio*; anything that spends money or enforces
//! a limit — [`clamp_to_budget`], [`eval::savings`] — uses
//! `ar_tokens::count_text`, which owns exact counting. A savings figure and a
//! budget verdict therefore cannot disagree.
//!
//! Each transform degrades in one direction only: no savings, never corruption.
//! [`caveman`] in particular refuses to touch text carrying a fence, a URL, or
//! code-shaped lines, [`clamp_to_budget`] never drops an [`is_anchor`] unit, and
//! every transform returns the input **borrowed** rather than an identical copy
//! when it declines.

#![deny(missing_docs)]

pub mod budget;
pub mod caveman;
pub mod error;
pub mod eval;
pub mod lite;
pub mod plan;
pub mod rtk;
pub mod stats;

pub use budget::{Clamp, clamp_to_budget, is_anchor};
pub use caveman::{caveman, caveman_at, caveman_at_with_stats, caveman_with_stats};
pub use error::CompressError;
pub use eval::{
    CaseReport, ContentKind, EvalCase, Savings, fidelity, load_corpus, report_table, run, savings,
};
pub use lite::{lite, lite_with_stats};
pub use plan::{
    Combo, Engine, Engines, Intensity, Layers, Plan, Source, Step, Transform, apply_plan,
    plan_resolution, registered,
};
pub use rtk::{rtk, rtk_at, rtk_with_stats};
pub use stats::{CHARS_PER_TOKEN, Stats};
