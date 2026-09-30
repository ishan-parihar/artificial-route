//! Translator goldens for both P0 dialect pairs.
//!
//! `docs/06` mandates `insta` goldens for translator pairs because a wire-shape
//! regression here is silent: `serde` accepts the old shape, so only a snapshot
//! catches that a field stopped round-tripping.

use ar_translate::{
    AnthropicMessages, CanonicalChat, CanonicalResponse, FinishReason, GeminiChat, Msg, OllamaChat,
    OpenAIChat, ResponsesApi, Role, anthropic_to_canonical, gemini_to_canonical, ollama_to_canonical,
    render_openai_body, responses_to_canonical, to_canonical, to_openai, Usage,
};

/// Pair 1: OpenAI chat request -> canonical.
#[test]
fn openai_chat_to_canonical_when_text_only() {
    let raw = r#"{
        "model": "gpt-5.4",
        "messages": [
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": [{"type": "text", "text": "hello"}, {"type": "text", "text": "again"}]},
            {"role": "assistant", "content": "hi"},
            {"role": "tool", "content": "42", "tool_call_id": "call_1"}
        ],
        "temperature": 0.2,
        "max_tokens": 256,
        "stream": true
    }"#;

    let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");
    let canonical: CanonicalChat = to_canonical(chat).expect("fixture is translatable");

    insta::assert_debug_snapshot!("openai_chat_to_canonical", canonical);
}

/// Pair 2: canonical response -> OpenAI JSON.
#[test]
fn canonical_response_to_openai_when_complete() {
    let response = CanonicalResponse {
        id: "chatcmpl-abc".to_owned(),
        model: "gpt-5.4".to_owned(),
        created: 1_700_000_000,
        message: Msg::new(Role::Assistant, "hello"),
        finish_reason: FinishReason::Stop,
        usage: Usage {
            prompt_tokens: 11,
            completion_tokens: 3,
            total_tokens: 14,
        },
    };

    insta::assert_json_snapshot!("canonical_response_to_openai", to_openai(&response));
}

/// A non-text content part is carried verbatim rather than flattened into the
/// turn's text. The golden exists because the flattening is exactly the
/// corruption a snapshot catches: `serde` accepts both shapes, so only the
/// stored value shows whether the payload survived.
#[test]
fn carries_image_part_when_modality_routable() {
    let raw = r#"{"model":"m","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"http://x"}}]}]}"#;
    let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

    let canonical = to_canonical(chat).expect("image parts are routable since P6");
    insta::assert_debug_snapshot!("carries_image_part", canonical);
}

/// An unrecognised part is still rejected. Routable is not the same as known,
/// and dropping it silently is the failure this whole path exists to prevent.
#[test]
fn rejects_part_when_modality_unknown() {
    let raw = r#"{"model":"m","messages":[{"role":"user","content":[{"type":"realtime_audio","audio":"x"}]}]}"#;
    let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

    let err = to_canonical(chat).expect_err("an unrouted part is not translatable");
    insta::assert_debug_snapshot!("rejects_part_when_modality_unknown", err);
}

/// `max_completion_tokens` is the current spelling; both map to canonical
/// `max_tokens` with `max_tokens` winning when a client sets both.
#[test]
fn prefers_max_tokens_when_both_spellings_present() {
    let raw = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],
                 "max_tokens":100,"max_completion_tokens":200}"#;
    let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

    let canonical = to_canonical(chat).expect("fixture is translatable");
    assert_eq!(canonical.max_tokens, Some(100));
}

/// P2 wire 1: Anthropic Messages request -> canonical.
///
/// The `system` field lives outside the message array on this wire, so the
/// golden's first turn is one the adapter synthesised. `tool_result` blocks
/// become their own tool turns, emitted before the parent turn that carried
/// them.
#[test]
fn anthropic_messages_to_canonical_when_text_only() {
    let raw = r#"{
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "system": [
            {"type": "text", "text": "be terse"},
            {"type": "text", "text": "and cite sources", "cache_control": {"type": "ephemeral"}}
        ],
        "messages": [
            {"role": "user", "content": "add 2 and 3"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "calling"},
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "5"}
            ]},
            {"role": "user", "content": "thanks"}
        ],
        "temperature": 0.2,
        "stop_sequences": ["</done>"],
        "stream": true
    }"#;

    let req: AnthropicMessages =
        serde_json::from_str(raw).expect("fixture is valid Anthropic Messages");
    let canonical: CanonicalChat = anthropic_to_canonical(req).expect("fixture is translatable");

    insta::assert_debug_snapshot!("anthropic_messages_to_canonical", canonical);
}

/// P2 wire 2: Responses request -> canonical.
///
/// `instructions` is the Responses spelling of a system prompt, and `input`
/// items carry `input_text` parts rather than a bare string.
#[test]
fn responses_to_canonical_when_text_only() {
    let raw = r#"{
        "model": "gpt-5.4",
        "instructions": "be terse",
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "message", "role": "agent_message",
             "content": [{"type": "output_text", "text": "hi"}]},
            {"type": "message", "role": "user", "content": "again"}
        ],
        "max_output_tokens": 256,
        "temperature": 0.1,
        "stream": true
    }"#;

    let req: ResponsesApi = serde_json::from_str(raw).expect("fixture is valid Responses");
    let canonical: CanonicalChat = responses_to_canonical(req).expect("fixture is translatable");

    insta::assert_debug_snapshot!("responses_to_canonical", canonical);
}

/// P2 wire 3: Ollama chat request -> canonical.
///
/// Sampling options are nested under `options` here, and the output ceiling is
/// `num_predict` rather than `max_tokens`.
#[test]
fn ollama_to_canonical_when_text_only() {
    let raw = r#"{
        "model": "llama3.2",
        "system": "be terse",
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"}
        ],
        "options": {"temperature": 0.4, "top_p": 0.9, "num_predict": 128},
        "keep_alive": "5m",
        "stream": true
    }"#;

    let req: OllamaChat = serde_json::from_str(raw).expect("fixture is valid Ollama chat");
    let canonical: CanonicalChat = ollama_to_canonical(req).expect("fixture is translatable");

    insta::assert_debug_snapshot!("ollama_to_canonical", canonical);
}

/// Vendor keys survive the round trip, which is why `rest` exists.
#[test]
fn preserves_unknown_fields_when_translating() {
    let raw = r#"{"model":"m","messages":[{"role":"user","content":"hi","name":"ada"}],
                 "top_p":0.9,"vendor_flag":true}"#;
    let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

    let canonical = to_canonical(chat).expect("fixture is translatable");
    assert_eq!(canonical.messages[0].content, "hi");
}

/// Wire 5: Gemini `generateContent` request -> canonical.
///
/// The model id is not in the body on this wire — it is the URL path — so the
/// fixture carries `model` as the field the server fills in from
/// `models/{model}`. Three shapes are Gemini's own and are what the golden exists
/// to pin: `systemInstruction` outside `contents`, a `model` role instead of
/// `assistant`, and a tool result that is a `functionResponse` *part* rather than
/// a turn of its own.
#[test]
fn gemini_to_canonical_when_text_only() {
    let raw = r#"{
        "model": "gemini-2.5-flash",
        "systemInstruction": {"parts": [{"text": "be terse"}]},
        "contents": [
            {"role": "user", "parts": [{"text": "add 2 and 3"}]},
            {"role": "model", "parts": [
                {"text": "calling"},
                {"functionResponse": {"name": "add", "response": {"result": 5}}}
            ]},
            {"role": "user", "parts": [{"text": "thanks"}]}
        ],
        "generationConfig": {"temperature": 0.3, "maxOutputTokens": 512, "topP": 0.9}
    }"#;

    let req: GeminiChat = serde_json::from_str(raw).expect("fixture is valid Gemini");
    let canonical = gemini_to_canonical(req).expect("fixture is translatable");

    insta::assert_debug_snapshot!("gemini_to_canonical", canonical);
}

/// canonical -> OpenAI request body.
///
/// The golden pins the two things the OpenAI wire does not forgive: absent
/// optional fields are *omitted* rather than written as `null`, and media lands
/// in `content` as typed parts rather than beside it under a key no provider
/// reads. `temperature`/`max_tokens` are set here so the snapshot shows both
/// halves — present when set, and the second golden shows them absent.
#[test]
fn canonical_chat_to_openai_body_when_sampling_set() {
    let raw = r#"{"model":"gpt-5.4","messages":[
        {"role":"system","content":"be terse"},
        {"role":"user","content":[
            {"type":"text","text":"what is this?"},
            {"type":"image_url","image_url":{"url":"http://x/i.png"}}]},
        {"role":"tool","content":"42","tool_call_id":"call_1"}],
        "temperature":0.2,"max_tokens":256,"stream":true}"#;
    let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");
    let canonical = to_canonical(chat).expect("fixture is translatable");

    let body = String::from_utf8(render_openai_body(&canonical))
        .expect("a rendered body is UTF-8");

    insta::assert_json_snapshot!("canonical_chat_to_openai_body", body.parse::<serde_json::Value>().expect("the body is JSON"));
}

/// The same body with no sampling set. `serde` accepts `"temperature": null`, and
/// several upstreams reject it — so the snapshot must show the keys *absent*.
#[test]
fn canonical_chat_to_openai_body_omits_absent_sampling() {
    let canonical = CanonicalChat {
        model: "gpt-5.4".to_owned(),
        messages: vec![Msg::new(Role::User, "hi")],
        temperature: None,
        max_tokens: None,
        stream: false,
    };

    let body = String::from_utf8(render_openai_body(&canonical))
        .expect("a rendered body is UTF-8");

    assert!(!body.contains("null"), "an absent field must be absent: {body}");
}

/// canonical -> OpenAI -> canonical, with a media part in the middle.
///
/// The round trip is what proves the render/parse pair agrees: an image that
/// survives canonical and then re-parses as the *same* part means the body the
/// upstream sees carries the client's bytes, not an approximation of them.
#[test]
fn round_trips_canonical_through_openai_when_media_carried() {
    let raw = r#"{"model":"gpt-5.4","messages":[{"role":"user","content":[
        {"type":"text","text":"what is this?"},
        {"type":"image_url","image_url":{"url":"http://x/i.png"}}]}]}"#;
    let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");
    let canonical = to_canonical(chat).expect("fixture is translatable");

    let body = String::from_utf8(render_openai_body(&canonical))
        .expect("a rendered body is UTF-8");
    let reparsed: OpenAIChat = serde_json::from_str(&body).expect("the body parses as OpenAI chat");
    let round_tripped = to_canonical(reparsed).expect("the reparsed body is translatable");

    insta::assert_debug_snapshot!("canonical_openai_round_trip_with_media", round_tripped);
}
