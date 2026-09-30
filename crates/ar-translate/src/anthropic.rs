//! Anthropic Messages wire shapes (`POST /v1/messages`).
//!
//! Ported from `../OmniRoute/open-sse/translator/request/claude-to-openai.ts`,
//! reading only what that file reads: `model`, `max_tokens`, `system`,
//! `messages`, `temperature`, `stream`. Everything else (`top_p`,
//! `stop_sequences`, `metadata`, `tools`, `thinking`, `tool_choice`) rides in
//! [`AnthropicMessages::rest`] unparsed — the canonical shape has nowhere to put
//! them, and a lossy parse would be worse than an honest drop.
//!
//! The one behaviour worth calling out is the `system` field. Anthropic puts the
//! system prompt *outside* the message array, so translating to canonical means
//! synthesising a leading [`Role::System`] turn — see
//! [`crate::translate::ArTranslate`] for the Anthropic impl.
//!
//! Block handling follows the reference: `text` accumulates into the parent
//! turn, `tool_result` becomes its own [`Role::Tool`] turn emitted *before* the
//! parent, and `tool_use` has no canonical representation so it is rejected
//! rather than dropped. See [`crate::translate`] for why rejection is the rule.

use serde::{Deserialize, Serialize};

/// An inbound `POST /v1/messages` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicMessages {
    /// Requested model id.
    pub model: String,
    /// Output token ceiling. Required by Anthropic, optional here so a body
    /// missing it still parses and fails on content rather than on serde.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// System prompt, outside the message array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<AnthropicSystem>,
    /// Conversation turns.
    pub messages: Vec<AnthropicMessage>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Whether the client asked for incremental delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// The `system` field. Untagged because the wire accepts both spellings: a bare
/// string, or an array of typed blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicSystem {
    /// Plain text.
    Text(String),
    /// Typed content blocks.
    Blocks(Vec<AnthropicBlock>),
}

impl AnthropicSystem {
    /// The text of the system prompt, joining blocks on a newline.
    ///
    /// An empty result means "no usable system text", which the adapter treats
    /// as "no system turn" rather than as a turn with empty content.
    #[must_use]
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter(|b| b.kind == "text")
                .map(|b| b.text.as_deref().unwrap_or_default())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// One inbound conversation turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicMessage {
    /// Wire role name: `user` or `assistant`.
    pub role: String,
    /// Turn content, in either accepted spelling.
    pub content: AnthropicContent,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// Turn content. Untagged, as in [`AnthropicSystem`]: a bare string, or typed
/// blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    /// Plain text.
    Text(String),
    /// Typed content blocks.
    Blocks(Vec<AnthropicBlock>),
}

/// One typed content block.
///
/// `kind` stays a `String` rather than an enum so an unrecognised block is
/// reportable by name in [`crate::TranslateError::UnsupportedPart`] instead of
/// failing to deserialize with no context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicBlock {
    /// Block discriminator, e.g. `text`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Text payload, present on `text` blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Correlates a `tool_result` with the `tool_use` that requested it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Result payload, present on `tool_result` blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<AnthropicBlockContent>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// A `tool_result` payload.
///
/// Untagged with the catch-all last, mirroring the reference: a string is used
/// verbatim, a block array is walked for text, and anything else is stringified
/// so the result still reaches the model rather than vanishing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicBlockContent {
    /// Plain text result.
    Text(String),
    /// Typed result blocks.
    Blocks(Vec<AnthropicBlock>),
    /// Any other JSON, stringified.
    Other(serde_json::Value),
}
