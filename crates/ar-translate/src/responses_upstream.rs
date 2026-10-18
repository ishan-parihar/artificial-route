//! The OpenAI Responses API as an *upstream* wire: a provider that speaks
//! `/v1/responses` answers in Responses' shape (`output` items and `response.*`
//! events), and an OpenAI chat client cannot read it.
//!
//! The reply and the stream are lifted into chat-completion form here, so the
//! client-dialect re-framer in `ar-server` sees one shape. The inbound direction
//! (a Responses *client*) is `translate.rs`.
//!
//! # Deliberate simplifications
//!
//! * Reasoning items are dropped, for the same reason `reasoning_content` is
//!   dropped on the way out: nothing downstream can echo them back correctly.
//! * Only text and function-call items cross. Built-in tool items (web search,
//!   file search, code interpreter) have no chat-completion counterpart and are
//!   dropped rather than mis-shaped.

use serde_json::{Value, json};

/// Lifts a whole Responses `response` object into an OpenAI chat completion.
///
/// `None` when the body is not a `response`, so an error object relays unchanged.
#[must_use]
pub fn responses_response_to_openai(body: &Value) -> Option<Value> {
    if body.get("object").and_then(Value::as_str) != Some("response") {
        return None;
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    for item in body.get("output")?.as_array()? {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                    }
                }
            }
            Some("function_call") => calls.push(call_json(item)),
            _ => {}
        }
    }
    let called = !calls.is_empty();
    let mut message = json!({ "role": "assistant", "content": text });
    if called {
        message["tool_calls"] = Value::Array(calls);
    }
    let prompt = count(body.get("usage"), "input_tokens");
    let completion = count(body.get("usage"), "output_tokens");
    let finish = finish_reason(body, called);
    Some(json!({
        "id": body.get("id").cloned().unwrap_or(Value::Null),
        "object": "chat.completion",
        "created": crate::gitlab::now_secs(),
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        },
    }))
}

/// Lifts a Responses event stream into OpenAI chat-completion chunks, one event
/// at a time.
///
/// Events are read by their `type`. The id and model come from `response.created`,
/// and the tool-call index is counted per call, as OpenAI does.
#[derive(Debug, Default)]
pub struct ResponsesStream {
    id: Value,
    model: Value,
    /// Function calls opened so far; the index of the latest one is `tools - 1`.
    tools: u64,
}

impl ResponsesStream {
    /// A lift that has seen no events yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The OpenAI chunks one Responses event payload lifts to. A payload that is
    /// not JSON, or is an event with no chat-completion counterpart, lifts to
    /// nothing.
    pub fn event(&mut self, payload: &[u8]) -> Vec<Value> {
        let Ok(event) = serde_json::from_slice::<Value>(payload) else {
            return Vec::new();
        };
        match event.get("type").and_then(Value::as_str) {
            Some("response.created") => {
                let response = event.get("response").unwrap_or(&Value::Null);
                self.id = response.get("id").cloned().unwrap_or(Value::Null);
                self.model = response.get("model").cloned().unwrap_or(Value::Null);
                vec![self.chunk(json!({ "role": "assistant", "content": "" }), Value::Null)]
            }
            Some("response.output_item.added") => {
                let item = event.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return Vec::new();
                }
                let index = self.tools;
                self.tools += 1;
                vec![self.chunk(
                    json!({ "tool_calls": [{
                        "index": index,
                        "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": {
                            "name": item.get("name").cloned().unwrap_or(Value::Null),
                            "arguments": "",
                        },
                    }] }),
                    Value::Null,
                )]
            }
            Some("response.output_text.delta") => {
                let text = event.get("delta").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    return Vec::new();
                }
                vec![self.chunk(json!({ "content": text }), Value::Null)]
            }
            Some("response.function_call_arguments.delta") => {
                let partial = event.get("delta").and_then(Value::as_str).unwrap_or("");
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
            // `response.incomplete` closes a reply that ran out of tokens; the
            // answer it carries is still complete as far as it goes.
            Some("response.completed" | "response.incomplete") => {
                let response = event.get("response").unwrap_or(&Value::Null);
                self.finish(response)
            }
            _ => Vec::new(),
        }
    }

    /// The finish chunk, then a usage-only chunk, as an OpenAI stream closes.
    fn finish(&self, response: &Value) -> Vec<Value> {
        let finish = finish_reason(response, self.tools > 0);
        let prompt = count(response.get("usage"), "input_tokens");
        let completion = count(response.get("usage"), "output_tokens");
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
                    "total_tokens": prompt + completion,
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

/// A `function_call` item as a chat-completion tool call.
fn call_json(item: &Value) -> Value {
    json!({
        "id": item.get("call_id").cloned().unwrap_or(Value::Null),
        "type": "function",
        "function": {
            "name": item.get("name").cloned().unwrap_or(Value::Null),
            "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
        },
    })
}

/// The OpenAI `finish_reason` a Responses reply closes with. A call wins over
/// everything; otherwise `incomplete_details` names why the reply stopped early.
fn finish_reason(response: &Value, called: bool) -> &'static str {
    if called {
        return "tool_calls";
    }
    match response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
    {
        Some("max_output_tokens") => "length",
        Some("content_filter") => "content_filter",
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
        let mut stream = ResponsesStream::new();
        events
            .iter()
            .flat_map(|e| stream.event(e.to_string().as_bytes()))
            .collect()
    }

    #[test]
    fn response_should_lift_text_and_a_call_into_a_chat_completion() {
        let body = json!({
            "id": "resp_1", "object": "response", "model": "grok",
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "content": [{"type": "output_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "c1", "name": "read", "arguments": "{\"p\":1}"},
            ],
            "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7},
        });
        let out = responses_response_to_openai(&body).expect("a response lifts");

        assert_eq!(out["choices"][0]["message"]["content"], "hi", "{out}");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"], "{\"p\":1}",
            "{out}"
        );
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls", "{out}");
        assert_eq!(out["usage"]["total_tokens"], 7, "{out}");
    }

    #[test]
    fn response_should_map_an_output_cap_to_length() {
        let body = json!({
            "object": "response", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "cut"}]}],
        });
        let out = responses_response_to_openai(&body).expect("a response lifts");

        assert_eq!(out["choices"][0]["finish_reason"], "length", "{out}");
    }

    #[test]
    fn response_should_refuse_an_error_object() {
        assert!(responses_response_to_openai(&json!({"error": {"message": "x"}})).is_none());
    }

    #[test]
    fn stream_should_lift_text_and_close_with_finish_and_usage() {
        let out = lift(&[
            json!({"type": "response.created", "response": {"id": "resp_1", "model": "grok"}}),
            json!({"type": "response.output_text.delta", "delta": "hi"}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 9, "output_tokens": 2}}}),
        ]);

        assert_eq!(out[1]["choices"][0]["delta"]["content"], "hi", "{out:?}");
        assert_eq!(out[2]["choices"][0]["finish_reason"], "stop", "{out:?}");
        assert_eq!(out[3]["usage"]["prompt_tokens"], 9, "{out:?}");
        assert_eq!(out[3]["usage"]["completion_tokens"], 2, "{out:?}");
    }

    #[test]
    fn stream_should_carry_a_call_and_its_arguments_under_one_index() {
        let out = lift(&[
            json!({"type": "response.created", "response": {"id": "r", "model": "m"}}),
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "call_id": "c1", "name": "read"}}),
            json!({"type": "response.function_call_arguments.delta", "delta": "{\"p\""}),
            json!({"type": "response.function_call_arguments.delta", "delta": ":1}"}),
            json!({"type": "response.completed", "response": {}}),
        ]);

        assert_eq!(
            out[1]["choices"][0]["delta"]["tool_calls"][0]["index"], 0,
            "{out:?}"
        );
        assert_eq!(
            out[1]["choices"][0]["delta"]["tool_calls"][0]["id"], "c1",
            "{out:?}"
        );
        assert_eq!(
            out[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], ":1}",
            "{out:?}"
        );
        assert_eq!(
            out[4]["choices"][0]["finish_reason"], "tool_calls",
            "{out:?}"
        );
    }

    #[test]
    fn stream_should_drop_a_reasoning_delta() {
        let out = lift(&[
            json!({"type": "response.created", "response": {"id": "r", "model": "m"}}),
            json!({"type": "response.reasoning_text.delta", "delta": "thinking"}),
        ]);

        assert_eq!(out.len(), 1, "only the role chunk: {out:?}");
    }
}
