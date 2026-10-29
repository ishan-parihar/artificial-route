//! AWS Bedrock's Converse dialect, both directions.
//!
//! The request is `POST {base}/model/{id}/converse` (or `converse-stream`) with
//! the model in the path, so the body carries no `modelId`. Authentication is a
//! Bedrock API key sent as `Authorization: Bearer`, which the reference's
//! `httpBearerAuth` scheme produces, so no SigV4 signing is involved.
//!
//! The reply is translated back into the OpenAI chat shape here, because the
//! route layer relays an upstream body in its own dialect and an OpenAI client
//! cannot read a Converse `output`.
//!
//! # Deliberate simplifications
//!
//! * An image given as an `https` URL is dropped. Converse takes image bytes, not
//!   a URL, and fetching one would be a network call the renderer must not make.
//!   `ponytail:` fetch and inline it if a catalog model needs remote images.
//! * `tool_choice: none` leaves `toolChoice` unset. Converse has no "forbid
//!   tools" state, so the model keeps its default choice.

use serde_json::{Map, Value, json};

use crate::canonical::{CanonicalChat, MediaPart, Role, ToolCall, ToolChoice};

/// Renders `chat` as a Converse request body.
///
/// Turns of the same role are merged into one message, because Converse expects
/// the user and assistant turns to alternate and a tool result is a user turn.
/// A system or developer turn leaves the message array for the top-level
/// `system` block, the only instruction channel Bedrock Converse exposes.
#[must_use]
pub fn render_bedrock_body(chat: &CanonicalChat) -> Vec<u8> {
    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<(&'static str, Vec<Value>)> = Vec::with_capacity(chat.messages.len());

    for msg in &chat.messages {
        if matches!(msg.role, Role::System | Role::Developer) {
            if !msg.content.is_empty() {
                system.push(json!({ "text": msg.content }));
            }
            continue;
        }
        let role = if msg.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let mut blocks: Vec<Value> = Vec::with_capacity(msg.tool_calls.len() + msg.media.len() + 1);
        match msg.tool_call_id.as_deref() {
            Some(id) if msg.role == Role::Tool => blocks.push(json!({
                "toolResult": {
                    "toolUseId": id,
                    "content": [{ "text": non_empty(&msg.content) }],
                    "status": "success",
                }
            })),
            _ if !msg.content.is_empty() => blocks.push(json!({ "text": msg.content })),
            _ => {}
        }
        blocks.extend(msg.media.iter().filter_map(bedrock_image));
        blocks.extend(msg.tool_calls.iter().map(bedrock_tool_use));
        if blocks.is_empty() {
            // Converse rejects an empty content array.
            blocks.push(json!({ "text": " " }));
        }
        match messages.last_mut() {
            Some((last, content)) if *last == role => content.append(&mut blocks),
            _ => messages.push((role, blocks)),
        }
    }

    if messages.is_empty() {
        messages.push(("user", vec![json!({ "text": "." })]));
    }
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect();

    let mut body = Map::with_capacity(4);
    body.insert("messages".to_owned(), Value::Array(messages));
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::Array(system));
    }

    let mut inference = Map::with_capacity(2);
    if let Some(max_tokens) = chat.max_tokens {
        inference.insert("maxTokens".to_owned(), max_tokens.into());
    }
    if let Some(temperature) = chat.temperature {
        inference.insert("temperature".to_owned(), temperature.into());
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".to_owned(), Value::Object(inference));
    }

    if !chat.tools.is_empty() {
        let tools: Vec<Value> = chat.tools.iter().map(bedrock_tool).collect();
        let mut config = Map::with_capacity(2);
        config.insert("tools".to_owned(), Value::Array(tools));
        if let Some(choice) = chat.tool_choice.as_ref().and_then(bedrock_tool_choice) {
            config.insert("toolChoice".to_owned(), choice);
        }
        body.insert("toolConfig".to_owned(), Value::Object(config));
    }

    serde_json::to_vec(&Value::Object(body)).unwrap_or_default()
}

/// A text value Converse accepts: an empty tool result is a 400.
fn non_empty(text: &str) -> &str {
    if text.is_empty() { " " } else { text }
}

/// The `toolUse` block answering a call the assistant made. `input` must be an
/// object on this wire, so a call whose arguments are not one is sent empty.
fn bedrock_tool_use(call: &ToolCall) -> Value {
    let input = if call.arguments.is_object() {
        call.arguments.clone()
    } else {
        json!({})
    };
    json!({ "toolUse": { "toolUseId": call.id, "name": call.name, "input": input } })
}

fn bedrock_tool(tool: &crate::canonical::CanonicalTool) -> Value {
    let mut spec = Map::with_capacity(3);
    spec.insert("name".to_owned(), tool.name.as_str().into());
    if let Some(description) = &tool.description {
        spec.insert("description".to_owned(), description.as_str().into());
    }
    spec.insert("inputSchema".to_owned(), json!({ "json": tool.parameters }));
    json!({ "toolSpec": Value::Object(spec) })
}

fn bedrock_tool_choice(choice: &ToolChoice) -> Option<Value> {
    match choice {
        ToolChoice::Auto => Some(json!({ "auto": {} })),
        ToolChoice::Required => Some(json!({ "any": {} })),
        ToolChoice::Named(name) => Some(json!({ "tool": { "name": name } })),
        ToolChoice::None => None,
    }
}

/// A `data:image/...;base64,` URL as a Converse `image` block. Anything else is
/// dropped; see the module docs.
fn bedrock_image(part: &MediaPart) -> Option<Value> {
    if part.kind != "image_url" {
        return None;
    }
    let url = part.body.get("image_url")?.get("url")?.as_str()?;
    let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    let format = match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    };
    Some(json!({ "image": { "format": format, "source": { "bytes": data } } }))
}

/// Maps a Converse `stopReason` onto the OpenAI `finish_reason` it answers.
fn finish_reason(stop: &str) -> &'static str {
    match stop {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "guardrail_intervened" | "content_filtered" => "content_filter",
        _ => "stop",
    }
}

fn usage(usage: &Value) -> Value {
    let input = usage
        .get("inputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("outputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage
        .get("totalTokens")
        .and_then(Value::as_u64)
        .unwrap_or(input + output);
    json!({ "prompt_tokens": input, "completion_tokens": output, "total_tokens": total })
}

/// Renders a non-streamed Converse reply as an OpenAI chat completion.
///
/// `None` when the body has no `output.message`, so an error object relays
/// unchanged rather than being rendered as an empty answer.
#[must_use]
pub fn bedrock_response_to_openai(body: &Value, model: &str) -> Option<Value> {
    let blocks = body.pointer("/output/message/content")?.as_array()?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in blocks {
        if let Some(t) = block.get("text").and_then(Value::as_str) {
            text.push_str(t);
        }
        if let Some(r) = block
            .pointer("/reasoningContent/reasoningText/text")
            .and_then(Value::as_str)
        {
            reasoning.push_str(r);
        }
        if let Some(call) = block.get("toolUse") {
            tool_calls.push(json!({
                "id": call.get("toolUseId").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": call.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": call.get("input").map_or_else(|| "{}".to_owned(), Value::to_string),
                },
            }));
        }
    }

    let mut message = Map::with_capacity(4);
    message.insert("role".to_owned(), "assistant".into());
    if tool_calls.is_empty() {
        message.insert("content".to_owned(), text.into());
    } else {
        // An OpenAI tool-call turn carries `null` content when it has no text.
        message.insert(
            "content".to_owned(),
            if text.is_empty() {
                Value::Null
            } else {
                text.into()
            },
        );
        message.insert("tool_calls".to_owned(), Value::Array(tool_calls));
    }
    if !reasoning.is_empty() {
        message.insert("reasoning_content".to_owned(), reasoning.into());
    }

    let stop = body
        .get("stopReason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    Some(json!({
        "id": format!("chatcmpl-bedrock-{model}"),
        "object": "chat.completion",
        "created": now_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason(stop),
        }],
        "usage": body.get("usage").map_or_else(|| usage(&Value::Null), usage),
    }))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Turns Converse stream events into OpenAI `chat.completion.chunk` values.
///
/// Stateful because a tool call's arguments arrive as fragments keyed by content
/// block, and the OpenAI stream wants them keyed by tool index. The caller feeds
/// each event by its `:event-type` name and collects the chunks it returns.
#[derive(Debug)]
pub struct BedrockStream {
    model: String,
    created: u64,
    /// Content block index → OpenAI tool index.
    tool_index: std::collections::HashMap<u64, usize>,
    next_tool: usize,
    finish: Option<&'static str>,
    usage: Option<Value>,
}

impl BedrockStream {
    /// A fresh mapper for one reply served as `model`.
    #[must_use]
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            created: now_secs(),
            tool_index: std::collections::HashMap::new(),
            next_tool: 0,
            finish: None,
            usage: None,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&'static str>) -> Value {
        json!({
            "id": format!("chatcmpl-bedrock-{}", self.model),
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    }

    /// The tool index for a content block, allocating one on first sight.
    fn tool_for(&mut self, block: u64) -> usize {
        if let Some(index) = self.tool_index.get(&block) {
            return *index;
        }
        let index = self.next_tool;
        self.next_tool += 1;
        self.tool_index.insert(block, index);
        index
    }

    /// Feeds one event and returns the chunks it produces, possibly none.
    pub fn event(&mut self, name: &str, payload: &Value) -> Vec<Value> {
        let block = payload
            .get("contentBlockIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        match name {
            "messageStart" => vec![self.chunk(json!({ "role": "assistant" }), None)],
            "contentBlockStart" => match payload.pointer("/start/toolUse") {
                Some(call) => {
                    let index = self.tool_for(block);
                    let id = call.get("toolUseId").cloned().unwrap_or(Value::Null);
                    let name = call.get("name").cloned().unwrap_or(Value::Null);
                    vec![self.chunk(
                        json!({ "tool_calls": [{
                            "index": index, "id": id, "type": "function",
                            "function": { "name": name, "arguments": "" },
                        }] }),
                        None,
                    )]
                }
                None => Vec::new(),
            },
            "contentBlockDelta" => self.delta(block, payload.get("delta")),
            "messageStop" => {
                if let Some(stop) = payload.get("stopReason").and_then(Value::as_str) {
                    self.finish = Some(finish_reason(stop));
                }
                Vec::new()
            }
            "metadata" => {
                if let Some(u) = payload.get("usage") {
                    self.usage = Some(usage(u));
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn delta(&mut self, block: u64, delta: Option<&Value>) -> Vec<Value> {
        let Some(delta) = delta else {
            return Vec::new();
        };
        if let Some(text) = delta
            .get("text")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            return vec![self.chunk(json!({ "content": text }), None)];
        }
        if let Some(text) = delta
            .pointer("/reasoningContent/text")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            return vec![self.chunk(json!({ "reasoning_content": text }), None)];
        }
        if let Some(fragment) = delta.pointer("/toolUse/input").and_then(Value::as_str) {
            let index = self.tool_for(block);
            return vec![self.chunk(
                json!({ "tool_calls": [{ "index": index, "function": { "arguments": fragment } }] }),
                None,
            )];
        }
        Vec::new()
    }

    /// The closing chunk, carrying the finish reason and usage.
    ///
    /// `None` when no `messageStop` arrived. A clean end of body without one is a
    /// truncated reply, and giving it a finish reason would make the cut look
    /// like a complete answer.
    pub fn finish(&mut self) -> Option<Value> {
        let reason = self.finish?;
        let mut chunk = self.chunk(json!({}), Some(reason));
        if let Some(usage) = self.usage.take() {
            chunk["usage"] = usage;
        }
        Some(chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{CanonicalTool, Msg};

    fn chat(messages: Vec<Msg>) -> CanonicalChat {
        CanonicalChat {
            model: "ignored".to_owned(),
            messages,
            temperature: None,
            max_tokens: None,
            stream: false,
            tools: Vec::new(),
            tool_choice: None,
        }
    }

    fn rendered(chat: &CanonicalChat) -> Value {
        serde_json::from_slice(&render_bedrock_body(chat)).expect("the renderer emits JSON")
    }

    #[test]
    fn render_should_leave_the_model_id_out_of_the_body() {
        let body = rendered(&chat(vec![Msg::new(Role::User, "hi")]));

        assert!(body.get("modelId").is_none(), "{body}");
    }

    #[test]
    fn render_should_move_system_text_out_of_the_message_array() {
        let body = rendered(&chat(vec![
            Msg::new(Role::System, "be terse"),
            Msg::new(Role::User, "hi"),
        ]));

        assert_eq!(body["system"][0]["text"], "be terse", "{body}");
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1), "{body}");
    }

    #[test]
    fn render_should_move_developer_text_into_bedrocks_instruction_channel() {
        let body = rendered(&chat(vec![
            Msg::new(Role::Developer, "always use Rust"),
            Msg::new(Role::User, "hi"),
        ]));

        assert_eq!(body["system"][0]["text"], "always use Rust", "{body}");
    }

    #[test]
    fn render_should_pair_a_tool_call_with_its_result_under_one_id() {
        let mut assistant = Msg::new(Role::Assistant, "");
        assistant.tool_calls = vec![ToolCall {
            id: "tu_1".to_owned(),
            name: "add".to_owned(),
            arguments: json!({ "a": 1 }),
        }];
        let mut result = Msg::new(Role::Tool, "3");
        result.tool_call_id = Some("tu_1".to_owned());
        let body = rendered(&chat(vec![Msg::new(Role::User, "sum"), assistant, result]));

        assert_eq!(
            body["messages"][1]["content"][0]["toolUse"]["toolUseId"], "tu_1",
            "{body}"
        );
        assert_eq!(body["messages"][2]["role"], "user", "{body}");
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"]["toolUseId"], "tu_1",
            "{body}"
        );
    }

    #[test]
    fn render_should_merge_a_run_of_same_role_turns() {
        let mut first = Msg::new(Role::Tool, "1");
        first.tool_call_id = Some("a".to_owned());
        let mut second = Msg::new(Role::Tool, "2");
        second.tool_call_id = Some("b".to_owned());
        let body = rendered(&chat(vec![Msg::new(Role::User, "go"), first, second]));

        // The user turn and both results are all `user`, so they share one message.
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1), "{body}");
        assert_eq!(
            body["messages"][0]["content"].as_array().map(Vec::len),
            Some(3),
            "{body}"
        );
    }

    #[test]
    fn render_should_map_tools_and_their_choice() {
        let mut c = chat(vec![Msg::new(Role::User, "hi")]);
        c.tools = vec![CanonicalTool {
            name: "lookup".to_owned(),
            description: None,
            parameters: json!({ "type": "object" }),
        }];
        c.tool_choice = Some(ToolChoice::Named("lookup".to_owned()));
        let body = rendered(&c);

        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"], "lookup",
            "{body}"
        );
        assert_eq!(
            body["toolConfig"]["toolChoice"]["tool"]["name"], "lookup",
            "{body}"
        );
    }

    #[test]
    fn render_should_send_a_data_url_image_as_bytes_and_drop_a_remote_one() {
        let mut msg = Msg::new(Role::User, "look");
        msg.media = vec![
            MediaPart::new(
                "image_url",
                json!({ "image_url": { "url": "data:image/png;base64,AAAA" } }),
            ),
            MediaPart::new(
                "image_url",
                json!({ "image_url": { "url": "https://x/a.png" } }),
            ),
        ];
        let body = rendered(&chat(vec![msg]));
        let blocks = body["messages"][0]["content"]
            .as_array()
            .expect("content array");

        assert_eq!(blocks.len(), 2, "{body}");
        assert_eq!(blocks[1]["image"]["source"]["bytes"], "AAAA", "{body}");
    }

    #[test]
    fn response_should_map_text_tool_use_and_usage_onto_the_openai_shape() {
        let reply = json!({
            "output": { "message": { "role": "assistant", "content": [
                { "text": "let me check" },
                { "toolUse": { "toolUseId": "tu_9", "name": "lookup", "input": { "q": "x" } } },
            ] } },
            "stopReason": "tool_use",
            "usage": { "inputTokens": 7, "outputTokens": 3, "totalTokens": 10 },
        });
        let out = bedrock_response_to_openai(&reply, "m").expect("a message renders");

        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls", "{out}");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["id"], "tu_9",
            "{out}"
        );
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"], r#"{"q":"x"}"#,
            "{out}"
        );
        assert_eq!(out["usage"]["total_tokens"], 10, "{out}");
    }

    #[test]
    fn response_should_relay_an_error_object_unchanged() {
        assert!(bedrock_response_to_openai(&json!({ "message": "denied" }), "m").is_none());
    }

    #[test]
    fn stream_should_emit_the_role_then_each_text_delta() {
        let mut s = BedrockStream::new("m");
        let role = s.event("messageStart", &json!({ "role": "assistant" }));
        let text = s.event(
            "contentBlockDelta",
            &json!({ "contentBlockIndex": 0, "delta": { "text": "hi" } }),
        );

        assert_eq!(role[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(text[0]["choices"][0]["delta"]["content"], "hi");
    }

    #[test]
    fn stream_should_index_each_tool_call_by_its_content_block() {
        let mut s = BedrockStream::new("m");
        let start = s.event(
            "contentBlockStart",
            &json!({ "contentBlockIndex": 2, "start": { "toolUse": { "toolUseId": "t", "name": "f" } } }),
        );
        let args = s.event(
            "contentBlockDelta",
            &json!({ "contentBlockIndex": 2, "delta": { "toolUse": { "input": "{\"a\"" } } }),
        );

        assert_eq!(start[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(args[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(
            args[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\""
        );
    }

    #[test]
    fn finish_should_be_none_without_a_message_stop() {
        let mut s = BedrockStream::new("m");
        s.event(
            "contentBlockDelta",
            &json!({ "delta": { "text": "partial" } }),
        );

        assert!(s.finish().is_none());
    }

    #[test]
    fn finish_should_carry_the_stop_reason_and_usage_once_the_stop_arrived() {
        let mut s = BedrockStream::new("m");
        s.event("messageStop", &json!({ "stopReason": "max_tokens" }));
        s.event(
            "metadata",
            &json!({ "usage": { "inputTokens": 1, "outputTokens": 2 } }),
        );
        let last = s.finish().expect("a stop was seen");

        assert_eq!(last["choices"][0]["finish_reason"], "length", "{last}");
        assert_eq!(last["usage"]["completion_tokens"], 2, "{last}");
    }
}
