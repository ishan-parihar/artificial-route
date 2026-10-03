//! `ar-route` — the router: four lean strategies, the `auto/*` virtual
//! factory, the attempt loop, the single per-key resilience layer, and the
//! two read-only routing introspection entry points.
//!
//! # What is here
//!
//! * **Twenty concrete strategies** ([`Strategy::all`]) over a
//!   caller-supplied candidate list: four lean-routing plus the load-shaped,
//!   quota-shaped, context-shaped and panel-shaped families from
//!   `combo/targetSorters.ts`, `quotaStrategies`, `promptCacheAffinity` and
//!   `dispatchPrelude`. An `auto/*` *strategy name* and a typo still surface as
//!   [`RouteError::DeferredStrategy`] — never a panic in the request path.
//! * **The panel-shaped pair, dispatched** — [`dispatch_fusion`] fans out over
//!   the whole panel and [`dispatch_pipeline`] chains the stages, both over an
//!   [`Executor`]. [`pick`] still answers for both with the panel leader and the
//!   chain's first stage.
//! * **`auto/*` virtual combos** — [`virtual_combo`] turns a model *name* into
//!   a [`AutoCombo`]; [`simulate_route`] and [`explain_route`] turn that plus a
//!   pool into a decision; [`auto_variant_for_model`] is the pure "is this
//!   model an `auto` alias?" check a server stream makes first.
//! * **`simulate_route`** — the whole decision with the dispatch removed, and
//!   [`explain_route`] — the same decision with the arithmetic behind it.
//! * **The `pipeline` strategy engine** — [`execute_pipeline`] runs the chained
//!   stages for [`Strategy::Pipeline`], with a `reflect` judge deciding whether
//!   `fix` is needed. Pure: the caller supplies the [`StageExecutor`].
//!
//! # How `auto/*` reaches the request path
//!
//! An `auto/<variant>` model name is a *combo*, not a model. It is resolved
//! before the router sees a request, exactly as upstream's `virtualFactory`
//! returns a combo rather than a target. Concretely:
//!
//! ```text
//! model "auto/cheap"
//!   -> virtual_combo("auto/cheap")            // AutoCombo { Cheap, cost-saver pack }
//!   -> simulate_route(&combo, &pool, ..)      // RoutePlan { chain }
//!   -> attempt_loop(req, plan.chain(), ..)    // the real dispatch
//! ```
//!
//! [`simulate_route`] is the *only* producer of that chain, so a dry run and
//! the request that follows it cannot disagree.
//!
//! ```
//! use ar_route::{AutoCandidate, AutoSelector, ProviderId, simulate_route, virtual_combo};
//!
//! let combo = virtual_combo("auto/cheap")?;
//! let pool = [
//!     AutoCandidate::new(ProviderId::new("openai"), "gpt-4o").with_price(2.50),
//!     AutoCandidate::new(ProviderId::new("together"), "mixtral").with_price(0.20),
//! ];
//! let plan = simulate_route(&combo, &pool, &AutoSelector::new(), |_| 0.5)?;
//! assert_eq!(plan.winner().map_or("", ar_route::ProviderId::as_str), "together");
//! assert_eq!(plan.fallbacks().len(), 1);
//! # Ok::<(), ar_route::RouteError>(())
//! ```
//!
//! ```
//! use ar_route::{Candidate, ProviderId, Strategy, pick};
//! use std::sync::atomic::AtomicU64;
//!
//! let candidates = [
//!     Candidate::new(ProviderId::new("openai"), "gpt-4o").with_price(2.50).with_rank(0),
//!     Candidate::new(ProviderId::new("groq"), "llama-3.3-70b").with_price(0.59).with_rank(1),
//! ];
//! let chosen = pick(Strategy::CostOptimized, None, &candidates, &AtomicU64::new(0), None)?;
//! assert_eq!(chosen.as_str(), "groq");
//! # Ok::<(), ar_route::RouteError>(())
//! ```

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod attempt;
mod auto;
mod contract;
mod error;
mod explain;
pub mod fusion_judge;
pub use fusion_judge::{JudgeOutcome, JudgePanel, JudgeTarget, synthesize};
mod lkgp;
mod pipeline;
mod resilience;
mod simulate;
mod strategy;

pub use attempt::{AbortReport, AttemptOutcome, MAX_ATTEMPTS, attempt_loop};
pub use auto::{
    AUTO_VARIANTS, AccountTier, AutoCandidate, AutoCombo, AutoSelector, AutoVariant, CircuitState,
    Factors, PoolMaxima, Scored, VirtualFactory, Weights, WinnerReason, auto_variant_for_model,
    factors, pool_maxima, score, score_pool, virtual_combo,
};
pub use contract::{
    ArExec, ArTranslate, Candidate, CanonicalRequest, ChunkStream, ExecError, Executor, MediaReply,
    ProviderId, QuotaWindow, Strng, Upstream,
};
pub use error::RouteError;
pub use explain::{RouteTrace, explain_route};
pub use lkgp::{DEFAULT_LKGP_TTL, LkgpPins};
pub use pipeline::{
    FitnessTier, PipelineConfig, PipelineResult, PipelineStage, ReflectResult, StageExecutor,
    StageExecutorArgs, StageExecutorResult, StageName, StagePrompt, StageResult, TaskType, Verdict,
    build_pipeline_config, execute_pipeline, parse_reflect_json,
};
pub use resilience::{DEFAULT_BASE_BACKOFF, DEFAULT_MAX_BACKOFF, Resilience};
pub use simulate::{RoutePlan, simulate_route};
pub use strategy::{
    FusionOutcome, PanelVerdict, PipelineOutcome, StageVerdict, Strategy, dispatch_fusion,
    dispatch_pipeline, pick, pick_filtered, pick_for_model,
};
