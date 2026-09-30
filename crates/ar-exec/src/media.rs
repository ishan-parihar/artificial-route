//! Media-family dispatch: embeddings, vision, audio, image-gen, OCR.
//!
//! One pooled client, one [`crate::ArExec`]. `docs/02` puts embeddings on "the same
//! `exec`" as chat, and that is the whole design here: [`crate::ArExec::post_media`] is
//! the non-streaming sibling of [`crate::ArExec::post_chat`], sharing the header merge,
//! the abort race and the upstream-error decoding, so a second pool is never
//! needed and a media failure is reported in the same terms as a chat one.
//!
//! # Why the bodies are forwarded verbatim
//!
//! These endpoints do not share one wire shape. `/v1/embeddings` is JSON and is
//! typed ([`crate::ArExec::post_embeddings`]); `/v1/audio/translations` is multipart and
//! `/v1/ocr` is vendor-defined, so both go through [`crate::ArExec::post_media`] with the bytes
//! the caller supplied and a content type the caller chose. Parsing a
//! multipart body here would mean re-encoding it into a shape no provider
//! documents, which is the one thing `AGENTS.md` forbids.
//!
//! Vision needs no adapter at all: on every dialect in this build an image is a
//! content part inside a chat request, so [`MediaEndpoint::for_modality`] sends
//! it to `/chat/completions` with the part intact.

use std::time::Duration;

use ar_config::Secret;
use ar_registry::ProviderDef;
use ar_translate::{
    EmbeddingRequest, EmbeddingUpstream, MediaError, Modality, create_embedding_response,
    family_guard,
};
use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::ExecError;

/// Which upstream endpoint a request is dispatched to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaEndpoint {
    /// `POST /chat/completions` — text and vision.
    ChatCompletions,
    /// `POST /embeddings`.
    Embeddings,
    /// `POST /audio/transcriptions`, target language supplied by the caller.
    Transcriptions,
    /// `POST /audio/translations`, source audio rendered to English.
    Translations,
    /// `POST /images/generations`.
    ///
    /// `/images/edits` is deliberately absent: it is multipart, and this build
    /// has no multipart encoder (see the module docs).
    ImageGenerations,
    /// `POST /ocr`, vendor-defined on the provider side.
    Ocr,
}

impl MediaEndpoint {
    /// The path appended to a provider's base URL.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/chat/completions",
            Self::Embeddings => "/embeddings",
            Self::Transcriptions => "/audio/transcriptions",
            Self::Translations => "/audio/translations",
            Self::ImageGenerations => "/images/generations",
            Self::Ocr => "/ocr",
        }
    }

    /// The endpoint a modality dispatches to.
    ///
    /// Vision lands on chat completions rather than a route of its own: every
    /// dialect here carries an image as a content part, so the chat endpoint is
    /// where the provider expects to find it. The remaining three modalities are
    /// not reachable from a canonical chat at all — a caller asks for them by
    /// endpoint, and the mapping here is the single place that says so.
    #[must_use]
    pub const fn for_modality(modality: Modality) -> Self {
        match modality {
            Modality::Text | Modality::Vision => Self::ChatCompletions,
            Modality::Audio => Self::Transcriptions,
            Modality::ImageGen => Self::ImageGenerations,
            Modality::Ocr => Self::Ocr,
            Modality::Embedding => Self::Embeddings,
        }
    }
}

/// A media request body, forwarded byte-for-byte.
///
/// The content type is `&'static str` rather than an owned header because every
/// value is a constant at the call site — a `String` here would allocate once per
/// request to hold a literal.
#[derive(Debug, Clone, Copy)]
pub struct MediaBody<'a> {
    /// Value of the outbound `Content-Type`.
    pub content_type: &'static str,
    /// The exact bytes to POST.
    pub bytes: &'a [u8],
}


/// A completed, non-streaming media response.
///
/// Holds the decoded body rather than the `reqwest::Response`: none of these
/// endpoints stream, so keeping the connection object alive past `await
/// post_media` would only delay returning the socket to the pool.
#[derive(Debug)]
pub struct MediaResponse {
    /// Upstream status, 2xx on success.
    pub status: reqwest::StatusCode,
    /// Decoded response body.
    pub bytes: Bytes,
    /// `Retry-After` the upstream sent, when present and valid.
    pub retry_after: Option<Duration>,
}

impl crate::ArExec {
    /// POSTs a media body to `endpoint` and reads the whole response.
    ///
    /// Same header merge, abort race and error decoding as [`post_chat`]; the
    /// only difference is that a 2xx is read eagerly instead of streamed.
    ///
    /// # Errors
    ///
    /// [`ExecError::Aborted`], [`ExecError::StartTimeout`],
    /// [`ExecError::Transport`] or [`ExecError::Upstream`], on the same terms as
    /// [`crate::ArExec::post_chat`].
    ///
    /// [`post_chat`]: crate::ArExec::post_chat
    pub async fn post_media(
        &self,
        endpoint: MediaEndpoint,
        body: &MediaBody<'_>,
        provider: &ProviderDef,
        api_key: &Secret,
        abort: &CancellationToken,
    ) -> Result<MediaResponse, ExecError> {
        let headers = crate::headers_for(api_key.expose(), body.content_type, "application/json");
        let response = crate::await_start(
            self.client
                .post(crate::url::endpoint_url(&provider.base_url, endpoint.path()))
                .headers(headers)
                .body(body.bytes.to_vec()),
            abort,
            crate::start_timeout(false),
        )
        .await?;

        // Read before taking the headers off: `bytes()` consumes the response,
        // so status and `Retry-After` are captured first rather than cloned.
        let status = response.status();
        let retry_after = crate::retry_after_of(response.headers());

        if !response.status().is_success() {
            return Err(crate::upstream_error(response, retry_after).await);
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| ExecError::Transport(e.to_string()))?;

        Ok(MediaResponse {
            status,
            bytes,
            retry_after,
        })
    }

    /// `POST /v1/embeddings`: guard, dispatch, render the OpenAI response shape.
    ///
    /// This is the port of `createEmbeddingResponse` + `familyGuard` +
    /// `embeddingRegistry`: the guard runs before the dispatch (an unknown model
    /// is the caller's bug, not an upstream 404), and the decoded upstream
    /// vectors are re-rendered through [`create_embedding_response`] so the
    /// client sees one shape whatever the provider answered.
    ///
    /// # Errors
    ///
    /// [`MediaError`] for a request the registry rejects, plus the
    /// [`ExecError`] set from [`Self::post_media`].
    pub async fn post_embeddings(
        &self,
        req: &EmbeddingRequest,
        provider: &ProviderDef,
        api_key: &Secret,
        abort: &CancellationToken,
    ) -> Result<serde_json::Value, ExecError> {
        family_guard(req).map_err(media_to_exec)?;

        let body = serde_json::to_vec(req)?;
        let response = self
            .post_media(
                MediaEndpoint::Embeddings,
                &MediaBody {
                    content_type: "application/json",
                    bytes: &body,
                },
                provider,
                api_key,
                abort,
            )
            .await?;

        let upstream: EmbeddingUpstream = serde_json::from_slice(&response.bytes)?;
        Ok(create_embedding_response(&upstream))
    }
}

/// Folds a media rejection into the executor's error set.
///
/// `MediaError` stays the crate-root type — the guard's own message is the
/// useful one — but an `ExecError` is what every `?` in this module expects, and
/// `#[from]` on a `String` variant is what joins them without a second match at
/// each call site.
fn media_to_exec(err: MediaError) -> ExecError {
    ExecError::Media(err.to_string())
}
