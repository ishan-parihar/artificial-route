//! GitLab Duo's `code_suggestions` dialect, both directions.
//!
//! `code_suggestions` is a single-prompt completion API with no chat roles, so
//! the conversation is flattened into one prompt: the system instructions and
//! the latest user message. The reply carries its text in `choices[0].text`,
//! which is re-spelled as an OpenAI chat completion.
//!
//! # Deliberate simplifications
//!
//! * Only the monolith endpoint is spoken, with a bearer token. The reference's
//!   direct-access exchange and OAuth refresh are not ported, so the connection
//!   takes a pasted token, which is why `auth_kind` is `apikey` in the catalog.
//! * Tool declarations are refused at render time (`ExecError::UnsupportedTools`
//!   in `ar-exec`), not emulated with `<tool>` blocks as the reference does.
//! * A tool-exchange history is flattened to its latest user turn, which is the
//!   reference's simple path. The reference's tool-exchange serialization is
//!   not ported.

use serde_json::{Value, json};

use crate::canonical::{CanonicalChat, Msg, Role};

/// The reference's default file name. Its per-connection project and file settings
/// are not carried, so every request names this file.
const FILE_NAME: &str = "snippet.txt";

/// Renders `chat` as a `code_suggestions` request body. `stream` is always
/// `false`: the upstream only answers whole, and a stream is synthesized from it.
#[must_use]
pub fn render_gitlab_body(chat: &CanonicalChat) -> Vec<u8> {
    let prompt = simple_prompt(&chat.messages);
    let mut body = json!({
        "current_file": {
            "file_name": FILE_NAME,
            "content_above_cursor": prompt,
            "content_below_cursor": "",
        },
        "intent": "generation",
        "generation_type": "small_file",
        "stream": false,
    });
    if !prompt.is_empty() {
        body["user_instruction"] = Value::String(prompt);
    }
    serde_json::to_vec(&body).unwrap_or_default()
}

/// The system instructions followed by the latest user message, as the reference's
/// `buildSimplePrompt` flattens them. Tool turns are not part of the prompt.
fn simple_prompt(messages: &[Msg]) -> String {
    let mut system: Vec<&str> = Vec::new();
    let mut latest_user = "";
    for msg in messages {
        let text = msg.content.trim();
        if text.is_empty() {
            continue;
        }
        match msg.role {
            Role::System => system.push(text),
            Role::User => latest_user = text,
            Role::Assistant | Role::Tool => {}
        }
    }
    if system.is_empty() {
        return latest_user.to_owned();
    }
    format!(
        "System instructions:\n{}\n\n{latest_user}",
        system.join("\n\n")
    )
    .trim()
    .to_owned()
}

/// Renders a `code_suggestions` reply as an OpenAI chat completion.
///
/// `None` when the body carries no text, so an error object relays unchanged.
#[must_use]
pub fn gitlab_response_to_openai(body: &Value, model: &str) -> Option<Value> {
    let text = body
        .pointer("/choices/0/text")
        .and_then(Value::as_str)
        .or_else(|| body.get("content").and_then(Value::as_str))?;
    let served = body.get("model").and_then(Value::as_str).unwrap_or(model);
    let created = now_secs();
    Some(json!({
        "id": format!("chatcmpl-gitlab-{created}"),
        "object": "chat.completion",
        "created": created,
        "model": served,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop",
        }],
    }))
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The chunks that stream a finished completion: the role, the text, and a closing
/// chunk carrying the finish reason. The caller frames them as SSE.
#[must_use]
pub fn completion_chunks(completion: &Value) -> Vec<Value> {
    let id = completion.get("id").cloned().unwrap_or(Value::Null);
    let created = completion.get("created").cloned().unwrap_or(Value::Null);
    let model = completion.get("model").cloned().unwrap_or(Value::Null);
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    };
    let choice = completion.pointer("/choices/0");
    let text = choice
        .and_then(|c| c.pointer("/message/content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .cloned()
        .unwrap_or_else(|| json!("stop"));

    let mut chunks = vec![chunk(json!({ "role": "assistant" }), Value::Null)];
    if !text.is_empty() {
        chunks.push(chunk(json!({ "content": text }), Value::Null));
    }
    chunks.push(chunk(json!({}), finish));
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(messages: Vec<Msg>) -> CanonicalChat {
        CanonicalChat {
            model: "ignored".to_owned(),
            messages,
            temperature: None,
            max_tokens: None,
            stream: true,
            tools: Vec::new(),
            tool_choice: None,
        }
    }

    fn rendered(chat: &CanonicalChat) -> Value {
        serde_json::from_slice(&render_gitlab_body(chat)).expect("the renderer emits JSON")
    }

    #[test]
    fn render_should_flatten_the_system_turn_and_latest_user_message_into_the_prompt() {
        let body = rendered(&chat(vec![
            Msg::new(Role::System, "be terse"),
            Msg::new(Role::User, "first"),
            Msg::new(Role::Assistant, "ok"),
            Msg::new(Role::User, "second"),
        ]));

        assert_eq!(
            body["current_file"]["content_above_cursor"],
            "System instructions:\nbe terse\n\nsecond",
            "{body}"
        );
    }

    #[test]
    fn render_should_always_ask_for_a_whole_reply() {
        let body = rendered(&chat(vec![Msg::new(Role::User, "hi")]));

        assert_eq!(body["stream"], false, "{body}");
        assert_eq!(body["generation_type"], "small_file", "{body}");
    }

    #[test]
    fn response_should_render_the_choice_text_as_a_chat_completion() {
        let reply = json!({ "choices": [{ "text": "hello" }] });
        let out = gitlab_response_to_openai(&reply, "combo").expect("text renders");

        assert_eq!(out["choices"][0]["message"]["content"], "hello", "{out}");
        assert_eq!(out["model"], "combo", "{out}");
    }

    #[test]
    fn response_should_refuse_a_body_with_no_text() {
        assert!(gitlab_response_to_openai(&json!({ "detail": "Validation error" }), "m").is_none());
    }

    #[test]
    fn completion_chunks_should_stream_the_role_text_and_finish_reason() {
        let completion = json!({
            "id": "c", "created": 1, "model": "m",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hi" }, "finish_reason": "stop" }],
        });
        let chunks = completion_chunks(&completion);

        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "hi");
        assert_eq!(chunks[2]["choices"][0]["finish_reason"], "stop");
    }
}
