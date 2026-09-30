//! Integration tests for the media family against an in-process mock upstream.
//!
//! Same harness as `mock_upstream.rs` — an `axum` dev-dependency serving one
//! canned reply on an ephemeral loopback port — so the whole dispatch path runs
//! with no network and no real provider. Each test asserts the endpoint the
//! request actually reached, because the whole point of the media family is
//! that a part is never silently dropped: if the path is wrong the caller's
//! media went somewhere else, and a 2xx from the wrong place would still pass a
//! status-only assertion.

use std::sync::{Arc, Mutex};

use ar_config::Secret;
use ar_exec::{ArExec, MediaBody, MediaEndpoint};
use ar_registry::{AuthClass, ProviderDef, WireFormat};
use ar_translate::{EmbeddingRequest, Modality, OpenAIChat, chat_modality, to_canonical};
use axum::Router;
use axum::body::Body;
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use tokio_util::sync::CancellationToken;

/// One `Vec<u8>`, `Clone`-able because [`Response`] is not.
type Reply = Arc<Vec<u8>>;

/// Serves `body` on every route while recording the paths that were requested.
async fn spawn_upstream(body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let reply: Reply = Arc::new(body.as_bytes().to_vec());

    let app = Router::new().fallback(move |uri: Uri| {
        let recorder = Arc::clone(&recorder);
        let reply = Arc::clone(&reply);
        async move {
            recorder.lock().expect("recorder lock").push(uri.path().to_owned());
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::from(reply.as_ref().clone()))
                .expect("mock reply is well formed")
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback binds");
    let addr = listener.local_addr().expect("bound socket has an address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), seen)
}

/// A `ProviderDef` pointing at `base_url`.
///
/// Built field-by-field rather than spread over a `Default`, because
/// `ar-registry` is extending `ProviderDef` in parallel and a `Default` spread
/// would silently inherit a *default* price or executor instead of "none".
fn provider(base_url: &str) -> ProviderDef {
    ProviderDef {
        base_url: base_url.to_owned(),
        wire_format: WireFormat::Openai,
        auth: AuthClass::ApiKey,
        env_hint: "TEST_API_KEY".to_owned(),
        models: vec![],
        prices: Default::default(),
        executor: ar_core::Strng::from("default"),
        auth_kind: ar_core::Strng::from("api_key"),
        flat_rate: false,
    }
}

/// Paths the mock was asked for, so far.
fn paths(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock().expect("recorder lock").clone()
}

#[tokio::test]
async fn embeds_when_valid() {
    let (base_url, seen) =
        spawn_upstream(r#"{"data":[{"embedding":[0.1,0.2],"index":0}],
            "model":"text-embedding-3-small",
            "usage":{"prompt_tokens":3,"total_tokens":3}}"#)
        .await;
    let exec = ArExec::new().expect("client builds");

    let body: EmbeddingRequest =
        serde_json::from_str(r#"{"model":"text-embedding-3-small","input":["a","b"]}"#)
            .expect("fixture is valid embeddings");

    let rendered = exec
        .post_embeddings(
            &body,
            &provider(&base_url),
            &Secret::new("sk-test"),
            &CancellationToken::new(),
        )
        .await
        .expect("guard passes and mock upstream answers 200");

    assert_eq!(paths(&seen), vec!["/embeddings".to_owned()]);
    assert_eq!(rendered["object"], "list");
}

#[tokio::test]
async fn rejects_embeddings_when_model_outside_registry() {
    let (base_url, seen) = spawn_upstream(r#"{"data":[]}"#).await;
    let exec = ArExec::new().expect("client builds");

    let body: EmbeddingRequest =
        serde_json::from_str(r#"{"model":"my-local-embed","input":"a"}"#)
            .expect("fixture is valid embeddings");

    let err = exec
        .post_embeddings(
            &body,
            &provider(&base_url),
            &Secret::new("sk-test"),
            &CancellationToken::new(),
        )
        .await
        .err();

    assert!(matches!(err, Some(ar_exec::ExecError::Media(_))));
    assert!(
        paths(&seen).is_empty(),
        "the guard runs before dispatch, so no upstream call is spent"
    );
}

#[tokio::test]
async fn routes_vision_when_image_part() {
    let (base_url, seen) = spawn_upstream(r#"{"id":"x","object":"chat.completion"}"#).await;
    let exec = ArExec::new().expect("client builds");

    // The inbound part decides the modality; the modality decides the endpoint.
    let inbound: OpenAIChat = serde_json::from_str(
        r#"{"model":"gpt-5.4","messages":[{"role":"user","content":[
            {"type":"text","text":"what is this?"},
            {"type":"image_url","image_url":{"url":"http://x/i.png"}}]}]}"#,
    )
    .expect("fixture is valid OpenAI chat");

    let canonical = to_canonical(inbound).expect("image parts are routable");
    assert_eq!(chat_modality(&canonical), Modality::Vision);

    let endpoint = MediaEndpoint::for_modality(chat_modality(&canonical));
    let response = exec
        .post_media(
            endpoint,
            &MediaBody { content_type: "application/json", bytes: b"{}" },
            &provider(&base_url),
            &Secret::new("sk-test"),
            &CancellationToken::new(),
        )
        .await
        .expect("mock upstream answers 200");

    assert_eq!(paths(&seen), vec!["/chat/completions".to_owned()]);
    assert_eq!(response.status, StatusCode::OK);
}

#[tokio::test]
async fn transcribes_when_audio() {
    let (base_url, seen) = spawn_upstream(r#"{"text":"hello"}"#).await;
    let exec = ArExec::new().expect("client builds");

    let response = exec
        .post_media(
            MediaEndpoint::Transcriptions,
            // The audio endpoints are multipart; the bytes are the caller's.
            &MediaBody { content_type: "multipart/form-data", bytes: b"--b\r\n\r\n" },
            &provider(&base_url),
            &Secret::new("sk-test"),
            &CancellationToken::new(),
        )
        .await
        .expect("mock upstream answers 200");

    assert_eq!(paths(&seen), vec!["/audio/transcriptions".to_owned()]);
    assert_eq!(response.bytes.as_ref(), br#"{"text":"hello"}"#);
}

#[tokio::test]
async fn forwards_translations_when_audio_targeted_english() {
    let (base_url, seen) = spawn_upstream(r#"{"text":"hello"}"#).await;
    let exec = ArExec::new().expect("client builds");

    exec.post_media(
        MediaEndpoint::Translations,
        &MediaBody { content_type: "multipart/form-data", bytes: b"--b\r\n\r\n" },
        &provider(&base_url),
        &Secret::new("sk-test"),
        &CancellationToken::new(),
    )
    .await
    .expect("mock upstream answers 200");

    assert_eq!(paths(&seen), vec!["/audio/translations".to_owned()]);
}

#[tokio::test]
async fn forwards_ocr_when_document_route_requested() {
    let (base_url, seen) = spawn_upstream(r#"{"pages":[{"text":"x"}]}"#).await;
    let exec = ArExec::new().expect("client builds");

    exec.post_media(
        MediaEndpoint::Ocr,
        &MediaBody { content_type: "application/json", bytes: br#"{"model":"ocr-1"}"# },
        &provider(&base_url),
        &Secret::new("sk-test"),
        &CancellationToken::new(),
    )
    .await
    .expect("mock upstream answers 200");

    assert_eq!(paths(&seen), vec!["/ocr".to_owned()]);
}

#[tokio::test]
async fn surfaces_message_when_media_upstream_errors() {
    let app = Router::new().fallback(|| async {
        Response::builder()
            .status(StatusCode::UNPROCESSABLE_ENTITY)
            .body(Body::from(r#"{"error":{"message":"bad document"}}"#))
            .expect("mock reply is well formed")
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback binds");
    let addr = listener.local_addr().expect("bound socket has an address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let exec = ArExec::new().expect("client builds");

    let err = exec
        .post_media(
            MediaEndpoint::Ocr,
            &MediaBody { content_type: "application/json", bytes: b"{}" },
            &provider(&format!("http://{addr}")),
            &Secret::new("sk-test"),
            &CancellationToken::new(),
        )
        .await
        .err();
    server.abort();

    match err {
        Some(ar_exec::ExecError::Upstream { status, message, .. }) => {
            assert_eq!(status, 422);
            assert_eq!(message, "bad document");
        }
        other => panic!("expected an Upstream error, got {other:?}"),
    }
}
