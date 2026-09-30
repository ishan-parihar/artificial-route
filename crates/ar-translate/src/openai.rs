//! OpenAI chat-completions wire shapes.
//!
//! Structurally mirrors `ar-llm`'s `types/completions.rs` rather than inventing
//! a parallel spelling: `Option<T>` fields carry only
//! `skip_serializing_if`, and every type keeps a flattened `rest` catch-all so
//! vendor extensions round-trip instead of being dropped. Anthropic Messages,
//! Ollama and Responses inbound are P2 (`docs/05`) and deliberately absent.

use serde::{Deserialize, Serialize};

/// An inbound `POST /v1/chat/completions` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIChat {
    /// Requested model id.
    pub model: String,
    /// Conversation turns as the client sent them.
    pub messages: Vec<OpenAIMessage>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Output token ceiling (legacy spelling).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Output token ceiling (current spelling). Both map to canonical
    /// `max_tokens`; `max_tokens` wins so a client setting both gets the older,
    /// more widely honoured field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// Whether the client asked for incremental delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// One inbound conversation turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIMessage {
    /// Wire role name.
    pub role: String,
    /// Turn content, in either accepted spelling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<OpenAIContent>,
    /// Correlates a `tool` turn with the call that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// Turn content. Untagged because the wire accepts both spellings for the same
/// thing: a bare string, or an array of typed parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenAIContent {
    /// Plain text.
    Text(String),
    /// Typed content parts.
    Parts(Vec<OpenAIContentPart>),
}

/// One typed content part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIContentPart {
    /// Part discriminator, e.g. `text`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Text payload, present on `text` parts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// A single outbound `chat.completion.chunk` delta.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIChunk {
    /// Response id, stable across every chunk of one response.
    pub id: String,
    /// Always `chat.completion.chunk`.
    pub object: String,
    /// Unix creation timestamp, seconds.
    pub created: u64,
    /// Model that produced the response.
    pub model: String,
    /// Choices; empty on a usage-only final chunk.
    pub choices: Vec<OpenAIChunkChoice>,
}

/// One choice within a chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIChunkChoice {
    /// Choice index; always 0 at P0 (`n` is not supported).
    pub index: u32,
    /// The incremental content.
    pub delta: OpenAIDelta,
    /// Set on the last chunk of a choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// The incremental payload of a chunk.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OpenAIDelta {
    /// Role, sent on the first chunk only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Text fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}