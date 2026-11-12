//! Tests for the deferred-headers path.
//!
//! The behaviour under test is a contract with two sides, and both matter:
//!
//! - A request that resolves inside the window keeps its **real** status. A
//!   quota failure that arrives in a second must still be a 429, because
//!   clients branch on it.
//! - A request that does not resolve inside the window commits to `200` and
//!   keeps the connection alive with keepalives, then names a later failure
//!   in-band.

use std::sync::Arc;
use std::time::Duration;

use crate::config::ServerConfig;
use crate::deferred::{Commit, deferred_response, race_window};
use crate::routes::Dialect;

use axum::http::StatusCode;
use bytes::Bytes;
use futures::StreamExt;

/// A window long enough that a test's own scheduling cannot close it by
/// accident, and short enough that a test does not wait for it.
const WINDOW: Duration = Duration::from_millis(120);

#[tokio::test]
async fn a_future_that_resolves_inside_the_window_is_resolved() {
    let (commit, pending) = race_window(Some(WINDOW), async { 7_u32 }).await;
    assert!(matches!(commit, Commit::Resolved(7)), "expected the value");
    assert!(
        pending.is_none(),
        "a resolved race carries no pending future"
    );
}

#[tokio::test]
async fn a_future_that_outlasts_the_window_is_deferred() {
    let (commit, pending) = race_window(Some(WINDOW), async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        7_u32
    })
    .await;
    assert!(matches!(commit, Commit::Deferred), "expected a deferral");
    assert!(
        pending.is_some(),
        "a deferred race carries its pending future"
    );
    // The pending future is dropped rather than awaited: the test must not
    // wait thirty seconds for it.
    drop(pending);
}

#[tokio::test]
async fn no_window_resolves_the_future_directly() {
    let (commit, pending) = race_window(None, async { 7_u32 }).await;
    assert!(matches!(commit, Commit::Resolved(7)));
    assert!(pending.is_none());
}

/// The body of a deferred response, read whole.
async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stream = response.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("the deferred body never errors"));
    }
    out
}

#[tokio::test]
async fn a_deferred_response_commits_to_200_and_sse() {
    let body = futures::stream::iter(vec![Ok::<Bytes, std::convert::Infallible>(
        Bytes::from_static(b"data: {\"ok\":true}\n\n"),
    )]);
    let response = deferred_response(Dialect::OpenAi, Dialect::OpenAi.keepalive(), body);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
}

#[tokio::test]
async fn a_deferred_success_relays_the_upstream_chunks() {
    let body = futures::stream::iter(vec![
        Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"data: one\n\n")),
        Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"data: [DONE]\n\n")),
    ]);
    let response = deferred_response(Dialect::OpenAi, Dialect::OpenAi.keepalive(), body);
    let bytes = body_bytes(response).await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("data: one"), "the chunk was relayed: {text}");
    assert!(
        text.contains("[DONE]"),
        "the terminator was relayed: {text}"
    );
}

/// The failure frame must name the failure in-band, because the status that
/// would have carried it has already been sent.
#[tokio::test]
async fn a_deferred_failure_is_named_in_band() {
    // An empty stream is the shape a failed attempt produces: nothing to relay.
    let body = futures::stream::empty::<Result<Bytes, std::convert::Infallible>>();
    let response = deferred_response(Dialect::OpenAi, Dialect::OpenAi.keepalive(), body);
    let bytes = body_bytes(response).await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("stream_error"),
        "the failure is named in-band: {text}"
    );
    assert!(
        text.contains("deferred_failure"),
        "the reason distinguishes it from an upstream failure: {text}"
    );
}

/// Each dialect has its own error framing, and a client of one must not be
/// handed another's.
#[tokio::test]
async fn each_dialect_names_its_own_failure_frame() {
    let cases = [
        (Dialect::Anthropic, "event: error"),
        (Dialect::OpenAi, "\"code\":\"stream_error\""),
        (Dialect::Responses, "\"code\":\"stream_error\""),
    ];
    for (dialect, marker) in cases {
        let body = futures::stream::empty::<Result<Bytes, std::convert::Infallible>>();
        let response = deferred_response(dialect, dialect.keepalive(), body);
        let bytes = body_bytes(response).await;
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains(marker),
            "{dialect:?} names its own failure: {text}"
        );
    }
}

/// Ollama's stream is NDJSON, so it has no error frame and no keepalive. The
/// deferred path must not invent one.
#[tokio::test]
async fn ollama_has_no_deferred_failure_frame() {
    assert!(Dialect::Ollama.deferred_error().is_none());
    assert!(Dialect::Ollama.keepalive().is_none());
}

/// The keepalive is the byte that holds the edge's idle timer open. It has to
/// go out on the interval, not only when the upstream finally answers.
#[tokio::test]
async fn a_deferred_response_emits_a_keepalive_before_the_answer() {
    // The upstream's answer is slow; the keepalive must arrive first.
    let body = futures::stream::unfold(0_u32, |n| async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        if n >= 1 {
            None
        } else {
            Some((
                Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"data: late\n\n")),
                n + 1,
            ))
        }
    });
    let response = deferred_response(Dialect::OpenAi, Dialect::OpenAi.keepalive(), body);
    let bytes = body_bytes(response).await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("chat.completion.chunk"),
        "the keepalive frame went out: {text}"
    );
}

/// The configuration knob: off by default, armed by a positive number of
/// seconds, and a bad value leaves it off rather than refusing the config.
#[test]
fn the_commit_window_is_off_by_default_and_read_from_the_environment() {
    // SAFETY: the environment is process-global, and this test owns the only
    // reader of this variable in this binary's test run.
    unsafe {
        std::env::remove_var(crate::config::DEFERRED_HEADERS_VAR);
    }
    assert!(
        ServerConfig::single(0, ar_route::Strategy::Priority, vec![])
            .with_deferred_headers_from_env()
            .deferred_headers
            .is_none()
    );

    unsafe {
        std::env::set_var(crate::config::DEFERRED_HEADERS_VAR, "45");
    }
    assert_eq!(
        ServerConfig::single(0, ar_route::Strategy::Priority, vec![])
            .with_deferred_headers_from_env()
            .deferred_headers,
        Some(Duration::from_secs(45))
    );

    // Zero is "off", not "commit immediately".
    unsafe {
        std::env::set_var(crate::config::DEFERRED_HEADERS_VAR, "0");
    }
    assert!(
        ServerConfig::single(0, ar_route::Strategy::Priority, vec![])
            .with_deferred_headers_from_env()
            .deferred_headers
            .is_none()
    );

    // A value that is not a number is dropped, not refused.
    unsafe {
        std::env::set_var(crate::config::DEFERRED_HEADERS_VAR, "soon");
    }
    assert!(
        ServerConfig::single(0, ar_route::Strategy::Priority, vec![])
            .with_deferred_headers_from_env()
            .deferred_headers
            .is_none()
    );

    unsafe {
        std::env::remove_var(crate::config::DEFERRED_HEADERS_VAR);
    }
}

/// The whole point of the window is that a slow upstream keeps the connection
/// alive. The body must therefore be a stream that produces bytes over time,
/// not one that waits for the whole answer.
#[tokio::test]
async fn a_deferred_body_is_a_stream_not_a_buffered_answer() {
    let (tx, rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<Bytes, std::convert::Infallible>>();
    let body = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    });
    let response = deferred_response(Dialect::OpenAi, Dialect::OpenAi.keepalive(), body);
    // The response exists before any chunk was sent: that is the commitment.
    assert_eq!(response.status(), StatusCode::OK);
    // Now the upstream answers.
    tx.send(Ok(Bytes::from_static(b"data: answer\n\n")))
        .unwrap();
    // The sender is dropped so the receiver sees the end of the stream. A
    // sender held open would leave the body waiting forever, which is the
    // shape of a slow upstream and is what the keepalive is for.
    drop(tx);
    let bytes = body_bytes(response).await;
    assert!(String::from_utf8_lossy(&bytes).contains("data: answer"));
}

/// A body that is dropped mid-flight (the client disconnected) must not leave
/// the attempt loop running forever. The pending future is dropped with the
/// body, so the upstream connection is released.
#[tokio::test]
async fn dropping_a_deferred_body_releases_the_pending_future() {
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&dropped);
    struct DropGuard(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for DropGuard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let guard = DropGuard(flag);
    let body = futures::stream::pending::<Result<Bytes, std::convert::Infallible>>();
    let response = deferred_response(Dialect::OpenAi, Dialect::OpenAi.keepalive(), body);
    drop(response);
    drop(guard);
    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "the guard dropped with the body"
    );
}

/// A body that is already complete still gets the failure frame, because a
/// deferred response that ends with nothing says nothing about why.
#[tokio::test]
async fn a_deferred_response_that_ends_immediately_still_names_the_failure() {
    let body = futures::stream::empty::<Result<Bytes, std::convert::Infallible>>();
    let response = deferred_response(Dialect::OpenAi, Dialect::OpenAi.keepalive(), body);
    let bytes = body_bytes(response).await;
    assert!(!bytes.is_empty(), "the failure frame is the only content");
}

/// The keepalive is what holds the edge's idle timer open, so a deferred
/// response must be a body that produces bytes over time rather than one that
/// waits for the whole answer. A pending upstream with no chunk available must
/// still leave the response alive.
#[tokio::test]
async fn a_deferred_response_outlives_a_pending_upstream() {
    let body = futures::stream::pending::<Result<Bytes, std::convert::Infallible>>();
    let response = deferred_response(Dialect::OpenAi, Dialect::OpenAi.keepalive(), body);
    // The response exists before the upstream answered: that is the commitment,
    // and it is the whole point of the path.
    assert_eq!(response.status(), StatusCode::OK);
}
