//! Token counting, usage normalization, cost, and the usage ledger.
//!
//! Three surfaces, one dependency chain: [`count_text`] turns a string into a
//! token count, [`NormalizedUsage`] turns a provider's `usage` object into
//! `{prompt, completion, total}`, and [`Ledger`] prices that usage, records it
//! append-only, and answers whether a key has budget left.
//! [`ResponseMeta`] closes the loop: the same priced usage, shaped for the
//! numbers a response reports.
//!
//! ```
//! use ar_tokens::{CostReport, count_text, estimate_request};
//!
//! assert_eq!(count_text("hello world"), 2);
//! assert_eq!(estimate_request(&serde_json::json!({ "max_tokens": 8 })).total, 8);
//! assert_eq!(CostReport::default().rows.len(), 0);
//! ```
//!
//! Ported from `../OmniRoute`:
//!
//! | Rust | OmniRoute |
//! |---|---|
//! | [`count_text`] | `src/shared/utils/tiktokenCounter.ts::countTextTokens` |
//! | [`estimate_request`] | `src/lib/quota/tokenEstimator.ts` |
//! | [`NormalizedUsage`] | `src/lib/usage/tokenAccounting.ts` |
//! | [`ResponseMeta`] | `src/domain/omnirouteResponseMeta.ts` |
//! | [`PricingTable`] | `src/lib/usage/{modelPricingRegistry,costCalculator}.ts` |
//! | [`Ledger`], [`Cap`] | `src/lib/usage/{usageLedger,budgetGuard}.ts` |
//!
//! Two invariants a caller depends on:
//!
//! * A flat-rate (subscription) provider costs $0 **and is still capped**. The
//!   cap has a token arm for exactly this; see [`Cap::tokens`].
//! * An over-budget request is refused *before* dispatch with a
//!   [`DenyReason`] that maps to HTTP 402, so it never reaches a provider.

#![deny(missing_docs)]

pub mod count;
pub mod error;
pub mod ledger;
pub mod meta;
pub mod pricing;
pub mod usage;

pub use count::{
    DEFAULT_OUTPUT_ALLOWANCE, Estimate, MAX_EXACT_TOKEN_COUNT_CHARS, count_text, estimate_request,
};
pub use error::TokenError;
pub use ledger::{Cap, CostReport, DenyReason, Entry, Ledger, LedgerRow, Spend, Verdict};
pub use meta::{ResponseMeta, usage_from_body};
pub use pricing::{Cost, Prices, PricingTable, Usd, cost_micros};
pub use usage::NormalizedUsage;
