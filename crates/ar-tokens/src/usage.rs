//! `NormalizedUsage` — a 1:1 port of OmniRoute's `lib/usage/tokenAccounting.ts`.
//!
//! Every provider spells token usage differently. The TS module's whole job is
//! to collapse that into `{input, output}` using a fixed key precedence, and
//! the precedence is the contract: a provider that reports several keys has to
//! resolve to the same number here as it did there.

use serde_json::Value;

/// Prompt / completion / total tokens, in OpenAI shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NormalizedUsage {
    /// Prompt tokens, cache reads and cache writes already included.
    pub prompt: u32,
    /// Completion tokens, reasoning already included.
    pub completion: u32,
    /// `prompt + completion`.
    pub total: u32,
}

impl NormalizedUsage {
    /// Builds a usage record from explicit counts, deriving `total`.
    #[must_use]
    pub fn new(prompt: u32, completion: u32) -> Self {
        Self { prompt, completion, total: prompt.saturating_add(completion) }
    }

    /// Collapses a provider `usage` object using the ported precedence.
    ///
    /// Input resolves in this order, and each step is only taken when the key is
    /// present and not `null`:
    ///
    /// 1. `input`
    /// 2. `prompt_tokens`
    /// 3. `input_tokens` **plus** `cache_read_input_tokens` and
    ///    `cache_creation_input_tokens` — Anthropic reports only the non-cached
    ///    portion in `input_tokens`, so the cache counters are separate
    ///    top-level fields and have to be added back.
    ///
    /// Output resolves as `output`, then `completion_tokens`, then
    /// `output_tokens`. `total` is always the sum; the TS module never reads a
    /// provider's own total, and neither does this.
    ///
    /// ```
    /// let usage: serde_json::Value = serde_json::json!({
    ///     "prompt_tokens": 10,
    ///     "completion_tokens": 5,
    /// });
    /// let n = ar_tokens::NormalizedUsage::from_usage(&usage);
    /// assert_eq!(n.total, 15);
    /// ```
    #[must_use]
    pub fn from_usage(usage: &Value) -> Self {
        Self::new(logged_input_tokens(usage), logged_output_tokens(usage))
    }
}

fn present<'a>(usage: &'a Value, key: &str) -> Option<&'a Value> {
    usage.get(key).filter(|value| !value.is_null())
}

/// `toFiniteNumber`: a finite JSON number, or a numeric string, else zero.
fn finite(value: &Value) -> u32 {
    let n = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.trim().is_empty() => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    n.filter(|f| f.is_finite()).map_or(0, to_u32)
}

fn finite_at(usage: &Value, key: &str) -> u32 {
    present(usage, key).map_or(0, finite)
}

/// Token counts are whole numbers; a provider that reports a fraction or a
/// negative is clamped rather than rejected, matching `toFiniteNumber`'s
/// "always a number" contract.
fn to_u32(n: f64) -> u32 {
    if n <= 0.0 {
        0
    } else {
        n.min(f64::from(u32::MAX)) as u32
    }
}

fn logged_input_tokens(usage: &Value) -> u32 {
    for key in ["input", "prompt_tokens"] {
        if let Some(value) = present(usage, key) {
            return finite(value);
        }
    }
    if let Some(value) = present(usage, "input_tokens") {
        return finite(value)
            .saturating_add(finite_at(usage, "cache_read_input_tokens"))
            .saturating_add(finite_at(usage, "cache_creation_input_tokens"));
    }
    // Ollama keeps its counts at the reply's root, outside any `usage` map.
    finite_at(usage, "prompt_eval_count")
}

fn logged_output_tokens(usage: &Value) -> u32 {
    if let Some(value) = present(usage, "output") {
        return finite(value);
    }
    if let Some(value) = present(usage, "completion_tokens") {
        return finite(value);
    }
    if present(usage, "output_tokens").is_some() {
        return finite_at(usage, "output_tokens");
    }
    // Ollama's root spelling of the same count.
    finite_at(usage, "eval_count")
}

#[cfg(test)]
mod tests {
    use super::NormalizedUsage;
    use serde_json::json;

    #[test]
    fn prefers_input_over_prompt_tokens_when_both_present() {
        assert_eq!(NormalizedUsage::from_usage(&json!({ "input": 7, "prompt_tokens": 9 })).prompt, 7);
    }

    #[test]
    fn falls_back_to_prompt_tokens_when_input_absent() {
        assert_eq!(NormalizedUsage::from_usage(&json!({ "prompt_tokens": 9 })).prompt, 9);
    }

    #[test]
    fn adds_cache_counters_when_only_input_tokens_present() {
        let usage = json!({ "input_tokens": 10, "cache_read_input_tokens": 4, "cache_creation_input_tokens": 1 });
        assert_eq!(NormalizedUsage::from_usage(&usage).prompt, 15);
    }

    #[test]
    fn skips_null_input_tokens_when_prompt_tokens_absent() {
        assert_eq!(NormalizedUsage::from_usage(&json!({ "input_tokens": null })).prompt, 0);
    }

    #[test]
    fn parses_numeric_string_when_field_is_a_string() {
        assert_eq!(NormalizedUsage::from_usage(&json!({ "prompt_tokens": "42" })).prompt, 42);
    }

    #[test]
    fn counts_zero_completion_tokens_when_reported_as_zero() {
        assert_eq!(NormalizedUsage::from_usage(&json!({ "completion_tokens": 0, "output_tokens": 8 })).completion, 0);
    }

    #[test]
    fn falls_back_to_output_tokens_when_completion_tokens_null() {
        assert_eq!(NormalizedUsage::from_usage(&json!({ "completion_tokens": null, "output_tokens": 8 })).completion, 8);
    }

    #[test]
    fn sums_total_when_prompt_and_completion_known() {
        assert_eq!(NormalizedUsage::new(3, 4).total, 7);
    }

    #[test]
    fn reads_zero_when_usage_object_is_empty() {
        assert_eq!(NormalizedUsage::from_usage(&json!({})), NormalizedUsage::new(0, 0));
    }

    #[test]
    fn counts_ollamas_root_eval_fields_when_no_usage_map() {
        // Ollama answers carry `prompt_eval_count`/`eval_count` at the reply's
        // root, never nested under `usage`; before this key the normalizer
        // read every Ollama reply as zero while the server's own comment
        // claimed the root was the usage.
        let usage = json!({ "prompt_eval_count": 9, "eval_count": 4 });
        assert_eq!(NormalizedUsage::from_usage(&usage), NormalizedUsage::new(9, 4));
    }

    #[test]
    fn prefers_standard_keys_over_ollama_root_when_both_present() {
        let usage = json!({ "prompt_tokens": 5, "eval_count": 4 });
        assert_eq!(NormalizedUsage::from_usage(&usage), NormalizedUsage::new(5, 4));
    }
}
