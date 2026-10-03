//! Translation between the four inbound wires, the canonical shape, and the
//! eight provider wires.
//!
//! Dispatch is generic over the inbound type, so a call site monomorphises and
//! the compiler inlines the conversion; there is no `dyn` translator table.
//! `docs/02` calls this crate "highest leverage", which is why every dialect in
//! either direction is one small function or one `impl ArTranslate<In>` rather
//! than a registered strategy.
//!
//! # The four inbound dialects
//!
//! `docs/02` deferred Anthropic Messages, Ollama and Responses inbound to P2,
//! and Gemini arrived with the vision family. They landed as more impls of the
//! same trait, not as a new abstraction — one canonical type, four markers:
//!
//! | Dialect | Marker | Wire | What the conversion actually does |
//! |---|---|---|---|
//! | Anthropic Messages | [`AnthropicInbound`] | `POST /v1/messages` | hoists `system` into a leading system turn; `tool_result` blocks become [`Role::Tool`] turns |
//! | Responses | [`ResponsesInbound`] | `POST /v1/responses` | hoists `instructions` into a system turn; `{input, instructions}` become a message array |
//! | Ollama | [`OllamaInbound`] | `POST /api/chat` | hoists the top-level `system` string; `options.num_predict` becomes `max_tokens` |
//! | Gemini | [`GeminiInbound`] | `POST /v1beta/models/{model}:generateContent` | hoists `systemInstruction` into a leading system turn; `functionResponse` parts become [`Role::Tool`] turns hoisted ahead of their turn; `inlineData` and `functionCall` cross as carried parts |
//!
//! The outbound direction is [`crate::OutboundWire`]: one renderer per provider
//! wire, reached through [`crate::render_for_wire`], and named as [`Pair`]
//! variants too — so one registry names every cell this build answers in either
//! direction.
//!
//! # Media, and what is still rejected
//!
//! Since P6 a turn carries its non-text parts in [`crate::Msg::media`],
//! verbatim: `image_url` / `input_image` / `image` / `images` and
//! `input_audio` / `audio` route to the vision and audio adapters, and an
//! Anthropic `tool_use` block crosses as a carried part. The upstream shapes
//! differ per dialect, so nothing is re-encoded — see [`crate::MediaPart`].
//!
//! A part naming *no* modality still raises [`TranslateError::UnsupportedPart`]
//! rather than being dropped. Unrecognised is not routable, and a silent drop
//! is the failure this whole path exists to prevent.
//!
//! # Fail loud on an unimplemented pair
//!
//! The reference (`../OmniRoute/open-sse/translator/`) registers a 5x5 dialect
//! matrix and answers every cell. This build implements six of them, and
//! [`supported_pairs`] is the list. Anything outside it returns
//! [`TranslateError::UnsupportedPair`] naming the pair — the alternative the
//! red-team report flagged was a caller reaching for a conversion that does not
//! exist and forwarding whatever shape it had, which reaches the upstream as a
//! plausible-looking body that says the wrong thing.

use serde_json::json;

use crate::anthropic::{
    AnthropicBlock, AnthropicBlockContent, AnthropicContent, AnthropicMessages,
};
use crate::canonical::{CanonicalChat, CanonicalResponse, MediaPart, Msg, Role};
use crate::gemini::{GeminiChat, GeminiContent, GeminiPart};
use crate::media::Modality;
use crate::ollama::OllamaChat;
use crate::openai::{OpenAIChat, OpenAIContent};
use crate::responses::{ResponsesApi, ResponsesInput, ResponsesItem, ResponsesItemContent};

/// One dialect conversion this crate can perform.
///
/// The enum holds **only what exists**. A pair with no implementation is not a
/// variant — it is a [`MissingPair`], a named cell with a reference file — so
/// there is no way to hold a `Pair` and dispatch on it into a conversion that was
/// never written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pair {
    /// `POST /v1/chat/completions` request -> canonical.
    OpenaiToCanonical,
    /// `POST /v1/messages` request -> canonical.
    AnthropicToCanonical,
    /// `POST /v1/responses` request -> canonical.
    ResponsesToCanonical,
    /// `POST /api/chat` request -> canonical.
    OllamaToCanonical,
    /// `POST /v1beta/models/{model}:generateContent` request -> canonical.
    GeminiToCanonical,
    /// canonical response -> `chat.completion` JSON.
    CanonicalToOpenai,
    /// canonical chat -> Anthropic Messages request body.
    CanonicalToClaude,
    /// canonical chat -> OpenAI Responses request body.
    CanonicalToResponses,
    /// canonical chat -> Gemini `generateContent` request body. Also the
    /// antigravity body: the reference registers one function under both names, so
    /// the cell is one and `OutboundWire` carries two spellings of it.
    CanonicalToGemini,
    /// canonical chat -> Cursor ask/agent request body.
    CanonicalToCursor,
    /// canonical chat -> CLOVA Studio v3 request body.
    CanonicalToClova,
    /// canonical chat -> Kiro `conversationState` request body.
    CanonicalToKiro,
    /// canonical response -> Anthropic Messages response object.
    CanonicalToClaudeResponse,
    /// canonical response -> OpenAI Responses response object.
    CanonicalToResponsesResponse,
}

impl Pair {
    /// Every pair, in declaration order. The single source both
    /// [`supported_pairs`] and [`pair_named`] read, so the enum and the registry
    /// cannot drift apart.
    pub const ALL: &'static [Self] = &[
        Self::OpenaiToCanonical,
        Self::AnthropicToCanonical,
        Self::ResponsesToCanonical,
        Self::OllamaToCanonical,
        Self::GeminiToCanonical,
        Self::CanonicalToOpenai,
        Self::CanonicalToClaude,
        Self::CanonicalToResponses,
        Self::CanonicalToGemini,
        Self::CanonicalToCursor,
        Self::CanonicalToClova,
        Self::CanonicalToKiro,
        Self::CanonicalToClaudeResponse,
        Self::CanonicalToResponsesResponse,
    ];

    /// The stable `source-to-target` name, as it appears in
    /// [`TranslateError::UnsupportedPair`] and in reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiToCanonical => "openai-to-canonical",
            Self::AnthropicToCanonical => "anthropic-to-canonical",
            Self::ResponsesToCanonical => "responses-to-canonical",
            Self::OllamaToCanonical => "ollama-to-canonical",
            Self::GeminiToCanonical => "gemini-to-canonical",
            Self::CanonicalToOpenai => "canonical-to-openai",
            Self::CanonicalToClaude => "canonical-to-claude",
            Self::CanonicalToResponses => "canonical-to-openai-responses",
            Self::CanonicalToGemini => "canonical-to-gemini",
            Self::CanonicalToCursor => "canonical-to-cursor",
            Self::CanonicalToClova => "canonical-to-clova",
            Self::CanonicalToKiro => "canonical-to-kiro",
            Self::CanonicalToClaudeResponse => "canonical-to-claude-response",
            Self::CanonicalToResponsesResponse => "canonical-to-responses-response",
        }
    }
}

impl std::fmt::Display for Pair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The dialect conversions this build implements.
///
/// The registry exists so an unimplemented pair is a *typed* miss rather than a
/// missing function a caller discovers only by reaching for it and forwarding
/// whatever shape it had — which reaches the upstream as a plausible-looking
/// body that quietly says the wrong thing.
///
/// ```
/// use ar_translate::{pair_named, supported_pairs};
///
/// assert!(!supported_pairs().is_empty());
/// assert!(pair_named("openai-to-canonical").is_ok());
/// assert!(pair_named("canonical-to-claude").is_ok());
/// assert!(pair_named("claude-to-gemini").is_err());
/// ```
#[must_use]
pub fn supported_pairs() -> &'static [Pair] {
    Pair::ALL
}

/// Resolves a reference matrix cell name to a [`Pair`].
///
/// This is the fail-loud entry point: dialect detection produces a *name*, and
/// anything this build does not implement — including every cell
/// [`missing_pairs`] names — comes back as [`TranslateError::UnsupportedPair`]
/// naming what was asked for, rather than proceeding with a conversion that does
/// not exist.
///
/// # Errors
///
/// [`TranslateError::UnsupportedPair`] when `name` is not in
/// [`supported_pairs`]. Matching is case-insensitive, because the names are
/// operator-facing and reach a log line or a header.
pub fn pair_named(name: &str) -> Result<Pair, TranslateError> {
    Pair::ALL
        .iter()
        .copied()
        .find(|pair| pair.as_str().eq_ignore_ascii_case(name.trim()))
        .ok_or_else(|| TranslateError::UnsupportedPair {
            pair: name.trim().to_owned(),
        })
}

/// A cell in the reference's dialect matrix that this build does **not**
/// implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingPair {
    /// The cell's `source-to-target` name, as `pair_named` is called with it.
    pub name: &'static str,
    /// The OmniRoute file to read when the cell is taken on.
    ///
    /// Carried as data rather than prose so the coverage test can hold this
    /// build to it: deleting an entry without implementing the pair fails a
    /// test, which is the only thing stopping the list from quietly becoming a
    /// wish list.
    pub reference: &'static str,
}

/// The reference cells this build does not implement.
///
/// Derived from the `register(...)` calls in
/// `../OmniRoute/open-sse/translator/{request,response}/`, read against
/// `formats.ts`, minus the fifteen in [`supported_pairs`]. Two adjustments to
/// that raw list, both stated so the count is traceable rather than asserted:
///
/// * `antigravity` is `gemini` under a second dialect name — the reference
///   registers the *same function* for both — so its four cells are omitted here
///   and their `reference` is the gemini file. Implementing one implements the
///   other; listing both would inflate the count without adding a cell.
/// * `ollama` is not registered in `translator/` at all; its inbound adapter is
///   [`Pair::OllamaToCanonical`] and its outbound response lives at
///   `open-sse/utils/ollamaTransform.ts`, outside the matrix.
///
/// # What is left, and why
///
/// * Every cell whose reference mapper is a **chunk-level state machine**:
///   `canonical-to-claude-response` and `canonical-to-gemini-response` need
///   block-index allocation, incremental tool-call argument accumulation and
///   deferred terminal emission, and this crate's response mapper is
///   non-streaming so there is no cross-chunk state to accumulate into. The two
///   non-streaming response mappers that *were* ported — the Claude and
///   Responses envelopes — are ported as envelopes, not as state machines.
/// * `canonical-to-responses-response`'s tool items, for the same reason:
///   canonical carries no tool registry to emit a `function_call` item from.
/// * **Every remaining `*-to-openai` response cell.** These are the *response*
///   direction: a provider's SSE body has to be re-framed into the INBOUND
///   dialect, and that path is a byte-relay today (see
///   `ar-server/src/exec.rs`). Six of them — `cursor-response-to-openai` in
///   particular — are a no-op in the reference itself, because the executor
///   already emits OpenAI frames; the other five are the chunk-level state
///   machines above. Until the relay path carries a re-framing stage they stay
///   named rather than half-ported: a partial re-framer drops tool calls.
/// * `claude-to-gemini` — a cross-dialect hop with no hub. Canonical *is* the hub
///   here, so this cell is claude-to-canonical then canonical-to-gemini, both of
///   which are implemented, and it needs nothing of its own.
#[must_use]
pub fn missing_pairs() -> &'static [MissingPair] {
    &[
        // ── Response direction: a provider's wire -> the inbound dialect ──
        // Each is a re-framer over the response stream. The relay in
        // `ar-server/src/exec.rs` hands the client's bytes straight through
        // today, so a non-OpenAI provider answers an Anthropic or Responses
        // client in the provider's own framing.
        MissingPair {
            name: "claude-response-to-openai",
            reference: "OmniRoute/open-sse/translator/response/claude-to-openai.ts",
        },
        MissingPair {
            name: "gemini-response-to-openai",
            reference: "OmniRoute/open-sse/translator/response/gemini-to-openai.ts",
        },
        MissingPair {
            name: "responses-response-to-openai",
            reference: "OmniRoute/open-sse/translator/response/openai-responses.ts",
        },
        MissingPair {
            name: "kiro-response-to-openai",
            reference: "OmniRoute/open-sse/translator/response/kiro-to-openai.ts",
        },
        MissingPair {
            name: "cursor-response-to-openai",
            reference: "OmniRoute/open-sse/translator/response/cursor-to-openai.ts (a passthrough in the reference too: the executor already emits OpenAI chunks, so the cell is empty only because ar's relay has no re-framing stage)",
        },
        MissingPair {
            name: "clova-response-to-openai",
            reference: "OmniRoute/open-sse/translator/response/clova-to-openai.ts",
        },
        // ── Response direction: canonical response -> a non-OpenAI inbound wire ──
        // Both reference mappers are chunk-level state machines: block-index
        // allocation, incremental tool-call argument accumulation, deferred
        // terminal emission until a trailing usage-only chunk arrives. The
        // non-streaming envelopes are ported; the streaming re-framers are not.
        MissingPair {
            name: "canonical-to-claude-response-stream",
            reference: "OmniRoute/open-sse/translator/response/openai-to-claude.ts",
        },
        MissingPair {
            name: "canonical-to-gemini-response",
            reference: "OmniRoute/open-sse/translator/response/openai-to-antigravity.ts (re-registered by openai-to-gemini.ts; streaming SSE in openai-to-gemini-sse.ts)",
        },
        MissingPair {
            name: "canonical-to-responses-response-stream",
            reference: "OmniRoute/open-sse/translator/response/openai-responses.ts + responsesToolItem.ts",
        },
        MissingPair {
            name: "canonical-to-ollama-response",
            reference: "OmniRoute/open-sse/utils/ollamaTransform.ts (outside translator/, so not a registered matrix cell)",
        },
        // ── Cross-dialect hop with no hub of its own ──
        MissingPair {
            name: "claude-to-gemini",
            reference: "OmniRoute/open-sse/translator/request/claude-to-gemini.ts (canonical is the hub: claude-to-canonical then canonical-to-gemini, both implemented)",
        },
    ]
}

/// Ways a canonical translation can fail.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// The request named no model.
    #[error("request model must not be empty")]
    EmptyModel,
    /// The request carried no messages.
    #[error("request carries no messages")]
    EmptyMessages,
    /// A message used a role no provider accepts.
    #[error("unrecognised message role: {role:?}")]
    UnknownRole {
        /// The offending wire role, verbatim.
        role: String,
    },
    /// A message used a content part this build cannot translate.
    #[error("unsupported content part type {kind:?}: not a routed modality")]
    UnsupportedPart {
        /// The offending part discriminator, verbatim.
        kind: String,
    },
    /// The requested conversion is not implemented by this build.
    ///
    /// Fail-loud by construction: a caller reaching for a dialect cell this crate
    /// has no conversion for gets this, naming the cell, instead of forwarding
    /// whatever shape it had. See [`missing_pairs`] for the cell and the
    /// reference file that implements it.
    #[error("dialect pair {pair:?} is not implemented by this build")]
    UnsupportedPair {
        /// The requested cell name, verbatim.
        pair: String,
    },
}

/// Inbound wire -> canonical.
///
/// Generic over `In` so the impl is static: `OpenAiInbound::to_canonical(chat)`
/// monomorphises and inlines. `ar-core::ArTranslate` is the cross-crate seam
/// and carries the same method name, but its signature is a `serde_json::Value`
/// placeholder pending this crate's concrete types.
///
/// # TODO(#p0-align)
/// Reconcile with `ar_core::ArTranslate` once `ar-route`/`ar-cli` call the typed
/// form; the placeholder cannot express streaming or the canonical struct, so
/// both seams cannot coexist long.
pub trait ArTranslate<In> {
    /// Normalises one inbound request into canonical shape.
    fn to_canonical(input: In) -> Result<CanonicalChat, TranslateError>;
}

/// The P0 inbound adapter: OpenAI chat completions.
///
/// A zero-sized marker rather than a value, so the trait stays generic and
/// there is no vtable to build. Each additional dialect is the same shape: a
/// zero-sized marker and one `impl`.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiInbound;

/// The P2 inbound adapter: Anthropic Messages (`POST /v1/messages`).
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicInbound;

/// The P2 inbound adapter: OpenAI Responses (`POST /v1/responses`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ResponsesInbound;

/// The P2 inbound adapter: Ollama chat (`POST /api/chat`).
#[derive(Debug, Clone, Copy, Default)]
pub struct OllamaInbound;

/// The Gemini inbound adapter: `generateContent`.
///
/// A fifth marker in the same shape as the other four — the dialect's model id
/// comes off the URL path rather than the body, which is why
/// [`gemini_to_canonical`] takes the extracted id rather than reading it from the
/// JSON.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiInbound;

impl ArTranslate<OpenAIChat> for OpenAiInbound {
    fn to_canonical(input: OpenAIChat) -> Result<CanonicalChat, TranslateError> {
        if input.model.trim().is_empty() {
            return Err(TranslateError::EmptyModel);
        }
        if input.messages.is_empty() {
            return Err(TranslateError::EmptyMessages);
        }

        let messages = input
            .messages
            .into_iter()
            .map(msg_to_canonical)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(CanonicalChat {
            model: input.model,
            messages,
            temperature: input.temperature,
            max_tokens: input.max_tokens.or(input.max_completion_tokens),
            stream: input.stream.unwrap_or(false),
        })
    }
}

impl ArTranslate<AnthropicMessages> for AnthropicInbound {
    fn to_canonical(input: AnthropicMessages) -> Result<CanonicalChat, TranslateError> {
        require_model(&input.model)?;
        if input.messages.is_empty() {
            return Err(TranslateError::EmptyMessages);
        }

        // Anthropic carries the system prompt outside the array, so canonical
        // needs a synthesised leading turn. A body whose system text is blank
        // gets no turn at all — an empty system turn is noise, not information.
        let mut messages = Vec::with_capacity(input.messages.len() + 1);
        if let Some(system) = input.system.as_ref() {
            let text = system.to_text();
            if !text.is_empty() {
                messages.push(Msg::new(Role::System, text));
            }
        }

        for msg in input.messages {
            let role = anthropic_role(&msg.role)?;
            let split = match &msg.content {
                AnthropicContent::Text(text) => AnthropicSplit {
                    tool_msgs: Vec::new(),
                    text: text.clone(),
                    media: Vec::new(),
                },
                AnthropicContent::Blocks(blocks) => anthropic_blocks(blocks)?,
            };
            // `tool_result` blocks become their own turns, emitted before the
            // parent so the correlation reads in order. A parent left with no
            // text *and* no media after that is dropped rather than sent as an
            // empty turn.
            messages.extend(split.tool_msgs);
            if !split.text.is_empty() || !split.media.is_empty() {
                messages.push(Msg {
                    role,
                    content: split.text,
                    media: split.media,
                });
            }
        }

        if messages.is_empty() {
            return Err(TranslateError::EmptyMessages);
        }

        Ok(CanonicalChat {
            model: input.model,
            messages,
            temperature: input.temperature,
            max_tokens: input.max_tokens,
            stream: input.stream.unwrap_or(false),
        })
    }
}

impl ArTranslate<ResponsesApi> for ResponsesInbound {
    fn to_canonical(input: ResponsesApi) -> Result<CanonicalChat, TranslateError> {
        require_model(&input.model)?;

        // `instructions` is the Responses spelling of a system prompt and
        // belongs at the front, matching the reference's message order.
        let mut messages = Vec::new();
        if let Some(instructions) = input.instructions.as_ref()
            && !instructions.is_empty()
        {
            messages.push(Msg::new(Role::System, instructions.clone()));
        }

        match input.input {
            // A bare string is a single user turn.
            ResponsesInput::Text(text) => {
                if !text.is_empty() {
                    messages.push(Msg::new(Role::User, text));
                }
            }
            ResponsesInput::Items(items) => {
                for item in items {
                    messages.push(responses_item(item)?);
                }
            }
        }

        if messages.is_empty() {
            return Err(TranslateError::EmptyMessages);
        }

        Ok(CanonicalChat {
            model: input.model,
            messages,
            temperature: input.temperature,
            max_tokens: input.max_output_tokens,
            stream: input.stream.unwrap_or(false),
        })
    }
}

impl ArTranslate<OllamaChat> for OllamaInbound {
    fn to_canonical(input: OllamaChat) -> Result<CanonicalChat, TranslateError> {
        require_model(&input.model)?;
        if input.messages.is_empty() {
            return Err(TranslateError::EmptyMessages);
        }

        // Ollama's system prompt is a top-level string, but clients also send
        // `role:"system"` turns; both land as system turns and order is
        // preserved either way.
        let mut messages = Vec::with_capacity(input.messages.len() + 1);
        if let Some(system) = input.system.as_ref()
            && !system.is_empty()
        {
            messages.push(Msg::new(Role::System, system.clone()));
        }

        for msg in input.messages {
            let role = Role::from_wire(&msg.role).ok_or_else(|| TranslateError::UnknownRole {
                role: msg.role.clone(),
            })?;
            // Ollama carries vision as a sibling `images` array rather than a
            // content part, so it becomes a carried part directly.
            let media = match msg.images {
                Some(images) if !images.is_empty() => vec![MediaPart::new("images", json!(images))],
                _ => Vec::new(),
            };
            messages.push(Msg {
                role,
                content: msg.content,
                media,
            });
        }

        Ok(CanonicalChat {
            model: input.model,
            messages,
            temperature: input.options.as_ref().and_then(|o| o.temperature),
            max_tokens: input.options.as_ref().and_then(|o| o.num_predict),
            stream: input.stream.unwrap_or(false),
        })
    }
}

/// Normalises one inbound OpenAI request.
///
/// Thin wrapper over [`ArTranslate::to_canonical`]; exists so call sites read
/// as `to_canonical(chat)` without a trait-qualified prefix. The other three
/// dialects get one named wrapper each rather than a generic function: a
/// single `to_canonical<In>` over four marker impls does not infer `In` from
/// the argument at a call site, and the wrappers are three lines each.
pub fn to_canonical(input: OpenAIChat) -> Result<CanonicalChat, TranslateError> {
    <OpenAiInbound as ArTranslate<OpenAIChat>>::to_canonical(input)
}

/// Normalises one inbound Anthropic Messages request.
pub fn anthropic_to_canonical(input: AnthropicMessages) -> Result<CanonicalChat, TranslateError> {
    <AnthropicInbound as ArTranslate<AnthropicMessages>>::to_canonical(input)
}

/// Normalises one inbound Responses request.
pub fn responses_to_canonical(input: ResponsesApi) -> Result<CanonicalChat, TranslateError> {
    <ResponsesInbound as ArTranslate<ResponsesApi>>::to_canonical(input)
}

/// Normalises one inbound Ollama chat request.
pub fn ollama_to_canonical(input: OllamaChat) -> Result<CanonicalChat, TranslateError> {
    <OllamaInbound as ArTranslate<OllamaChat>>::to_canonical(input)
}

impl ArTranslate<GeminiChat> for GeminiInbound {
    fn to_canonical(input: GeminiChat) -> Result<CanonicalChat, TranslateError> {
        require_model(&input.model)?;

        // `systemInstruction` is a content object outside the array, so canonical
        // needs the same synthesised leading turn Anthropic and Ollama get. A
        // prompt that is all whitespace gets no turn: an empty system turn is
        // noise, not information.
        let mut messages = Vec::with_capacity(input.contents.len() + 1);
        if let Some(system) = input.system_instruction.as_ref() {
            let text = gemini_text(&system.parts);
            if !text.is_empty() {
                messages.push(Msg::new(Role::System, text));
            }
        }

        for content in input.contents {
            messages.extend(gemini_content(content)?);
        }

        if messages.is_empty() {
            return Err(TranslateError::EmptyMessages);
        }

        let config = input.generation_config;
        Ok(CanonicalChat {
            model: input.model,
            messages,
            temperature: config.as_ref().and_then(|c| c.temperature),
            max_tokens: config.as_ref().and_then(|c| c.max_output_tokens),
            // `generateContent` streams, `generateContentStream` is a different
            // path — the model id the caller extracted already says which, so the
            // canonical flag mirrors the OpenAI one and the server decides. Kept
            // `false` because a body alone cannot distinguish them.
            stream: false,
        })
    }
}

/// Normalises one inbound Gemini `generateContent` request.
///
/// `input.model` is the id the caller lifted off the URL path; the Gemini wire
/// carries it nowhere in the body. See [`crate::gemini`].
pub fn gemini_to_canonical(input: GeminiChat) -> Result<CanonicalChat, TranslateError> {
    <GeminiInbound as ArTranslate<GeminiChat>>::to_canonical(input)
}

/// Turns one Gemini turn into zero or more canonical turns.
///
/// Gemini has no `role: "tool"`, so tool turns come out of the *parts* rather
/// than the turn, and one turn can produce several: the reference's
/// `splitCoLocatedFunctionResponses` emits one tool message per `functionResponse`
/// and hoists it ahead of whatever else the turn carried, because a tool result
/// must be read before the text that answers it. That split is reproduced here.
///
/// An empty result means the turn carried nothing a canonical turn can hold.
fn gemini_content(content: GeminiContent) -> Result<Vec<Msg>, TranslateError> {
    let role = match content.role.as_deref() {
        Some("model") => Role::Assistant,
        // The reference maps anything that is not exactly `"user"` to assistant.
        // Matching that would turn a typo'd role into a fabricated assistant turn,
        // so only the two wire roles are accepted and the rest is an error.
        Some("user") | None => Role::User,
        Some(other) => {
            return Err(TranslateError::UnknownRole {
                role: other.to_owned(),
            });
        }
    };

    let Some(parts) = content.parts else {
        return Ok(Vec::new());
    };

    // Owned, not `&str`: `parts` is consumed by the loop, so a borrow of
    // `part.text` would not outlive the iteration.
    let mut text: Vec<String> = Vec::new();
    let mut media = Vec::new();
    let mut tool_msgs = Vec::new();

    for part in parts {
        // A thought part is the model's own reasoning from a prior turn. The
        // reference splits it into `reasoning_content`; canonical has nowhere to
        // put that, and folding it into `content` would leak private reasoning
        // into the visible message. Reject it rather than misplace it.
        if part.thought == Some(true) {
            return Err(TranslateError::UnsupportedPart {
                kind: "thought".to_owned(),
            });
        }

        if let Some(call) = part.function_call.as_ref() {
            // A tool call names no modality, but the request that made it is the
            // caller's own, so it crosses as a carried part — the same rule the
            // Anthropic `tool_use` block follows.
            media.push(MediaPart::new("functionCall", json!(call)));
            continue;
        }

        if let Some(response) = part.function_response.as_ref() {
            tool_msgs.push(Msg::new(
                Role::Tool,
                gemini_function_response_text(response),
            ));
            continue;
        }

        if let Some(blob) = part.inline_data.as_ref() {
            // Carried verbatim as `inlineData`, Gemini's own spelling. The
            // reference rewrites this into an OpenAI `image_url` data URI; this
            // build does not, because `MediaPart` exists so a dialect's spelling
            // survives canonical rather than being re-encoded into a shape two of
            // the three dialects would then have to invent. See `MediaPart`.
            media.push(MediaPart::new("inlineData", json!(blob)));
            continue;
        }

        if let Some(value) = part.text {
            text.push(value);
        }
    }

    // Tool results first, mirroring the reference, then the turn they shared.
    let mut out = tool_msgs;
    if !text.is_empty() || !media.is_empty() {
        out.push(Msg {
            role,
            content: text.join("\n"),
            media,
        });
    }
    Ok(out)
}

/// Flattens a `functionResponse` payload to text.
///
/// The reference unwraps a `result` key when the response object has one and
/// stringifies the remainder otherwise. Canonical's [`Role::Tool`] turn carries
/// text only, so the JSON is stringified rather than parsed — a tool result that
/// reached the model as valid JSON text is what an OpenAI `tool` turn carries
/// anyway.
fn gemini_function_response_text(response: &crate::gemini::GeminiFunctionResponse) -> String {
    let payload = match response.response.as_ref() {
        Some(serde_json::Value::Object(fields)) if fields.contains_key("result") => fields
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        Some(other) => other.clone(),
        None => serde_json::json!({}),
    };
    payload.to_string()
}

/// Joins the text of a Gemini content object, skipping blank parts.
///
/// The reference concatenates with no separator (`map(p => p.text || "").join("")`).
/// A newline join is used here to match every other adapter in this crate, and
/// the two agree on every part that carries text — Gemini's `text` parts are
/// whole-paragraph units, not fragments of one string.
fn gemini_text(parts: &Option<Vec<GeminiPart>>) -> String {
    parts
        .iter()
        .flatten()
        .filter_map(|part| part.text.as_deref())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders a canonical response as OpenAI chat-completion JSON.
///
/// Returns `serde_json::Value` rather than a typed struct because the consumer
/// forwards it verbatim to the client: there is no second consumer to keep a
/// struct in sync with. `serde_json::Value` is the wire, not a shortcut.
pub fn to_openai(response: &CanonicalResponse) -> serde_json::Value {
    json!({
        "id": response.id,
        "object": "chat.completion",
        "created": response.created,
        "model": response.model,
        "choices": [{
            "index": 0,
            "message": {
                "role": response.message.role.as_wire(),
                "content": response.message.content,
            },
            "finish_reason": response.finish_reason.as_wire(),
        }],
        "usage": {
            "prompt_tokens": response.usage.prompt_tokens,
            "completion_tokens": response.usage.completion_tokens,
            "total_tokens": response.usage.total_tokens,
        },
    })
}

/// Rejects a blank model, shared by all three new adapters.
fn require_model(model: &str) -> Result<(), TranslateError> {
    if model.trim().is_empty() {
        return Err(TranslateError::EmptyModel);
    }
    Ok(())
}

/// Normalises one message, splitting text from carried media parts.
///
/// Text parts join on a newline, matching `ar-llm`'s `extractTextContent`.
/// Everything else becomes a [`MediaPart`] so the media adapter can forward it
/// byte-for-byte; a part naming no modality is rejected rather than dropped.
fn msg_to_canonical(msg: crate::openai::OpenAIMessage) -> Result<Msg, TranslateError> {
    let role = Role::from_wire(&msg.role).ok_or_else(|| TranslateError::UnknownRole {
        role: msg.role.clone(),
    })?;

    let (content, media) = match msg.content {
        None => (String::new(), Vec::new()),
        Some(OpenAIContent::Text(text)) => (text, Vec::new()),
        Some(OpenAIContent::Parts(parts)) => {
            let mut text = Vec::new();
            let mut media = Vec::new();
            for part in parts {
                if part.kind == "text" {
                    text.push(part.text.unwrap_or_default());
                    continue;
                }
                require_routable(&part.kind)?;
                media.push(MediaPart::new(part.kind, part.rest));
            }
            (text.join("\n"), media)
        }
    };

    Ok(Msg {
        role,
        content,
        media,
    })
}

/// Rejects a part that names no modality this build can route.
fn require_routable(kind: &str) -> Result<(), TranslateError> {
    if Modality::of_part(kind).is_some() {
        Ok(())
    } else {
        Err(TranslateError::UnsupportedPart {
            kind: kind.to_owned(),
        })
    }
}

/// Maps an Anthropic wire role onto canonical. Only `user` and `assistant` are
/// wire roles here; `system` exists in this dialect only as the `system` field.
fn anthropic_role(raw: &str) -> Result<Role, TranslateError> {
    match raw {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        other => Err(TranslateError::UnknownRole {
            role: other.to_owned(),
        }),
    }
}

/// Splits one Anthropic turn's blocks into (tool turns, text, media).
///
/// `text` accumulates into the parent turn; `tool_result` becomes its own tool
/// turn; `tool_use` and `image` cross as carried parts. `thinking` is still
/// rejected — it is provider-internal bookkeeping with no place in the request
/// this proxy makes, and forwarding it would leak a prior turn's reasoning.
fn anthropic_blocks(blocks: &[AnthropicBlock]) -> Result<AnthropicSplit, TranslateError> {
    let mut tool_msgs = Vec::new();
    let mut text_parts: Vec<&str> = Vec::new();
    let mut media = Vec::new();

    for block in blocks {
        match block.kind.as_str() {
            "text" => text_parts.push(block.text.as_deref().unwrap_or_default()),
            "tool_result" => tool_msgs.push(Msg::new(
                Role::Tool,
                block
                    .content
                    .as_ref()
                    .map(tool_result_text)
                    .transpose()?
                    .unwrap_or_default(),
            )),
            // A tool call names no modality, but it is a request the caller made
            // and dropping it would turn a tool-using turn into a bare one, so
            // it crosses as a carried part rather than an error.
            "tool_use" => media.push(MediaPart::new("tool_use", json!(block))),
            other => {
                require_routable(other)?;
                media.push(MediaPart::new(other, json!(block)));
            }
        }
    }

    Ok(AnthropicSplit {
        tool_msgs,
        text: text_parts.join("\n"),
        media,
    })
}

/// One Anthropic turn split into its three canonical projections.
struct AnthropicSplit {
    /// Turns lifted out of the parent by `tool_result` blocks.
    tool_msgs: Vec<Msg>,
    /// Joined text blocks.
    text: String,
    /// Carried non-text blocks, in order.
    media: Vec<MediaPart>,
}

/// Flattens a `tool_result` payload to text, mirroring the reference: a string
/// verbatim, a block array walked for its text parts, anything else stringified.
fn tool_result_text(content: &AnthropicBlockContent) -> Result<String, TranslateError> {
    match content {
        AnthropicBlockContent::Text(text) => Ok(text.clone()),
        AnthropicBlockContent::Blocks(blocks) => Ok(anthropic_blocks(blocks)?.text),
        AnthropicBlockContent::Other(value) => Ok(value.to_string()),
    }
}

/// Normalises one Responses input item into a canonical turn.
fn responses_item(item: ResponsesItem) -> Result<Msg, TranslateError> {
    if !item.is_message() {
        return Err(TranslateError::UnsupportedPart {
            kind: item.kind_name().to_owned(),
        });
    }

    // `agent_message` is the reference's spelling for an assistant turn.
    let role = match item.role.as_deref() {
        Some("agent_message") => Role::Assistant,
        Some(raw) => Role::from_wire(raw).ok_or_else(|| TranslateError::UnknownRole {
            role: raw.to_owned(),
        })?,
        None => {
            return Err(TranslateError::UnknownRole {
                role: String::new(),
            });
        }
    };

    let (content, media) = match item.content {
        None => (String::new(), Vec::new()),
        Some(ResponsesItemContent::Text(text)) => (text, Vec::new()),
        Some(ResponsesItemContent::Parts(parts)) => {
            let mut text = Vec::new();
            let mut media = Vec::new();
            for part in parts {
                match part.kind.as_str() {
                    // `output_text` is the assistant's own prior output;
                    // `refusal` carries its text in `refusal` rather than `text`.
                    "input_text" | "output_text" => text.push(part.text.unwrap_or_default()),
                    "refusal" => text.push(part.refusal.unwrap_or_default()),
                    other => {
                        require_routable(other)?;
                        media.push(MediaPart::new(other, part.rest));
                    }
                }
            }
            (text.join("\n"), media)
        }
    };

    Ok(Msg {
        role,
        content,
        media,
    })
}

/// The modality a canonical request needs, decided by its parts.
///
/// The first media part wins, and the order is the client's — a prompt that
/// leads with text and trails an image is a vision request either way, so there
/// is nothing to arbitrate between them.
#[must_use]
pub fn chat_modality(chat: &CanonicalChat) -> Modality {
    chat.messages
        .iter()
        .flat_map(|msg| &msg.media)
        .find_map(Modality::of_media)
        .unwrap_or(Modality::Text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_openai_chat_when_valid() {
        let raw = r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hi"}],
                      "temperature":0.5,"max_tokens":64,"stream":true}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        let canonical = to_canonical(chat).expect("fixture is translatable");

        assert_eq!(canonical.model, "gpt-5.4");
        assert_eq!(canonical.messages, vec![Msg::new(Role::User, "hi")]);
        assert_eq!(canonical.temperature, Some(0.5));
        assert_eq!(canonical.max_tokens, Some(64));
        assert!(canonical.stream);
    }

    #[test]
    fn converts_anthropic_when_valid() {
        let raw = r#"{"model":"claude-sonnet-4-5","max_tokens":1024,
            "system":[{"type":"text","text":"be terse"},{"type":"text","text":"and kind"}],
            "messages":[
                {"role":"user","content":"add 2 and 3"},
                {"role":"assistant","content":[
                    {"type":"text","text":"calling"},
                    {"type":"tool_result","tool_use_id":"tu_1","content":"5"}]},
                {"role":"user","content":"thanks"}],
            "temperature":0.2,"stream":true}"#;
        let req: AnthropicMessages =
            serde_json::from_str(raw).expect("fixture is valid Anthropic Messages");

        let canonical = anthropic_to_canonical(req).expect("fixture is translatable");

        assert_eq!(
            canonical.messages,
            vec![
                Msg::new(Role::System, "be terse\nand kind"),
                Msg::new(Role::User, "add 2 and 3"),
                Msg::new(Role::Tool, "5"),
                Msg::new(Role::Assistant, "calling"),
                Msg::new(Role::User, "thanks"),
            ]
        );
    }

    #[test]
    fn converts_responses_when_valid() {
        let raw = r#"{"model":"gpt-5.4","instructions":"be terse",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"message","role":"agent_message","content":"hello"}],
            "max_output_tokens":256,"temperature":0.1,"stream":true}"#;
        let req: ResponsesApi = serde_json::from_str(raw).expect("fixture is valid Responses");

        let canonical = responses_to_canonical(req).expect("fixture is translatable");

        assert_eq!(
            canonical.messages,
            vec![
                Msg::new(Role::System, "be terse"),
                Msg::new(Role::User, "hi"),
                Msg::new(Role::Assistant, "hello"),
            ]
        );
    }

    #[test]
    fn converts_ollama_when_valid() {
        let raw = r#"{"model":"llama3.2","system":"be terse",
            "messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"}],
            "options":{"temperature":0.4,"num_predict":128},"stream":true}"#;
        let req: OllamaChat = serde_json::from_str(raw).expect("fixture is valid Ollama chat");

        let canonical = ollama_to_canonical(req).expect("fixture is translatable");

        assert_eq!(canonical.max_tokens, Some(128));
    }

    #[test]
    fn defaults_stream_when_absent() {
        let raw = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        assert!(!to_canonical(chat).expect("fixture is translatable").stream);
    }

    #[test]
    fn maps_max_completion_tokens_when_legacy_absent() {
        let raw = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],
                      "max_completion_tokens":128}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        assert_eq!(
            to_canonical(chat)
                .expect("fixture is translatable")
                .max_tokens,
            Some(128)
        );
    }

    #[test]
    fn rejects_blank_model_when_missing() {
        let raw = r#"{"model":"  ","messages":[{"role":"user","content":"hi"}]}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        assert!(matches!(
            to_canonical(chat),
            Err(TranslateError::EmptyModel)
        ));
    }

    #[test]
    fn rejects_request_when_messages_empty() {
        let raw = r#"{"model":"m","messages":[]}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        assert!(matches!(
            to_canonical(chat),
            Err(TranslateError::EmptyMessages)
        ));
    }

    #[test]
    fn rejects_unrecognised_role() {
        let raw = r#"{"model":"m","messages":[{"role":"wizard","content":"hi"}]}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        assert!(matches!(
            to_canonical(chat),
            Err(TranslateError::UnknownRole { role }) if role == "wizard"
        ));
    }

    #[test]
    fn carries_image_part_when_openai_uses_image_url() {
        let raw = r#"{"model":"m","messages":[{"role":"user","content":[
                      {"type":"text","text":"what?"},
                      {"type":"image_url","image_url":{"url":"http://x"}}]}]}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        let canonical = to_canonical(chat).expect("image parts are routable since P6");

        assert_eq!(canonical.messages[0].content, "what?");
    }

    #[test]
    fn rejects_content_part_when_modality_unknown() {
        let raw = r#"{"model":"m","messages":[{"role":"user","content":[
                      {"type":"realtime_audio","audio":"x"}]}]}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        assert!(matches!(
            to_canonical(chat),
            Err(TranslateError::UnsupportedPart { kind }) if kind == "realtime_audio"
        ));
    }

    #[test]
    fn accepts_null_content_when_tool_turn() {
        let raw =
            r#"{"model":"m","messages":[{"role":"tool","content":null,"tool_call_id":"c1"}]}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        let canonical = to_canonical(chat).expect("fixture is translatable");
        assert_eq!(canonical.messages, vec![Msg::new(Role::Tool, "")]);
    }

    #[test]
    fn carries_anthropic_tool_use_when_modality_none() {
        let raw = r#"{"model":"m","messages":[{"role":"assistant","content":[
                      {"type":"tool_use","id":"tu_1","name":"add","input":{}}]}]}"#;
        let req: AnthropicMessages =
            serde_json::from_str(raw).expect("fixture is valid Anthropic Messages");

        let canonical = anthropic_to_canonical(req).expect("tool_use is carried since P6");

        assert_eq!(canonical.messages[0].media[0].kind, "tool_use");
    }

    #[test]
    fn carries_responses_input_image_when_modality_routable() {
        let raw = r#"{"model":"m","input":[{"type":"message","role":"user",
                      "content":[{"type":"input_image","image_url":"http://x"}]}]}"#;
        let req: ResponsesApi = serde_json::from_str(raw).expect("fixture is valid Responses");

        let canonical = responses_to_canonical(req).expect("input_image is routable since P6");

        assert_eq!(canonical.messages[0].media[0].kind, "input_image");
    }

    #[test]
    fn carries_ollama_images_when_modality_routable() {
        let raw = r#"{"model":"m","messages":[{"role":"user","content":"hi","images":["AAA"]}]}"#;
        let req: OllamaChat = serde_json::from_str(raw).expect("fixture is valid Ollama chat");

        let canonical = ollama_to_canonical(req).expect("images are routable since P6");

        assert_eq!(canonical.messages[0].media[0].kind, "images");
    }

    #[test]
    fn rejects_anthropic_thinking_when_not_routable() {
        let raw = r#"{"model":"m","messages":[{"role":"assistant","content":[
                      {"type":"thinking","thinking":"hmm"}]}]}"#;
        let req: AnthropicMessages =
            serde_json::from_str(raw).expect("fixture is valid Anthropic Messages");

        assert!(matches!(
            anthropic_to_canonical(req),
            Err(TranslateError::UnsupportedPart { kind }) if kind == "thinking"
        ));
    }

    #[test]
    fn reports_vision_when_chat_carries_image_part() {
        let raw = r#"{"model":"m","messages":[{"role":"user","content":[
                      {"type":"image_url","image_url":{"url":"http://x"}}]}]}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        let canonical = to_canonical(chat).expect("image parts are routable since P6");

        assert_eq!(chat_modality(&canonical), Modality::Vision);
    }

    #[test]
    fn reports_text_when_chat_has_no_media_parts() {
        let raw = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let chat: OpenAIChat = serde_json::from_str(raw).expect("fixture is valid OpenAI chat");

        assert_eq!(
            chat_modality(&to_canonical(chat).expect("text is translatable")),
            Modality::Text
        );
    }

    #[test]
    fn converts_bare_string_input_when_responses_prompt_only() {
        let raw = r#"{"model":"m","input":"just this"}"#;
        let req: ResponsesApi = serde_json::from_str(raw).expect("fixture is valid Responses");

        let canonical = responses_to_canonical(req).expect("fixture is translatable");
        assert_eq!(canonical.messages, vec![Msg::new(Role::User, "just this")]);
    }

    // ── Gemini inbound ──

    fn gemini(raw: &str) -> Result<CanonicalChat, TranslateError> {
        let mut req: GeminiChat =
            serde_json::from_str(raw).expect("fixture is valid Gemini generateContent");
        // The Gemini wire carries the model in the URL path, so the server fills
        // the field in; the fixtures omit it and set it here, as the server does.
        req.model = "gemini-2.5-flash".to_owned();
        gemini_to_canonical(req)
    }

    #[test]
    fn converts_gemini_when_valid() {
        let canonical = gemini(
            r#"{"systemInstruction":{"parts":[{"text":"be terse"}]},
                "contents":[
                  {"role":"user","parts":[{"text":"add 2 and 3"}]},
                  {"role":"model","parts":[{"text":"calling"}]}],
                "generationConfig":{"temperature":0.3,"maxOutputTokens":512}}"#,
        )
        .expect("fixture is translatable");

        assert_eq!(
            canonical.messages,
            vec![
                Msg::new(Role::System, "be terse"),
                Msg::new(Role::User, "add 2 and 3"),
                Msg::new(Role::Assistant, "calling"),
            ]
        );
    }

    #[test]
    fn reads_gemini_generation_config_when_present() {
        let canonical = gemini(
            r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],
                "generationConfig":{"temperature":0.3,"maxOutputTokens":512}}"#,
        )
        .expect("fixture is translatable");

        assert_eq!(canonical.temperature, Some(0.3));
        assert_eq!(canonical.max_tokens, Some(512));
    }

    /// A `functionResponse` carries no `tool_call_id` on this wire, so the
    /// correlation id is dropped and only the payload crosses. The turn must still
    /// come out as a tool turn, ahead of the text it was co-located with —
    /// reading order, not wire order.
    #[test]
    fn hoists_gemini_function_response_ahead_of_its_turn() {
        let canonical = gemini(
            r#"{"contents":[{"role":"user","parts":[
                  {"text":"and now"},
                  {"functionResponse":{"name":"add","response":{"result":5}}}]}]}"#,
        )
        .expect("fixture is translatable");

        assert_eq!(
            canonical.messages,
            vec![Msg::new(Role::Tool, "5"), Msg::new(Role::User, "and now")]
        );
    }

    #[test]
    fn carries_gemini_inline_image_when_modality_routable() {
        let canonical = gemini(
            r#"{"contents":[{"role":"user","parts":[
                  {"text":"what?"},
                  {"inlineData":{"mimeType":"image/png","data":"AAA"}}]}]}"#,
        )
        .expect("inlineData is routable since P6");

        assert_eq!(canonical.messages[0].content, "what?");
        assert_eq!(canonical.messages[0].media[0].kind, "inlineData");
    }

    #[test]
    fn carries_gemini_function_call_when_modality_none() {
        let canonical = gemini(
            r#"{"contents":[{"role":"model","parts":[
                  {"functionCall":{"name":"add","args":{"a":1}}}]}]}"#,
        )
        .expect("functionCall is carried since P6");

        assert_eq!(canonical.messages[0].media[0].kind, "functionCall");
    }

    /// Thought parts are the model's own reasoning from a prior turn. Folding them
    /// into `content` would leak it into the visible message, which is the reason
    /// the reference routes them to `reasoning_content` — a field canonical does
    /// not have.
    #[test]
    fn rejects_gemini_thought_when_modality_none() {
        assert!(matches!(
            gemini(
                r#"{"contents":[{"role":"model","parts":[
                      {"text":"hmm","thought":true}]}]}"#
            ),
            Err(TranslateError::UnsupportedPart { kind }) if kind == "thought"
        ));
    }

    #[test]
    fn rejects_gemini_when_contents_empty() {
        assert!(matches!(
            gemini(r#"{"contents":[]}"#),
            Err(TranslateError::EmptyMessages)
        ));
    }

    #[test]
    fn rejects_gemini_when_role_unrecognised() {
        assert!(matches!(
            gemini(r#"{"contents":[{"role":"system","parts":[{"text":"hi"}]}]}"#),
            Err(TranslateError::UnknownRole { role }) if role == "system"
        ));
    }

    /// A body whose URL-path id was never lifted reaches the adapter with an empty
    /// model, and must fail there rather than at the upstream.
    #[test]
    fn rejects_gemini_when_model_never_lifted_from_the_path() {
        let req: GeminiChat =
            serde_json::from_str(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#)
                .expect("fixture is valid Gemini");

        assert!(matches!(
            gemini_to_canonical(req),
            Err(TranslateError::EmptyModel)
        ));
    }

    #[test]
    fn reports_vision_when_gemini_carries_inline_image() {
        let canonical = gemini(
            r#"{"contents":[{"role":"user","parts":[
                  {"inlineData":{"mimeType":"image/png","data":"AAA"}}]}]}"#,
        )
        .expect("inlineData is routable since P6");

        assert_eq!(chat_modality(&canonical), Modality::Vision);
    }

    // ── Pair registry ──

    /// Every cell the reference registers and this build does not implement must
    /// be missing *and* must error. A pair that quietly became supported without a
    /// conversion behind it is the exact defect the registry exists to prevent, so
    /// the assertion is on behaviour (`pair_named` errors) and not only on the
    /// list's contents.
    ///
    /// The count is deliberately not asserted as a literal. It is derived from
    /// the reference's `register(...)` calls minus the antigravity aliases of
    /// gemini (see [`missing_pairs`]), so pinning a number here would break on a
    /// legitimate reference change without saying anything about behaviour. What
    /// must hold is that the list is non-empty, every entry names the file that
    /// implements it, and every entry errors.
    #[test]
    fn errors_for_every_missing_pair_in_the_reference_matrix() {
        let missing = missing_pairs();
        assert!(
            !missing.is_empty(),
            "the matrix is not empty and this build misses cells"
        );

        for pair in missing {
            let err = pair_named(pair.name)
                .expect_err("a listed missing pair must not resolve to an implementation");
            assert!(
                matches!(&err, TranslateError::UnsupportedPair { pair: name } if name == pair.name)
            );
            assert!(
                !pair.reference.is_empty(),
                "{} names no reference file",
                pair.name
            );
        }
    }

    #[test]
    fn covers_the_whole_reference_matrix() {
        // Every name is listed exactly once. A duplicate would let one cell's
        // reference file drift from another's without any test noticing.
        let mut names: Vec<&str> = missing_pairs().iter().map(|m| m.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "a missing pair is listed twice");

        // The two lists partition what this build knows about: nothing is claimed
        // as implemented and missing at once, and nothing is supported without a
        // variant behind it.
        assert!(
            supported_pairs().len() == Pair::ALL.len(),
            "supported_pairs must be the whole enum, not a subset",
        );
    }

    #[test]
    fn resolves_every_supported_pair_by_name() {
        for pair in supported_pairs() {
            assert_eq!(
                pair_named(pair.as_str()).expect("a registered pair resolves"),
                *pair
            );
        }
    }

    /// The two lists must partition the reference matrix. A name in both means a
    /// conversion is claimed and missing at once, which is worse than either.
    #[test]
    fn supported_and_missing_pairs_never_overlap() {
        for pair in supported_pairs() {
            assert!(
                !missing_pairs().iter().any(|m| m.name == pair.as_str()),
                "{} is both implemented and listed as missing",
                pair.as_str()
            );
        }
    }

    #[test]
    fn errors_on_an_unknown_pair_name() {
        assert!(matches!(
            pair_named("no-such-dialect-to-canonical"),
            Err(TranslateError::UnsupportedPair { pair }) if pair == "no-such-dialect-to-canonical"
        ));
    }

    /// Every outbound renderer the dispatch table offers must be named here, or
    /// the registry would report a cell as missing while a provider dispatches on
    /// it. This is the one test that catches a renderer added to `OutboundWire`
    /// without a `Pair` behind it.
    ///
    /// `antigravity` is exempt because the reference registers one function under
    /// both names — implementing gemini implements it — and the next test holds
    /// that exemption to exactly one cell.
    #[test]
    fn names_every_outbound_renderer_in_the_pair_registry() {
        for wire in crate::OutboundWire::ALL {
            if *wire == crate::OutboundWire::Antigravity {
                continue;
            }
            let name = format!("canonical-to-{}", wire.as_str());
            assert!(
                pair_named(&name).is_ok(),
                "{name} has a renderer but no Pair: it would be reported missing while dispatching",
            );
        }
    }

    /// The antigravity and gemini renderers are one function, so the matrix claims
    /// one cell for it — naming both would count a cell twice, and the reference's
    /// own accounting does the same.
    #[test]
    fn keeps_one_pair_name_for_the_antigravity_gemini_twin() {
        assert!(pair_named("canonical-to-gemini").is_ok());
        assert!(
            pair_named("canonical-to-antigravity").is_err(),
            "the twin is one cell, not two"
        );
    }
}
