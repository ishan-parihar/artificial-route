//! Per-response accounting: what one served request spent and how fast it came
//! back.
//!
//! Ported from `../OmniRoute/src/domain/omnirouteResponseMeta.ts`
//! (`buildOmniRouteResponseMetaHeaders`).
//!
//! [`NormalizedUsage`] collapses a provider's `usage` object into two numbers and
//! [`crate::PricingTable`] prices them; everything left on a response — cost,
//! speed, latency — is arithmetic over those two plus a clock the caller read.
//! This module holds that arithmetic so the header a client sees and the row the
//! ledger stores come out of *one* computation rather than two that can drift.
//!
//! Two rules the reference's own comments insist on, kept here:
//!
//! * Speed is `output_tokens / elapsed`. When the elapsed time is unknown the
//!   field is **absent**, never `0` — a zero would read as "instant", and a
//!   client that divides by it learns nothing.
//! * An unpriced model reports `$0` **and** `priced: false`. Collapsing the two
//!   is what [`crate::Cost`] exists to prevent; [`ResponseMeta::cost`] carries
//!   the distinction through unchanged.

use std::time::Duration;

use serde_json::Value;

use crate::pricing::{Cost, PricingTable};
use crate::usage::NormalizedUsage;

/// Reads the `usage` object out of a provider response body.
///
/// `None` for a body that is not JSON, or is JSON with no `usage` — which is a
/// real case, not only a malformed one: a stream's usage arrives in the final
/// frame, and a provider that meters nothing sends no object at all. A caller
/// that gets `None` records nothing rather than recording a zero, because a
/// zero row and a missing row are different facts.
///
/// ```
/// let body = br#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
/// assert_eq!(ar_tokens::usage_from_body(body).expect("usage").total, 15);
/// ```
#[must_use]
pub fn usage_from_body(body: &[u8]) -> Option<NormalizedUsage> {
    let value: Value = serde_json::from_slice(body).ok()?;
    Some(NormalizedUsage::from_usage(value.get("usage")?))
}

/// One served request's accounting, in the shape a response header carries it.
///
/// `Copy` because it is six words of numbers, not state: the response path holds
/// it on the stack next to the `Response` it is stamping and neither outlives
/// the other. Latency is a [`Duration`] rather than a millisecond count so the
/// rounding to whole milliseconds happens once, at the edge, in
/// [`ResponseMeta::latency_ms`].
///
/// The defaults are the honest "nothing observed" values — zero tokens, zero
/// cost, zero elapsed — which is what a provider that reports no usage gets.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ResponseMeta {
    usage: NormalizedUsage,
    cost: Cost,
    latency: Duration,
    savings_tokens: u32,
}

impl ResponseMeta {
    /// Prices `usage` for one completed response.
    ///
    /// The two derivations the response path would otherwise repeat per call:
    /// [`NormalizedUsage::from_usage`] over the provider's object, and
    /// [`PricingTable::cost`] over the resolved row. Cost travels with usage so a
    /// caller cannot pair one request's tokens with another's price.
    #[must_use]
    pub fn from_upstream(prices: &PricingTable, provider: &str, model: &str, usage: &Value) -> Self {
        let usage = NormalizedUsage::from_usage(usage);
        let cost = prices.cost(provider, model, usage);
        Self { usage, cost, latency: Duration::ZERO, savings_tokens: 0 }
    }

    /// The same meta, stamped with how long the request took.
    #[must_use]
    pub fn with_latency(self, latency: Duration) -> Self {
        Self { latency, ..self }
    }

    /// The same meta, stamped with how many prompt tokens compression removed.
    ///
    /// A request no compression stage rewrote saves nothing, which is `0` rather
    /// than "unknown": there is no arm here that compresses, so no arm can have
    /// saved.
    #[must_use]
    pub fn with_savings_tokens(self, saved: u32) -> Self {
        Self { savings_tokens: saved, ..self }
    }

    /// What the request reported, in `{prompt, completion, total}`.
    #[must_use]
    pub fn usage(self) -> NormalizedUsage {
        self.usage
    }

    /// What the request cost, priced or deliberately not.
    #[must_use]
    pub fn cost(self) -> Cost {
        self.cost
    }

    /// Prompt tokens. The `x-ar-tokens-in` header value.
    #[must_use]
    pub fn tokens_in(self) -> u32 {
        self.usage.prompt
    }

    /// Completion tokens. The `x-ar-tokens-out` header value.
    #[must_use]
    pub fn tokens_out(self) -> u32 {
        self.usage.completion
    }

    /// Prompt tokens compression removed before dispatch.
    #[must_use]
    pub fn savings_tokens(self) -> u32 {
        self.savings_tokens
    }

    /// Whole milliseconds elapsed, truncated rather than rounded: a request that
    /// took 1.9 ms reports `1`, and one that took 0.4 ms reports `0` — the same
    /// "too fast to measure" state [`ResponseMeta::tokens_per_second`] refuses to
    /// publish rather than dividing by.
    #[must_use]
    pub fn latency_ms(self) -> u64 {
        u64::try_from(self.latency.as_millis()).unwrap_or(u64::MAX)
    }

    /// Completion tokens per second, or `None` when the denominator is unknown.
    ///
    /// `None` for a zero-length elapsed time and only that: dividing by it would
    /// be `inf` on one side and a bare `0` on the other, and a client reading
    /// `tokens / total_latency` as generation speed would then report nonsense
    /// instead of nothing. The reference omits the field for the same reason.
    #[must_use]
    pub fn tokens_per_second(self) -> Option<f64> {
        let secs = self.latency.as_secs_f64();
        if secs <= 0.0 {
            return None;
        }
        Some(f64::from(self.usage.completion) / secs)
    }
}

#[cfg(test)]
mod tests {
    use super::{ResponseMeta, usage_from_body};
    use crate::pricing::{Cost, Prices, PricingTable};
    use crate::usage::NormalizedUsage;
    use serde_json::json;
    use std::time::Duration;

    fn table() -> PricingTable {
        let mut t = PricingTable::default();
        t.set("openai", "gpt-4o", Prices { input_micros_per_mtok: 2_500_000, output_micros_per_mtok: 10_000_000 });
        t
    }

    #[test]
    fn reads_usage_when_the_body_carries_one() {
        let usage = json!({ "prompt_tokens": 10, "completion_tokens": 5 });
        assert_eq!(usage_from_body(br#"{"usage":{"prompt_tokens":10,"completion_tokens":5}}"#), Some(NormalizedUsage::from_usage(&usage)));
    }

    #[test]
    fn returns_none_when_the_body_has_no_usage_object() {
        assert_eq!(usage_from_body(br#"{"choices":[]}"#), None);
    }

    #[test]
    fn returns_none_when_the_body_is_not_json() {
        assert_eq!(usage_from_body(b"not json at all"), None);
    }

    #[test]
    fn normalizes_an_anthropic_body_with_cache_counters() {
        let body = br#"{"usage":{"input_tokens":10,"cache_read_input_tokens":4,"output_tokens":2}}"#;
        assert_eq!(usage_from_body(body).expect("usage").prompt, 14);
    }

    #[test]
    fn prices_the_usage_it_normalized() {
        let meta = ResponseMeta::from_upstream(&table(), "openai", "gpt-4o", &json!({ "prompt_tokens": 1_000_000, "completion_tokens": 1_000_000 }));
        assert_eq!(meta.cost().usd.micros, 12_500_000);
    }

    #[test]
    fn reports_the_token_pair_the_headers_print() {
        let meta = ResponseMeta::from_upstream(&table(), "openai", "gpt-4o", &json!({ "prompt_tokens": 10, "completion_tokens": 5 }));
        assert_eq!((meta.tokens_in(), meta.tokens_out()), (10, 5));
    }

    #[test]
    fn marks_an_unknown_model_unpriced_rather_than_free() {
        // The distinction `Cost` exists for: `$0 unpriced` must not read as a
        // budget decision that the request was cheap.
        let meta = ResponseMeta::from_upstream(&table(), "openai", "nope", &json!({ "prompt_tokens": 10 }));
        assert_eq!(meta.cost(), Cost::UNPRICED);
    }

    #[test]
    fn computes_speed_from_completion_tokens_over_elapsed_time() {
        let meta = ResponseMeta::from_upstream(&table(), "openai", "gpt-4o", &json!({ "completion_tokens": 500 }))
            .with_latency(Duration::from_secs(2));
        assert_eq!(meta.tokens_per_second(), Some(250.0));
    }

    #[test]
    fn omits_speed_when_the_elapsed_time_is_zero() {
        // A zero would read as "instant"; the reference omits the field so a
        // plugin cannot divide by it.
        let meta = ResponseMeta::from_upstream(&table(), "openai", "gpt-4o", &json!({ "completion_tokens": 500 }));
        assert_eq!(meta.tokens_per_second(), None);
    }

    #[test]
    fn reports_zero_speed_for_a_response_with_no_completion_tokens() {
        let meta = ResponseMeta::from_upstream(&table(), "openai", "gpt-4o", &json!({ "prompt_tokens": 10 }))
            .with_latency(Duration::from_millis(500));
        assert_eq!(meta.tokens_per_second(), Some(0.0));
    }

    #[test]
    fn truncates_latency_to_whole_milliseconds() {
        let meta = ResponseMeta::default().with_latency(Duration::from_micros(1_900));
        assert_eq!(meta.latency_ms(), 1);
    }

    #[test]
    fn carries_the_savings_a_compression_stage_reported() {
        assert_eq!(ResponseMeta::default().with_savings_tokens(42).savings_tokens(), 42);
    }

    #[test]
    fn defaults_to_nothing_observed() {
        assert_eq!(ResponseMeta::default(), ResponseMeta { usage: NormalizedUsage::new(0, 0), cost: Cost::UNPRICED, latency: Duration::ZERO, savings_tokens: 0 });
    }
}