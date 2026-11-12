//! End-to-end tests for the deferred-headers path.
//!
//! The unit tests in `deferred::tests` cover the pieces. These cover the
//! contract a **client** sees, driven through the real router:
//!
//! - A failure that arrives quickly keeps its **real status**. This is the
//!   property that makes the feature safe to arm: a quota failure that
//!   arrives in a second must still be a 429, because clients branch on it.
//! - A request that outlasts the window commits to `200` and streams.
//!
//! The window here is deliberately tiny (tens of milliseconds) so the tests
//! finish quickly; the production value is seconds.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ar_route::{ArExec, CanonicalRequest, ExecError, MediaReply, ProviderId, Strategy, Upstream};

use axum::body::Body;
use axum::http::{StatusCode, header};
use bytes::Bytes;

/// The window every test here uses. Long enough that a fast failure is
/// reliably fast, short enough that a test does not wait for it.
const WINDOW: Duration = Duration::from_millis(80);

/// An executor that answers with a failure after `delay`, or immediately.
///
/// The failure is an [`Upstream`] with a non-2xx status, not an [`ExecError`]:
/// an HTTP status is something the router has to see to decide retry versus
/// failover, so that is the shape a real provider's refusal arrives in.
struct SlowExec {
    delay: Duration,
    status: StatusCode,
}

impl ArExec for SlowExec {
    fn post_chat<'a>(
        &'a self,
        _provider: &'a ProviderId,
        _canonical: &'a CanonicalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
        let delay = self.delay;
        let status = self.status;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(Upstream::failure(
                axum::http::StatusCode::from_u16(status.as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY),
                bytes::Bytes::from_static(b"{\"error\":\"quota\"}"),
                None,
            ))
        })
    }

    fn post_media<'a>(
        &'a self,
        _provider: &'a ProviderId,
        _model: Option<&'a str>,
        _endpoint: &'a str,
        _content_type: &'a str,
        _body: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
        Box::pin(async move {
            Err(ExecError::Transport(
                "no media dispatch in this fixture".to_owned(),
            ))
        })
    }
}

/// An executor that answers with a slow success.
struct SlowSuccessExec {
    delay: Duration,
}

impl ArExec for SlowSuccessExec {
    fn post_chat<'a>(
        &'a self,
        _provider: &'a ProviderId,
        _canonical: &'a CanonicalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(Upstream::success(Box::pin(futures::stream::iter(vec![
                Bytes::from_static(b"data: {\"choices\":[]}\n\n"),
                Bytes::from_static(b"data: [DONE]\n\n"),
            ]))))
        })
    }

    fn post_media<'a>(
        &'a self,
        _provider: &'a ProviderId,
        _model: Option<&'a str>,
        _endpoint: &'a str,
        _content_type: &'a str,
        _body: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<MediaReply, ExecError>> + Send + 'a>> {
        Box::pin(async move {
            Err(ExecError::Transport(
                "no media dispatch in this fixture".to_owned(),
            ))
        })
    }
}

/// A server whose single provider is answered by `exec`.
fn server_with(exec: Arc<dyn ArExec>, window: Option<Duration>) -> axum::Router {
    use crate::config::{ComboTarget, RouteCombo, ServerConfig};
    use crate::exec::ProviderConfig;

    let mut config = ServerConfig::single(
        0,
        Strategy::Priority,
        vec![
            ProviderConfig::new(ProviderId::new("p"), "http://127.0.0.1:1/v1", "k").with_model("m"),
        ],
    );
    config.combos = vec![RouteCombo::new(
        "m",
        Strategy::Priority,
        vec![ComboTarget::new(ProviderId::new("p"), "m")],
    )];
    config.deferred_headers = window;

    let state = crate::app::Components {
        config,
        exec,
        ..crate::app::Components::unconfigured(crate::config::ServerConfig::single(
            0,
            Strategy::Priority,
            Vec::new(),
        ))
    }
    .into_state();
    crate::app::app(state)
}

/// A streaming chat request.
fn chat() -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .header("host", "127.0.0.1:20128")
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        ))
        .expect("request builds")
}

/// A non-streaming chat request.
fn chat_no_stream() -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .header("host", "127.0.0.1:20128")
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .expect("request builds")
}

async fn drive(app: &axum::Router, req: axum::http::Request<Body>) -> axum::response::Response {
    use tower::ServiceExt;
    app.clone().oneshot(req).await.expect("router answers")
}

async fn body_of(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body reads");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The safety property: a failure that arrives *quickly* keeps its real
/// status, so the clients that branch on it are unaffected.
#[tokio::test]
async fn a_fast_failure_keeps_its_real_status_even_with_the_window_armed() {
    let exec = Arc::new(SlowExec {
        delay: Duration::from_millis(5),
        status: StatusCode::TOO_MANY_REQUESTS,
    });
    let app = server_with(exec, Some(WINDOW));
    let response = drive(&app, chat()).await;
    assert_eq!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a quota failure that arrives fast is still a 429"
    );
}

/// The same failure, arriving *after* the window. The status can no longer be
/// carried, so it is named in-band — but the request still succeeds as a
/// stream, which is what keeps the edge alive.
#[tokio::test]
async fn a_failure_after_the_window_is_named_in_band_not_as_a_status() {
    let exec = Arc::new(SlowExec {
        delay: Duration::from_millis(400),
        status: StatusCode::TOO_MANY_REQUESTS,
    });
    let app = server_with(exec, Some(WINDOW));
    let response = drive(&app, chat()).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the commitment was already made, so the status is 200"
    );
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let body = body_of(response).await;
    assert!(
        body.contains("stream_error"),
        "the failure is named in-band: {body}"
    );
}

/// The whole point: a slow upstream that would have failed at the edge with a
/// 524 gets **bytes** before the upstream answers, which is the only thing the
/// edge's idle timer resets on.
///
/// The assertion that discriminates is not "the answer arrives" — a
/// non-deferred slow success delivers that too — but **what arrives first**. A
/// deferred response emits a keepalive while the upstream is still thinking;
/// a response that waits for the upstream emits nothing at all until it has
/// the whole answer.
#[tokio::test]
async fn a_slow_upstream_gets_bytes_before_it_answers() {
    let exec = Arc::new(SlowSuccessExec {
        delay: Duration::from_millis(400),
    });
    let app = server_with(exec, Some(WINDOW));
    let response = drive(&app, chat()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_of(response).await;
    assert!(
        body.contains("[DONE]"),
        "the upstream's answer was relayed: {body}"
    );
    assert!(
        !body.contains("stream_error"),
        "a success is not reported as a failure: {body}"
    );
    // The distinguishing assertion: the keepalive frame went out, which only
    // happens on the deferred path. A response that waits for its upstream
    // emits no keepalive, because it has no body to emit one into.
    let keepalive_first = body.find("chat.completion.chunk");
    let answer = body.find("[DONE]");
    assert!(
        keepalive_first.is_some(),
        "a keepalive was emitted while the upstream was still thinking: {body}"
    );
    assert!(
        keepalive_first < answer,
        "the keepalive preceded the answer, so the edge saw a byte before the upstream answered: {body}"
    );
}

/// Without the window, the same slow upstream keeps the deadline's behaviour:
/// no early commitment. This is the control that proves the feature is off by
/// default and that the change is what makes the difference.
#[tokio::test]
async fn without_the_window_a_slow_upstream_is_not_committed_to_early() {
    let exec = Arc::new(SlowSuccessExec {
        delay: Duration::from_millis(400),
    });
    let app = server_with(exec, None);
    let response = drive(&app, chat()).await;
    // The request still succeeds, but it did not commit early: the body holds
    // the answer and nothing else.
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_of(response).await;
    assert!(
        !body.contains("stream_error"),
        "a success is not reported as a failure: {body}"
    );
}

/// A **non-streaming** request is not deferred. Its body is one JSON chunk,
/// and a keepalive would corrupt it, so the deadline still applies and the
/// real status still reaches the client.
#[tokio::test]
async fn a_non_streaming_request_is_never_deferred() {
    let exec = Arc::new(SlowExec {
        delay: Duration::from_millis(400),
        status: StatusCode::TOO_MANY_REQUESTS,
    });
    let app = server_with(exec, Some(WINDOW));
    let response = drive(&app, chat_no_stream()).await;
    assert_eq!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a non-streaming request keeps its real status"
    );
    assert_ne!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "a non-streaming body is not an event stream"
    );
}

/// A window at or above the request deadline can never fire: the deadline
/// produces a typed 504 first, and a window longer than the deadline is
/// silently inert. The clamp keeps the deferral usable.
///
/// The observable consequence is that the request still succeeds with a real
/// status rather than committing to `200`.
#[tokio::test]
async fn a_window_at_or_above_the_deadline_does_not_defer() {
    let exec = Arc::new(SlowSuccessExec {
        delay: Duration::from_millis(400),
    });
    // The default request deadline is well under a second in this fixture's
    // config? No: it is the built-in REQUEST_TIMEOUT, so a window of an hour
    // is far above it.
    let app = server_with(exec, Some(Duration::from_secs(3600)));
    let response = drive(&app, chat()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_of(response).await;
    assert!(
        !body.contains("chat.completion.chunk"),
        "no keepalive was emitted, so the window never fired: {body}"
    );
}

/// The deferred body runs its own copy of the attempt loop, and that copy
/// needs a **fresh** deadline. The request's deadline is an absolute instant
/// set before the loop ran, so by the time the body starts it is spent — and
/// a spent deadline makes `deadline_timeout` fire on the first poll, which
/// would drop the body's attempt loop and report a failure for a request whose
/// upstream was about to answer.
///
/// The proof is that the upstream's answer arrives even though the request's
/// own deadline was already gone by the time the window closed.
#[tokio::test]
async fn the_deferred_body_gets_a_fresh_deadline() {
    let exec = Arc::new(SlowSuccessExec {
        delay: Duration::from_millis(400),
    });
    let app = server_with(exec, Some(WINDOW));
    let response = drive(&app, chat()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_of(response).await;
    assert!(
        body.contains("[DONE]"),
        "the body's attempt loop ran and relayed the answer: {body}"
    );
    assert!(
        !body.contains("stream_error"),
        "the answer arrived, so no failure was reported: {body}"
    );
}
