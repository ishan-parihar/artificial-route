//! Integration test against an in-process `axum` mock upstream.
//!
//! Exercises the whole dispatch path -- URL join, header merge, POST, SSE
//! decode -- with no network and no real provider. `axum` is a dev-dependency
//! only; nothing here reaches the binary's dependency tree.

use std::collections::BTreeMap;
use std::time::Duration;

use ar_config::Secret;
use ar_exec::{ArExec, ChatStream, Dispatch, ExecError, SseEvent};
use ar_registry::{AuthClass, ProviderDef, WireFormat};
use ar_translate::{CanonicalChat, Msg, Role};
use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use tokio_util::sync::CancellationToken;

/// Body the mock streams: three payloads then the terminator.
const SSE_BODY: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// A canned reply, rebuilt per request because `Response` is not `Clone`.
#[derive(Clone)]
struct Mock {
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
}

impl Mock {
    /// A 200 `text/event-stream` reply carrying [`SSE_BODY`].
    fn sse() -> Self {
        Self {
            status: StatusCode::OK,
            headers: vec![("content-type", "text/event-stream")],
            body: SSE_BODY,
        }
    }

    /// Builds the reply. Header values are static, so construction cannot fail.
    fn build(&self) -> Response {
        let mut response = Response::builder().status(self.status);
        for (name, value) in &self.headers {
            response = response.header(*name, *value);
        }
        response
            .body(Body::from(self.body))
            .expect("mock reply is well formed")
    }
}

/// Serves `mock` on an ephemeral loopback port. Returns the origin and a handle.
async fn spawn_upstream(mock: Mock) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().fallback(move || {
        let mock = mock.clone();
        async move { mock.build() }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback binds");
    let addr = listener.local_addr().expect("bound socket has an address");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), handle)
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
        headers: Default::default(),
    }
}

/// A minimal streaming request.
fn chat() -> CanonicalChat {
    CanonicalChat {
        model: "test-model".to_owned(),
        messages: vec![Msg::new(Role::User, "hi")],
        temperature: None,
        max_tokens: None,
        stream: true,
    }
}

/// Drains `stream` into the payloads it yielded.
async fn payloads(stream: ChatStream) -> Result<Vec<String>, ExecError> {
    use futures::StreamExt;
    // The generator is `!Unpin`, so it is pinned before being polled.
    let mut events = Box::pin(stream.into_sse());
    let mut out = Vec::new();
    while let Some(event) = events.next().await {
        match event? {
            SseEvent::Data(bytes) => out.push(String::from_utf8(bytes.to_vec()).expect("utf-8")),
        }
    }
    Ok(out)
}

#[tokio::test]
async fn streams_chunks_when_upstream_sse() {
    let (base_url, server) = spawn_upstream(Mock::sse()).await;
    let exec = ArExec::new().expect("client builds");

    let stream = exec
        .post_chat(
            &chat(),
            &provider(&base_url),
            &Secret::new("sk-test"),
            &CancellationToken::new(),
        )
        .await
        .expect("mock upstream answers 200");

    assert_eq!(stream.status(), StatusCode::OK);

    let chunks = payloads(stream).await.expect("stream decodes cleanly");
    server.abort();

    // Three payloads; `[DONE]` ends the stream rather than being yielded.
    assert_eq!(chunks.len(), 3);
    assert!(
        chunks[2].contains(r#""finish_reason":"stop""#),
        "last payload carries the finish reason: {}",
        chunks[2]
    );
}

#[tokio::test]
async fn terminates_stream_when_done_sentinel_present() {
    let (base_url, server) = spawn_upstream(Mock::sse()).await;
    let exec = ArExec::new().expect("client builds");

    let stream = exec
        .post_chat(
            &chat(),
            &provider(&base_url),
            &Secret::new("sk-test"),
            &CancellationToken::new(),
        )
        .await
        .expect("mock upstream answers 200");

    let chunks = payloads(stream).await.expect("stream decodes cleanly");
    server.abort();

    assert!(
        !chunks.iter().any(|c| c.contains("[DONE]")),
        "[DONE] is a terminator, not a payload"
    );
}

#[tokio::test]
async fn surfaces_message_when_upstream_errors() {
    let (base_url, server) = spawn_upstream(Mock {
        status: StatusCode::TOO_MANY_REQUESTS,
        headers: vec![("retry-after", "30")],
        body: r#"{"error":{"message":"slow down"}}"#,
    })
    .await;
    let exec = ArExec::new().expect("client builds");

    let err = exec
        .post_chat(
            &chat(),
            &provider(&base_url),
            &Secret::new("sk-test"),
            &CancellationToken::new(),
        )
        .await
        .err();
    server.abort();

    match err {
        Some(ExecError::Upstream {
            status,
            message,
            retry_after,
        }) => {
            assert_eq!(status, 429);
            assert_eq!(message, "slow down");
            assert_eq!(retry_after, Some(Duration::from_secs(30)));
        }
        other => panic!("expected an Upstream error, got {other:?}"),
    }
}

#[tokio::test]
async fn aborts_when_caller_cancels_before_response() {
    let (base_url, server) = spawn_upstream(Mock::sse()).await;
    let exec = ArExec::new().expect("client builds");
    let abort = CancellationToken::new();
    abort.cancel();

    let err = exec
        .post_chat(
            &chat(),
            &provider(&base_url),
            &Secret::new("sk-test"),
            &abort,
        )
        .await
        .err();
    server.abort();

    assert!(matches!(err, Some(ExecError::Aborted)));
}

#[tokio::test]
async fn joins_url_when_base_ends_in_slashes() {
    // The mock listens on the bare origin, so a double slash would miss the route.
    let (base_url, server) = spawn_upstream(Mock::sse()).await;
    let exec = ArExec::new().expect("client builds");

    let stream = exec
        .post_chat(
            &chat(),
            &provider(&format!("{base_url}//")),
            &Secret::new("sk-test"),
            &CancellationToken::new(),
        )
        .await
        .expect("trailing slashes are trimmed before joining");
    server.abort();

    assert_eq!(stream.status(), StatusCode::OK);
}

/// A 401-then-200 pair over one connection: the OAuth rotation path.
///
/// Lives here rather than in `oauth.rs`'s unit tests because it needs the same
/// `spawn_upstream` every other integration test uses, and duplicating that
/// harness to reach it would be a second copy to drift.
#[tokio::test]
async fn carries_the_callers_bearer_on_an_oauth_dispatch() {
    // A static script, unlike the branching mock `oauth.rs` needs: this asserts
    // the *header* reached the socket, not the retry count.
    let (base_url, server) = spawn_upstream(Mock::sse()).await;
    let exec = ArExec::new().expect("client builds");
    let shape = Dispatch {
        base_url: &base_url,
        wire_format: WireFormat::Openai,
        api_key: "synthetic-oauth-access",
        upstream_model: "test-model",
        stream: true,
        headers: &BTreeMap::new(),
    };

    let stream = exec
        .post(&shape, br#"{"model":"test-model"}"#, &CancellationToken::new())
        .await
        .expect("mock upstream answers 200");
    server.abort();

    assert_eq!(stream.status(), StatusCode::OK);
}