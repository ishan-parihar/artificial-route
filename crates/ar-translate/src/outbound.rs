//! Canonical -> provider wire: the outbound request mappers, and the two
//! non-streaming response mappers that close the loop for the inbound
//! dialects.
//!
//! # Why this module exists
//!
//! `ar-exec` renders exactly one wire. Everything a provider registry lists
//! under another [`crate::OutboundWire`] was previously refused at the gate, so
//! 24 providers could not dispatch at all. Each mapper below is one
//! `CanonicalChat` -> bytes function, mirroring the shape of the reference
//! mapper it was transcribed from without its provider-registry coupling.
//!
//! # What is deliberately not ported
//!
//! The reference mappers are sized by what their provider actually validates,
//! and most of that size is not translation:
//!
//! * **`openai-to-claude.ts`** — no `tools` / `tool_choice` (canonical has no
//!   tool registry, so a tool-using request reaches the upstream without its
//!   declarations), no `thinking` / `reasoning_effort` (canonical rejects both
//!   inbound), no `cache_control` breakpoints, no OAuth `proxy_` tool-name
//!   prefix, no empty-text-block scrubbing. `max_tokens` gets Anthropic's
//!   required default when canonical carries none. Image translation is ported
//!   (`openai-to-claude/imageBlocks.ts`, minus the nested `tool_result` walk),
//!   because forwarding the OpenAI spelling is a 400 rather than a loss.
//! * **`openai-responses.ts`** — no `function_call` / `function_call_output`
//!   items, no namespace-tool flattening, no `store` / `background` handling,
//!   no `text.format` -> `response_format` promotion, no
//!   `requiresPlainStringContent` collapse.
//! * **`openai-to-gemini.ts`** — no `tools` / `toolConfig`, no
//!   `thoughtSignature` bookkeeping, no `safetySettings` defaults, no
//!   `capMaxOutputTokens` per-model clamp, no tool-pair repair across turns.
//!   Image translation is ported as well (`inlineData` / `fileData`), because
//!   Gemini rejects the OpenAI `image_url` wrapper. The parts canonical carries —
//!   `inlineData`, `functionCall`, `functionResponse` — are already Gemini's own
//!   discriminators and cross unchanged.
//! * **`openai-to-cursor.ts`** — only its two structural rules survive: a system
//!   turn is re-spelled as a `[System Instructions]` user turn, and a tool turn
//!   is re-spelled as a `<tool_result>` block, because Cursor's ask/agent format
//!   has no `tool` role. The tool-name map it builds from assistant
//!   `tool_calls` is moot here (no tool registry), so the name is `tool`.
//! * **`openai-to-clova.ts`** — the live-verified mode/cap tables are keyed on
//!   the full OpenAI body (`tools`, `response_format`, `reasoning_effort`),
//!   none of which canonical carries, so this is always the `plain` mode: typed
//!   text parts, `maxTokens`, and temperature clamped to the vendor's 0..=1.
//!   The tool-mode message shape is ported, since the `tools` array is the only
//!   thing selecting it and canonical has none. Image parts use the reference's
//!   own `image_url` -> `imageUrl.url` / `dataUri.data` translation rather than
//!   a passthrough, because CLOVA 400s the OpenAI spelling.
//! * **`openai-to-kiro.ts`** — no tool specifications, no tool-result context
//!   blocks, no adaptive-thinking directive, and no `history` replay (Kiro's own
//!   conversion of a tool-bearing history needs a `userInputMessageContext`
//!   tools schema, so a multi-turn conversation sends its current turn only).
//!   The `conversationState` envelope and the deterministic `conversationId`
//!   are kept because they *are* the wire shape; the reference derives the id
//!   from a uuidv5 over the first 4000 characters of the first user turn, and
//!   this uses the same input without the uuidv5 machinery (`ar-translate` has
//!   no uuid dependency), so the id is a plain FNV-1a hash of the same seed.
//!   Same property — stable for a conversation, distinct across conversations
//!   — which is what the upstream cache needs. Image handling *is* ported
//!   (`images: [{format, source:{bytes}}]`, gated on a `claude` model id),
//!   because dropping an attachment the caller sent would be a silent loss
//!   rather than a visible gap.
//!
//!   Each of those is a *request capability this build does not have*, and each is
//!   visible: a tool-using request still reaches the provider, without its
//!   declarations, rather than being refused.
//!
//! # Media, per wire
//!
//! Canonical crosses a wire carrying media verbatim, which is right for Responses
//! and Cursor (both *are* the OpenAI spelling) and wrong for the other three,
//! which each have their own image shape and reject the OpenAI one. Those three
//! re-spell an `image_url` part through [`claude_part`], [`gemini_part`] and
//! [`clova_part`] respectively, and leave every other part type — `tool_use`,
//! `inlineData`, `functionCall` — alone, because those are already the target
//! wire's own spelling and re-encoding them would only risk losing a field.

use serde_json::{Map, Value, json};

use crate::canonical::{
    CanonicalChat, CanonicalResponse, FinishReason, MediaPart, Msg, Role, render_openai_body,
};

/// Which provider wire a dispatch must render into.
///
/// Deliberately not [`ar_registry::WireFormat`]: this crate has no registry
/// dependency, and the two disagree on purpose. `ar-registry` carries every
/// dialect the catalog *records*; this carries the ones this build can *send*.
/// `Custom` has no mapper and so has no variant here — a provider on it stays
/// `UnsupportedWire`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutboundWire {
    /// OpenAI chat completions. The default path, byte-identical to
    /// [`render_openai_body`].
    Openai,
    /// Anthropic messages (`POST /v1/messages`).
    Claude,
    /// OpenAI Responses (`POST /v1/responses`).
    Responses,
    /// Google Generative Language (`generateContent`).
    Gemini,
    /// Antigravity. The same `candidates` body Gemini uses — the reference's
    /// `openai-to-gemini.ts` is a 14-line re-registration of the antigravity
    /// twin, so one mapper serves both names.
    Antigravity,
    /// Cursor's ask/agent dialect.
    Cursor,
    /// Naver CLOVA Studio chat completions v3.
    Clova,
    /// Kiro's `conversationState` envelope.
    Kiro,
}

impl OutboundWire {
    /// Every wire this build can render, in the order the reference registers
    /// them. The single source [`render_for_wire`] matches on.
    pub const ALL: &'static [Self] = &[
        Self::Openai,
        Self::Claude,
        Self::Responses,
        Self::Gemini,
        Self::Antigravity,
        Self::Cursor,
        Self::Clova,
        Self::Kiro,
    ];

    /// The stable `canonical-to-*` name, as it appears in a report.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Claude => "claude",
            Self::Responses => "openai-responses",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
            Self::Cursor => "cursor",
            Self::Clova => "clova",
            Self::Kiro => "kiro",
        }
    }
}

impl std::fmt::Display for OutboundWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Renders `chat` into the bytes for `wire`.
///
/// The OpenAI arm is a delegation, not a reimplementation: it calls
/// [`render_openai_body`] so the default path stays byte-identical by
/// construction rather than by agreement between two copies of the same code.
///
/// ```
/// use ar_translate::{CanonicalChat, Msg, OutboundWire, Role, render_for_wire};
///
/// let chat = CanonicalChat {
///     model: "claude-sonnet-4-5".to_owned(),
///     messages: vec![
///         Msg::new(Role::System, "be terse"),
///         Msg::new(Role::User, "hi"),
///     ],
///     temperature: None,
///     max_tokens: None,
///     stream: true,
/// };
/// let body: serde_json::Value =
///     serde_json::from_slice(&render_for_wire(&chat, OutboundWire::Claude)).unwrap();
///
/// // The system turn left the message array for the top-level `system` block.
/// assert_eq!(body["system"][0]["text"], "be terse", "{body}");
/// // Anthropic rejects a body with no `max_tokens` at all.
/// assert_eq!(body["max_tokens"], 4096, "{body}");
/// ```
#[must_use]
pub fn render_for_wire(chat: &CanonicalChat, wire: OutboundWire) -> Vec<u8> {
    match wire {
        OutboundWire::Openai => render_openai_body(chat),
        OutboundWire::Claude => render_claude_body(chat),
        OutboundWire::Responses => render_openai_responses_body(chat),
        OutboundWire::Gemini | OutboundWire::Antigravity => render_gemini_body(chat),
        OutboundWire::Cursor => render_cursor_body(chat),
        OutboundWire::Clova => render_clova_body(chat),
        OutboundWire::Kiro => render_kiro_body(chat),
    }
}

/// Re-spells a media part into an Anthropic content block, or returns it as-is.
///
/// An image goes through [`claude_part`], because the OpenAI `image_url` spelling
/// is a 400 on this wire. Everything else crosses unchanged — an Anthropic
/// `tool_use` block is already this wire's own spelling and re-encoding it would
/// only risk losing a field.
fn claude_block(part: &MediaPart) -> Value {
    claude_part(part).unwrap_or_else(|| part.to_wire())
}

/// Re-spells a canonical media part into a Claude content block.
///
/// The reference's `openai-to-claude/imageBlocks.ts`, minus the nested
/// `tool_result` walk (canonical has no nested tool-result content to recurse
/// into — a `Role::Tool` turn carries text only). Returns `None` for a part that
/// is not an image, which [`claude_block`] passes through untouched.
fn claude_part(part: &MediaPart) -> Option<Value> {
    let block = match part.kind.as_str() {
        // `image_url` is OpenAI's spelling; Anthropic 400s it.
        "image_url" => {
            let url = part.body.get("image_url")?.get("url")?.as_str()?.trim();
            match data_url(url) {
                Some((media_type, bytes)) => json!({
                    "type": "image",
                    "source": { "type": "base64", "media_type": media_type, "data": bytes },
                }),
                // Not a base64 data URL, so it is a plain URL — Anthropic's own
                // `url` source, which is a real part of its schema.
                _ if !url.is_empty() => {
                    json!({ "type": "image", "source": { "type": "url", "url": url } })
                }
                _ => return None,
            }
        }
        // Already Anthropic's shape: `source` is its own, or an AI-SDK `image`.
        "image" => {
            if part.body.get("source").is_some_and(Value::is_object) {
                json!({ "type": "image", "source": part.body["source"].clone() })
            } else {
                let url = part.body.get("image")?.as_str()?.trim();
                match data_url(url) {
                    Some((media_type, bytes)) => json!({
                        "type": "image",
                        "source": { "type": "base64", "media_type": media_type, "data": bytes },
                    }),
                    _ => json!({ "type": "image", "source": { "type": "url", "url": url } }),
                }
            }
        }
        _ => return None,
    };
    Some(block)
}

/// Anthropic's floor when a request names no output ceiling.
///
/// The reference calls `adjustMaxTokens(body)`, which reads the OpenAI body's
/// `max_tokens` and falls back through the model table. Canonical has one
/// optional field and no model table, so the fallback is a single conservative
/// constant — Anthropic 400s a body with no `max_tokens`, so the field cannot
/// simply be omitted as it is on the OpenAI wire.
const CLAUDE_DEFAULT_MAX_TOKENS: u32 = 4096;

/// Kiro's fallback output ceiling, matching the reference's
/// `body.max_tokens ?? body.max_completion_tokens ?? 32000`.
const KIRO_DEFAULT_MAX_TOKENS: u32 = 32_000;

/// Renders `chat` as an Anthropic Messages request body.
///
/// System turns leave the message array and become the top-level `system`
/// block — the inverse of [`crate::anthropic_to_canonical`], and the reason that
/// function has to synthesise a leading system turn. A request whose turns were
/// *all* system would leave `messages` empty, which Anthropic rejects, so a
/// minimal user turn is synthesised exactly as the reference does (#5245).
///
/// Image parts are re-spelled through [`claude_part`]; a non-image part crosses
/// as itself, which is what keeps an Anthropic `tool_use` block intact.
#[must_use]
pub fn render_claude_body(chat: &CanonicalChat) -> Vec<u8> {
    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for msg in &chat.messages {
        if msg.role == Role::System {
            if !msg.content.is_empty() {
                system.push(json!({ "type": "text", "text": msg.content }));
            }
            continue;
        }
        // Anthropic has no `tool` role: a tool turn is a user turn carrying a
        // `tool_result` block. Canonical keeps no `tool_use_id`, so the block
        // carries the result text alone — documented above as the missing tool
        // registry, which is the same gap.
        let role = if msg.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let mut content: Vec<Value> = Vec::with_capacity(msg.media.len() + 1);
        if !msg.content.is_empty() {
            if msg.role == Role::Tool {
                content.push(json!({ "type": "tool_result", "content": msg.content }));
            } else {
                content.push(json!({ "type": "text", "text": msg.content }));
            }
        }
        content.extend(msg.media.iter().map(claude_block));
        if content.is_empty() {
            continue;
        }
        messages.push(json!({ "role": role, "content": content }));
    }

    if messages.is_empty() {
        messages.push(json!({
            "role": "user",
            "content": [{ "type": "text", "text": "." }],
        }));
    }

    let mut body = Map::with_capacity(5);
    body.insert("model".to_owned(), chat.model.as_str().into());
    body.insert(
        "max_tokens".to_owned(),
        chat.max_tokens.unwrap_or(CLAUDE_DEFAULT_MAX_TOKENS).into(),
    );
    body.insert("stream".to_owned(), chat.stream.into());
    body.insert("messages".to_owned(), Value::Array(messages));
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::Array(system));
    }
    if let Some(temperature) = chat.temperature {
        body.insert("temperature".to_owned(), temperature.into());
    }
    encode(Value::Object(body))
}

/// Renders `chat` as an OpenAI Responses request body.
///
/// The inverse of [`crate::responses_to_canonical`]: system turns become the
/// top-level `instructions` string, and every other turn becomes a
/// `{"type":"message"}` item whose parts use the `input_text` / `output_text`
/// discriminators keyed on the role.
///
/// Media is untouched: Responses parts are typed the way OpenAI's are, so an
/// `image_url` part is already this wire's own spelling.
#[must_use]
pub fn render_openai_responses_body(chat: &CanonicalChat) -> Vec<u8> {
    let mut instructions: Vec<&str> = Vec::new();
    let mut input: Vec<Value> = Vec::with_capacity(chat.messages.len());

    for msg in &chat.messages {
        if msg.role == Role::System {
            if !msg.content.is_empty() {
                instructions.push(&msg.content);
            }
            continue;
        }
        let part_type = if msg.role == Role::Assistant {
            "output_text"
        } else {
            "input_text"
        };
        let mut parts: Vec<Value> = Vec::with_capacity(msg.media.len() + 1);
        if !msg.content.is_empty() {
            parts.push(json!({ "type": part_type, "text": msg.content }));
        }
        // Responses parts are typed the same way OpenAI's are (`input_image` with
        // an `image_url`), so a carried part crosses unchanged — which is also
        // what keeps a `function_call` part intact, since Responses' own tool
        // spelling is the OpenAI one.
        parts.extend(msg.media.iter().map(MediaPart::to_wire));
        // A turn with neither text nor media is dropped rather than emitted with
        // an empty `content` array, which the wire has no spelling for.
        if parts.is_empty() {
            continue;
        }
        input.push(json!({
            "type": "message",
            "role": msg.role.as_wire(),
            "content": parts,
        }));
    }

    let mut body = Map::with_capacity(5);
    body.insert("model".to_owned(), chat.model.as_str().into());
    body.insert("input".to_owned(), Value::Array(input));
    if !instructions.is_empty() {
        body.insert("instructions".to_owned(), instructions.join("\n").into());
    }
    if let Some(max_output_tokens) = chat.max_tokens {
        body.insert("max_output_tokens".to_owned(), max_output_tokens.into());
    }
    body.insert("stream".to_owned(), chat.stream.into());
    if let Some(temperature) = chat.temperature {
        body.insert("temperature".to_owned(), temperature.into());
    }
    encode(Value::Object(body))
}

/// Renders `chat` as a Gemini `generateContent` request body.
///
/// The mirror of [`crate::gemini_to_canonical`]: system turns become
/// `systemInstruction`, `Role::User` and `Role::Tool` become `role: "user"`,
/// `Role::Assistant` becomes `role: "model"`, and the model id — which the
/// Gemini wire carries on the URL and nowhere in the body — is still emitted so
/// the body is self-describing for a log or a cache key.
///
/// A carried `functionCall` part round-trips to the `functionCall` key and a
/// carried `functionResponse` part to `functionResponse`; a canonical
/// `Role::Tool` turn has no part for it, so its text becomes a `text` part. That
/// is the one place a tool result is *not* a `functionResponse`, and it is
/// visible to the model as text rather than silently dropped.
#[must_use]
pub fn render_gemini_body(chat: &CanonicalChat) -> Vec<u8> {
    let mut system: Vec<Value> = Vec::new();
    let mut contents: Vec<Value> = Vec::with_capacity(chat.messages.len());

    for msg in &chat.messages {
        if msg.role == Role::System {
            if !msg.content.is_empty() {
                system.push(json!({ "text": msg.content }));
            }
            continue;
        }
        let role = if msg.role == Role::Assistant {
            "model"
        } else {
            "user"
        };
        let mut parts: Vec<Value> = Vec::with_capacity(msg.media.len() + 1);
        if !msg.content.is_empty() {
            parts.push(json!({ "text": msg.content }));
        }
        parts.extend(msg.media.iter().map(gemini_part));
        if parts.is_empty() {
            continue;
        }
        contents.push(json!({ "role": role, "parts": parts }));
    }

    if contents.is_empty() {
        contents.push(json!({ "role": "user", "parts": [{ "text": "." }] }));
    }

    let mut config = Map::with_capacity(2);
    if let Some(temperature) = chat.temperature {
        config.insert("temperature".to_owned(), temperature.into());
    }
    if let Some(max_output_tokens) = chat.max_tokens {
        config.insert("maxOutputTokens".to_owned(), max_output_tokens.into());
    }

    let mut body = Map::with_capacity(4);
    body.insert("model".to_owned(), chat.model.as_str().into());
    body.insert("contents".to_owned(), Value::Array(contents));
    body.insert("generationConfig".to_owned(), Value::Object(config));
    if !system.is_empty() {
        body.insert(
            "systemInstruction".to_owned(),
            json!({ "role": "system", "parts": system }),
        );
    }
    encode(Value::Object(body))
}

/// Re-spells a media part into a Gemini part, or returns it as-is.
///
/// Gemini's own discriminators — `inlineData`, `functionCall`, `functionResponse`
/// — are what canonical already carries verbatim (see
/// [`crate::gemini_to_canonical`]), so those cross untouched and this is the
/// identity. What it does add is the one shape Gemini has and OpenAI spells
/// differently: an `image_url`. Gemini takes `fileData` for a URI or `inlineData`
/// for bytes, and rejects the OpenAI wrapper around it.
fn gemini_part(part: &MediaPart) -> Value {
    if part.kind != "image_url" {
        return part.to_wire();
    }
    let Some(url) = part
        .body
        .get("image_url")
        .and_then(|u| u.get("url"))
        .and_then(Value::as_str)
    else {
        return part.to_wire();
    };
    // A `data:` URL splits into `data:<mime>[;params],<bytes>`, and only a
    // `;base64` parameter means the payload is bytes. Anything else is a URI.
    match data_url(url) {
        Some((mime, bytes)) => json!({ "inlineData": { "mimeType": mime, "data": bytes } }),
        None => json!({ "fileData": { "mimeType": "", "fileUri": url } }),
    }
}

/// Splits a base64 `data:` URL into `(media type, bytes)`.
///
/// Returns `None` for anything else — a plain `http` URL, or a `data:` URL with
/// no `;base64` parameter, which is a URI rather than a payload. One parser for
/// all four wires that need it, because they agree on the format and disagree
/// only on what they wrap the result in.
fn data_url(url: &str) -> Option<(&str, &str)> {
    let (header, bytes) = url.strip_prefix("data:")?.split_once(',')?;
    let (media_type, params) = header.split_once(';')?;
    if !params.split(';').any(|p| p == "base64") || bytes.is_empty() {
        return None;
    }
    Some((media_type, bytes))
}

/// Renders `chat` as a Cursor ask/agent request body.
///
/// Two structural rules from `openai-to-cursor.ts` survive, and both are forced
/// by Cursor having no `system` and no `tool` role:
///
/// * a system turn becomes a user turn prefixed `[System Instructions]`, so the
///   prompt still reaches the model as context rather than as a fabricated
///   user ask;
/// * a tool turn becomes a user turn carrying a `<tool_result>` block.
///
/// The reference resolves the tool name from a map built out of the assistant's
/// `tool_calls`; canonical keeps no tool registry, so the name is the `tool`
/// placeholder the reference itself falls back to.
///
/// Media is untouched: Cursor's dialect *is* the OpenAI one, so an `image_url`
/// part is already the right spelling and re-encoding it would only risk
/// dropping a field.
#[must_use]
pub fn render_cursor_body(chat: &CanonicalChat) -> Vec<u8> {
    let mut messages: Vec<Value> = Vec::with_capacity(chat.messages.len());

    for msg in &chat.messages {
        match msg.role {
            Role::System => messages.push(json!({
                "role": "user",
                "content": format!("[System Instructions]\n{}", msg.content),
            })),
            Role::Tool => messages.push(json!({
                "role": "user",
                "content": cursor_tool_result(&msg.content),
            })),
            Role::User | Role::Assistant => {
                if msg.media.is_empty() {
                    if !msg.content.is_empty() {
                        messages
                            .push(json!({ "role": msg.role.as_wire(), "content": msg.content }));
                    }
                    continue;
                }
                // Vision input survives: the reference keeps `image_url` parts as
                // an OpenAI content array so its executor can still see them, and
                // Cursor's dialect *is* the OpenAI one, so the part is unchanged.
                let mut parts: Vec<Value> = Vec::with_capacity(msg.media.len() + 1);
                if !msg.content.is_empty() {
                    parts.push(json!({ "type": "text", "text": msg.content }));
                }
                parts.extend(msg.media.iter().map(MediaPart::to_wire));
                messages.push(json!({ "role": msg.role.as_wire(), "content": parts }));
            }
        }
    }

    let mut body = Map::with_capacity(5);
    body.insert("model".to_owned(), chat.model.as_str().into());
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("stream".to_owned(), chat.stream.into());
    if let Some(temperature) = chat.temperature {
        body.insert("temperature".to_owned(), temperature.into());
    }
    if let Some(max_tokens) = chat.max_tokens {
        body.insert("max_tokens".to_owned(), max_tokens.into());
    }
    encode(Value::Object(body))
}

/// Builds Cursor's `<tool_result>` block, escaping the XML it contains.
fn cursor_tool_result(text: &str) -> String {
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    // The reference strips C0 control characters here: Cursor's backend 400s a
    // request body containing one. `\t` and the line breaks are kept because a
    // tool result is multi-line by nature and dropping them would join the lines.
    let clean: String = text
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t' | '\r'))
        .collect();
    format!(
        "<tool_result>\n<tool_name>tool</tool_name>\n<tool_call_id></tool_call_id>\n<result>{}</result>\n</tool_result>",
        escape(&clean)
    )
}

/// Renders `chat` as a CLOVA Studio v3 request body.
///
/// Always the reference's `plain` mode: its three modes are selected by `tools`,
/// `response_format` and `reasoning_effort`, none of which canonical carries, so
/// resolving a mode would be a constant. What is kept is the part that is a
/// property of the *wire* rather than of the request — typed text parts, the
/// `maxTokens` spelling, and the vendor's `0..=1` temperature clamp, which
/// live-verified rejection makes load-bearing.
#[must_use]
pub fn render_clova_body(chat: &CanonicalChat) -> Vec<u8> {
    let messages: Vec<Value> = chat
        .messages
        .iter()
        .filter(|m| !m.content.is_empty() || !m.media.is_empty())
        .map(|m| {
            let mut parts: Vec<Value> = Vec::with_capacity(m.media.len() + 1);
            if !m.content.is_empty() {
                parts.push(json!({ "type": "text", "text": m.content }));
            }
            // CLOVA 400s the OpenAI `{"type":"image_url","image_url":{…}}`
            // spelling, so an image is re-spelled the vendor's way: a `data:`
            // URL becomes `dataUri` with the full prefix intact, anything else
            // becomes `imageUrl`. A part with no readable URL is dropped rather
            // than forwarded in a shape the vendor rejects.
            parts.extend(m.media.iter().filter_map(clova_part));
            // The vendor rejects a message with an empty content array.
            if parts.is_empty() {
                parts.push(json!({ "type": "text", "text": "" }));
            }
            json!({ "role": clova_role(m.role), "content": parts })
        })
        .collect();

    let mut body = Map::with_capacity(4);
    body.insert("messages".to_owned(), Value::Array(messages));
    if let Some(max_tokens) = chat.max_tokens {
        body.insert(
            "maxTokens".to_owned(),
            max_tokens.min(CLOVA_MAX_OUTPUT_TOKENS).into(),
        );
    }
    if let Some(temperature) = chat.temperature {
        body.insert("temperature".to_owned(), temperature.clamp(0.0, 1.0).into());
    }
    encode(Value::Object(body))
}

/// Output cap the reference applies to CLOVA's non-reasoning v3 models.
const CLOVA_MAX_OUTPUT_TOKENS: u32 = 4096;

/// Re-spells a canonical media part into CLOVA v3's typed content part.
///
/// Returns `None` for anything without a readable URL: CLOVA has no part for a
/// bare tool-use block, and forwarding one would be a `40001`, so it is dropped
/// here and the module doc names it as a loss.
fn clova_part(part: &MediaPart) -> Option<Value> {
    if part.kind != "image_url" {
        return None;
    }
    let url = part.body.get("image_url")?.get("url")?.as_str()?;
    Some(if url.starts_with("data:") {
        // The full `data:<mime>;base64,` prefix has to survive; dropping it is
        // the live-verified rejection this branch exists to avoid.
        json!({ "type": "image_url", "dataUri": { "data": url } })
    } else {
        json!({ "type": "image_url", "imageUrl": { "url": url } })
    })
}

/// CLOVA's role vocabulary: it has no `tool` role, so a tool turn is a user one.
const fn clova_role(role: Role) -> &'static str {
    match role {
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::User | Role::Tool => "user",
    }
}

/// Renders `chat` as a Kiro `conversationState` request body.
///
/// Kiro is the one wire with no `messages` array at all: the conversation is a
/// `history` of already-answered turns plus a single `currentMessage`. Only the
/// last non-system turn becomes the current message — see the note on `history`
/// below — and the system prompt is folded onto it, because Kiro has no system
/// field either.
///
/// `conversationId` is stable per conversation so AWS's Builder-ID context cache
/// hits, and distinct across conversations so one session cannot read another's
/// state. The reference uses uuidv5 over the first 4000 characters of the first
/// user turn; this crate has no uuid dependency, so the same seed is hashed with
/// FNV-1a. Same property, different function — noted above as a simplification.
#[must_use]
pub fn render_kiro_body(chat: &CanonicalChat) -> Vec<u8> {
    let mut system = String::new();
    let mut dialogue: Vec<&Msg> = Vec::with_capacity(chat.messages.len());

    for msg in &chat.messages {
        if msg.role == Role::System {
            if !msg.content.is_empty() {
                system.push_str(&msg.content);
                system.push('\n');
            }
            continue;
        }
        dialogue.push(msg);
    }

    // Kiro has no system field, so the system prompt is a prefix on the
    // current message — the same place the reference puts its tool docs.
    //
    // Only the current turn is sent. Kiro has no `history` requirement this build
    // can satisfy: populating it means replaying every prior turn as
    // `userInputMessage`, and the reference's own conversion of a tool-bearing
    // history needs a `userInputMessageContext` tools schema. Empty history is the
    // shape a first turn sends anyway, so a multi-turn Kiro conversation loses
    // its earlier turns rather than being rejected.
    let (last, seed) = match dialogue.split_last() {
        Some((last, prior)) => (
            last,
            prior
                .iter()
                .find(|m| m.role == Role::User)
                .map_or_else(|| last.content.clone(), |m| m.content.clone()),
        ),
        // No dialogue at all: Kiro has no spelling for a request with no current
        // message, so synthesise the same minimal turn the Claude mapper does.
        None => {
            let mut message = Map::new();
            message.insert("content".to_owned(), format!("{system}.").into());
            message.insert("modelId".to_owned(), chat.model.as_str().into());
            message.insert("origin".to_owned(), "AI_EDITOR".into());
            return encode(kiro_payload(&message, &[], "", chat));
        }
    };

    let images = kiro_images(last, &chat.model);
    // Kiro accepts an empty user `content` when the turn carries images, and only
    // needs a placeholder for a genuinely bare turn.
    let mut message = String::new();
    if !system.is_empty() {
        message.push_str(&system);
        message.push('\n');
    }
    if !last.content.is_empty() {
        message.push_str(&last.content);
    } else if images.is_empty() {
        message.push_str("(empty)");
    }

    let mut current = Map::with_capacity(4);
    current.insert("content".to_owned(), message.into());
    current.insert("modelId".to_owned(), chat.model.as_str().into());
    current.insert("origin".to_owned(), "AI_EDITOR".into());
    if !images.is_empty() {
        current.insert("images".to_owned(), Value::Array(images));
    }

    encode(kiro_payload(&current, &[], &seed, chat))
}

/// Wraps a rendered `userInputMessage` in Kiro's `conversationState` envelope.
///
/// `seed` is the conversation identity, kept separate from `current` because it
/// must not move as the conversation grows: the reference seeds it from the
/// *first* user turn, which is what makes AWS's Builder-ID context cache hit on
/// every turn after the first.
/// Wraps a rendered `userInputMessage` in Kiro's `conversationState` envelope.
///
/// `seed` is the conversation identity, kept separate from `current` because it
/// must not move as the conversation grows: the reference seeds it from the
/// *first* user turn, which is what makes AWS's Builder-ID context cache hit on
/// every turn after the first.
fn kiro_payload(
    current: &Map<String, Value>,
    history: &[Value],
    seed: &str,
    chat: &CanonicalChat,
) -> Value {
    let mut inference = Map::new();
    inference.insert(
        "maxTokens".to_owned(),
        chat.max_tokens.unwrap_or(KIRO_DEFAULT_MAX_TOKENS).into(),
    );
    if let Some(temperature) = chat.temperature {
        inference.insert("temperature".to_owned(), temperature.into());
    }

    Value::Object(Map::from_iter([
        (
            "conversationState".to_owned(),
            json!({
                "chatTriggerType": "MANUAL",
                "conversationId": kiro_conversation_id(seed),
                "currentMessage": { "userInputMessage": Value::Object(current.clone()) },
                // Kiro requires the field; a first turn carries an empty history.
                "history": history,
            }),
        ),
        ("inferenceConfig".to_owned(), Value::Object(inference)),
    ]))
}

/// Extracts a turn's base64 media into Kiro's `images` array.
///
/// The reference reads an OpenAI `image_url` data URL, an Anthropic base64
/// `image`, or an AI-SDK `{type:"image", image:"data:…"}` part, splits the
/// `data:<mime>;base64,` header off, and pushes `{format, source:{bytes}}` — and
/// only for a model whose id contains `claude`, because Kiro's non-Claude routes
/// reject image attachments. Both rules are kept: an unreachable image would be
/// a 400, and a data URL is the only form this wire carries bytes in, so a plain
/// URL is dropped rather than forwarded as bytes.
///
/// This mapper sends the current turn alone, so `history` is always empty — see
/// the note on `render_kiro_body`.
fn kiro_images(msg: &Msg, model: &str) -> Vec<Value> {
    if !model.to_lowercase().contains("claude") {
        return Vec::new();
    }
    msg.media
        .iter()
        .filter_map(|part| {
            let url = kiro_image_url(part)?;
            let (_, bytes) = data_url(url)?;
            // `data:image/jpeg;base64,…` -> `jpeg`. The reference defaults to
            // `jpeg` for a header it cannot read a subtype out of.
            let format = url
                .strip_prefix("data:")?
                .split_once(';')?
                .0
                .split_once('/')?
                .1;
            Some(json!({
                "format": if format.is_empty() { "jpeg" } else { format },
                "source": { "bytes": bytes },
            }))
        })
        .collect()
}

/// Reads a media part's bytes as a `data:` URL, across the three inbound shapes
/// the reference accepts.
fn kiro_image_url(part: &MediaPart) -> Option<&str> {
    match part.kind.as_str() {
        "image_url" => part.body.get("image_url")?.get("url")?.as_str(),
        "image" => part
            .body
            .get("image")
            .and_then(Value::as_str)
            .or_else(|| part.body.get("source")?.get("data").and_then(Value::as_str)),
        _ => None,
    }
}

/// FNV-1a over the reference's seed, formatted as a UUID-shaped string.
///
/// The shape is cosmetic — Kiro treats `conversationId` as an opaque string —
/// but keeping it UUID-shaped means a log line or a capture is not obviously
/// foreign next to a reference-derived one.
/// FNV-1a over the reference's seed, formatted as a UUID-shaped string.
///
/// The shape is cosmetic — Kiro treats `conversationId` as an opaque string —
/// but keeping it UUID-shaped means a log line or a capture is not obviously
/// foreign next to a reference-derived one.
fn kiro_conversation_id(seed: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in seed.as_bytes().iter().take(4000).copied() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    let hi = hash;
    let lo = hash.rotate_left(32);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (hi >> 32) as u32,
        (hi >> 16) as u16,
        (hi & 0x0fff) as u16,
        (lo >> 48) as u16,
        lo & 0xffff_ffff_ffff
    )
}

/// Renders a canonical response as an Anthropic Messages response object.
///
/// The non-streaming counterpart of [`render_claude_body`], and the mirror of
/// [`crate::anthropic_to_canonical`]'s response side. The reference's chunk-level
/// state machine — block-index allocation, incremental tool-argument
/// accumulation, deferred terminal emission until a trailing usage-only chunk
/// arrives — is not ported: this crate's response mapper is non-streaming, so
/// there is no cross-chunk state to accumulate into. See the module docs.
///
/// ```
/// use ar_translate::{
///     CanonicalResponse, FinishReason, Msg, Role, Usage, to_anthropic_response,
/// };
///
/// let response = CanonicalResponse {
///     id: "msg_1".to_owned(),
///     model: "claude-sonnet-4-5".to_owned(),
///     created: 0,
///     message: Msg::new(Role::Assistant, "hi"),
///     finish_reason: FinishReason::Stop,
///     usage: Usage { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 },
/// };
/// let value = to_anthropic_response(&response);
///
/// assert_eq!(value["type"], "message");
/// assert_eq!(value["stop_reason"], "end_turn");
/// assert_eq!(value["content"][0]["text"], "hi");
/// assert_eq!(value["usage"]["input_tokens"], 3);
/// ```
#[must_use]
pub fn to_anthropic_response(response: &CanonicalResponse) -> Value {
    json!({
        "id": response.id,
        "type": "message",
        "role": "assistant",
        "model": response.model,
        "content": [
            { "type": "text", "text": response.message.content },
        ],
        "stop_reason": anthropic_stop_reason(response.finish_reason),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": response.usage.prompt_tokens,
            "output_tokens": response.usage.completion_tokens,
        },
    })
}

/// Anthropic spells its stop reasons its own way, and its `end_turn` is not
/// OpenAI's `stop`.
const fn anthropic_stop_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "end_turn",
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        // Anthropic has no content-filter reason; `refusal` is the closest one
        // it documents, and it is a string the client can branch on.
        FinishReason::ContentFilter => "refusal",
    }
}

/// Renders a canonical response as an OpenAI Responses response object.
///
/// The non-streaming counterpart of [`render_openai_responses_body`]. The output
/// is the `output` item array Responses uses in place of `choices`, and the
/// usage block is Responses' own spelling.
///
/// The reference's tool-item mapper (`responsesToolItem.ts`) is not ported, for
/// the same reason as the Claude one: it is a chunk-level state machine over
/// `function_call` items, and canonical has no tool registry to emit them from.
#[must_use]
pub fn to_responses_response(response: &CanonicalResponse) -> Value {
    json!({
        "id": response.id,
        "object": "response",
        "created_at": response.created,
        "model": response.model,
        "status": "completed",
        "output": [{
            "type": "message",
            "id": format!("msg_{}", response.id),
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": response.message.content,
                "annotations": [],
            }],
        }],
        "usage": {
            "input_tokens": response.usage.prompt_tokens,
            "output_tokens": response.usage.completion_tokens,
            "total_tokens": response.usage.total_tokens,
        },
    })
}

/// Serialises a rendered request body.
///
/// The same two-branch shape as [`render_openai_body`]: a body made of strings,
/// integers, bools and already-valid `Value`s cannot fail, so the fallback exists
/// only to keep a request path free of `panic!`.
fn encode(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap_or_else(|err| {
        serde_json::to_vec(&json!({
            "error": { "message": format!("could not render canonical chat: {err}") },
        }))
        .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{
        OutboundWire, render_for_wire, render_openai_body, to_anthropic_response,
        to_responses_response,
    };
    use crate::anthropic::AnthropicMessages;
    use crate::canonical::{
        CanonicalChat, CanonicalResponse, FinishReason, MediaPart, Msg, Role, Usage,
    };
    use crate::gemini::GeminiChat;
    use crate::ollama::OllamaChat;
    use crate::openai::OpenAIChat;
    use crate::responses::ResponsesApi;
    use crate::translate::{
        anthropic_to_canonical, gemini_to_canonical, ollama_to_canonical, responses_to_canonical,
        to_canonical,
    };

    fn user(text: &str) -> Msg {
        Msg::new(Role::User, text)
    }

    fn chat(messages: Vec<Msg>) -> CanonicalChat {
        CanonicalChat {
            model: "m-1".to_owned(),
            messages,
            temperature: Some(0.5),
            max_tokens: Some(128),
            stream: true,
        }
    }

    /// The shape every mapper test starts from: a system turn, a user turn and an
    /// assistant turn, so hoisting logic is exercised rather than assumed.
    fn tri() -> CanonicalChat {
        chat(vec![
            Msg::new(Role::System, "be terse"),
            user("hi"),
            Msg::new(Role::Assistant, "hello"),
        ])
    }

    /// Renders `chat` into `wire` and parses the result back, so a round-trip
    /// test asserts on structure rather than on a golden string. The caller
    /// re-parses the value with its own dialect's inbound type — that is what
    /// makes each test a round trip rather than a body-shape assertion.
    fn rendered(chat: &CanonicalChat, wire: OutboundWire) -> Value {
        let body = String::from_utf8(render_for_wire(chat, wire)).expect("utf-8 body");
        serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("{wire} body is not json: {body}\n{e}"))
    }

    /// The shape every response test starts from, with self-consistent usage so a
    /// usage assertion in one of them is about the mapper rather than the fixture.
    fn sample_response() -> CanonicalResponse {
        CanonicalResponse {
            id: "msg_1".to_owned(),
            model: "m-1".to_owned(),
            created: 42,
            message: Msg::new(Role::Assistant, "hi"),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 3,
                completion_tokens: 1,
                total_tokens: 4,
            },
        }
    }

    // ── OpenAI stays the default path ──────────────────────────────────

    #[test]
    fn openai_is_byte_identical_to_the_pre_existing_renderer() {
        let canonical = tri();
        assert_eq!(
            render_for_wire(&canonical, OutboundWire::Openai),
            render_openai_body(&canonical),
            "the dispatch table must delegate, not reimplement",
        );
    }

    // ── canonical -> claude ────────────────────────────────────────────

    #[test]
    fn hoists_a_system_turn_out_of_the_claude_message_array() {
        let value = rendered(&tri(), OutboundWire::Claude);
        assert_eq!(value["system"][0]["text"], "be terse", "{value}");
        assert_eq!(
            value["messages"].as_array().expect("messages").len(),
            2,
            "{value}"
        );
    }

    #[test]
    fn keys_a_claude_turn_on_its_role() {
        let value = rendered(&tri(), OutboundWire::Claude);
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(value["messages"][1]["role"], "assistant");
    }

    /// `system` and `max_tokens` are Anthropic's own keys, and `max_tokens` is
    /// *required* there — the one field a wire makes mandatory that the OpenAI wire
    /// makes omittable.
    #[test]
    fn substitutes_the_claude_required_max_tokens_default() {
        // Anthropic 400s a body with no `max_tokens`; the OpenAI wire omits the
        // field instead, so this cannot be the same renderer. The caller asked for
        // none, which is the case the default exists for.
        let unnamed = CanonicalChat {
            max_tokens: None,
            ..chat(vec![user("hi")])
        };
        assert_eq!(rendered(&unnamed, OutboundWire::Claude)["max_tokens"], 4096);
        // A caller-supplied ceiling is still honoured rather than overwritten.
        let named = CanonicalChat {
            max_tokens: Some(64),
            ..chat(vec![user("hi")])
        };
        assert_eq!(rendered(&named, OutboundWire::Claude)["max_tokens"], 64);
    }

    #[test]
    fn round_trips_canonical_through_the_claude_wire_and_back() {
        let value = rendered(&tri(), OutboundWire::Claude);
        let back = anthropic_to_canonical(
            serde_json::from_value::<AnthropicMessages>(value).expect("an anthropic body"),
        )
        .expect("canonicalises back");
        assert_eq!(back.messages[0], Msg::new(Role::System, "be terse"));
        assert_eq!(back.messages[1], user("hi"));
        assert_eq!(back.messages[2], Msg::new(Role::Assistant, "hello"));
    }

    #[test]
    fn synthesises_a_claude_user_turn_when_every_turn_was_a_system_turn() {
        // The reference's #5245 guard: Anthropic rejects an empty `messages`.
        let value = rendered(
            &chat(vec![Msg::new(Role::System, "only")]),
            OutboundWire::Claude,
        );
        assert_eq!(value["messages"][0]["role"], "user", "{value}");
    }

    #[test]
    fn renders_a_tool_turn_as_a_claude_tool_result_block() {
        let value = rendered(
            &chat(vec![user("hi"), Msg::new(Role::Tool, "42")]),
            OutboundWire::Claude,
        );
        assert_eq!(
            value["messages"][1]["role"], "user",
            "claude has no tool role"
        );
        assert_eq!(value["messages"][1]["content"][0]["type"], "tool_result");
    }

    // ── canonical -> responses ─────────────────────────────────────────

    /// `max_output_tokens`, `input` and `instructions` are Responses' own keys — a
    /// chat-completions body reaching this wire is the defect the per-wire
    /// renderer exists to prevent.
    #[test]
    fn renders_responses_instructions_and_input_items() {
        let value = rendered(&tri(), OutboundWire::Responses);
        assert_eq!(value["instructions"], "be terse", "{value}");
        assert_eq!(value["input"][0]["role"], "user");
        assert_eq!(value["input"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn keys_a_responses_part_discriminator_on_the_role() {
        let value = rendered(&tri(), OutboundWire::Responses);
        assert_eq!(
            value["input"][1]["content"][0]["type"], "output_text",
            "an assistant turn",
        );
    }

    #[test]
    fn renames_the_token_ceiling_for_the_responses_wire() {
        let value = rendered(&tri(), OutboundWire::Responses);
        assert_eq!(value["max_output_tokens"], 128);
        assert!(
            value.get("max_tokens").is_none(),
            "the OpenAI spelling is not a Responses key"
        );
    }

    #[test]
    fn carries_a_responses_image_as_an_openai_content_part() {
        let mut turn = user("what is this?");
        turn.media.push(MediaPart::new(
            "image_url",
            serde_json::json!({ "image_url": { "url": "data:image/png;base64,QUJD" } }),
        ));
        let value = rendered(&chat(vec![turn]), OutboundWire::Responses);
        assert_eq!(
            value["input"][0]["content"][1]["image_url"]["url"], "data:image/png;base64,QUJD",
            "{value}"
        );
    }

    #[test]
    fn round_trips_canonical_through_the_responses_wire_and_back() {
        let value = rendered(&tri(), OutboundWire::Responses);
        let back = responses_to_canonical(
            serde_json::from_value::<ResponsesApi>(value).expect("a responses body"),
        )
        .expect("canonicalises back");
        assert_eq!(back.messages[0], Msg::new(Role::System, "be terse"));
        assert_eq!(back.messages[1], user("hi"));
        assert_eq!(back.messages[2], Msg::new(Role::Assistant, "hello"));
    }

    // ── canonical -> gemini ────────────────────────────────────────────

    /// `systemInstruction`, `contents` and `generationConfig` are Gemini's own keys — a
    /// chat-completions body reaching this wire is the defect the per-wire
    /// renderer exists to prevent.
    #[test]
    fn renders_a_gemini_system_instruction_and_contents() {
        let value = rendered(&tri(), OutboundWire::Gemini);
        assert_eq!(
            value["systemInstruction"]["parts"][0]["text"], "be terse",
            "{value}"
        );
        assert_eq!(value["contents"][0]["role"], "user");
        assert_eq!(
            value["contents"][1]["role"], "model",
            "gemini spells assistant `model`"
        );
    }

    #[test]
    fn renders_a_gemini_image_part_in_a_gemini_spelling() {
        // The OpenAI `image_url` wrapper is not a Gemini part; `inlineData` is.
        let mut turn = user("what is this?");
        turn.media.push(MediaPart::new(
            "image_url",
            serde_json::json!({ "image_url": { "url": "data:image/png;base64,QUJD" } }),
        ));
        let value = rendered(&chat(vec![turn]), OutboundWire::Gemini);
        let part = &value["contents"][0]["parts"][1];
        assert_eq!(part["inlineData"]["mimeType"], "image/png", "{value}");
        assert_eq!(part["inlineData"]["data"], "QUJD", "{value}");
    }

    #[test]
    fn nests_sampling_under_the_gemini_generation_config() {
        let value = rendered(&tri(), OutboundWire::Gemini);
        assert_eq!(value["generationConfig"]["maxOutputTokens"], 128);
        assert_eq!(value["generationConfig"]["temperature"], 0.5);
    }

    #[test]
    fn round_trips_canonical_through_the_gemini_wire_and_back() {
        let value = rendered(&tri(), OutboundWire::Gemini);
        let back = gemini_to_canonical(
            serde_json::from_value::<GeminiChat>(value).expect("a gemini body"),
        )
        .expect("canonicalises back");
        assert_eq!(back.messages[0], Msg::new(Role::System, "be terse"));
        assert_eq!(back.messages[1], user("hi"));
        assert_eq!(back.messages[2], Msg::new(Role::Assistant, "hello"));
    }

    #[test]
    fn renders_antigravity_with_the_gemini_body() {
        // The reference's `openai-to-gemini.ts` is a 14-line re-registration of the
        // antigravity twin, so the two names must produce one body.
        let canonical = tri();
        assert_eq!(
            render_for_wire(&canonical, OutboundWire::Antigravity),
            render_for_wire(&canonical, OutboundWire::Gemini),
        );
    }

    // ── canonical -> cursor ────────────────────────────────────────────

    #[test]
    fn respells_a_cursor_system_turn_as_a_user_turn() {
        // Cursor has no `system` role, so the prompt has to arrive as context on
        // a user turn or it is lost.
        let value = rendered(&tri(), OutboundWire::Cursor);
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(
            value["messages"][0]["content"],
            "[System Instructions]\nbe terse"
        );
    }

    #[test]
    fn respells_a_cursor_tool_turn_as_a_tool_result_block() {
        let value = rendered(
            &chat(vec![user("hi"), Msg::new(Role::Tool, "42")]),
            OutboundWire::Cursor,
        );
        assert_eq!(
            value["messages"][1]["role"], "user",
            "cursor has no tool role"
        );
        let content = value["messages"][1]["content"]
            .as_str()
            .expect("string content");
        assert!(content.contains("<result>42</result>"), "{content}");
    }

    #[test]
    fn escapes_xml_inside_a_cursor_tool_result() {
        let value = rendered(
            &chat(vec![Msg::new(Role::Tool, "a < b & c")]),
            OutboundWire::Cursor,
        );
        let content = value["messages"][0]["content"]
            .as_str()
            .expect("string content");
        assert!(content.contains("a &lt; b &amp; c"), "{content}");
    }

    // ── Cursor: media survives as an OpenAI content array ───────────

    #[test]
    fn carries_a_cursor_image_as_an_openai_content_part() {
        let mut turn = user("what is this?");
        turn.media.push(MediaPart::new(
            "image_url",
            serde_json::json!({ "image_url": { "url": "data:image/png;base64,QUJD" } }),
        ));
        let value = rendered(&chat(vec![turn]), OutboundWire::Cursor);
        assert_eq!(
            value["messages"][0]["content"][1]["image_url"]["url"], "data:image/png;base64,QUJD",
            "{value}"
        );
    }

    #[test]
    fn round_trips_canonical_through_the_cursor_wire_and_back() {
        let value = rendered(&tri(), OutboundWire::Cursor);
        let back =
            to_canonical(serde_json::from_value::<OpenAIChat>(value).expect("a cursor body"))
                .expect("canonicalises back");
        // The system turn returns as a user turn: that is the loss Cursor's ask
        // format forces, and the reason its mapper prefixes the text.
        assert_eq!(back.messages[0], user("[System Instructions]\nbe terse"));
        assert_eq!(back.messages[1], user("hi"));
    }

    // ── canonical -> clova ─────────────────────────────────────────────

    /// `maxTokens` and camelCase keys are CLOVA's own; a snake_case `max_tokens`
    /// on this wire is a body the vendor would reject with `40001`.
    #[test]
    fn renders_clova_typed_text_parts_and_its_own_ceiling_spelling() {
        let value = rendered(&tri(), OutboundWire::Clova);
        assert_eq!(value["messages"][0]["content"][0]["type"], "text");
        assert_eq!(value["maxTokens"], 128);
        assert!(
            value.get("max_tokens").is_none(),
            "the OpenAI spelling is not a CLOVA key"
        );
    }

    #[test]
    fn respells_a_clova_image_url_the_way_the_vendor_spells_it() {
        // The OpenAI `image_url` part is a `40001` on this wire.
        let mut remote = user("what is this?");
        remote.media.push(MediaPart::new(
            "image_url",
            serde_json::json!({ "image_url": { "url": "http://x/i.png" } }),
        ));
        let value = rendered(&chat(vec![remote]), OutboundWire::Clova);
        assert_eq!(
            value["messages"][0]["content"][1]["imageUrl"]["url"], "http://x/i.png",
            "{value}"
        );

        // A `data:` URL keeps its full prefix, including the `data:` and `;base64,`.
        let mut inline = user("what is this?");
        inline.media.push(MediaPart::new(
            "image_url",
            serde_json::json!({ "image_url": { "url": "data:image/png;base64,QUJD" } }),
        ));
        let value = rendered(&chat(vec![inline]), OutboundWire::Clova);
        assert_eq!(
            value["messages"][0]["content"][1]["dataUri"]["data"], "data:image/png;base64,QUJD",
            "{value}"
        );
    }

    #[test]
    fn clamps_clova_temperature_to_the_vendor_range() {
        // Live-verified rejection above 1, so this clamp is load-bearing rather
        // than cosmetic.
        let hot = CanonicalChat {
            temperature: Some(1.9),
            ..chat(vec![user("hi")])
        };
        assert_eq!(rendered(&hot, OutboundWire::Clova)["temperature"], 1.0);
    }

    #[test]
    fn caps_clova_output_tokens_at_the_vendor_ceiling() {
        let loud = CanonicalChat {
            max_tokens: Some(999_999),
            ..chat(vec![user("hi")])
        };
        assert_eq!(rendered(&loud, OutboundWire::Clova)["maxTokens"], 4096);
    }

    #[test]
    fn folds_a_clova_tool_turn_into_a_user_turn() {
        let value = rendered(
            &chat(vec![user("hi"), Msg::new(Role::Tool, "42")]),
            OutboundWire::Clova,
        );
        assert_eq!(
            value["messages"][1]["role"], "user",
            "clova has no tool role"
        );
        assert_eq!(value["messages"][1]["content"][0]["text"], "42");
    }

    /// The vendor carries the model on the URL
    /// (`/v3/chat-completions/{modelName}`) and nowhere in the body, so the body is
    /// not an OpenAI one — which is also why the round trip below re-reads the
    /// roles straight out of the parsed value instead of through `OpenAIChat`.
    /// The vendor carries the model on the URL
    /// (`/v3/chat-completions/{modelName}`) and nowhere in the body, so the body
    /// is not an OpenAI one — which is why this round trip re-injects the model
    /// before re-parsing, rather than pretending the body is one.
    #[test]
    fn round_trips_canonical_through_the_clova_wire_and_back() {
        let canonical = chat(vec![user("hi"), Msg::new(Role::Assistant, "hello")]);
        let value = rendered(&canonical, OutboundWire::Clova);
        // The vendor carries the model on the URL
        // (`/v3/chat-completions/{modelName}`) and nowhere in the body, so the
        // round trip re-injects it into a value the OpenAI inbound type can read
        // rather than pretending the body is one.
        let mut body = value;
        body["model"] = serde_json::Value::String(canonical.model);
        let back = to_canonical(serde_json::from_value::<OpenAIChat>(body).expect("a clova body"))
            .expect("canonicalises back");
        assert_eq!(back.messages[0], user("hi"));
        assert_eq!(back.messages[1], Msg::new(Role::Assistant, "hello"));
    }

    // ── canonical -> kiro ──────────────────────────────────────────────

    /// `conversationState` and `currentMessage` are Kiro's own envelope: the wire
    /// has no `messages` array at all, so a body with one is a body that went to
    /// the wrong shape.
    #[test]
    fn renders_a_kiro_conversation_state_envelope() {
        let value = rendered(&tri(), OutboundWire::Kiro);
        let message = &value["conversationState"]["currentMessage"]["userInputMessage"];
        assert_eq!(message["modelId"], "m-1", "{value}");
        assert_eq!(message["origin"], "AI_EDITOR");
        assert_eq!(value["conversationState"]["chatTriggerType"], "MANUAL");
    }

    #[test]
    fn folds_the_kiro_system_prompt_onto_the_current_message() {
        // Kiro has no system field, so the prompt is a prefix or it is lost.
        let value = rendered(&tri(), OutboundWire::Kiro);
        let content = value["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .expect("string content");
        assert!(content.starts_with("be terse\n"), "{content}");
        assert!(
            content.ends_with("hello"),
            "the current turn is the last one: {content}"
        );
    }

    /// A cache hit needs the same conversation to produce the same id on every
    /// render; a fresh one per request would defeat the upstream cache entirely.
    #[test]
    fn names_the_kiro_conversation_id_per_conversation_and_not_per_turn() {
        // A cache hit needs the same conversation to produce the same id on every
        // render; a fresh one per request would defeat the upstream cache entirely.
        // The id is seeded from the *first* user turn, so adding a turn must not
        // move it — that is what a growing conversation looks like.
        let growing = chat(vec![
            user("hi"),
            Msg::new(Role::Assistant, "hello"),
            user("and now?"),
        ]);
        let earlier = chat(vec![user("hi")]);
        assert_eq!(
            rendered(&growing, OutboundWire::Kiro)["conversationState"]["conversationId"],
            rendered(&earlier, OutboundWire::Kiro)["conversationState"]["conversationId"],
            "a new turn must not change the conversation identity",
        );
    }

    /// The upstream cache is keyed on this id, and so is the *separation* between
    /// two conversations: a shared id would leak one session's context into
    /// another's, which is why stability alone is not enough to assert.
    #[test]
    fn separates_kiro_conversation_ids_across_conversations() {
        // The upstream cache is keyed on this: a shared id would leak one
        // session's context into another's.
        let a = rendered(&chat(vec![user("alpha")]), OutboundWire::Kiro);
        let b = rendered(&chat(vec![user("beta")]), OutboundWire::Kiro);
        assert_ne!(
            a["conversationState"]["conversationId"],
            b["conversationState"]["conversationId"],
        );
    }

    #[test]
    fn carries_the_kiro_current_turn_verbatim() {
        // Kiro has no `messages` array at all, so the meaningful round trip is on
        // the content: the current message carries the turn unaltered.
        let value = rendered(&chat(vec![user("hi")]), OutboundWire::Kiro);
        let content = value["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .expect("string content");
        assert!(content.contains("hi"), "{content}");
    }

    // ── media crosses every wire unchanged ─────────────────────────────

    /// Every renderer either passes the part through (Responses, Cursor — both
    /// *are* the OpenAI spelling) or re-encodes it into its own image envelope;
    /// none may drop the bytes. The model id is Claude's because Kiro gates
    /// images on one — its non-Claude routes reject attachments, and the
    /// dropped-image case is asserted separately below.
    #[test]
    fn carries_a_media_part_through_every_renderer() {
        let mut turn = user("what is this?");
        turn.media.push(MediaPart::new(
            "image_url",
            serde_json::json!({ "image_url": { "url": "data:image/png;base64,QUJD" } }),
        ));
        let canonical = CanonicalChat {
            model: "claude-sonnet-4-5".into(),
            ..chat(vec![turn])
        };
        for wire in OutboundWire::ALL {
            let body = String::from_utf8(render_for_wire(&canonical, *wire)).expect("utf-8");
            assert!(
                body.contains("QUJD"),
                "{wire} dropped the image bytes: {body}"
            );
        }
    }

    #[test]
    fn carries_a_kiro_image_for_a_claude_model() {
        // Kiro carries image bytes in `images`, not as a content part, and only for
        // a Claude model — its other routes reject attachments.
        let mut turn = user("what is this?");
        turn.media.push(MediaPart::new(
            "image_url",
            serde_json::json!({ "image_url": { "url": "data:image/png;base64,QUJD" } }),
        ));
        let value = rendered(
            &CanonicalChat {
                model: "claude-sonnet-4-5".into(),
                ..chat(vec![turn])
            },
            OutboundWire::Kiro,
        );
        let images = &value["conversationState"]["currentMessage"]["userInputMessage"]["images"];
        assert_eq!(images[0]["format"], "png", "{value}");
        assert_eq!(images[0]["source"]["bytes"], "QUJD", "{value}");
    }

    #[test]
    fn carries_no_kiro_image_for_a_non_claude_model() {
        // Kiro's non-Claude routes reject attachments, so the same part is dropped
        // rather than forwarded as a body they would 400 on.
        let mut turn = user("what is this?");
        turn.media.push(MediaPart::new(
            "image_url",
            serde_json::json!({ "image_url": { "url": "data:image/png;base64,QUJD" } }),
        ));
        let value = rendered(
            &CanonicalChat {
                model: "deepseek-chat".into(),
                ..chat(vec![turn])
            },
            OutboundWire::Kiro,
        );
        assert!(
            value["conversationState"]["currentMessage"]["userInputMessage"]
                .get("images")
                .is_none(),
            "{value}"
        );
    }

    #[test]
    fn carries_an_anthropic_image_block_in_the_anthropic_spelling() {
        // An OpenAI `image_url` data URL, which Anthropic 400s in that spelling.
        let mut turn = user("what is this?");
        turn.media.push(MediaPart::new(
            "image_url",
            serde_json::json!({ "image_url": { "url": "data:image/png;base64,QUJD" } }),
        ));
        let value = rendered(&chat(vec![turn]), OutboundWire::Claude);
        let block = &value["messages"][0]["content"][1];
        assert_eq!(block["type"], "image", "{value}");
        assert_eq!(block["source"]["type"], "base64", "{value}");
        assert_eq!(block["source"]["media_type"], "image/png", "{value}");
        assert_eq!(block["source"]["data"], "QUJD", "{value}");
        assert!(
            block.get("image_url").is_none(),
            "the openai spelling leaked: {value}"
        );
    }

    #[test]
    fn carries_a_non_data_url_anthropic_image_as_a_url_source() {
        // Not a base64 data URL, so it is Anthropic's own `url` source.
        let mut turn = user("what is this?");
        turn.media.push(MediaPart::new(
            "image_url",
            serde_json::json!({ "image_url": { "url": "http://x/i.png" } }),
        ));
        let value = rendered(&chat(vec![turn]), OutboundWire::Claude);
        let block = &value["messages"][0]["content"][1];
        assert_eq!(block["source"]["type"], "url", "{value}");
        assert_eq!(block["source"]["url"], "http://x/i.png", "{value}");
    }

    #[test]
    fn passes_a_non_image_part_through_unchanged_on_the_claude_wire() {
        // A `tool_use` block is already Anthropic's spelling; re-encoding it would
        // only risk dropping a field.
        let mut turn = Msg::new(Role::Assistant, "");
        turn.media.push(MediaPart::new(
            "tool_use",
            serde_json::json!({ "id": "t1", "name": "lookup", "input": { "q": "x" } }),
        ));
        let value = rendered(&chat(vec![turn]), OutboundWire::Claude);
        assert_eq!(
            value["messages"][0]["content"][0]["type"], "tool_use",
            "{value}"
        );
        assert_eq!(
            value["messages"][0]["content"][0]["input"]["q"], "x",
            "{value}"
        );
    }

    #[test]
    fn drops_a_turn_with_neither_text_nor_media() {
        let empty = vec![user("hi"), Msg::new(Role::Assistant, "")];
        let value = rendered(&chat(empty), OutboundWire::Claude);
        assert_eq!(
            value["messages"].as_array().expect("messages").len(),
            1,
            "{value}"
        );
    }

    // ── response direction ─────────────────────────────────────────────

    #[test]
    fn renders_an_anthropic_response_envelope() {
        let value = to_anthropic_response(&sample_response());
        assert_eq!(value["type"], "message");
        assert_eq!(
            value["stop_reason"], "end_turn",
            "anthropic does not spell it `stop`"
        );
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["usage"]["input_tokens"], 3);
    }

    #[test]
    fn maps_every_finish_reason_onto_an_anthropic_stop_reason() {
        for (reason, wire) in [
            (FinishReason::Stop, "end_turn"),
            (FinishReason::Length, "max_tokens"),
            (FinishReason::ToolCalls, "tool_use"),
            (FinishReason::ContentFilter, "refusal"),
        ] {
            let mut response = sample_response();
            response.finish_reason = reason;
            assert_eq!(
                to_anthropic_response(&response)["stop_reason"],
                wire,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn renders_a_responses_response_envelope_with_an_output_item() {
        let value = to_responses_response(&sample_response());
        assert_eq!(value["object"], "response");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["output"][0]["type"], "message");
        assert_eq!(value["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(value["usage"]["total_tokens"], 4);
    }

    // ── wire registry ──────────────────────────────────────────────────

    #[test]
    fn names_every_wire_it_can_render() {
        for wire in OutboundWire::ALL {
            assert!(!wire.as_str().is_empty(), "{wire:?} has no name");
        }
    }

    #[test]
    fn renders_a_json_body_for_every_named_wire() {
        for wire in OutboundWire::ALL {
            rendered(&tri(), *wire);
        }
    }

    #[test]
    fn round_trips_an_ollama_inbound_request_out_on_the_openai_wire() {
        // The one inbound dialect with no provider wire of its own, so its
        // canonical form has to leave on the OpenAI arm.
        let inbound: OllamaChat = serde_json::from_str(
            r#"{"model":"llama3","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .expect("an ollama body");
        let canonical = ollama_to_canonical(inbound).expect("canonicalises");
        let back = to_canonical(
            serde_json::from_value::<OpenAIChat>(rendered(&canonical, OutboundWire::Openai))
                .expect("an openai body"),
        )
        .expect("canonicalises back");
        assert_eq!(back.messages[0], user("hi"));
    }
}
