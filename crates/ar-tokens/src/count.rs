//! Token counting: exact BPE under a size cap, `chars/4` heuristic above it.
//!
//! Ported from `../OmniRoute/src/shared/utils/tiktokenCounter.ts` (exact path,
//! `MAX_EXACT_TOKEN_COUNT_CHARS` guard) and
//! `../OmniRoute/src/lib/quota/tokenEstimator.ts` (the pre-flight cheap path).

use serde_json::{Map, Value};

/// Above this many characters the exact tokenizer is skipped in favour of the
/// `chars/4` heuristic.
///
/// The reason is cost, not accuracy: the BPE encoder is near-quadratic on large
/// inputs, and a base64 attachment is large. Counting feeds compression stats
/// and quota pre-flight only, so an estimate on oversized input is acceptable
/// and keeps the request path responsive.
pub const MAX_EXACT_TOKEN_COUNT_CHARS: usize = 50_000;

/// Chars per token in the heuristic path. OpenAI's classic approximation: it
/// tracks Latin prose well and *under*-counts CJK and dense code, so the
/// heuristic result is a floor, never an over-estimate.
pub const HEURISTIC_CHARS_PER_TOKEN: usize = 4;

/// Output tokens reserved when a request names no `max_tokens`.
pub const DEFAULT_OUTPUT_ALLOWANCE: u32 = 1024;

/// Input is multiplied by `11/10` before the output reservation is added, so a
/// budget that is nearly exhausted is judged exhausted rather than available.
const OVER_PROVISION_NUM: u64 = 11;
const OVER_PROVISION_DEN: u64 = 10;

fn heuristic(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(HEURISTIC_CHARS_PER_TOKEN)).unwrap_or(u32::MAX)
}

/// Exact token count for `text`, or the `chars/4` heuristic above
/// [`MAX_EXACT_TOKEN_COUNT_CHARS`].
///
/// The size guard is on **bytes**, not Unicode scalar values. The hazard being
/// bounded is encoder work, and the encoder is fed bytes; a multi-byte prompt
/// therefore trips the guard earlier than a character count would.
///
/// ```
/// assert_eq!(ar_tokens::count_text(""), 0);
/// assert!(ar_tokens::count_text("hello world") > 0);
/// ```
///
/// // ponytail: an image data URI (`data:image/png;base64,…`) is counted as
// raw bytes/4, so it over-counts. That direction is safe — the quota
// pre-flight deliberately over-provisions so an exhausted budget is never
// misjudged as available — and a 10 MB payload trips the size guard and never
// reaches the encoder. Add a data-URI strip only if exact image-token
// accounting is ever needed for a cost report.
#[must_use]
pub fn count_text(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    if text.len() > MAX_EXACT_TOKEN_COUNT_CHARS {
        return heuristic(text.len());
    }
    // `encode_with_special_tokens` never rejects input, which is the property
    // the TS port relies on. It allocates one `usize` per token (~100 KB for a
    // 50 kB prompt) and frees it here.
    let n = tiktoken_rs::cl100k_base_singleton()
        .encode_with_special_tokens(text)
        .len();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A pre-flight request cost estimate, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// Estimated prompt tokens.
    pub input: u32,
    /// Reserved output tokens: `max_tokens` when present, else
    /// [`DEFAULT_OUTPUT_ALLOWANCE`].
    pub output: u32,
    /// `ceil(input * 1.1) + output`.
    pub total: u32,
}

fn estimate_string_tokens(text: &str) -> u32 {
    heuristic(text.len())
}

fn text_of(value: Option<&Value>) -> u32 {
    value
        .and_then(Value::as_str)
        .map_or(0, estimate_string_tokens)
}

/// `content` is either a bare string or an array of parts carrying `text`.
fn content_tokens(content: Option<&Value>) -> u32 {
    match content {
        Some(Value::String(text)) => estimate_string_tokens(text),
        Some(Value::Array(parts)) => parts.iter().fold(0_u32, |acc, part| {
            acc.saturating_add(text_of(part.get("text")))
        }),
        _ => 0,
    }
}

fn reserved_output(body: &Map<String, Value>) -> u32 {
    body.get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(Value::as_f64)
        .filter(|max| *max > 0.0)
        .map_or(DEFAULT_OUTPUT_ALLOWANCE, |max| max.ceil() as u32)
}

fn allowance_only() -> Estimate {
    Estimate {
        input: 0,
        output: DEFAULT_OUTPUT_ALLOWANCE,
        total: DEFAULT_OUTPUT_ALLOWANCE,
    }
}

/// Cheap, allocation-light estimate of what a chat request will cost in tokens.
///
/// Reads `messages` (chat.completions) and `input` (Responses API) plus a
/// top-level `system` string, exactly as the TS estimator does. Never fails:
/// an unparseable body degrades to the output allowance alone, which the
/// scheduler reads as "cheap".
///
/// ```
/// let body: serde_json::Value = serde_json::json!({
///     "messages": [{ "content": "hi" }],
///     "max_tokens": 64,
/// });
/// let est = ar_tokens::estimate_request(&body);
/// assert_eq!(est.output, 64);
/// ```
#[must_use]
pub fn estimate_request(body: &Value) -> Estimate {
    let Some(obj) = body.as_object() else {
        return allowance_only();
    };

    let mut input: u64 = 0;
    if let Some(Value::Array(messages)) = obj.get("messages") {
        for message in messages {
            input += u64::from(content_tokens(message.get("content")));
        }
    }
    if let Some(Value::Array(items)) = obj.get("input") {
        for item in items {
            input += u64::from(text_of(item.get("text")));
        }
    }
    input += u64::from(text_of(obj.get("system")));

    let output = reserved_output(obj);
    let over = input
        .saturating_mul(OVER_PROVISION_NUM)
        .div_ceil(OVER_PROVISION_DEN);
    let total = u32::try_from(over.saturating_add(u64::from(output))).unwrap_or(u32::MAX);
    Estimate {
        input: u32::try_from(input).unwrap_or(u32::MAX),
        output,
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_OUTPUT_ALLOWANCE, Estimate, MAX_EXACT_TOKEN_COUNT_CHARS, count_text,
        estimate_request,
    };
    use serde_json::json;

    #[test]
    fn counts_exactly_when_short() {
        assert!(count_text("the quick brown fox") <= 6);
    }

    #[test]
    fn returns_zero_when_empty() {
        assert_eq!(count_text(""), 0);
    }

    #[test]
    fn estimates_heuristically_when_over_size_cap() {
        let long = "x".repeat(MAX_EXACT_TOKEN_COUNT_CHARS + 4);
        assert_eq!(count_text(&long), 12_501);
    }

    #[test]
    fn estimates_cheap_when_long_prompt() {
        let long = "a".repeat(40_000);
        let body = json!({ "messages": [{ "content": long }] });
        let est = estimate_request(&body);
        // 10 000 raw + 10% over-provision = 11 000. The point of the cheap path
        // is that a 40 kB prompt costs the arithmetic above, not an encode.
        assert_eq!(est.input, 10_000);
    }

    #[test]
    fn over_provisions_input_by_ten_percent_when_body_parsed() {
        let body = json!({ "messages": [{ "content": "a".repeat(40) }] });
        let est = estimate_request(&body);
        assert_eq!(est.total, 11 + DEFAULT_OUTPUT_ALLOWANCE);
    }

    #[test]
    fn reserves_output_allowance_when_max_tokens_absent() {
        let body = json!({ "messages": [{ "content": "hi" }] });
        assert_eq!(estimate_request(&body).output, DEFAULT_OUTPUT_ALLOWANCE);
    }

    #[test]
    fn prefers_max_completion_tokens_when_set() {
        let body = json!({ "messages": [{ "content": "hi" }], "max_completion_tokens": 12 });
        assert_eq!(estimate_request(&body).output, 12);
    }

    #[test]
    fn sums_multipart_content_when_message_is_array() {
        let body = json!({ "messages": [{ "content": [{ "text": "a".repeat(8) }, { "text": "b".repeat(8) }] }] });
        assert_eq!(estimate_request(&body).input, 4);
    }

    #[test]
    fn falls_back_to_allowance_when_body_not_an_object() {
        let only_output = Estimate {
            input: 0,
            output: DEFAULT_OUTPUT_ALLOWANCE,
            total: DEFAULT_OUTPUT_ALLOWANCE,
        };
        assert_eq!(estimate_request(&json!([1, 2, 3])), only_output);
    }
}
