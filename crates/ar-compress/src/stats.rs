//! What a transform saved, expressed in the unit the caller is billed in.
//!
//! Ported from `../OmniRoute/open-sse/services/compression/stats.ts`. Two of that
//! file's three paths are gone here and deliberately so: the exact BPE path
//! (`countTextTokens`) belongs to `ar-tokens`, which already owns counting, and
//! the image-token path belongs to a multimodal request that this crate never
//! sees. What remains is `charTokensOf` — a 4-chars-per-token estimate — which
//! is the same estimate OmniRoute falls back to above
//! `MAX_EXACT_TOKEN_COUNT_CHARS`.

/// Characters per token in the fallback estimate.
///
/// Ported from `stats.ts::CHARS_PER_TOKEN`. `provisional:` inherited from the
/// reference, not fitted here — see `docs/02` and the `charTokensOf` name. It is
/// a *ratio* for reporting savings, not a billing figure; anything that spends
/// money must call `ar_tokens::count_text`, which owns exact counting.
pub const CHARS_PER_TOKEN: usize = 4;

/// Tokens a transform removed, and the fraction of the input they were.
///
/// `ratio` is `saved_tokens / original_tokens` in `0.0..=1.0`. Both fields are
/// zero when nothing was saved — including when a transform made the text
/// *longer*, which is reported as zero rather than as negative savings. A
/// transform that inflates is a bug in that transform, and a negative
/// "savings" line would hide it behind a plausible-looking metric.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    /// Estimated tokens the transform removed.
    pub saved_tokens: usize,
    /// Share of the original token count that was removed, `0.0..=1.0`.
    pub ratio: f64,
}

impl Stats {
    /// Tokens `text` is estimated to cost. Empty text costs nothing.
    pub fn estimate_tokens(text: &str) -> usize {
        text.len().div_ceil(CHARS_PER_TOKEN)
    }

    /// Savings from rewriting `before` into `after`.
    ///
    /// `ratio` is `0.0` when `before` is empty: nothing to save from, and
    /// dividing by it would be a `NaN` in a metric that ends up on a dashboard.
    #[must_use]
    pub fn between(before: &str, after: &str) -> Self {
        let original = Self::estimate_tokens(before);
        let saved_tokens = original.saturating_sub(Self::estimate_tokens(after));
        let ratio = if original == 0 {
            0.0
        } else {
            saved_tokens as f64 / original as f64
        };
        Self {
            saved_tokens,
            ratio,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Stats;

    #[test]
    fn ratio_should_be_fraction_when_text_shrinks() {
        assert!((Stats::between("aaaaaaaa", "aaaa").ratio - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn savings_should_be_zero_when_text_grows() {
        assert_eq!(Stats::between("a", "aaaaaaaaaa").saved_tokens, 0);
    }

    #[test]
    fn ratio_should_be_zero_when_input_empty() {
        assert_eq!(Stats::between("", "aaaa").ratio, 0.0);
    }

    #[test]
    fn estimate_should_round_partial_token_up() {
        assert_eq!(Stats::estimate_tokens("abcde"), 2);
    }
}
