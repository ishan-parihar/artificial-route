//! GitLab Duo's whole-reply body, re-spelled as an OpenAI SSE stream.
//!
//! The `code_suggestions` monolith endpoint answers one JSON document and never
//! streams, so a streamed client gets its reply synthesized from that document:
//! the role, the text, the finish reason, then `[DONE]`. The reply is read whole
//! before the first chunk, which is the only way to stream an answer the upstream
//! has not split into parts.

use bytes::{Bytes, BytesMut};
use futures::Stream;
use tokio_stream::StreamExt;

use crate::ExecError;
use crate::bedrock::sse;

/// Relays a whole `code_suggestions` reply as OpenAI chat SSE, ending with `[DONE]`.
///
/// A body that carries no completion ends the relay with an error rather than an
/// empty answer, so the router counts it as a failed attempt.
pub fn relay(
    upstream: impl Stream<Item = Result<Bytes, ExecError>> + Send + 'static,
    model: String,
) -> impl Stream<Item = Result<Bytes, ExecError>> + Send {
    async_stream::stream! {
        let mut upstream = Box::pin(upstream);
        let mut body = BytesMut::new();
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(chunk) => body.extend_from_slice(&chunk),
                Err(e) => {
                    yield Err(e);
                    return;
                }
            }
        }
        let completion = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| ar_translate::gitlab_response_to_openai(&value, &model));
        let Some(completion) = completion else {
            yield Err(ExecError::Transport(
                "gitlab reply is not a completion".to_owned(),
            ));
            return;
        };
        for chunk in ar_translate::completion_chunks(&completion) {
            yield Ok(sse(&chunk));
        }
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_should_stream_the_whole_reply_as_chunks_then_done() {
        let reply = Bytes::from_static(br#"{"choices":[{"text":"hello"}]}"#);
        let text: String = relay(futures::stream::iter(vec![Ok(reply)]), "combo".to_owned())
            .map(|c| String::from_utf8_lossy(&c.expect("no transport error")).into_owned())
            .collect::<Vec<_>>()
            .await
            .concat();

        assert!(text.contains(r#""content":"hello""#), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    }

    #[tokio::test]
    async fn relay_should_fail_rather_than_stream_an_empty_answer() {
        let reply = Bytes::from_static(br#"{"detail":"Validation error"}"#);
        let mut stream = Box::pin(relay(
            futures::stream::iter(vec![Ok(reply)]),
            "m".to_owned(),
        ));
        let first = stream.next().await;

        assert!(
            matches!(first, Some(Err(ExecError::Transport(_)))),
            "{first:?}"
        );
    }

    #[test]
    fn render_should_refuse_media_on_the_gitlab_wire() {
        let core = crate::ArExec::new().expect("the client builds");
        let headers = std::collections::BTreeMap::new();
        let shape = crate::Dispatch {
            base_url: "https://gitlab.com",
            endpoint: None,
            wire_format: ar_registry::WireFormat::Gitlab,
            api_key: "k",
            upstream_model: "claude-haiku-4-5",
            stream: false,
            headers: &headers,
        };
        let canonical = br#"{"model":"m","messages":[{"role":"user","content":"look","media":[{"kind":"image_url","body":{"image_url":{"url":"data:image/png;base64,QUJD"}}}]}]}"#;

        let err = core
            .render_request(&shape, canonical)
            .expect_err("media is refused");

        assert!(
            matches!(err, crate::ExecError::UnsupportedContent(_, "media parts")),
            "{err:?}"
        );
    }
}
