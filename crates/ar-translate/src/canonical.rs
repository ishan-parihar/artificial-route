//! Canonical, provider-neutral shapes.
//!
//! `docs/02` makes this the only place a provider wire format is understood:
//! inbound dialects land in [`CanonicalChat`], and outbound dialects are
//! rendered from it by [`crate::render_for_wire`]. Everything downstream
//! (`ar-exec`, `ar-route`, the cache key in `ar-cache`) speaks these types and
//! never a wire format.
//!
//! # Why these exist at all
//!
//! `ar-llm` has no bidirectional intermediate representation: its
//! `NormalizedMessage` is `Serialize`-only and documented as "an
//! observability representation, not a lossless wire format", and its
//! cross-provider path goes through `serde_json::Value`. So a canonical type
//! has to be defined here rather than reused. Keeping it *small* is the point:
//! a canonical type that could express a modality no adapter implements would
//! be a claim without an implementation.
//!
//! # Text and media
//!
//! Since P6 a turn carries two views, and the split is deliberate:
//! [`Msg::content`] is the joined text, for the text-only callers that read
//! nothing else; [`Msg::media`] holds every non-text part, verbatim. Media is
//! **not** re-encoded into a typed canonical variant because the upstream wire
//! shapes differ per dialect (`image_url` object vs. base64 `images` array vs.
//! Anthropic `source.base64`), and a lossy middle would either drop a field or
//! invent one. The part crosses this boundary unchanged and the media adapter
//! forwards it, which is also why a part type this build cannot route still
//! errors instead of vanishing.

use serde::{Deserialize, Serialize};

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Instructions that frame the conversation.
    System,
    /// The end user.
    User,
    /// The model.
    Assistant,
    /// The output of a tool call, correlated by `tool_call_id`.
    Tool,
}

impl Role {
    /// The wire spelling of this role.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    /// Parses a wire role, returning `None` for one no provider accepts.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "system" => Some(Self::System),
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// One canonical conversation turn.
///
/// Text lives in `content`; non-text parts live in `media` and are never folded
/// into it. The pair is not redundant: `content` is what a text-only caller
/// renders, and collapsing a base64 image into it is the silent corruption this
/// split exists to prevent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Msg {
    /// Author of the turn.
    pub role: Role,
    /// Turn text, with text parts joined on a newline. Empty when every part is
    /// media, and for a `tool` turn whose payload is not textual.
    pub content: String,
    /// Non-text parts, in wire order. Empty for a text-only turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaPart>,
}

impl Msg {
    /// Builds a text-only message.
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            media: Vec::new(),
        }
    }
}

/// One non-text content part, carried across the canonical boundary unchanged.
///
/// `body` is every field of the inbound part except its discriminator, held as
/// raw JSON. Re-encoding it into a typed variant is what this deliberately does
/// not do: the same concept is an `{"url":…}` object on OpenAI, a base64
/// string array on Ollama and a nested `source` on Anthropic, and a shared
/// canonical shape would have to invent a spelling for two of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaPart {
    /// Wire discriminator, e.g. `image_url`, `input_audio`, `tool_use`.
    pub kind: String,
    /// The rest of the part, exactly as received.
    pub body: serde_json::Value,
}

impl MediaPart {
    /// Builds a media part from its discriminator and remaining fields.
    pub fn new(kind: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            kind: kind.into(),
            body,
        }
    }

    /// Rebuilds the original wire object: `{"type": kind, ..body}`.
    ///
    /// This is what makes the part *passthrough* rather than merely preserved —
    /// the upstream receives the bytes the client sent.
    #[must_use]
    pub fn to_wire(&self) -> serde_json::Value {
        let mut map = match &self.body {
            serde_json::Value::Object(fields) => fields.clone(),
            // A non-object body cannot carry a `type` alongside it, so it is
            // preserved under the part's own name instead of being dropped.
            other => {
                return serde_json::json!({
                    "type": self.kind,
                    self.kind.as_str(): other,
                });
            }
        };
        map.insert(
            "type".to_owned(),
            serde_json::Value::String(self.kind.clone()),
        );
        serde_json::Value::Object(map)
    }
}

/// A chat request in the canonical shape `ar-exec` posts upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalChat {
    /// Provider-native model id, as requested.
    pub model: String,
    /// Conversation turns, in order.
    pub messages: Vec<Msg>,
    /// Sampling temperature, when the caller set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Output token ceiling, when the caller set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Whether the caller asked for incremental delivery.
    pub stream: bool,
}

/// Renders `chat` as an OpenAI chat-completions request body.
///
/// This is the outbound counterpart of [`crate::to_canonical`], and the seam the
/// server stream posts upstream from. `Vec<u8>` rather than `serde_json::Value`
/// because the consumer writes these bytes straight to a request body: there is
/// no second consumer for a typed struct to stay in sync with, and the value
/// would only be serialized once anyway.
///
/// # Two properties the OpenAI wire does not forgive
///
/// **No explicit nulls.** `temperature: null` is not "unset" to a strict
/// upstream — several reject the body outright. Every optional field is
/// therefore *omitted* when absent, which is what
/// `#[serde(skip_serializing_if = "Option::is_none")]` on
/// [`CanonicalChat`] and [`Msg::media`] buys, and what
/// [`render_openai_message`] does by construction.
///
/// **Media goes in `content`, never beside it.** A turn's non-text parts are
/// rendered through [`MediaPart::to_wire`] and spliced into a content-part array
/// alongside the turn's text. There is deliberately no `media` key: no OpenAI
/// provider reads one, and a part smuggled under an invented key is a request
/// the upstream accepts while silently ignoring the image — the exact failure a
/// vision request must not have.
///
/// ```
/// use ar_translate::{CanonicalChat, Msg, Role, render_openai_body};
///
/// let chat = CanonicalChat {
///     model: "gpt-5.4".to_owned(),
///     messages: vec![Msg::new(Role::User, "hi")],
///     temperature: None,
///     max_tokens: None,
///     stream: true,
/// };
/// let body = String::from_utf8(render_openai_body(&chat)).unwrap();
///
/// assert!(!body.contains("temperature"), "an absent field must be absent: {body}");
/// assert!(!body.contains("max_tokens"), "an absent field must be absent: {body}");
/// ```
///
/// # What this drops, and why that is the honest failure
///
/// Canonical is deliberately small, and a canonical type that could express a
/// field no adapter routes would be a claim without an implementation. So the
/// inbound dialects parse these and canonical has nowhere to put them:
///
/// * `tools` / `tool_choice` — no canonical tool registry. An Anthropic
///   `tool_use` crosses as a carried [`MediaPart`], which is enough to *observe*
///   that a turn used tools and not enough to declare them, so a tool-using
///   request reaches the upstream without its tool declarations. Every outbound
///   renderer inherits this gap; `TODO(#p1-tools)` is where it closes.
/// * `top_p` — parsed by two inbound adapters (`OllamaOptions::top_p`,
///   `GeminiGenerationConfig::top_p`), no canonical field. Sampling becomes
///   temperature-only upstream.
/// * `stop` / `stop_sequences` — parsed by no inbound adapter. Output runs to
///   `max_tokens` or the model's own end of turn.
/// * `n` — multiple candidates are not a canonical concept; one request yields
///   one response, and the renderer's `choices` is a single index-0 entry.
/// * `reasoning_effort` — thinking traces are *rejected* on the way in
///   ([`crate::TranslateError::UnsupportedPart`] on an Anthropic `thinking`
///   block, `GeminiPart::thought` on the Gemini side), so there is nothing to
///   ask the upstream to reason harder.
///
/// Each is a request capability this build does not have, and each is visible
/// rather than silently substituted. `TODO(#p1-tools)`: give canonical a tool
/// registry, then the four samplers, `stop`, `n` and `reasoning_effort` follow
/// from it; re-add them here and in the per-wire renderers alongside the
/// canonical fields they read.
#[must_use]
pub fn render_openai_body(chat: &CanonicalChat) -> Vec<u8> {
    let messages: Vec<serde_json::Value> =
        chat.messages.iter().map(render_openai_message).collect();

    // Built as a map rather than a `json!` literal on purpose: `json!` writes
    // `Option::None` as an explicit `null`, which is the defect this function
    // exists to remove. Insertion is the omission.
    let mut body = serde_json::Map::with_capacity(5);
    body.insert("model".to_owned(), chat.model.as_str().into());
    body.insert("messages".to_owned(), serde_json::Value::Array(messages));
    body.insert("stream".to_owned(), chat.stream.into());
    if let Some(temperature) = chat.temperature {
        body.insert("temperature".to_owned(), temperature.into());
    }
    if let Some(max_tokens) = chat.max_tokens {
        body.insert("max_tokens".to_owned(), max_tokens.into());
    }

    // `to_vec` on strings, integers, bools and already-valid `Value`s cannot
    // fail, so the fallback exists only to keep a request path free of `panic!`.
    serde_json::to_vec(&serde_json::Value::Object(body)).unwrap_or_else(|err| {
        // Emitting a body the upstream rejects with a 400 naming the problem
        // beats a 500: a 500 here reads as "the proxy broke" and sends an
        // operator to the wrong dashboard.
        serde_json::to_vec(&serde_json::json!({
            "error": { "message": format!("could not render canonical chat: {err}") },
        }))
        .unwrap_or_default()
    })
}

/// Renders one canonical turn as an OpenAI message object.
///
/// The branch is on "does this turn have any media", not on "is there text": a
/// media-only turn still emits a content array, because a bare string would drop
/// the part.
fn render_openai_message(msg: &Msg) -> serde_json::Value {
    let mut map = serde_json::Map::with_capacity(3);
    map.insert("role".to_owned(), msg.role.as_wire().into());

    if msg.media.is_empty() {
        map.insert("content".to_owned(), msg.content.clone().into());
        return serde_json::Value::Object(map);
    }

    let mut parts: Vec<serde_json::Value> = Vec::with_capacity(msg.media.len() + 1);
    if !msg.content.is_empty() {
        parts.push(serde_json::json!({ "type": "text", "text": msg.content }));
    }
    // The single media renderer, unchanged. A part this build cannot route never
    // reaches here — inbound adapters reject it — so every `to_wire` result is
    // the dialect's own spelling, and the upstream sees exactly what the client
    // sent.
    parts.extend(msg.media.iter().map(MediaPart::to_wire));
    map.insert("content".to_owned(), serde_json::Value::Array(parts));
    serde_json::Value::Object(map)
}

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural end of turn or a matched stop sequence.
    Stop,
    /// The output token ceiling was reached.
    Length,
    /// The model emitted a tool call.
    ToolCalls,
    /// A provider-side safety filter stopped generation.
    ContentFilter,
}

impl FinishReason {
    /// The wire spelling of this reason.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
        }
    }
}

/// Token accounting for one completed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed by the prompt.
    pub prompt_tokens: u32,
    /// Tokens produced by the model.
    pub completion_tokens: u32,
    /// Sum of both.
    pub total_tokens: u32,
}

/// A completed, non-streaming response in canonical shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalResponse {
    /// Provider-assigned response id.
    pub id: String,
    /// Model that produced the response.
    pub model: String,
    /// Unix creation timestamp, seconds.
    pub created: u64,
    /// The assistant turn.
    pub message: Msg,
    /// Why generation stopped.
    pub finish_reason: FinishReason,
    /// Token accounting.
    pub usage: Usage,
}