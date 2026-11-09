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
//! * On the Anthropic dialect, `reasoning_content` is dropped, not rendered as
//!   `thinking` blocks: the inbound parse rejects a thinking block, so echoing
//!   one back on the next turn would fail the request. On the Responses
//!   dialect it *is* rendered, as `response.reasoning_summary_text.delta`
//!   events inside a `reasoning` output item: Codex's TUI shows thinking only
//!   from those events, and a Responses client that received none never echoes
//!   a `reasoning` item back (the client clears it for non-OpenAI providers),
//!   so the echo constraint that holds for Anthropic does not hold here.
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
    AnthropicStream, GeminiStream, ResponsesStream, ToolNamespaces, claude_stop_reason,
    openai_reply_to_anthropic, openai_reply_to_ollama, openai_reply_to_responses,
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

/// Re-spells one whole OpenAI completion in `dialect`, restoring the tool
/// namespaces the request declared.
///
/// A client that groups its tools resolves a call by `(namespace, name)`, so
/// the `function_call` item has to carry the namespace beside the name. The
/// upstream only knows the flat name, so the namespace comes from what the
/// request declared.
#[must_use]
pub(crate) fn reply_with_namespaces(
    dialect: Dialect,
    completion: Value,
    namespaces: &ToolNamespaces,
) -> Value {
    if namespaces.is_empty() || !matches!(dialect, Dialect::Responses) {
        return reply(dialect, completion);
    }
    let mut out = reply(dialect, completion);
    let Some(calls) = out
        .get_mut("output")
        .and_then(Value::as_array_mut)
    else {
        return out;
    };
    for item in calls {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        let Some(flat) = item.get("name").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        let (name, namespace) = namespaces.split(&flat);
        let Some(namespace) = namespace else {
            continue;
        };
        item["name"] = name.into();
        item["namespace"] = namespace.into();
    }
    out
}

/// Re-frames a stream from `source`'s shape as `dialect`'s own events.
///
/// The bytes go through one decoder, so a frame split across reads is joined
/// before it is read. A source that is not OpenAI chunks is lifted first, by
/// [`Lift`].
///
/// `namespaces` is the request's own map of the flat tool names it declared
/// under a namespace. A client that groups its tools resolves a call by
/// `(namespace, name)`, so the `function_call` item has to carry the namespace
/// beside the name, and the upstream only knows the flat name.
pub(crate) fn stream(
    mut upstream: ChunkStream,
    dialect: Dialect,
    model: &str,
    source: WireFormat,
    namespaces: &ToolNamespaces,
) -> ChunkStream {
    let mut reframer = Reframer::new(dialect, model, source, namespaces);
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
    /// The request's own map of the flat tool names it declared under a
    /// namespace, so a `function_call` item can carry the namespace back.
    namespaces: ToolNamespaces,
}

/// A content block or output item left open.
struct Open {
    index: usize,
    kind: OpenKind,
}

enum OpenKind {
    /// The text so far, so the closing events can repeat the whole of it.
    Text(String),
    /// The reasoning summary text so far, rendered as Responses `reasoning`
    /// summary events. Only the Responses dialect opens one: Anthropic drops
    /// the deltas (its inbound parse rejects a `thinking` block, so an echoed
    /// one would 400 the next turn) and the OpenAI chat dialect relays the
    /// chunk unchanged, `reasoning_content` included.
    Reasoning(String),
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
    fn new(
        dialect: Dialect,
        model: &str,
        source: WireFormat,
        namespaces: &ToolNamespaces,
    ) -> Self {
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
            namespaces: namespaces.clone(),
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
        // The upstream's own thinking, in the `reasoning_content` field GLM,
        // DeepSeek and friends use. The OpenAI chat dialect relays the chunk
        // unchanged above, so this branch only serves the dialects that re-frame:
        // Responses renders it as reasoning-summary events, Anthropic drops it
        // (its inbound parse would reject an echoed `thinking` block).
        if let Some(reasoning) = delta
            .and_then(|d| d.get("reasoning_content"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            && matches!(self.dialect, Dialect::Responses)
        {
            self.reasoning(reasoning, &mut out);
        }
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

    /// Streams one `reasoning_content` delta on the Responses dialect.
    ///
    /// Codex's TUI renders thinking from `response.reasoning_summary_text.delta`
    /// events (and the `reasoning` output item around them); a stream that
    /// carries none shows no thinking at all, which is the gap this closes.
    /// The text is accumulated so the closing `done` events can repeat the
    /// whole summary, the same way `text` accumulates the answer.
    fn reasoning(&mut self, text: &str, out: &mut Vec<Bytes>) {
        if !matches!(
            self.current,
            Some(Open {
                kind: OpenKind::Reasoning(_),
                ..
            })
        ) {
            self.close(out);
            self.open_reasoning(out);
        }
        let index = match self.current.as_mut() {
            Some(Open {
                index,
                kind: OpenKind::Reasoning(acc),
            }) => {
                acc.push_str(text);
                *index
            }
            _ => return,
        };
        let item_id = self.reasoning_item_id(index);
        self.emit(
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "delta": text,
            }),
            out,
        );
    }

    /// Opens a Responses `reasoning` output item: the `output_item.added`
    /// frame, then the first summary part.
    fn open_reasoning(&mut self, out: &mut Vec<Bytes>) {
        let index = self.next_index;
        self.next_index += 1;
        self.current = Some(Open {
            index,
            kind: OpenKind::Reasoning(String::new()),
        });
        let item_id = self.reasoning_item_id(index);
        self.emit(
            json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": {
                    "type": "reasoning",
                    "id": item_id,
                    "summary": [],
                },
            }),
            out,
        );
        self.emit(
            json!({
                "type": "response.reasoning_summary_part.added",
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": "" },
            }),
            out,
        );
    }

    /// The id a Responses `reasoning` output item carries.
    fn reasoning_item_id(&self, index: usize) -> String {
        format!("rs_{}_{index}", self.id())
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
        // The upstream knows the flat name; the client resolves a call by
        // `(namespace, name)`, so both the opening item and the closing one
        // carry the split. Emitting the flat name in one and the split in the
        // other would make a client that reads either one see a different
        // tool.
        let (name, namespace) = self.namespaces.split(&call.name);
        let mut item = json!({
            "type": "function_call",
            "id": format!("fc_{}", call.id),
            "call_id": call.id,
            "name": name,
            "arguments": "",
            "status": "in_progress",
        });
        if let Some(namespace) = namespace {
            item["namespace"] = namespace.into();
        }
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
                "item": item,
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
            (Dialect::Responses, OpenKind::Reasoning(summary)) => {
                // Close the reasoning item the way the Responses API spells it:
                // the summary part done, the summary text done, then the item
                // done carrying the whole summary. The item is pushed into the
                // terminal output so the completed response carries it too.
                let item_id = self.reasoning_item_id(open.index);
                let part = json!({ "type": "summary_text", "text": summary });
                self.emit(
                    json!({
                        "type": "response.reasoning_summary_part.done",
                        "item_id": item_id,
                        "output_index": open.index,
                        "summary_index": 0,
                        "part": part,
                    }),
                    out,
                );
                self.emit(
                    json!({
                        "type": "response.reasoning_summary_text.done",
                        "item_id": item_id,
                        "output_index": open.index,
                        "summary_index": 0,
                        "text": summary,
                    }),
                    out,
                );
                let item = json!({
                    "type": "reasoning",
                    "id": item_id,
                    "summary": [part],
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
                // The upstream knows the flat name; the client resolves a call
                // by `(namespace, name)`, so the split comes from what the
                // request declared and the item carries both halves.
                let (name, namespace) = self.namespaces.split(&call.name);
                let mut item = json!({
                    "type": "function_call",
                    "id": format!("fc_{}", call.id),
                    "call_id": call.id,
                    "name": name,
                    "arguments": call.arguments,
                    "status": "completed",
                });
                if let Some(namespace) = namespace {
                    item["namespace"] = namespace.into();
                }
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
        frames_with(dialect, chunks, &ToolNamespaces::default())
    }

    /// The same, with the request's own namespace map.
    fn frames_with(
        dialect: Dialect,
        chunks: &[Value],
        namespaces: &ToolNamespaces,
    ) -> Vec<Value> {
        let mut reframer = Reframer::new(dialect, "served", WireFormat::Openai, namespaces);
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

    /// The upstream knows only the flat name; a client that groups its tools
    /// resolves the call by `(namespace, name)`, so the `function_call` item
    /// has to carry the namespace back. Without it the client looks the name
    /// up in its default namespace and refuses the call as unknown.
    #[test]
    fn responses_stream_should_restore_the_namespace_on_a_tool_call() {
        let namespaces = ToolNamespaces::from_declarations(&[ar_translate::CanonicalTool {
            name: "activate_project".to_owned(),
            description: None,
            parameters: json!({ "type": "object" }),
            namespace: Some("mcp__serena".to_owned()),
        }]);
        let out = frames_with(
            Dialect::Responses,
            &[
                json!({"id":"c1","model":"m","choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"call_1","type":"function",
                     "function":{"name":"mcp__serena__activate_project","arguments":"{}"}}]}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ],
            &namespaces,
        );

        let item = out
            .iter()
            .find_map(|f| f.pointer("/item"))
            .expect("the tool call item");
        assert_eq!(item["name"], "activate_project", "{out:?}");
        assert_eq!(item["namespace"], "mcp__serena", "{out:?}");
    }

    /// A tool the request never declared under a namespace keeps its flat name
    /// and gets no namespace: the client resolves it in its own default
    /// namespace, which is what it always did.
    #[test]
    fn responses_stream_should_leave_an_undeclared_tool_name_alone() {
        let out = frames(
            Dialect::Responses,
            &[
                json!({"id":"c1","model":"m","choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"call_1","type":"function",
                     "function":{"name":"exec_command","arguments":"{}"}}]}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ],
        );

        let item = out
            .iter()
            .find_map(|f| f.pointer("/item"))
            .expect("the tool call item");
        assert_eq!(item["name"], "exec_command", "{out:?}");
        assert!(item.get("namespace").is_none(), "{out:?}");
    }

    /// The whole (non-streaming) reply gets the same treatment: the namespace
    /// comes back on the `function_call` item.
    #[test]
    fn a_whole_reply_should_restore_the_namespace_on_a_tool_call() {
        let namespaces = ToolNamespaces::from_declarations(&[ar_translate::CanonicalTool {
            name: "memory_recall".to_owned(),
            description: None,
            parameters: json!({ "type": "object" }),
            namespace: Some("mcp__memory_wire".to_owned()),
        }]);
        let completion = json!({
            "id": "c1",
            "model": "m",
            "choices": [{"index":0,"message":{"role":"assistant","content":null,
                "tool_calls":[{"id":"call_1","type":"function",
                    "function":{"name":"mcp__memory_wire__memory_recall","arguments":"{}"}}]},
                "finish_reason":"tool_calls"}],
            "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2},
        });

        let out = reply_with_namespaces(Dialect::Responses, completion, &namespaces);

        let item = &out["output"][0];
        assert_eq!(item["type"], "function_call", "{out}");
        assert_eq!(item["name"], "memory_recall", "{out}");
        assert_eq!(item["namespace"], "mcp__memory_wire", "{out}");
    }

    /// The split is driven by the *declared* namespace, so a name that itself
    /// carries the separator round-trips exactly.
    #[test]
    fn a_whole_reply_should_restore_a_namespace_that_carries_the_separator() {
        let namespaces = ToolNamespaces::from_declarations(&[ar_translate::CanonicalTool {
            name: "_web_run".to_owned(),
            description: None,
            parameters: json!({ "type": "object" }),
            namespace: Some("mcp__codex_apps__search_service".to_owned()),
        }]);
        let completion = json!({
            "id": "c1",
            "model": "m",
            "choices": [{"index":0,"message":{"role":"assistant","content":null,
                "tool_calls":[{"id":"call_1","type":"function",
                    "function":{"name":"mcp__codex_apps__search_service___web_run","arguments":"{}"}}]},
                "finish_reason":"tool_calls"}],
        });

        let out = reply_with_namespaces(Dialect::Responses, completion, &namespaces);

        assert_eq!(out["output"][0]["name"], "_web_run", "{out}");
        assert_eq!(
            out["output"][0]["namespace"], "mcp__codex_apps__search_service",
            "{out}"
        );
    }

    /// A request that declared no namespace gets the reply it always got.
    #[test]
    fn a_whole_reply_should_leave_an_undeclared_tool_name_alone() {
        let completion = json!({
            "id": "c1",
            "model": "m",
            "choices": [{"index":0,"message":{"role":"assistant","content":null,
                "tool_calls":[{"id":"call_1","type":"function",
                    "function":{"name":"exec_command","arguments":"{}"}}]},
                "finish_reason":"tool_calls"}],
        });

        let out = reply_with_namespaces(
            Dialect::Responses,
            completion,
            &ToolNamespaces::default(),
        );

        assert_eq!(out["output"][0]["name"], "exec_command", "{out}");
        assert!(out["output"][0].get("namespace").is_none(), "{out}");
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

    #[test]
    fn responses_stream_should_render_reasoning_as_summary_events() {
        // The gap this closes: Codex's TUI shows thinking only from
        // `response.reasoning_summary_text.delta` events inside a `reasoning`
        // output item. A responses stream that carried none showed no thinking
        // at all, even though the upstream's chat stream carried
        // `reasoning_content` on every delta.
        let out = frames(
            Dialect::Responses,
            &[
                json!({"id":"r1","model":"m","choices":[{"delta":{"role":"assistant","reasoning_content":"thinking "}}]}),
                json!({"choices":[{"delta":{"reasoning_content":"hard"}}]}),
                json!({"choices":[{"delta":{"content":"the answer"}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}),
            ],
        );

        let types: Vec<&str> = out
            .iter()
            .filter_map(|f| f.get("type").and_then(Value::as_str))
            .collect();
        assert!(
            types.contains(&"response.output_item.added"),
            "a reasoning item must open: {types:?}"
        );
        assert!(
            types.contains(&"response.reasoning_summary_part.added"),
            "the summary part must open: {types:?}"
        );
        assert!(
            types
                .iter()
                .filter(|t| **t == "response.reasoning_summary_text.delta")
                .count()
                == 2,
            "one summary delta per reasoning_content delta: {types:?}"
        );
        assert!(
            types.contains(&"response.reasoning_summary_text.done"),
            "the summary text must close: {types:?}"
        );
        assert!(
            types.contains(&"response.completed"),
            "the stream must complete: {types:?}"
        );

        // The reasoning item opens as type "reasoning" and closes carrying the
        // accumulated summary.
        let added = out
            .iter()
            .find(|f| f.get("type").and_then(Value::as_str) == Some("response.output_item.added"))
            .and_then(|f| f.get("item"))
            .unwrap_or(&Value::Null);
        assert_eq!(added.get("type").and_then(Value::as_str), Some("reasoning"), "{added:?}");

        let completed = out
            .iter()
            .find(|f| f.get("type").and_then(Value::as_str) == Some("response.completed"))
            .and_then(|f| f.get("response"))
            .cloned()
            .unwrap_or(Value::Null);
        let empty = Vec::new();
        let output = completed.get("output").and_then(Value::as_array).unwrap_or(&empty);
        let reasoning_item = output
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"));
        let summary_text = reasoning_item
            .and_then(|i| i.pointer("/summary/0/text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(summary_text, "thinking hard", "the whole summary must be in the item: {output:?}");

        // The answer text still follows, as its own message item.
        let message = output
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("message"));
        assert!(
            message
                .and_then(|m| m.pointer("/content/0/text"))
                .and_then(Value::as_str)
                .is_some_and(|t| t.contains("the answer")),
            "the answer must still be there: {output:?}"
        );
    }

    #[test]
    fn responses_whole_reply_should_carry_a_reasoning_item() {
        // The non-streaming path: `openai_reply_to_responses` builds the whole
        // response at once, and it must include the reasoning item too, or a
        // non-streaming Codex turn shows no thinking either.
        let reply = json!({
            "id": "r1",
            "model": "m",
            "created": 1,
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "the answer",
                    "reasoning_content": "thinking hard",
                },
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
        });
        let out = openai_reply_to_responses(&reply);
        let empty = Vec::new();
        let output = out.get("output").and_then(Value::as_array).unwrap_or(&empty);
        let reasoning = output
            .iter()
            .find(|i| i.get("type").and_then(Value::as_str) == Some("reasoning"));
        assert!(
            reasoning
                .and_then(|i| i.pointer("/summary/0/text"))
                .and_then(Value::as_str)
                == Some("thinking hard"),
            "the whole reply must carry the reasoning item: {out:?}"
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

        let got: Vec<Bytes> = stream(
            upstream,
            Dialect::Anthropic,
            "m",
            WireFormat::Openai,
            &ToolNamespaces::default(),
        )
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

        let got: Vec<Bytes> = stream(
            upstream,
            Dialect::Anthropic,
            "m",
            WireFormat::Openai,
            &ToolNamespaces::default(),
        )
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

        let got: Vec<Bytes> = stream(
            upstream,
            Dialect::Responses,
            "m",
            WireFormat::Anthropic,
            &ToolNamespaces::default(),
        )
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
            &ToolNamespaces::default(),
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

        let got: Vec<Bytes> = stream(
            upstream,
            Dialect::OpenAi,
            "m",
            WireFormat::Gemini,
            &ToolNamespaces::default(),
        )
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
