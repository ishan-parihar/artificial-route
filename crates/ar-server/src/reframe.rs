//! Re-frames an OpenAI-shaped upstream answer in the dialect the client spoke.
//!
//! A `/v1/messages`, `/v1/responses` or `/api/chat` client cannot read the
//! chat-completion form the router works in, and an Anthropic-, Responses- or
//! Gemini-wire provider's answer is not in it either; an `/v1/chat/completions`
//! client cannot read those providers' own forms. A whole reply is re-spelled
//! through the `openai_reply_to_*` builders in `ar-translate`; a stream is pulled
//! through [`Reframer`], which turns each `chat.completion.chunk` into the
//! dialect's own events.
//!
//! # Deliberate simplifications
//!
//! * `reasoning_content` is dropped, not rendered as `thinking` blocks: the
//!   inbound parse rejects a thinking block, so echoing one back on the next turn
//!   would fail the request.
//! * The terminal events wait for the upstream to end, because the usage a stream
//!   reports often rides on a chunk after its `finish_reason`. A stream that ends
//!   before its finish gets no terminal event, and the relay names the truncation
//!   in-band.
//! * A tool call's argument deltas are emitted only while that call's block is
//!   open. ponytail: OpenAI sends parallel calls one after another, so this holds
//!   in practice; a late delta for an earlier call is dropped. Buffer per call if
//!   an upstream interleaves them.

use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};

use ar_exec::{DEFAULT_FRAME_CAP, SseDecoder, SseEvent};
use ar_registry::WireFormat;
use ar_route::ChunkStream;
use ar_translate::{
    AnthropicStream, GeminiStream, ResponsesStream, claude_stop_reason, openai_reply_to_anthropic,
    openai_reply_to_ollama, openai_reply_to_responses,
};

use crate::routes::Dialect;

/// The id a stream carries when the upstream never named one.
const FALLBACK_ID: &str = "aroute-stream";

/// The OpenAI completion a whole upstream reply is, read in its source wire's
/// shape. `None` for a body that is no completion, which then relays as it arrived.
pub(crate) fn completion(source: WireFormat, value: Value) -> Option<Value> {
    match source {
        WireFormat::Anthropic => ar_translate::anthropic_response_to_openai(&value),
        WireFormat::OpenaiResponses => ar_translate::responses_response_to_openai(&value),
        _ => value.get("choices").is_some().then_some(value),
    }
}

/// Re-spells one whole OpenAI completion in `dialect`.
#[must_use]
pub(crate) fn reply(dialect: Dialect, completion: Value) -> Value {
    match dialect {
        Dialect::OpenAi => completion,
        Dialect::Anthropic => openai_reply_to_anthropic(&completion),
        Dialect::Ollama => openai_reply_to_ollama(&completion),
        Dialect::Responses => openai_reply_to_responses(&completion),
    }
}

/// Re-frames a stream from `source`'s shape as `dialect`'s own events.
///
/// The bytes go through one decoder, so a frame split across reads is joined
/// before it is read. A source that is not OpenAI chunks is lifted first, by
/// [`Lift`].
pub(crate) fn stream(
    mut upstream: ChunkStream,
    dialect: Dialect,
    model: &str,
    source: WireFormat,
) -> ChunkStream {
    let mut reframer = Reframer::new(dialect, model, source);
    Box::pin(async_stream::stream! {
        let mut decoder = SseDecoder::new(DEFAULT_FRAME_CAP);
        while let Some(chunk) = upstream.next().await {
            // An oversize frame cannot be framed. Ending here means no terminal
            // event follows, and the relay names the cut.
            if decoder.feed(&chunk).is_err() {
                break;
            }
            while let Some(SseEvent::Data(payload)) = decoder.next_event() {
                for frame in reframer.push(&payload) {
                    yield frame;
                }
            }
        }
        if let Some(SseEvent::Data(payload)) = decoder.finish() {
            for frame in reframer.push(&payload) {
                yield frame;
            }
        }
        // `[DONE]` is the OpenAI terminator, so a stream that closed with it is
        // complete even when no chunk named a finish reason.
        if decoder.done() {
            reframer.close_by_done();
        }
        for frame in reframer.end() {
            yield frame;
        }
    })
}

/// An upstream stream's own event shape, lifted to OpenAI chunks before the
/// state machine sees them.
enum Lift {
    Anthropic(AnthropicStream),
    Responses(ResponsesStream),
    Gemini(GeminiStream),
}

impl Lift {
    /// The lift a source wire needs. `None` for a source already in chunk form.
    fn for_source(source: WireFormat) -> Option<Self> {
        match source {
            WireFormat::Anthropic => Some(Self::Anthropic(AnthropicStream::new())),
            WireFormat::OpenaiResponses => Some(Self::Responses(ResponsesStream::new())),
            WireFormat::Gemini => Some(Self::Gemini(GeminiStream::new())),
            _ => None,
        }
    }

    fn event(&mut self, payload: &[u8]) -> Vec<Value> {
        match self {
            Self::Anthropic(lift) => lift.event(payload),
            Self::Responses(lift) => lift.event(payload),
            Self::Gemini(lift) => lift.event(payload),
        }
    }
}

/// The stream state machine: one OpenAI chunk in, this dialect's frames out.
struct Reframer {
    dialect: Dialect,
    /// The served model, taken from the upstream and kept from the request until
    /// it names one.
    model: String,
    /// The upstream's completion id, from the first chunk that names one.
    id: String,
    /// Whether the dialect's opening frame has gone out.
    opened: bool,
    /// Responses' `sequence_number`, counted across every event it emits.
    sequence: u64,
    /// The next content block or output item index. Anthropic and Responses number
    /// them the same way.
    next_index: usize,
    /// The block or item open now. Both dialects hold one open at a time.
    current: Option<Open>,
    /// Tool calls in arrival order, keyed by OpenAI's `tool_calls[].index`.
    tools: Vec<Tool>,
    /// Responses' closed output items, carried into the terminal event.
    items: Vec<Value>,
    /// The `finish_reason` the upstream closed the answer with.
    finish_reason: Option<String>,
    /// The last `usage` object the upstream sent.
    usage: Option<Value>,
    /// Set when the upstream speaks a non-OpenAI stream: its events are lifted to
    /// OpenAI chunks before they reach the state machine above.
    lift: Option<Lift>,
}

/// A content block or output item left open.
struct Open {
    index: usize,
    kind: OpenKind,
}

enum OpenKind {
    /// The text so far, so the closing events can repeat the whole of it.
    Text(String),
    /// A slot into [`Reframer::tools`].
    Tool(usize),
}

/// One tool call, accumulated from its deltas.
struct Tool {
    key: u64,
    id: String,
    name: String,
    arguments: String,
}

impl Reframer {
    fn new(dialect: Dialect, model: &str, source: WireFormat) -> Self {
        Self {
            dialect,
            model: model.to_owned(),
            id: String::new(),
            opened: false,
            sequence: 0,
            next_index: 0,
            current: None,
            tools: Vec::new(),
            items: Vec::new(),
            finish_reason: None,
            usage: None,
            lift: Lift::for_source(source),
        }
    }

    /// Frames for one upstream payload. A payload that is not a chunk contributes
    /// nothing.
    fn push(&mut self, payload: &[u8]) -> Vec<Bytes> {
        let chunks = match self.lift.as_mut() {
            Some(lift) => lift.event(payload),
            None => serde_json::from_slice::<Value>(payload)
                .into_iter()
                .collect(),
        };
        chunks
            .iter()
            .flat_map(|chunk| self.push_chunk(chunk))
            .collect()
    }

    /// Frames for one OpenAI chunk. An OpenAI client gets the chunk back as the
    /// `data:` line it already is.
    fn push_chunk(&mut self, chunk: &Value) -> Vec<Bytes> {
        let mut out = Vec::new();
        if self.id.is_empty()
            && let Some(id) = chunk.get("id").and_then(Value::as_str)
        {
            self.id = id.to_owned();
        }
        if let Some(model) = chunk
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| *m != self.model)
        {
            self.model = model.to_owned();
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage.clone());
        }
        // An OpenAI client gets the chunk back as the `data:` line it already is,
        // usage-only chunks included. The finish is still recorded, so the terminal
        // `[DONE]` knows the answer closed.
        if matches!(self.dialect, Dialect::OpenAi) {
            if let Some(reason) = chunk
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
            {
                self.finish_reason = Some(reason.to_owned());
            }
            return vec![Bytes::from(format!("data: {chunk}\n\n"))];
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return out;
        };
        if !self.opened {
            self.opened = true;
            self.open(&mut out);
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_owned());
        }
        let delta = choice.get("delta");
        if let Some(text) = delta
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            self.text(text, &mut out);
        }
        if let Some(calls) = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for call in calls {
                self.tool_call(call, &mut out);
            }
        }
        out
    }

    /// Records a finish for a stream that ended on `[DONE]` without one.
    fn close_by_done(&mut self) {
        self.finish_reason.get_or_insert_with(|| "stop".to_owned());
    }

    /// The frames that end the stream.
    ///
    /// Only a stream that reached its finish reason gets them. A cut stream gets
    /// none, so the relay names the truncation in-band.
    fn end(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        let Some(reason) = self.finish_reason.clone() else {
            return out;
        };
        self.close(&mut out);
        let (prompt, completion) = (self.count("prompt_tokens"), self.count("completion_tokens"));
        match self.dialect {
            Dialect::Anthropic => {
                self.emit(
                    json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": claude_stop_reason(Some(&reason)),
                            "stop_sequence": null,
                        },
                        "usage": { "input_tokens": prompt, "output_tokens": completion },
                    }),
                    &mut out,
                );
                self.emit(json!({ "type": "message_stop" }), &mut out);
            }
            Dialect::Responses => {
                let (event, status) = if reason == "length" {
                    ("response.incomplete", "incomplete")
                } else {
                    ("response.completed", "completed")
                };
                let output = Value::Array(std::mem::take(&mut self.items));
                let mut response = self.response_shell(status, output);
                response["usage"] = json!({
                    "input_tokens": prompt,
                    "output_tokens": completion,
                    "total_tokens": prompt + completion,
                });
                self.emit(json!({ "type": event, "response": response }), &mut out);
            }
            Dialect::Ollama => {
                let mut message = json!({ "role": "assistant", "content": "" });
                if !self.tools.is_empty() {
                    message["tool_calls"] = Value::Array(
                        self.tools
                            .iter()
                            .map(|call| {
                                json!({ "function": {
                                    "name": call.name,
                                    "arguments": serde_json::from_str::<Value>(&call.arguments)
                                        .unwrap_or_else(|_| json!({})),
                                } })
                            })
                            .collect(),
                    );
                }
                let mut line = json!({
                    "model": self.model,
                    "message": message,
                    "done": true,
                    "done_reason": if reason == "length" { "length" } else { "stop" },
                });
                if self.usage.is_some() {
                    line["prompt_eval_count"] = json!(prompt);
                    line["eval_count"] = json!(completion);
                }
                out.push(Bytes::from(format!("{line}\n")));
            }
            Dialect::OpenAi => out.push(Bytes::from_static(b"data: [DONE]\n\n")),
        }
        out
    }

    /// The opening frame: Anthropic's `message_start`, Responses' `response.created`.
    /// Ollama and OpenAI have none.
    fn open(&mut self, out: &mut Vec<Bytes>) {
        match self.dialect {
            Dialect::Anthropic => self.emit(
                json!({
                    "type": "message_start",
                    "message": {
                        "id": self.id(),
                        "type": "message",
                        "role": "assistant",
                        "model": self.model,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 },
                    },
                }),
                out,
            ),
            Dialect::Responses => self.emit(
                json!({
                    "type": "response.created",
                    "response": self.response_shell("in_progress", json!([])),
                }),
                out,
            ),
            Dialect::Ollama | Dialect::OpenAi => {}
        }
    }

    fn text(&mut self, text: &str, out: &mut Vec<Bytes>) {
        if matches!(self.dialect, Dialect::Ollama) {
            out.push(Bytes::from(format!(
                "{}\n",
                json!({
                    "model": self.model,
                    "message": { "role": "assistant", "content": text },
                    "done": false,
                })
            )));
            return;
        }
        if !matches!(
            self.current,
            Some(Open {
                kind: OpenKind::Text(_),
                ..
            })
        ) {
            self.close(out);
            self.open_text(out);
        }
        let index = match self.current.as_mut() {
            Some(Open {
                index,
                kind: OpenKind::Text(acc),
            }) => {
                acc.push_str(text);
                *index
            }
            _ => return,
        };
        let data = match self.dialect {
            Dialect::Anthropic => json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "text_delta", "text": text },
            }),
            _ => json!({
                "type": "response.output_text.delta",
                "item_id": self.message_item_id(index),
                "output_index": index,
                "content_index": 0,
                "delta": text,
            }),
        };
        self.emit(data, out);
    }

    fn open_text(&mut self, out: &mut Vec<Bytes>) {
        let index = self.next_index;
        self.next_index += 1;
        self.current = Some(Open {
            index,
            kind: OpenKind::Text(String::new()),
        });
        let data = match self.dialect {
            Dialect::Anthropic => json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "text", "text": "" },
            }),
            Dialect::Responses => {
                let item_id = self.message_item_id(index);
                self.emit(
                    json!({
                        "type": "response.output_item.added",
                        "output_index": index,
                        "item": {
                            "type": "message",
                            "id": item_id,
                            "status": "in_progress",
                            "role": "assistant",
                            "content": [],
                        },
                    }),
                    out,
                );
                json!({
                    "type": "response.content_part.added",
                    "item_id": item_id,
                    "output_index": index,
                    "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] },
                })
            }
            Dialect::Ollama | Dialect::OpenAi => return,
        };
        self.emit(data, out);
    }

    fn tool_call(&mut self, call: &Value, out: &mut Vec<Bytes>) {
        let key = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let slot = if let Some(slot) = self.tools.iter().position(|t| t.key == key) {
            slot
        } else {
            self.close(out);
            self.tools.push(Tool {
                key,
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                name: call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments: String::new(),
            });
            let slot = self.tools.len() - 1;
            self.open_tool(slot, out);
            slot
        };
        let Some(arguments) = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty())
        else {
            return;
        };
        self.tools[slot].arguments.push_str(arguments);
        let index = match &self.current {
            Some(Open {
                index,
                kind: OpenKind::Tool(open),
            }) if *open == slot => *index,
            _ => return,
        };
        let data = match self.dialect {
            Dialect::Anthropic => json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "input_json_delta", "partial_json": arguments },
            }),
            Dialect::Responses => json!({
                "type": "response.function_call_arguments.delta",
                "item_id": format!("fc_{}", self.tools[slot].id),
                "output_index": index,
                "delta": arguments,
            }),
            Dialect::Ollama | Dialect::OpenAi => return,
        };
        self.emit(data, out);
    }

    fn open_tool(&mut self, slot: usize, out: &mut Vec<Bytes>) {
        let index = self.next_index;
        self.next_index += 1;
        self.current = Some(Open {
            index,
            kind: OpenKind::Tool(slot),
        });
        let call = &self.tools[slot];
        let data = match self.dialect {
            Dialect::Anthropic => json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": {},
                },
            }),
            Dialect::Responses => json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": {
                    "type": "function_call",
                    "id": format!("fc_{}", call.id),
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": "",
                    "status": "in_progress",
                },
            }),
            Dialect::Ollama | Dialect::OpenAi => return,
        };
        self.emit(data, out);
    }

    /// Closes the open block or item, if any. A Responses item is recorded for the
    /// terminal event, which carries every item the response produced.
    fn close(&mut self, out: &mut Vec<Bytes>) {
        let Some(open) = self.current.take() else {
            return;
        };
        match (self.dialect, open.kind) {
            (Dialect::Anthropic, _) => {
                self.emit(
                    json!({ "type": "content_block_stop", "index": open.index }),
                    out,
                );
            }
            (Dialect::Responses, OpenKind::Text(text)) => {
                let item_id = self.message_item_id(open.index);
                let part = json!({ "type": "output_text", "text": text, "annotations": [] });
                self.emit(
                    json!({
                        "type": "response.output_text.done",
                        "item_id": item_id,
                        "output_index": open.index,
                        "content_index": 0,
                        "text": text,
                    }),
                    out,
                );
                self.emit(
                    json!({
                        "type": "response.content_part.done",
                        "item_id": item_id,
                        "output_index": open.index,
                        "content_index": 0,
                        "part": part,
                    }),
                    out,
                );
                let item = json!({
                    "type": "message",
                    "id": item_id,
                    "status": "completed",
                    "role": "assistant",
                    "content": [part],
                });
                self.emit(
                    json!({
                        "type": "response.output_item.done",
                        "output_index": open.index,
                        "item": item,
                    }),
                    out,
                );
                self.items.push(item);
            }
            (Dialect::Responses, OpenKind::Tool(slot)) => {
                let call = &self.tools[slot];
                let item = json!({
                    "type": "function_call",
                    "id": format!("fc_{}", call.id),
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments,
                    "status": "completed",
                });
                self.emit(
                    json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": format!("fc_{}", call.id),
                        "output_index": open.index,
                        "arguments": call.arguments,
                    }),
                    out,
                );
                self.emit(
                    json!({
                        "type": "response.output_item.done",
                        "output_index": open.index,
                        "item": item,
                    }),
                    out,
                );
                self.items.push(item);
            }
            (Dialect::Ollama | Dialect::OpenAi, _) => {}
        }
    }

    /// Queues one event. Anthropic and Responses name their SSE event by the
    /// payload's own `type`, and Responses stamps a `sequence_number` on each one.
    fn emit(&mut self, mut data: Value, out: &mut Vec<Bytes>) {
        if matches!(self.dialect, Dialect::Responses) {
            self.sequence += 1;
            data["sequence_number"] = json!(self.sequence);
        }
        let name = data.get("type").and_then(Value::as_str).unwrap_or_default();
        out.push(Bytes::from(format!("event: {name}\ndata: {data}\n\n")));
    }

    fn response_shell(&self, status: &str, output: Value) -> Value {
        json!({
            "id": self.id(),
            "object": "response",
            "status": status,
            "model": self.model,
            "output": output,
        })
    }

    fn id(&self) -> &str {
        if self.id.is_empty() {
            FALLBACK_ID
        } else {
            &self.id
        }
    }

    fn message_item_id(&self, index: usize) -> String {
        format!("msg_{}_{index}", self.id())
    }

    fn count(&self, key: &str) -> u64 {
        self.usage
            .as_ref()
            .and_then(|usage| usage.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `chunks` through the state machine the way the stream does, and
    /// returns each frame's JSON payload in order.
    fn frames(dialect: Dialect, chunks: &[Value]) -> Vec<Value> {
        let mut reframer = Reframer::new(dialect, "served", WireFormat::Openai);
        let mut out: Vec<Bytes> = Vec::new();
        for chunk in chunks {
            out.extend(reframer.push(chunk.to_string().as_bytes()));
        }
        out.extend(reframer.end());
        out.iter().map(payload_of).collect()
    }

    /// The JSON on a frame's `data:` line, or the whole line for NDJSON.
    fn payload_of(frame: &Bytes) -> Value {
        let text = std::str::from_utf8(frame).expect("frames are UTF-8");
        let json = text
            .lines()
            .find_map(|line| {
                line.strip_prefix("data: ")
                    .or_else(|| line.starts_with('{').then_some(line))
            })
            .expect("every frame carries a JSON payload");
        serde_json::from_str(json).expect("the payload is JSON")
    }

    fn kinds(frames: &[Value]) -> Vec<&str> {
        frames
            .iter()
            .map(|f| f.get("type").and_then(Value::as_str).unwrap_or("ndjson"))
            .collect()
    }

    #[test]
    fn anthropic_stream_should_wrap_text_in_one_block_and_end_with_message_stop() {
        let out = frames(
            Dialect::Anthropic,
            &[
                json!({"id":"c1","model":"m","choices":[{"delta":{"role":"assistant","content":"Hel"}}]}),
                json!({"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}),
            ],
        );

        assert_eq!(
            kinds(&out),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "{out:?}"
        );
        assert_eq!(out[5]["usage"]["output_tokens"], 2, "{out:?}");
    }

    #[test]
    fn anthropic_stream_should_open_a_tool_use_block_for_each_call() {
        let out = frames(
            Dialect::Anthropic,
            &[
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"read","arguments":""}}]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]}),
            ],
        );

        assert_eq!(out[1]["content_block"]["type"], "tool_use", "{out:?}");
        assert_eq!(out[1]["content_block"]["name"], "read", "{out:?}");
        assert_eq!(out[2]["delta"]["type"], "input_json_delta", "{out:?}");
        assert_eq!(out[5]["delta"]["stop_reason"], "tool_use", "{out:?}");
    }

    #[test]
    fn responses_stream_should_emit_the_item_lifecycle_and_completed() {
        let out = frames(
            Dialect::Responses,
            &[
                json!({"id":"c1","model":"m","choices":[{"delta":{"content":"hi"}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}),
            ],
        );

        assert_eq!(
            out.first().and_then(|f| f.get("type")),
            Some(&json!("response.created")),
            "{out:?}"
        );
        assert_eq!(
            out.last().and_then(|f| f.get("type")),
            Some(&json!("response.completed")),
            "{out:?}"
        );
        assert_eq!(
            out.last()
                .and_then(|f| f.pointer("/response/output/0/content/0/text")),
            Some(&json!("hi")),
            "{out:?}"
        );
    }

    #[test]
    fn responses_stream_should_number_every_event() {
        let out = frames(
            Dialect::Responses,
            &[json!({"choices":[{"delta":{"content":"a"},"finish_reason":"stop"}]})],
        );

        let sequence: Vec<u64> = out
            .iter()
            .filter_map(|f| f.get("sequence_number").and_then(Value::as_u64))
            .collect();
        assert_eq!(sequence.len(), out.len(), "{out:?}");
        assert!(sequence.windows(2).all(|w| w[0] < w[1]), "{sequence:?}");
    }

    #[test]
    fn ollama_stream_should_emit_text_lines_then_a_done_line_with_counts() {
        let out = frames(
            Dialect::Ollama,
            &[
                json!({"model":"m","choices":[{"delta":{"content":"hi"}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":6}}),
            ],
        );

        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[1]["done"], true, "{out:?}");
        assert_eq!(out[1]["eval_count"], 6, "{out:?}");
    }

    #[test]
    fn stream_should_emit_no_terminal_event_when_the_upstream_is_cut() {
        let out = frames(
            Dialect::Anthropic,
            &[json!({"choices":[{"delta":{"content":"partial"}}]})],
        );

        assert!(
            out.iter()
                .all(|f| f.get("type") != Some(&json!("message_stop"))),
            "{out:?}"
        );
    }

    #[test]
    fn stream_should_drop_reasoning_deltas_rather_than_render_thinking() {
        let out = frames(
            Dialect::Anthropic,
            &[
                json!({"choices":[{"delta":{"reasoning_content":"hmm"}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            ],
        );

        assert!(
            out.iter()
                .all(|f| f.pointer("/delta/type") != Some(&json!("thinking_delta"))),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn stream_should_frame_an_event_split_across_reads() {
        use futures::StreamExt as _;

        let whole =
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n";
        let (left, right) = whole.split_at(20);
        let upstream: ChunkStream = Box::pin(futures::stream::iter(vec![
            Bytes::from(left.to_owned()),
            Bytes::from(right.to_owned()),
        ]));

        let got: Vec<Bytes> = stream(upstream, Dialect::Anthropic, "m", WireFormat::Openai)
            .collect()
            .await;

        assert!(
            got.iter()
                .any(|f| f.starts_with(b"event: content_block_delta")),
            "{got:?}"
        );
    }

    #[tokio::test]
    async fn stream_should_close_an_answer_that_ended_on_done_without_a_finish_reason() {
        // A compatible upstream may omit `finish_reason` and still end with the
        // `[DONE]` terminator. That is a clean end, not a cut, so the Anthropic
        // client gets its `message_stop` rather than a stream error.
        use futures::StreamExt as _;

        let upstream: ChunkStream = Box::pin(futures::stream::iter(vec![
            Bytes::from("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"),
            Bytes::from("data: [DONE]\n\n"),
        ]));

        let got: Vec<Bytes> = stream(upstream, Dialect::Anthropic, "m", WireFormat::Openai)
            .collect()
            .await;
        let text: String = got
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();

        assert!(text.contains("event: message_stop"), "{text}");
        assert!(!text.contains("event: error"), "{text}");
    }

    #[tokio::test]
    async fn stream_should_lift_an_anthropic_upstream_into_the_responses_dialect() {
        use futures::StreamExt as _;

        let upstream: ChunkStream = Box::pin(futures::stream::iter(vec![
            Bytes::from(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"c\",\"usage\":{\"input_tokens\":2}}}\n\n",
            ),
            Bytes::from(
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            ),
            Bytes::from(
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            ),
        ]));

        let got: Vec<Bytes> = stream(upstream, Dialect::Responses, "m", WireFormat::Anthropic)
            .collect()
            .await;
        let text: String = got
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();

        assert!(text.contains("event: response.output_text.delta"), "{text}");
        assert!(text.contains("\"text\":\"hi\""), "{text}");
        assert!(text.contains("event: response.completed"), "{text}");
    }

    #[tokio::test]
    async fn stream_should_lift_a_responses_upstream_into_the_anthropic_dialect() {
        use futures::StreamExt as _;

        let upstream: ChunkStream = Box::pin(futures::stream::iter(vec![
            Bytes::from(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"grok\"}}\n\n",
            ),
            Bytes::from(
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            ),
            Bytes::from(
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n",
            ),
        ]));

        let got: Vec<Bytes> = stream(
            upstream,
            Dialect::Anthropic,
            "m",
            WireFormat::OpenaiResponses,
        )
        .collect()
        .await;
        let text: String = got
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();

        assert!(text.contains("event: content_block_delta"), "{text}");
        assert!(text.contains("\"text\":\"hi\""), "{text}");
        assert!(text.contains("event: message_stop"), "{text}");
        assert!(!text.contains("event: error"), "{text}");
    }

    #[tokio::test]
    async fn stream_should_lift_a_gemini_sse_upstream_into_the_openai_dialect() {
        use futures::StreamExt as _;

        let upstream: ChunkStream = Box::pin(futures::stream::iter(vec![
            Bytes::from(
                "data: {\"responseId\":\"g1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\r\n\r\n",
            ),
            Bytes::from(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":2,\"totalTokenCount\":4}}\r\n\r\n",
            ),
        ]));

        let got: Vec<Bytes> = stream(upstream, Dialect::OpenAi, "m", WireFormat::Gemini)
            .collect()
            .await;
        let text: String = got
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();

        assert!(text.contains("\"content\":\"hi\""), "{text}");
        assert!(text.contains("\"finish_reason\":\"stop\""), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    }

    #[test]
    fn completion_should_lift_a_responses_reply_into_a_chat_completion() {
        let body = json!({
            "id": "resp_1", "object": "response", "model": "grok", "status": "completed",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "hi"}]}],
        });

        let out = completion(WireFormat::OpenaiResponses, body).expect("a response lifts");

        assert_eq!(out["choices"][0]["message"]["content"], "hi", "{out}");
    }

    #[test]
    fn reply_should_respell_a_completion_in_each_dialect() {
        let completion = json!({
            "id": "c", "model": "m", "created": 1,
            "choices": [{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 3},
        });

        assert_eq!(
            reply(Dialect::Anthropic, completion.clone())["content"][0]["text"],
            "hi"
        );
        assert_eq!(
            reply(Dialect::Ollama, completion.clone())["message"]["content"],
            "hi"
        );
        assert_eq!(
            reply(Dialect::Responses, completion)["output"][0]["content"][0]["text"],
            "hi"
        );
    }
}
