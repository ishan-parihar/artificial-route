//! `auto/*` virtual combos: the factory, the scorer, and selection.
//!
//! Ported from `../OmniRoute/open-sse/services/autoCombo/{virtualFactory,scoring,engine,modePacks}.ts`.
//!
//! A `auto/<variant>` model name is not a model. It is a *combo* resolved at
//! request time against whatever pool the caller supplies: [`virtual_combo`]
//! turns the name into a [`AutoCombo`] (a weight pack plus a stickiness flag),
//! and [`super::simulate_route`] / [`super::explain_route`] turn that plus a
//! pool into a decision. Nothing here dispatches — the caller already owns the
//! attempt loop, and a dry run that dispatched would not be a dry run.

mod engine;
mod scoring;
pub(crate) mod selection;

pub use engine::{
    AUTO_VARIANTS, AutoCombo, AutoVariant, VirtualFactory, auto_variant_for_model, virtual_combo,
};
pub use scoring::{
    AccountTier, AutoCandidate, CircuitState, Factors, PoolMaxima, Scored, Weights, factors,
    pool_maxima, score, score_pool,
};
pub use selection::{AutoSelector, WinnerReason};
