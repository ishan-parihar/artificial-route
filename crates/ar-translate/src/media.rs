//! The media family: modalities, the embeddings endpoint, and `familyGuard`.
//!
//! `docs/02` defers vision/audio/video, `/v1/ocr`, `/v1/audio/translations` and
//! image-gen to P6 as "separate adapters", with `lib/embeddings/service.ts`'s
//! `createEmbeddingResponse` + `familyGuard` + `embeddingRegistry` arriving with
//! them: the modality vocabulary every adapter routes through, the registry the
//! guard reads, and the embeddings response builder.
//!
//! The guard covers embeddings only, and only against what the public spec
//! states — a `dimensions` override is honoured by `text-embedding-3` and not by
//! the fixed-width 2-series. Guarding text, vision or audio would need a
//! per-model capability table no source describes, and an invented one is a table
//! nobody keeps honest; those modalities pass and the provider's 400 is the signal.

use serde::{Deserialize, Serialize};

use crate::canonical::MediaPart;

/// What a request needs from a provider.
///
/// The routing key shared by the chat adapters and `ar_exec`'s media dispatch:
/// a request's parts decide its modality, and its modality decides its
/// endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    /// No media parts.
    Text,
    /// At least one image part.
    Vision,
    /// At least one audio part.
    Audio,
    /// An image-synthesis request.
    ImageGen,
    /// A document-extraction request.
    Ocr,
    /// An embedding-vector request.
    Embedding,
}

impl Modality {
    /// The modality a wire part discriminator denotes, or `None` for one that
    /// names no modality this build routes.
    ///
    /// A `None` is the reason an unknown part still raises
    /// [`crate::TranslateError::UnsupportedPart`] instead of being dropped:
    /// unrecognised is not the same as routable.
    #[must_use]
    pub const fn of_part(kind: &str) -> Option<Self> {
        match kind.as_bytes() {
            // OpenAI chat + Responses vision, Anthropic images, Ollama's
            // base64 array, and Gemini's `inlineData` — the same three arms as
            // upstream, under the spelling Gemini's own wire uses.
            b"image_url" | b"input_image" | b"image" | b"images" | b"inlineData" => {
                Some(Self::Vision)
            }
            b"input_audio" | b"audio" => Some(Self::Audio),
            _ => None,
        }
    }

    /// The modality a canonical media part denotes, or `None` when it is not a
    /// modality-bearing part (an Anthropic `tool_use`, say — carried, not
    /// routed).
    #[must_use]
    pub fn of_media(part: &MediaPart) -> Option<Self> {
        Self::of_part(&part.kind)
    }
}

/// One embedding family, as the public OpenAI spec describes it.
///
/// The registry exists so `familyGuard` can reject a request the provider will
/// certainly reject, before spending an upstream call on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingFamily {
    /// Model-id prefix this family claims. Matching is by prefix, so a dated
    /// variant such as `text-embedding-3-large-2025-01-01` resolves without a
    /// new entry.
    pub prefix: &'static str,
    /// Fixed output width, when the family has one. `None` means the family
    /// accepts a `dimensions` override.
    pub fixed_dimensions: Option<u32>,
}

/// The registry [`family_guard`] reads.
///
/// Order is significant: `text-embedding-3` is checked before the 2-series
/// prefix it would otherwise be swallowed by.
pub const EMBEDDING_REGISTRY: &[EmbeddingFamily] = &[
    EmbeddingFamily {
        prefix: "text-embedding-3",
        fixed_dimensions: None,
    },
    EmbeddingFamily {
        prefix: "text-embedding-",
        fixed_dimensions: Some(1536),
    },
];

/// Ways a media request fails before it reaches an upstream.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// The request named no model.
    #[error("request model must not be empty")]
    EmptyModel,
    /// No input text, or an empty batch.
    #[error("embedding request carries no input")]
    EmptyInput,
    /// The model id matches no family in [`EMBEDDING_REGISTRY`].
    #[error("model {model:?} is not a known embedding model")]
    UnknownEmbeddingModel {
        /// The requested model id, verbatim.
        model: String,
    },
    /// A `dimensions` override was sent to a fixed-width family.
    #[error(
        "model {model:?} has a fixed {fixed}-dimension output and does not accept `dimensions`"
    )]
    DimensionsUnsupported {
        /// The requested model id, verbatim.
        model: String,
        /// The width the family always emits.
        fixed: u32,
    },
    /// `dimensions` was zero.
    #[error("`dimensions` must be a positive integer")]
    InvalidDimensions,
    /// `encoding_format` named something other than `float` or `base64`.
    #[error("`encoding_format` must be `float` or `base64`")]
    UnknownEncodingFormat,
}

/// An inbound `POST /v1/embeddings` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    /// Requested model id.
    pub model: String,
    /// Text to embed: one string, or a batch.
    pub input: EmbeddingInput,
    /// Vector encoding. Only `float` and `base64` exist on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    /// Truncation width, honoured only by the 3-series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    /// Opaque end-user identifier, passed through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Vendor and future fields, preserved verbatim.
    #[serde(flatten, default)]
    pub rest: serde_json::Value,
}

/// The `input` field. Untagged, as everywhere in this crate: the wire accepts
/// either a single string or an array of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    /// One text.
    Text(String),
    /// A batch of texts, indexed in the response.
    Batch(Vec<String>),
}

impl EmbeddingInput {
    /// The texts to embed, whether the wire sent one or many.
    #[must_use]
    pub fn texts(&self) -> Vec<&str> {
        match self {
            Self::Text(text) => vec![text.as_str()],
            Self::Batch(texts) => texts.iter().map(String::as_str).collect(),
        }
    }
}

/// One vector from an upstream embeddings response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingVector {
    /// The vector itself.
    pub embedding: Vec<f32>,
    /// Position within the input batch.
    pub index: u32,
}

/// A decoded upstream embeddings response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingUpstream {
    /// One vector per input text.
    pub data: Vec<EmbeddingVector>,
    /// Provider-native model that produced them.
    pub model: String,
    /// Token accounting.
    pub usage: EmbeddingUsage,
}

/// Token accounting for an embeddings call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EmbeddingUsage {
    /// Tokens consumed by the inputs.
    pub prompt_tokens: u32,
    /// Sum with any completion tokens; embeddings produce none.
    pub total_tokens: u32,
}

/// Renders an OpenAI-shaped `POST /v1/embeddings` response.
///
/// Port of `createEmbeddingResponse`. Three things it pins down, all observable
/// on the wire:
///
/// * `object` is `list`, and each entry is `embedding`;
/// * entries are ordered by `index`, not by arrival, so an upstream that
///   answers out of order still produces the batch the caller sent;
/// * the response carries no `created` field, unlike chat completions.
#[must_use]
pub fn create_embedding_response(upstream: &EmbeddingUpstream) -> serde_json::Value {
    let mut data: Vec<&EmbeddingVector> = upstream.data.iter().collect();
    data.sort_by_key(|vector| vector.index);

    serde_json::json!({
        "object": "list",
        "data": data.iter().map(|vector| serde_json::json!({
            "object": "embedding",
            "index": vector.index,
            "embedding": vector.embedding,
        })).collect::<Vec<_>>(),
        "model": upstream.model,
        "usage": {
            "prompt_tokens": upstream.usage.prompt_tokens,
            "total_tokens": upstream.usage.total_tokens,
        },
    })
}

/// Rejects an embeddings request the provider would certainly reject.
///
/// See the module docs for why text, vision and audio are not guarded: only the
/// constraints the spec states are checked here, and an invented capability
/// table would be worse than the upstream's own 400.
///
/// # Errors
///
/// [`MediaError::EmptyModel`], [`MediaError::EmptyInput`],
/// [`MediaError::InvalidDimensions`] or [`MediaError::UnknownEncodingFormat`]
/// for a self-inconsistent body, and [`MediaError::UnknownEmbeddingModel`] or
/// [`MediaError::DimensionsUnsupported`] when the model is outside the
/// registry or its family is fixed-width.
///
/// # Example
///
/// ```
/// use ar_translate::{EmbeddingRequest, family_guard};
///
/// let raw = r#"{"model":"text-embedding-3-small","input":["a","b"]}"#;
/// let req: EmbeddingRequest = serde_json::from_str(raw).unwrap();
/// assert!(family_guard(&req).is_ok());
/// ```
pub fn family_guard(req: &EmbeddingRequest) -> Result<(), MediaError> {
    if req.model.trim().is_empty() {
        return Err(MediaError::EmptyModel);
    }
    if req
        .encoding_format
        .as_deref()
        .is_some_and(|f| f != "float" && f != "base64")
    {
        return Err(MediaError::UnknownEncodingFormat);
    }
    if req.dimensions == Some(0) {
        return Err(MediaError::InvalidDimensions);
    }
    if req.input.texts().iter().all(|text| text.is_empty()) {
        return Err(MediaError::EmptyInput);
    }

    let family = EMBEDDING_REGISTRY
        .iter()
        .find(|family| req.model.starts_with(family.prefix))
        .ok_or_else(|| MediaError::UnknownEmbeddingModel {
            model: req.model.clone(),
        })?;

    match (family.fixed_dimensions, req.dimensions) {
        (Some(fixed), Some(_)) => Err(MediaError::DimensionsUnsupported {
            model: req.model.clone(),
            fixed,
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(raw: &str) -> Result<EmbeddingRequest, serde_json::Error> {
        serde_json::from_str(raw)
    }

    #[test]
    fn orders_vectors_by_index_when_upstream_reorders() {
        let upstream = EmbeddingUpstream {
            data: vec![
                EmbeddingVector {
                    embedding: vec![0.2],
                    index: 1,
                },
                EmbeddingVector {
                    embedding: vec![0.1],
                    index: 0,
                },
            ],
            model: "text-embedding-3-small".into(),
            usage: EmbeddingUsage {
                prompt_tokens: 4,
                total_tokens: 4,
            },
        };

        let rendered = create_embedding_response(&upstream);

        assert_eq!(rendered["data"][0]["index"], 0);
    }

    #[test]
    fn accepts_dimensions_when_family_is_configurable() {
        let body = req(r#"{"model":"text-embedding-3-large","input":"a","dimensions":256}"#)
            .expect("fixture is valid embeddings");
        assert!(family_guard(&body).is_ok());
    }

    #[test]
    fn rejects_dimensions_when_family_is_fixed_width() {
        let body = req(r#"{"model":"text-embedding-ada-002","input":"a","dimensions":256}"#)
            .expect("fixture is valid embeddings");

        assert!(matches!(
            family_guard(&body),
            Err(MediaError::DimensionsUnsupported { fixed: 1536, .. })
        ));
    }

    #[test]
    fn rejects_input_when_empty() {
        let body = req(r#"{"model":"text-embedding-3-small","input":[]}"#)
            .expect("fixture is valid embeddings");

        assert!(matches!(family_guard(&body), Err(MediaError::EmptyInput)));
    }
}
