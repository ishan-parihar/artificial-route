//! Google Gemini wire shapes (`POST /v1beta/models/{model}:generateContent`).
//!
//! Ported from `../OmniRoute/open-sse/translator/request/gemini-to-openai.ts`,
//! reading the fields that file reads and nothing more: the model id comes off
//! the URL path rather than the body, so [`GeminiChat::model`] is the adapter's
//! own field (see [`crate::translate::gemini_to_canonical`]); `systemInstruction`,
//! `contents`, `generationConfig` and `tools` are the body's. Everything else
//! rides in [`GeminiChat::rest`] unparsed.
//!
//! # Three shapes that are genuinely Gemini's own
//!
//! * **The system prompt lives outside the array** as `systemInstruction`, and
//!   is a *content object* (with `parts`), not a bare string. Canonical needs a
//!   leading [`crate::Role::System`] turn synthesised for it.
//! * **A turn has no `role: "tool"`.** A tool result is a `functionResponse`
//!   part inside a `user` turn, so tool turns are recovered from the *part*, not
//!   the turn. The reference emits one tool message per `functionResponse` and
//!   hoists it ahead of the turn it was co-located with
//!   (`splitCoLocatedFunctionResponses`) — that split is reproduced here.
//! * **Images are inline base64**, `inlineData.mimeType` + `inlineData.data`,
//!   not a URL. The reference rewrites that into an OpenAI `image_url` data
//!   URI; this adapter carries the part verbatim instead, because
//!   [`crate::MediaPart`] exists precisely so a dialect's own spelling survives
//!   the canonical boundary. See [`crate::MediaPart`] for why re-encoding is
//!   refused.

use serde::{Deserialize, Serialize};

/// An inbound Gemini `generateContent` body, plus the model id from the path.
///
/// `model` is not a JSON field on this wire — it is `models/{model}` in the URL —
/// so the caller extracts it and assigns it before calling
/// [`crate::gemini_to_canonical`]. It is therefore `default`, and a body parsed
/// without that assignment yields an empty id, which
/// [`crate::TranslateError::EmptyModel`] then rejects: a Gemini request whose
/// path id was never lifted fails loudly rather than reaching an upstream with
/// no model at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeminiChat {
    /// Provider-native model id, taken from the URL path by the caller.
    #[serde(default)]
    pub model: String,
    /// System prompt, outside the content array. The Gemini spelling.
    #[serde(
        default,
        rename = "systemInstruction",
        skip_serializing_if = "Option::is_none"
    )]
    pub system_instruction: Option<GeminiContent>,
    /// Conversation turns. Gemini calls them `contents`.
    #[serde(default)]
    pub contents: Vec<GeminiContent>,
    /// Sampling configuration. `None` when the client sent none.
    #[serde(
        default,
        rename = "generationConfig",
        skip_serializing_if = "Option::is_none"
    )]
    pub generation_config: Option<GeminiGenerationConfig>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// One Gemini conversation turn.
///
/// `role` is optional and only ever `user` or `model`; the adapter maps `model`
/// to [`crate::Role::Assistant`] and treats anything else as `user`, which is
/// what the reference does (`content.role === "user" ? "user" : "assistant"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeminiContent {
    /// Wire role: `user` or `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The turn's parts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<GeminiPart>>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// One part of a Gemini turn.
///
/// Each arm is a separate optional field rather than an enum because Gemini
/// permits more than one key on a single part (`text` plus `inlineData` is
/// legal), so the alternatives are not mutually exclusive and a
/// `#[serde(untagged)]` enum would silently keep only the first match.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GeminiPart {
    /// Visible text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Inline base64 payload: an image on this wire.
    #[serde(
        default,
        rename = "inlineData",
        alias = "inline_data",
        skip_serializing_if = "Option::is_none"
    )]
    pub inline_data: Option<GeminiBlob>,
    /// A tool call the model made.
    #[serde(
        default,
        rename = "functionCall",
        alias = "function_call",
        skip_serializing_if = "Option::is_none"
    )]
    pub function_call: Option<GeminiFunctionCall>,
    /// A tool result the client is returning to the model.
    #[serde(
        default,
        rename = "functionResponse",
        alias = "function_response",
        skip_serializing_if = "Option::is_none"
    )]
    pub function_response: Option<GeminiFunctionResponse>,
    /// `true` marks the part as model reasoning rather than visible output.
    ///
    /// Parsed so it can be rejected explicitly: the reference splits thought
    /// parts out into `reasoning_content`, and canonical has nowhere to put
    /// that. Forwarding a thought into ordinary `content` would leak a prior
    /// turn's reasoning into the visible message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// An inline base64 payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeminiBlob {
    /// MIME type, e.g. `image/png`.
    #[serde(
        default,
        rename = "mimeType",
        alias = "mime_type",
        skip_serializing_if = "Option::is_none"
    )]
    pub mime_type: Option<String>,
    /// The base64 payload itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// A tool call the model made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeminiFunctionCall {
    /// Tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Call arguments, as an object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// A tool result the client is returning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeminiFunctionResponse {
    /// Correlates the result with the `functionCall` that requested it.
    #[serde(
        default,
        rename = "toolCallId",
        alias = "tool_call_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<String>,
    /// Tool name, echoed by some clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The result payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<serde_json::Value>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// Gemini's `generationConfig`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeminiGenerationConfig {
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Output token ceiling. The Gemini spelling of `max_tokens`.
    #[serde(
        default,
        rename = "maxOutputTokens",
        alias = "max_output_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_output_tokens: Option<u32>,
    /// Nucleus sampling. No canonical field; read for completeness, dropped.
    #[serde(
        default,
        rename = "topP",
        alias = "top_p",
        skip_serializing_if = "Option::is_none"
    )]
    pub top_p: Option<f32>,
    /// Vendor and future config keys, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}
