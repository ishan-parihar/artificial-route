//! Inbound and outbound wire adapters: wire -> canonical -> wire.
//!
//! P0 shipped exactly one inbound dialect (OpenAI chat completions) and one
//! canonical shape, and rendered exactly one outbound wire. P2 added the four
//! `docs/05` deferred as inbound dialects: Anthropic Messages, Responses, Ollama
//! and Gemini. Each is one `impl ArTranslate<In>` against the same
//! [`CanonicalChat`], not a new abstraction — which is why adding a dialect is a
//! small file and not a registry. The outbound direction grew the same way: one
//! renderer per provider wire, behind [`render_for_wire`].
//!
//! # Contract
//!
//! * Inbound: [`to_canonical`], [`anthropic_to_canonical`],
//!   [`responses_to_canonical`], [`ollama_to_canonical`],
//!   [`gemini_to_canonical`] — each one `ArTranslate::to_canonical` for its own
//!   wire, producing [`CanonicalChat`].
//! * Outbound request: [`render_for_wire`] — [`CanonicalChat`] -> the bytes the
//!   server posts upstream, in any of the eight wires [`OutboundWire`] names.
//!   [`render_openai_body`] is the OpenAI arm and stays byte-identical to it:
//!   the dispatch table delegates rather than reimplementing, so the default path
//!   cannot drift.
//! * Outbound response: [`to_openai`] for OpenAI, and — since the response
//!   direction has to reach back to the *inbound* dialect, not the provider's —
//!   [`to_anthropic_response`] and [`to_responses_response`] for the two inbound
//!   dialects. Not fully closed: [`missing_pairs`] names every remaining
//!   re-framing cell.
//!
//! Everything the reference's dialect matrix registers that this build does not
//! answer is a named cell: [`supported_pairs`] is what exists, [`missing_pairs`]
//! is what does not, and [`pair_named`] errors rather than guessing. The request
//! direction is closed — a canonical body reaches every named wire in that wire's
//! own shape — and the response direction is not.
//!
//! # The response direction
//!
//! The response direction has two non-streaming envelopes and no wired path
//! yet: the relay hands the client's bytes through in the *provider's* framing,
//! so a Claude-dialect client receives Gemini SSE when the router picked a
//! Gemini provider. [`to_anthropic_response`] and [`to_responses_response`] are
//! built for that job and [`missing_pairs`] names every remaining re-framing
//! cell with its reason.
//!
//! # Example
//!
//! ```
//! use ar_translate::{Role, OpenAIChat, to_canonical};
//!
//! let raw = r#"{"model":"gpt-5.4","messages":[{"role":"system","content":"be terse"},
//!     {"role":"user","content":"hi"}],"stream":true}"#;
//! let chat: OpenAIChat = serde_json::from_str(raw).unwrap();
//! let chat = to_canonical(chat).unwrap();
//!
//! assert!(chat.stream);
//! assert_eq!(chat.messages[0].role, Role::System);
//! ```
//!
//! A non-OpenAI dialect differs only in the entry point named at the call site:
//!
//! ```
//! use ar_translate::{AnthropicMessages, Msg, Role, anthropic_to_canonical};
//!
//! let raw = r#"{"model":"claude-sonnet-4-5","max_tokens":64,
//!     "system":"be terse",
//!     "messages":[{"role":"user","content":"hi"}]}"#;
//! let req: AnthropicMessages = serde_json::from_str(raw).unwrap();
//!
//! let chat = anthropic_to_canonical(req).unwrap();
//! assert_eq!(chat.messages[0], Msg::new(Role::System, "be terse"));
//! ```
//!
//! Since P6 the media family rides the same canonical shape. A turn's non-text
//! parts are carried verbatim rather than flattened, and [`chat_modality`] is
//! what routes them:
//!
//! ```
//! use ar_translate::{Modality, OpenAIChat, chat_modality, to_canonical};
//!
//! let raw = r#"{"model":"gpt-5.4","messages":[{"role":"user","content":[
//!     {"type":"text","text":"what is this?"},
//!     {"type":"image_url","image_url":{"url":"http://x/i.png"}}]}]}"#;
//! let chat: OpenAIChat = serde_json::from_str(raw).unwrap();
//!
//! let canonical = to_canonical(chat).unwrap();
//! assert_eq!(canonical.messages[0].content, "what is this?");
//! assert_eq!(canonical.messages[0].media[0].kind, "image_url");
//! assert_eq!(chat_modality(&canonical), Modality::Vision);
//! ```
//!
//! And back out, with the media spliced into `content` and no `media` key for an
//! upstream to ignore:
//!
//! ```
//! use ar_translate::render_openai_body;
//! use ar_translate::{OpenAIChat, to_canonical};
//!
//! let raw = r#"{"model":"gpt-5.4","messages":[{"role":"user","content":[
//!     {"type":"text","text":"what is this?"},
//!     {"type":"image_url","image_url":{"url":"http://x/i.png"}}]}]}"#;
//! let chat: OpenAIChat = serde_json::from_str(raw).unwrap();
//!
//! let body = String::from_utf8(render_openai_body(&to_canonical(chat).unwrap())).unwrap();
//!
//! assert!(body.contains(r#""image_url":{"url":"http://x/i.png"}"#), "{body}");
//! assert!(!body.contains("\"media\""), "no dialect invents a `media` key: {body}");
//! ```
//!
//! The same canonical request reaches a Claude provider in Anthropic's own
//! shape, which is what makes the non-OpenAI registry providers dispatchable.
//! The request direction is closed; the response direction is not — see above.
//!
//! ```
//! use ar_translate::render_for_wire;
//! use ar_translate::{AnthropicMessages, Msg, OutboundWire, Role, anthropic_to_canonical};
//!
//! let raw = r#"{"model":"claude-sonnet-4-5","max_tokens":64,
//!     "system":"be terse",
//!     "messages":[{"role":"user","content":"hi"}]}"#;
//! let canonical = anthropic_to_canonical(serde_json::from_str(raw).unwrap()).unwrap();
//!
//! let body: serde_json::Value =
//!     serde_json::from_slice(&render_for_wire(&canonical, OutboundWire::Claude)).unwrap();
//!
//! assert_eq!(canonical.messages[0], Msg::new(Role::System, "be terse"));
//! assert_eq!(body["system"][0]["text"], "be terse", "{body}");
//! ```
//!
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod anthropic;
mod canonical;
mod gemini;
mod media;
mod ollama;
mod openai;
mod outbound;
mod responses;
mod translate;

pub use anthropic::{
    AnthropicBlock, AnthropicBlockContent, AnthropicContent, AnthropicMessages, AnthropicSystem,
};
pub use canonical::{
    CanonicalChat, CanonicalResponse, FinishReason, MediaPart, Msg, Role, Usage, render_openai_body,
};
pub use gemini::{
    GeminiBlob, GeminiChat, GeminiContent, GeminiFunctionCall, GeminiFunctionResponse,
    GeminiGenerationConfig, GeminiPart,
};
pub use media::{
    EMBEDDING_REGISTRY, EmbeddingFamily, EmbeddingInput, EmbeddingRequest, EmbeddingUpstream,
    EmbeddingUsage, EmbeddingVector, MediaError, Modality, create_embedding_response, family_guard,
};
pub use ollama::{OllamaChat, OllamaMessage, OllamaOptions};
pub use openai::{
    OpenAIChat, OpenAIChunk, OpenAIChunkChoice, OpenAIContent, OpenAIContentPart, OpenAIDelta,
    OpenAIMessage,
};
pub use outbound::{
    OutboundWire, render_claude_body, render_clova_body, render_cursor_body, render_for_wire,
    render_gemini_body, render_kiro_body, render_openai_responses_body, to_anthropic_response,
    to_responses_response,
};
pub use responses::{
    ResponsesApi, ResponsesContentPart, ResponsesInput, ResponsesItem, ResponsesItemContent,
};
pub use translate::{
    AnthropicInbound, ArTranslate, GeminiInbound, MissingPair, OllamaInbound, OpenAiInbound, Pair,
    ResponsesInbound, TranslateError, anthropic_to_canonical, chat_modality, gemini_to_canonical,
    missing_pairs, ollama_to_canonical, pair_named, responses_to_canonical, supported_pairs,
    to_canonical, to_openai,
};
