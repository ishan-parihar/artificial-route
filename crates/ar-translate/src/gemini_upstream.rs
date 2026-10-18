//! Gemini's `streamGenerateContent?alt=sse` as an *upstream* wire: each SSE
//! `data:` line is a whole `GenerateContentResponse`, and an OpenAI client cannot
//! read it.
//!
//! The stream is lifted into chat-completion chunks here, so the client-dialect
//! re-framer in `ar-server` sees one shape. The whole-reply lift is
//! `translate.rs`'s `gemini_response_to_openai`.
//!
//! # Deliberate simplifications
//!
//! * `thought` parts are dropped, as `reasoning_content` is on the way out.
//! * Usage is read from the event that carries `finishReason`. Gemini sends
//!   `usageMetadata` on that event; a count that arrived only on a later event
//!   would be lost.

use serde_json::{Value, json};

use crate::gemini::GeminiFunctionCall;
use crate::translate::gemini_tool_call;

/// Lifts a Gemini SSE stream into OpenAI chat-completion chunks, one event at a
/// time.
#[derive(Debug, Default)]
pub struct GeminiStream {
    id: Value,
    model: Value,
    /// Whether the opening role chunk has gone out.
    started: bool,
    /// Function calls opened so far; Gemini sends each call whole, so the index is
    /// the count before it.
    tools: u64,
}

impl GeminiStream {
    /// A lift that has seen no events yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The OpenAI chunks one Gemini event payload lifts to. A payload that is not
    /// JSON lifts to nothing.
    pub fn event(&mut self, payload: &[u8]) -> Vec<Value> {
        let Ok(event) = serde_json::from_slice::<Value>(payload) else {
            return Vec::new();
        };
        if let Some(id) = event.get("responseId") {
            self.id = id.clone();
        }
        if let Some(model) = event.get("modelVersion") {
            self.model = model.clone();
        }
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(self.chunk(json!({ "role": "assistant", "content": "" }), Value::Null));
        }
        let candidate = event.pointer("/candidates/0");
        let parts = candidate
            .and_then(|c| c.pointer("/content/parts"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for part in parts {
            if part
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    out.push(self.chunk(json!({ "content": text }), Value::Null));
                }
            } else if let Some(call) = part
                .get("functionCall")
                .and_then(|c| serde_json::from_value::<GeminiFunctionCall>(c.clone()).ok())
                .and_then(|c| gemini_tool_call(&c))
            {
                let index = self.tools;
                self.tools += 1;
                out.push(self.chunk(
                    json!({ "tool_calls": [{
                        "index": index,
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments.to_string(),
                        },
                    }] }),
                    Value::Null,
                ));
            }
        }
        if let Some(reason) = candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(Value::as_str)
        {
            out.extend(self.finish(&event, reason));
        }
        out
    }

    /// The finish chunk, then a usage-only chunk, as an OpenAI stream closes.
    fn finish(&self, event: &Value, reason: &str) -> Vec<Value> {
        let finish = if self.tools > 0 {
            "tool_calls"
        } else {
            finish_reason(reason)
        };
        let usage = event.get("usageMetadata");
        let prompt = count(usage, "promptTokenCount");
        let completion = count(usage, "candidatesTokenCount");
        vec![
            self.chunk(json!({}), json!(finish)),
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "model": self.model,
                "choices": [],
                "usage": {
                    "prompt_tokens": prompt,
                    "completion_tokens": completion,
                    "total_tokens": count(usage, "totalTokenCount"),
                },
            }),
        ]
    }

    fn chunk(&self, delta: Value, finish: Value) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    }
}

/// Gemini's `finishReason` onto OpenAI's `finish_reason`, under the same mapping
/// the whole-reply lift uses.
fn finish_reason(reason: &str) -> &'static str {
    match reason {
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "LANGUAGE" => {
            "content_filter"
        }
        _ => "stop",
    }
}

fn count(usage: Option<&Value>, key: &str) -> u64 {
    usage
        .and_then(|u| u.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lift(events: &[Value]) -> Vec<Value> {
        let mut stream = GeminiStream::new();
        events
            .iter()
            .flat_map(|e| stream.event(e.to_string().as_bytes()))
            .collect()
    }

    #[test]
    fn stream_should_lift_text_and_close_with_finish_and_usage() {
        let out = lift(&[
            json!({"responseId": "r1", "modelVersion": "gemini-x", "candidates": [{"content": {"parts": [{"text": "hi"}]}}]}),
            json!({"candidates": [{"content": {"parts": [{"text": " there"}]}, "finishReason": "STOP"}],
                   "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "totalTokenCount": 7}}),
        ]);

        assert_eq!(
            out[0]["choices"][0]["delta"]["role"], "assistant",
            "{out:?}"
        );
        assert_eq!(out[1]["choices"][0]["delta"]["content"], "hi", "{out:?}");
        assert_eq!(
            out[2]["choices"][0]["delta"]["content"], " there",
            "{out:?}"
        );
        assert_eq!(out[3]["choices"][0]["finish_reason"], "stop", "{out:?}");
        assert_eq!(out[4]["usage"]["total_tokens"], 7, "{out:?}");
        assert_eq!(out[4]["id"], "r1", "{out:?}");
    }

    #[test]
    fn stream_should_lift_a_function_call_and_finish_as_tool_calls() {
        let out = lift(&[json!({
            "candidates": [{
                "content": {"parts": [{"functionCall": {"name": "read", "args": {"p": 1}}}]},
                "finishReason": "STOP",
            }],
        })]);

        assert_eq!(
            out[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "read",
            "{out:?}"
        );
        assert_eq!(
            out[1]["choices"][0]["delta"]["tool_calls"][0]["id"], "read",
            "{out:?}"
        );
        assert_eq!(
            out[2]["choices"][0]["finish_reason"], "tool_calls",
            "{out:?}"
        );
    }

    #[test]
    fn stream_should_drop_a_thought_part() {
        let out = lift(&[json!({
            "candidates": [{"content": {"parts": [{"text": "pondering", "thought": true}]}}],
        })]);

        assert_eq!(out.len(), 1, "only the role chunk: {out:?}");
    }

    #[test]
    fn stream_should_map_a_token_cap_to_length() {
        let out = lift(&[json!({
            "candidates": [{"content": {"parts": [{"text": "cut"}]}, "finishReason": "MAX_TOKENS"}],
        })]);

        assert_eq!(out[2]["choices"][0]["finish_reason"], "length", "{out:?}");
    }
}
