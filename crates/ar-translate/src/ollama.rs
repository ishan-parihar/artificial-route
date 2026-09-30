//! Ollama chat wire shapes (`POST /api/chat`).
//!
//! OmniRoute's `/v1/api/chat` advertises "compatibility with Ollama's
//! `/api/chat` format" (`docs/reference/API_REFERENCE.md`) but reuses its
//! OpenAI-shaped chat handler on the way in and only transforms on the way out
//! (`open-sse/utils/ollamaTransform.ts` emits `{model, message, done}`). So the
//! outbound side of the reference is authoritative for the field *spellings*
//! used here, and the request shape below is the Ollama `/api/chat` body those
//! spellings belong to.
//!
//! The two things that are genuinely different from the OpenAI wire, and the
//! reason a separate adapter exists at all:
//!
//! * Sampling options are nested under `options`, and the output ceiling is
//!   `options.num_predict`, not `max_tokens`.
//! * The system prompt is a top-level `system` string, not a `role:"system"`
//!   turn — though `role:"system"` turns do occur and are accepted.
//!
//! `images` (base64 vision) is parsed and then rejected by the adapter: vision
//! is P6 (`docs/05`), and dropping a list of base64 blobs into a text field is
//! the silent corruption [`crate::TranslateError::UnsupportedPart`] exists to
//! prevent.

use serde::{Deserialize, Serialize};

/// An inbound `POST /api/chat` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OllamaChat {
    /// Requested model id, e.g. `llama3.2`.
    pub model: String,
    /// Conversation turns.
    pub messages: Vec<OllamaMessage>,
    /// System prompt, outside the message array. The Ollama spelling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Whether the client asked for incremental delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Sampling and runtime options. Ollama nests these; OpenAI does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<OllamaOptions>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// Ollama's nested `options` object.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OllamaOptions {
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Output token ceiling. Maps to canonical `max_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<u32>,
    /// Nucleus sampling. No canonical field; read for completeness, dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Vendor and future option keys, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// One inbound conversation turn.
///
/// `content` is a plain string on this wire — Ollama has no typed content parts
/// — so the multimodal field is the separate `images` array, which is what the
/// adapter rejects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OllamaMessage {
    /// Wire role name: `system`, `user`, `assistant` or `tool`.
    pub role: String,
    /// Turn text.
    #[serde(default)]
    pub content: String,
    /// Base64 image payloads. Vision, so rejected rather than flattened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}
