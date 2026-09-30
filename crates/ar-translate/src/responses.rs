//! OpenAI Responses API wire shapes (`POST /v1/responses`).
//!
//! Ported from `../OmniRoute/open-sse/translator/request/openai-responses.ts`.
//! The reference's whole job is turning `{input, instructions}` into a `messages`
//! array; the token budget and sampling fields it passes through untouched, so
//! this module reads them under their Responses spellings and renames them once,
//! in the adapter.
//!
//! Two shapes the reference is explicit about, and this port preserves:
//!
//! * `input` is either a bare string (one user turn) or an item array.
//! * An item is a message when its `type` is `"message"` or absent-but-`role`.
//!   Content parts are `input_text` / `output_text` (both plain text) or
//!   `refusal` (which carries its text in `refusal`, not `text`).
//!
//! `agent_message` is the reference's spelling for an assistant turn and maps
//! onto [`crate::Role::Assistant`].

use serde::{Deserialize, Serialize};

/// An inbound `POST /v1/responses` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponsesApi {
    /// Requested model id.
    pub model: String,
    /// The conversation, in either accepted spelling.
    pub input: ResponsesInput,
    /// System-level instructions, outside the input array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Output token ceiling. The Responses spelling of `max_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
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

/// The `input` field. Untagged because the wire accepts both: a bare prompt
/// string, or an item array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    /// A single prompt, equivalent to one user turn.
    Text(String),
    /// The conversation, as items.
    Items(Vec<ResponsesItem>),
}

/// One input item.
///
/// `kind` is `Option<String>` because the reference accepts an item with no
/// `type` at all as long as it has a `role` — clients differ on this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponsesItem {
    /// Item discriminator; `None` on role-only items, which are messages.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Wire role name, e.g. `user` or `agent_message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Turn content, in either accepted spelling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ResponsesItemContent>,
    /// Correlates a `tool` turn with the call that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

impl ResponsesItem {
    /// Whether this item is a conversation turn.
    ///
    /// A missing `type` with a present `role` counts, per the reference: some
    /// clients send role-based items and omit the discriminator entirely.
    #[must_use]
    pub fn is_message(&self) -> bool {
        match self.kind.as_deref() {
            None => self.role.is_some(),
            Some("message") => true,
            Some(_) => false,
        }
    }

    /// The part discriminator, for error reporting when the item is not a
    /// message and therefore has no canonical representation.
    #[must_use]
    pub fn kind_name(&self) -> &str {
        self.kind.as_deref().unwrap_or("message")
    }
}

/// Turn content. Untagged, as elsewhere in this crate: a bare string, or typed
/// parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesItemContent {
    /// Plain text.
    Text(String),
    /// Typed content parts.
    Parts(Vec<ResponsesContentPart>),
}

/// One typed content part.
///
/// Only the text-bearing discriminators are modelled; `input_image` and
/// `input_file` fall through to [`ResponsesContentPart::kind`] and are rejected
/// by the adapter, because they carry a modality the canonical shape has no
/// room for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponsesContentPart {
    /// Part discriminator: `input_text`, `output_text`, `refusal`, ...
    #[serde(rename = "type")]
    pub kind: String,
    /// Text payload, on `input_text` / `output_text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Text payload, on `refusal` — the reference reads this field, not `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}
