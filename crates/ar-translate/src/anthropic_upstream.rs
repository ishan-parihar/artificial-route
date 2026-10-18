//! Anthropic Messages as an *upstream* wire: a provider that speaks `/v1/messages`
//! answers in Anthropic's shape, and an OpenAI-dialect client cannot read it.
//!
//! The reply and the stream are lifted into OpenAI chat-completion form here, so
//! the router and the client-dialect re-framer in `ar-server` see one shape. The
//! inbound direction (an Anthropic *client*) is `anthropic.rs`.
//!
//! # Deliberate simplifications
//!
//! * `thinking` blocks are dropped, for the same reason `reasoning_content` is
//!   dropped on the way out: nothing downstream can echo them back correctly.
//! * Cache-read and cache-creation counts are not carried; the OpenAI usage
//!   object has no field for them.

use serde_json::{Value, json};

/// Lifts a whole Anthropic `message` reply into an OpenAI chat completion.
///
/// `None` when the body is not a `message`, so an error object relays unchanged.
#[must_use]
pub fn anthropic_response_to_openai(body: &Value) -> Option<Value> {
    if body.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in body.get("content")?.as_array()? {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => calls.push(json!({
                "id": block.get("id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": block.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": block.get("input").cloned().unwrap_or_else(|| json!({})).to_string(),
                },
            })),
            _ => {}
        }
    }
    let mut message = json!({ "role": "assistant", "content": text });
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    let prompt = count(body.get("usage"), "input_tokens");
    let completion = count(body.get("usage"), "output_tokens");
    Some(json!({
        "id": body.get("id").cloned().unwrap_or(Value::Null),
        "object": "chat.completion",
        "created": crate::gitlab::now_secs(),
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason(body.get("stop_reason").and_then(Value::as_str)),
        }],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        },
    }))
}

/// Lifts an Anthropic event stream into OpenAI chat-completion chunks, one event
/// at a time.
///
/// The state is what the lift needs to be consistent across events: the id and
/// model from `message_start`, the input count that `message_delta` does not
/// repeat, and the tool-call index, which OpenAI counts per call rather than per
/// block.
#[derive(Debug, Default)]
pub struct AnthropicStream {
    id: Value,
    model: Value,
    input_tokens: u64,
    /// Tool calls opened so far; the index of the latest one is `tools - 1`.
    tools: u64,
}

impl AnthropicStream {
    /// A lift that has seen no events yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The OpenAI chunks one Anthropic event payload lifts to. A payload that is
    /// not JSON, or is an event with no OpenAI counterpart, lifts to nothing.
    pub fn event(&mut self, payload: &[u8]) -> Vec<Value> {
        let Ok(event) = serde_json::from_slice::<Value>(payload) else {
            return Vec::new();
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = event.get("message").unwrap_or(&Value::Null);
                self.id = message.get("id").cloned().unwrap_or(Value::Null);
                self.model = message.get("model").cloned().unwrap_or(Value::Null);
                self.input_tokens = count(message.get("usage"), "input_tokens");
                vec![self.chunk(json!({ "role": "assistant", "content": "" }), Value::Null)]
            }
            Some("content_block_start") => {
                let block = event.get("content_block").unwrap_or(&Value::Null);
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    return Vec::new();
                }
                let index = self.tools;
                self.tools += 1;
                vec![self.chunk(
                    json!({ "tool_calls": [{
                        "index": index,
                        "id": block.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": {
                            "name": block.get("name").cloned().unwrap_or(Value::Null),
                            "arguments": "",
                        },
                    }] }),
                    Value::Null,
                )]
            }
            Some("content_block_delta") => self.delta(&event),
            Some("message_delta") => self.finish(&event),
            _ => Vec::new(),
        }
    }

    fn delta(&self, event: &Value) -> Vec<Value> {
        let delta = event.get("delta").unwrap_or(&Value::Null);
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    return Vec::new();
                }
                vec![self.chunk(json!({ "content": text }), Value::Null)]
            }
            Some("input_json_delta") => {
                let partial = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if partial.is_empty() || self.tools == 0 {
                    return Vec::new();
                }
                vec![self.chunk(
                    json!({ "tool_calls": [{
                        "index": self.tools - 1,
                        "function": { "arguments": partial },
                    }] }),
                    Value::Null,
                )]
            }
            _ => Vec::new(),
        }
    }

    /// The finish chunk, then a usage-only chunk, which is how OpenAI sends a
    /// stream's counts after the answer is closed.
    fn finish(&self, event: &Value) -> Vec<Value> {
        let reason = event.pointer("/delta/stop_reason").and_then(Value::as_str);
        let completion = count(event.get("usage"), "output_tokens");
        vec![
            self.chunk(json!({}), json!(finish_reason(reason))),
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "model": self.model,
                "choices": [],
                "usage": {
                    "prompt_tokens": self.input_tokens,
                    "completion_tokens": completion,
                    "total_tokens": self.input_tokens + completion,
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

/// Anthropic's `stop_reason` onto OpenAI's `finish_reason`.
fn finish_reason(stop: Option<&str>) -> &'static str {
    match stop {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
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
        let mut stream = AnthropicStream::new();
        events
            .iter()
            .flat_map(|e| stream.event(e.to_string().as_bytes()))
            .collect()
    }

    #[test]
    fn response_should_lift_text_and_tool_use_into_a_chat_completion() {
        let body = json!({
            "id": "msg_1", "type": "message", "model": "claude",
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "a"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 3, "output_tokens": 4},
        });
        let out = anthropic_response_to_openai(&body).expect("a message lifts");

        assert_eq!(out["choices"][0]["message"]["content"], "hi", "{out}");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "read",
            "{out}"
        );
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls", "{out}");
        assert_eq!(out["usage"]["total_tokens"], 7, "{out}");
    }

    #[test]
    fn response_should_refuse_an_error_object() {
        assert!(anthropic_response_to_openai(&json!({"type": "error"})).is_none());
    }

    #[test]
    fn stream_should_lift_text_and_close_with_finish_and_usage() {
        let out = lift(&[
            json!({"type": "message_start", "message": {"id": "m", "model": "c", "usage": {"input_tokens": 9}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        ]);

        assert_eq!(out[1]["choices"][0]["delta"]["content"], "hi", "{out:?}");
        assert_eq!(out[2]["choices"][0]["finish_reason"], "stop", "{out:?}");
        assert_eq!(out[3]["usage"]["prompt_tokens"], 9, "{out:?}");
    }

    #[test]
    fn stream_should_index_tool_calls_per_call_not_per_block() {
        let out = lift(&[
            json!({"type": "message_start", "message": {"id": "m", "model": "c"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1", "name": "a", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
        ]);

        assert_eq!(
            out[1]["choices"][0]["delta"]["tool_calls"][0]["index"], 0,
            "{out:?}"
        );
        assert_eq!(
            out[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{}",
            "{out:?}"
        );
    }
}
