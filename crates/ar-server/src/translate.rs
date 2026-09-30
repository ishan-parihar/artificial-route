//! Inbound translation: one canonical request out of four wire dialects.
//!
//! The four inbound shapes a client can POST, and where each one lands:
//!
//! | route | dialect | `ar-translate` entry point |
//! |---|---|---|
//! | `POST /v1/chat/completions` | OpenAI chat | `to_canonical(OpenAIChat)` |
//! | `POST /v1/messages` | Anthropic Messages | `anthropic_to_canonical(AnthropicMessages)` |
//! | `POST /v1/responses` | OpenAI Responses | `responses_to_canonical(ResponsesApi)` |
//! | `POST /api/chat` | Ollama chat | `ollama_to_canonical(OllamaChat)` |
//!
//! All four leave as the *same* [`CanonicalRequest`], because that is the shape
//! the router, the cache key and `ar-exec` speak. This crate used to carry its
//! own OpenAI-only `ArTranslate` impl that forwarded bytes verbatim; it now
//! dispatches to `ar-translate` so there is one place a wire dialect is
//! understood, and a non-OpenAI body is never silently POSTed to a provider as
//! if it were OpenAI.
//!
//! # Shape mismatch is a 400, not a passthrough
//!
//! Each dialect is deserialised as *its own* type and converted by its own
//! `ar-translate` function, so a body that is not that dialect fails here with a
//! 400 naming the route rather than being forwarded and producing a 400 from the
//! *provider* — which reads as our fault and sends an operator to the wrong
//! dashboard.
//!
//! Where the shapes genuinely do not overlap, a body is refused outright:
//! `/v1/responses` requires `input`, which no other dialect has, so an
//! OpenAI-chat body posted there is a 400 rather than a translation of a request
//! the client did not make.
//!
//! `/v1/messages` and `/api/chat` share `{model, messages[{role, content}]}` with
//! `/v1/chat/completions` almost exactly — Anthropic's `max_tokens` is optional in
//! this build (`ar-translate` makes it so deliberately), and Ollama's `options` is
//! optional. A body that satisfies both is a valid body for both, and refusing it
//! would be refusing a request we can serve correctly. What those routes do *not*
//! do is passthrough: they go through `anthropic_to_canonical` /
//! `ollama_to_canonical`, so a `system` string is hoisted into a leading system
//! turn and `options.num_predict` becomes `max_tokens`.
//!
//! # Bytes out, not a struct out
//!
//! `CanonicalRequest::body` is bytes, so the OpenAI dialect still forwards the
//! client's exact bytes (no re-serialisation, no dropped `tools` field). The
//! other three dialects *must* re-encode: their whole job is turning a
//! different shape into this one, and `ar_translate::render_openai_body` is the
//! single renderer for that.
//!
//! The OpenAI dialect is therefore the one route that does **not** go through
//! `ar-translate`, and that is a contract difference rather than an oversight —
//! see [`OpenAiChatTranslate`].

use ar_route::CanonicalRequest;
use ar_translate::{
    AnthropicMessages, OllamaChat, ResponsesApi, anthropic_to_canonical, ollama_to_canonical,
    render_openai_body, responses_to_canonical,
};
use bytes::Bytes;
use serde::Deserialize;

/// The minimum every inbound dialect has: a non-empty `model` and a boolean
/// `stream`.
///
/// Read once, up front, so a missing model is the same clear error on all four
/// routes rather than a serde field path the caller has never heard of.
///
/// `deny_unknown_fields` is deliberately **off**: OpenAI clients send `tools`,
/// `logprobs`, `parallel_tool_calls` and a dozen other fields, and rejecting them
/// would break every real client. The whole body is forwarded verbatim below;
/// this struct only extracts what routing needs.
#[derive(Debug, Deserialize)]
struct DialectHead {
    model: String,
    #[serde(default)]
    stream: bool,
}

/// The P0 OpenAI-chat-inbound translator.
///
/// Deliberately *not* `ar_translate::OpenAiInbound`. That adapter normalises a
/// typed `OpenAIChat` — it validates `messages`, drops unmodelled fields and
/// re-encodes. This one extracts two fields and forwards the client's bytes, so a
/// `tools` array the router never looks at reaches the provider intact. A proxy
/// that drops `tools` is worse than one that forwards a field the provider will
/// reject with a clearer error, so the two contracts stay separate.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiChatTranslate;

impl ar_route::ArTranslate for OpenAiChatTranslate {
    /// Normalises an OpenAI chat request.
    ///
    /// # Errors
    /// - body is not valid JSON, or not a JSON object
    /// - `model` is missing, not a string, or empty
    fn to_canonical(&self, inbound: &[u8]) -> Result<CanonicalRequest, String> {
        let head: DialectHead = serde_json::from_slice(inbound)
            .map_err(|e| format!("invalid chat completions body: {e}"))?;
        if head.model.is_empty() {
            return Err("`model` must be a non-empty string".to_owned());
        }
        Ok(
            CanonicalRequest::new(head.model, Bytes::copy_from_slice(inbound))
                .with_stream(head.stream),
        )
    }
}

/// Turns an inbound body into the canonical request the router routes.
///
/// `route` is the path the client posted to, and it selects the dialect. The
/// route is the authority, not the body: sniffing the shape instead would mean a
/// malformed Anthropic body posted to `/v1/messages` silently becomes an OpenAI
/// request, which is the failure this module exists to prevent.
///
/// # Errors
///
/// A client-facing message naming the route and what it expected. Every failure
/// here is a 400: the caller sent something this route cannot represent, and
/// retrying the same bytes anywhere produces the same answer.
pub fn to_canonical_for_route(route: &str, inbound: &[u8]) -> Result<CanonicalRequest, String> {
    let head: DialectHead = serde_json::from_slice(inbound)
        .map_err(|e| format!("{route} expects a JSON object with `model`: {e}"))?;
    if head.model.trim().is_empty() {
        return Err(format!("`model` must be a non-empty string for {route}"));
    }

    match route {
        "/v1/chat/completions" => Ok(
            CanonicalRequest::new(head.model, Bytes::copy_from_slice(inbound))
                .with_stream(head.stream),
        ),
        "/v1/messages" => typed(inbound, |raw| {
            let req: AnthropicMessages = serde_json::from_slice(raw)
                .map_err(|e| format!("/v1/messages is not an Anthropic Messages body: {e}"))?;
            anthropic_to_canonical(req).map_err(|e| e.to_string())
        }),
        "/v1/responses" => {
            // `input` is required by `ResponsesApi` and by no other dialect, so a
            // body without it is one of the others posted to the wrong route.
            require_field(route, inbound, "input")?;
            typed(inbound, |raw| {
                let req: ResponsesApi = serde_json::from_slice(raw)
                    .map_err(|e| format!("/v1/responses is not a Responses body: {e}"))?;
                responses_to_canonical(req).map_err(|e| e.to_string())
            })
        }
        "/api/chat" => typed(inbound, |raw| {
            let req: OllamaChat = serde_json::from_slice(raw)
                .map_err(|e| format!("/api/chat is not an Ollama chat body: {e}"))?;
            ollama_to_canonical(req).map_err(|e| e.to_string())
        }),
        other => Err(format!("no inbound dialect is registered for {other}")),
    }
}

/// Refuses a body that lacks the one field its route's dialect requires.
fn require_field(route: &str, inbound: &[u8], field: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_slice(inbound)
        .map_err(|e| format!("{route} expects a JSON object: {e}"))?;
    if value.get(field).is_some() {
        return Ok(());
    }
    Err(format!(
        "{route} requires `{field}`, which no other inbound dialect has; this looks like a body posted to the wrong route"
    ))
}

/// Runs one non-OpenAI dialect and renders its canonical chat back to OpenAI
/// bytes.
///
/// `render` owns the deserialise-and-convert half so each call site above is one
/// line; this owns the half they share, which is the re-encode. Re-encoding is
/// mandatory for these three — the client sent a different shape and the upstream
/// only reads this one.
fn typed<F>(inbound: &[u8], render: F) -> Result<CanonicalRequest, String>
where
    F: FnOnce(&[u8]) -> Result<ar_translate::CanonicalChat, String>,
{
    let chat = render(inbound)?;
    // Render before the move: `chat.model` is consumed by `CanonicalRequest::new`
    // and the renderer still needs the whole struct.
    let body = Bytes::from(render_openai_body(&chat));
    Ok(CanonicalRequest::new(chat.model, body).with_stream(chat.stream))
}

#[cfg(test)]
mod tests {
    use ar_route::ArTranslate;

    use super::{OpenAiChatTranslate, to_canonical_for_route};

    #[test]
    fn extracts_model_and_stream_flag() {
        let got = OpenAiChatTranslate
            .to_canonical(br#"{"model":"gpt-4o","stream":true,"messages":[]}"#)
            .expect("valid body");
        assert_eq!((got.model.as_ref(), got.stream), ("gpt-4o", true));
    }

    #[test]
    fn forwards_unknown_fields_verbatim() {
        let raw = br#"{"model":"m","tools":[{"type":"function"}],"parallel_tool_calls":true,"messages":[]}"#;
        let got = OpenAiChatTranslate.to_canonical(raw).expect("valid body");
        assert_eq!(got.body, bytes::Bytes::from_static(raw));
    }

    #[test]
    fn rejects_body_without_model() {
        let got = OpenAiChatTranslate.to_canonical(br#"{"messages":[]}"#);
        assert!(got.is_err());
    }

    #[test]
    fn rejects_malformed_json() {
        let got = OpenAiChatTranslate.to_canonical(b"{not json");
        assert!(got.is_err());
    }

    #[test]
    fn rejects_empty_model_string() {
        let got = OpenAiChatTranslate.to_canonical(br#"{"model":""}"#);
        assert!(got.is_err());
    }

    #[test]
    fn accepts_an_anthropic_messages_body_on_its_own_route() {
        let raw = br#"{"model":"claude-sonnet-4-5","max_tokens":64,"system":"be terse",
            "messages":[{"role":"user","content":"hi"}]}"#;
        let got = to_canonical_for_route("/v1/messages", raw).expect("anthropic body");
        assert_eq!((got.model.as_ref(), got.stream), ("claude-sonnet-4-5", false));
    }

    #[test]
    fn hoists_an_anthropic_system_prompt_into_the_canonical_body() {
        let raw = br#"{"model":"claude-sonnet-4-5","system":"be terse",
            "messages":[{"role":"user","content":"hi"}]}"#;
        let got = to_canonical_for_route("/v1/messages", raw).expect("anthropic body");
        let v: serde_json::Value =
            serde_json::from_slice(&got.body).expect("canonical body is JSON");
        assert_eq!(v["messages"][0]["role"], serde_json::json!("system"));
        assert_eq!(v["messages"][0]["content"], serde_json::json!("be terse"));
    }

    #[test]
    fn renders_an_anthropic_body_as_openai_chat_bytes() {
        // The upstream only reads OpenAI, so the re-encode is not optional here.
        let raw = br#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}]}"#;
        let got = to_canonical_for_route("/v1/messages", raw).expect("anthropic body");
        let v: serde_json::Value =
            serde_json::from_slice(&got.body).expect("canonical body is JSON");
        assert_eq!(v["messages"][0]["content"], serde_json::json!("hi"));
    }

    #[test]
    fn accepts_a_responses_body_on_its_own_route() {
        let raw = br#"{"model":"gpt-5.4","input":"hi","instructions":"be terse"}"#;
        let got = to_canonical_for_route("/v1/responses", raw).expect("responses body");
        let v: serde_json::Value =
            serde_json::from_slice(&got.body).expect("canonical body is JSON");
        assert_eq!(v["messages"][0]["role"], serde_json::json!("system"));
        assert_eq!(v["messages"][1]["content"], serde_json::json!("hi"));
    }

    #[test]
    fn accepts_an_ollama_body_on_its_own_route() {
        let raw = br#"{"model":"llama3.2","system":"be terse","messages":[{"role":"user","content":"hi"}],
            "options":{"num_predict":16}}"#;
        let got = to_canonical_for_route("/api/chat", raw).expect("ollama body");
        let v: serde_json::Value =
            serde_json::from_slice(&got.body).expect("canonical body is JSON");
        assert_eq!(v["max_tokens"], serde_json::json!(16));
    }

    #[test]
    fn refuses_an_openai_chat_body_posted_to_the_responses_route() {
        // `input` is the one field only Responses has, so this is the case where
        // the routes genuinely do not overlap.
        let raw = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
        let err = to_canonical_for_route("/v1/responses", raw)
            .expect_err("shape mismatch refused");
        assert!(err.contains("input"), "unhelpful error: {err}");
    }

    #[test]
    fn accepts_a_body_that_is_valid_for_both_anthropic_and_chat() {
        // Documented overlap, not a hole: `{model, messages[{role, content}]}` is
        // a legal Anthropic body *and* a legal OpenAI one. Refusing it would be
        // refusing a request this build can serve correctly.
        let raw = br#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}]}"#;
        let got = to_canonical_for_route("/v1/messages", raw).expect("valid anthropic body");
        let v: serde_json::Value =
            serde_json::from_slice(&got.body).expect("canonical body is JSON");
        assert_eq!(v["messages"][0]["content"], serde_json::json!("hi"));
    }

    #[test]
    fn converts_an_ollama_body_rather_than_passing_it_through() {
        // The overlapping shape still goes through `ollama_to_canonical`, so the
        // upstream sees OpenAI chat and not an Ollama body.
        let raw = br#"{"model":"llama3.2","messages":[{"role":"user","content":"hi"}]}"#;
        let got = to_canonical_for_route("/api/chat", raw).expect("valid ollama body");
        let v: serde_json::Value =
            serde_json::from_slice(&got.body).expect("canonical body is JSON");
        assert!(v["messages"].is_array(), "no messages array: {v}");
        assert!(v.get("options").is_none(), "ollama options leaked: {v}");
    }

    #[test]
    fn refuses_a_body_with_no_model_on_every_route() {
        for route in ["/v1/chat/completions", "/v1/messages", "/v1/responses", "/api/chat"] {
            let err = to_canonical_for_route(route, br#"{"messages":[]}"#)
                .expect_err("missing model refused");
            assert!(err.contains("model"), "{route}: unhelpful error: {err}");
        }
    }

    #[test]
    fn names_the_route_it_refused() {
        let err = to_canonical_for_route("/api/chat", b"{not json")
            .expect_err("malformed refused");
        assert!(err.contains("/api/chat"), "unhelpful error: {err}");
    }

    #[test]
    fn rejects_a_route_with_no_registered_dialect() {
        let err = to_canonical_for_route("/v1/nope", br#"{"model":"m"}"#)
            .expect_err("unknown route refused");
        assert!(err.contains("/v1/nope"), "unhelpful error: {err}");
    }
}
