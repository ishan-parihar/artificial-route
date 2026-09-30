//! Inbound wire adapters: wire -> canonical -> wire.
//!
//! P0 shipped exactly one inbound dialect (OpenAI chat completions) and one
//! canonical shape. P2 adds the four `docs/05` deferred: Anthropic Messages,
//! Responses, Ollama and Gemini. Each is one `impl ArTranslate<In>` against the
//! same [`CanonicalChat`], not a new abstraction — which is why adding a dialect
//! is a small file and not a registry.
//!
//! # Contract
//!
//! * Inbound: [`to_canonical`], [`anthropic_to_canonical`],
//!   [`responses_to_canonical`], [`ollama_to_canonical`],
//!   [`gemini_to_canonical`] — each one `ArTranslate::to_canonical` for its own
//!   wire, producing [`CanonicalChat`].
//! * Outbound request: [`render_openai_body`] — [`CanonicalChat`] -> the bytes
//!   the server posts upstream.
//! * Outbound response: [`to_openai`] — [`CanonicalResponse`] -> OpenAI JSON.
//!
//! Everything else in the reference's dialect matrix is a named, unimplemented
//! cell: [`supported_pairs`] is what exists, [`missing_pairs`] is what does not,
//! and [`pair_named`] errors rather than guessing.
//!
//! # Example
//!
//! ```
//! use ar_translate::{Role, to_canonical, OpenAIChat};
//!
//! let raw = r#"{"model":"gpt-5.4","messages":[
//!     {"role":"system","content":"be terse"},
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
//! use ar_translate::{render_openai_body, to_canonical, OpenAIChat};
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

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod anthropic;
mod canonical;
mod gemini;
mod media;
mod ollama;
mod openai;
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
pub use responses::{
    ResponsesApi, ResponsesContentPart, ResponsesInput, ResponsesItem, ResponsesItemContent,
};
pub use translate::{
    AnthropicInbound, ArTranslate, GeminiInbound, MissingPair, OllamaInbound, OpenAiInbound, Pair,
    ResponsesInbound, TranslateError, anthropic_to_canonical, chat_modality, gemini_to_canonical,
    missing_pairs, ollama_to_canonical, pair_named, responses_to_canonical, supported_pairs,
    to_canonical, to_openai,
};
