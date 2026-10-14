//! End-to-end: one configured provider, mock upstream, real HTTP.
//!
//! The proxy is driven through `tower::ServiceExt::oneshot` on the real router
//! (so every layer runs) and the *upstream* is a real second axum server on an
//! ephemeral port, because that is the only way the reqwest executor is
//! exercised at all. The mock is hand-rolled rather than `wiremock` because the
//! dep tree is already here and this test needs to emit deliberately shaped SSE
//! frames plus a `Retry-After` header — easier as a handler than a fixture.

use std::net::SocketAddr;
use std::sync::Arc;

use ar_route::{ProviderId, Strategy};
use ar_server::{
    Components, HttpExec, ModelCard, ProviderConfig, ServerConfig, TRACE_HEADER, server,
};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A minimal response, used only where a builder failure would mean the test
/// itself is broken.
fn fallback(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Boots a mock OpenAI-compatible upstream and returns its base URL.
///
/// The handler echoes back the model it received and whether the bearer token
/// arrived, so the test can prove the body *and* the credential crossed the
/// proxy rather than being reconstructed on the far side.
async fn mock_upstream() -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|headers: HeaderMap, body: Body| async move {
            let bytes = axum::body::to_bytes(body, 64 * 1024)
                .await
                .unwrap_or_default();
            let auth = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none")
                .to_owned();
            let got: serde_json::Value =
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            let echo = serde_json::json!({
                "model": got["model"],
                "auth": auth,
                "stream": got["stream"],
            })
            .to_string();

            // Three frames, so a buffered implementation that lost frame
            // boundaries would fail the relay assertions.
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(futures::stream::iter([
                    Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {echo}\n\n"))),
                    Ok(Bytes::from_static(
                        b"data: {\"delta\":{\"content\":\"Hel\"}}\n\n",
                    )),
                    Ok(Bytes::from_static(b"data: [DONE]\n\n")),
                ])))
                .unwrap_or_else(|_| fallback(StatusCode::INTERNAL_SERVER_ERROR))
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr: SocketAddr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

/// Builds the proxy router plus the state behind it.
fn boot(upstream: &str) -> (Router, ar_server::AppState) {
    let config = ServerConfig::from_provider(
        None, // port: the router never reads it
        Some(upstream.to_owned()),
        Some("mock-model".to_owned()),
        Some("sk-test-key".to_owned()),
        Some("mockprov".to_owned()),
        Some("priority".to_owned()),
        Some("0.15".to_owned()),
    );
    boot_with(config, vec![ModelCard::new("mockprov", "mock-model")])
}

/// Builds the router from a caller-built config, with an exact cache enabled.
///
/// The cache is on because the live path now consults it, and a suite that only
/// ever ran with the cache off would not notice a cache that never hits.
fn boot_with(config: ServerConfig, extra_models: Vec<ModelCard>) -> (Router, ar_server::AppState) {
    let exec = HttpExec::new(config.providers.clone()).expect("executor builds");
    let s = server(Components {
        cache_bytes: Some(Some(1 << 20)),
        extra_models,
        ..Components::with_exec(config, Arc::new(exec) as Arc<dyn ar_route::ArExec>)
    });
    (s.router, s.state)
}

/// Drives one request through the full layer stack.
async fn call(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let resp = app.clone().oneshot(req).await.expect("router responds");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn post_chat(body: &str, extra: &[(&str, &str)]) -> Request<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "text/event-stream")
        .body(Body::from(body.to_owned()))
        .expect("request builds");
    for (k, v) in extra {
        req.headers_mut().insert(
            axum::http::HeaderName::try_from(*k).expect("header name"),
            axum::http::HeaderValue::from_str(v).expect("header value"),
        );
    }
    req
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("request builds")
}

fn header_of(map: &HeaderMap, name: &str) -> String {
    map.get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn e2e_single_provider_when_key_set() {
    let upstream = mock_upstream().await;
    let (router, state) = boot(&upstream);

    // --- POST /v1/chat/completions, streaming ---------------------------
    let body = r#"{"model":"mockprov/mock-model","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, headers, text) =
        call(&router, post_chat(body, &[("x-ar-session", "e2e-1")])).await;

    assert_eq!(status, 200, "body was: {text}");
    // SSE frames relayed, boundaries intact, terminator present.
    assert_eq!(text.matches("data:").count(), 3, "frames lost: {text}");
    assert!(text.contains("[DONE]"), "stream not terminated: {text}");
    // The upstream's echo proves the body and the bearer token crossed the
    // proxy, and that the model was rewritten to the provider's spelling.
    assert!(
        text.contains("sk-test-key"),
        "upstream never saw the key: {text}"
    );
    assert!(
        text.contains("mock-model"),
        "model was not rewritten: {text}"
    );
    assert_eq!(header_of(&headers, "content-type"), "text/event-stream");

    // --- decision headers ----------------------------------------------
    let decision = header_of(&headers, "x-ar-decision");
    assert!(
        decision.contains("strategy=priority")
            && decision.contains("outcome=ok")
            && decision.contains("provider=mockprov"),
        "unexpected decision header: {decision}"
    );
    assert!(header_of(&headers, "x-ar-usage").contains("attempts=1"));
    assert_eq!(header_of(&headers, "x-ar-cache"), "bypass");
    assert!(
        !header_of(&headers, TRACE_HEADER).is_empty(),
        "no trace id echoed"
    );

    // --- GET /v1/models ------------------------------------------------
    let (status, headers, text) = call(&router, get("/v1/models")).await;
    assert_eq!(status, 200);
    let listed: serde_json::Value = serde_json::from_str(&text).expect("models body is JSON");
    assert_eq!(listed["object"], "list");
    assert_eq!(listed["data"][0]["id"], "mockprov/mock-model");
    assert_eq!(header_of(&headers, "x-ar-cache"), "fresh");

    // --- GET /healthz --------------------------------------------------
    let (status, _, text) = call(&router, get("/healthz")).await;
    assert_eq!((status.as_u16(), text.trim()), (200, "ok"));

    // --- GET /metrics --------------------------------------------------
    let (status, headers, text) = call(&router, get("/metrics")).await;
    assert_eq!(status, 200);
    assert!(header_of(&headers, "content-type").starts_with("text/plain"));
    assert!(
        text.contains("ar_http_requests_total"),
        "metrics missing: {text}"
    );
    // P0 gate from docs/04-obs: no request content may reach /metrics.
    assert!(
        !text.contains("hi\""),
        "metrics leaked request content: {text}"
    );

    // The lkgp pin from the streaming request must have landed.
    assert!(
        state.lkgp.get("e2e-1").is_some(),
        "successful request did not record an lkgp pin"
    );
    assert!(
        state
            .metrics
            .render()
            .contains("ar_upstream_attempts_total 1"),
        "attempt was not counted"
    );
}

#[tokio::test]
async fn e2e_fails_over_when_first_provider_429s() {
    let throttled = always_429().await;
    let config = two_provider_config(&throttled);
    let (router, _state) = boot_with(config, vec![]);

    // p1 429s with Retry-After: 30; p2 points at a closed port so it also
    // fails. The client must be told to come back, not handed a 502 that reads
    // like "our fault, try a different chain".
    let (status, headers, text) = call(&router, post_chat(r#"{"model":"m1"}"#, &[])).await;

    assert_eq!(status, 429, "expected a retry verdict: {text}");
    assert!(
        !header_of(&headers, "retry-after").is_empty(),
        "no Retry-After advertised to the client"
    );
    let decision = header_of(&headers, "x-ar-decision");
    assert!(
        decision.contains("outcome=retry"),
        "decision was: {decision}"
    );
    assert!(decision.contains("provider=p1"), "decision was: {decision}");
}

#[tokio::test]
async fn e2e_returns_503_when_no_provider_configured() {
    let config = ServerConfig::from_provider(None, None, None, None, None, None, None);
    let exec = HttpExec::new(vec![]).expect("executor builds with no providers");
    let s = server(Components::with_exec(
        config,
        Arc::new(exec) as Arc<dyn ar_route::ArExec>,
    ));

    let (status, _, text) = call(&s.router, post_chat(r#"{"model":"m"}"#, &[])).await;
    assert_eq!(status, 503);
    // The message says what to configure rather than leaking internals.
    assert!(
        text.contains("no provider is configured"),
        "unhelpful error: {text}"
    );
}

#[tokio::test]
async fn e2e_rejects_oversized_body() {
    let upstream = mock_upstream().await;
    let (router, _state) = boot(&upstream);
    // 50MB, the reference platform's ceiling, plus 1KB. This asserts the real
    // edge limit rather than the old 2MiB one, and it fails if axum's own
    // 2MiB `DefaultBodyLimit` is ever left enabled underneath — that is exactly
    // how a raised MAX_BODY_BYTES silently did nothing.
    let big = "x".repeat(ar_server::MAX_BODY_BYTES + 1024);
    let (status, _, _) = call(&router, post_chat(&big_body(&big), &[])).await;
    assert_eq!(status, 413);
}

#[tokio::test]
async fn e2e_admits_a_body_above_the_old_two_mib_limit() {
    let upstream = mock_upstream().await;
    let (router, _state) = boot(&upstream);
    // The regression this guards: a coding agent's pasted file is a few MB and
    // used to be refused here. 4MiB is comfortably inside the new ceiling.
    let big = "x".repeat(4 * 1024 * 1024);
    let (status, _, _) = call(&router, post_chat(&big_body(&big), &[])).await;
    assert_ne!(
        status, 413,
        "a 4MiB body must reach the handler, not the edge"
    );
}

#[tokio::test]
async fn e2e_returns_400_for_body_without_model() {
    let upstream = mock_upstream().await;
    let (router, _state) = boot(&upstream);
    let (status, _, text) = call(&router, post_chat(r#"{"messages":[]}"#, &[])).await;
    assert_eq!(status, 400);
    assert!(text.contains("model"), "unhelpful error: {text}");
}

#[tokio::test]
async fn e2e_fails_over_when_a_200_carries_an_error_body() {
    // p1 answers 200 with a provider error object, p2 is healthy: the
    // non-streaming request must fail over, not relay the error as a 200.
    let broken = error_shaped_200().await;
    let healthy = mock_upstream().await;
    let config = error_shaped_config(&broken, &healthy);
    let (router, _state) = boot_with(config, vec![]);
    let (status, headers, text) = call(
        &router,
        post_chat(r#"{"model":"m1","stream":false}"#, &[]),
    )
    .await;

    assert_eq!(status, 200, "the healthy member must serve: {text}");
    assert!(
        text.contains("\"content\":\"Hel\""),
        "p2's answer did not arrive: {text}"
    );
    let decision = header_of(&headers, "x-ar-decision");
    assert!(
        decision.contains("outcome=ok") && decision.contains("provider=p2"),
        "decision was: {decision}"
    );
    assert!(
        header_of(&headers, "x-ar-usage").contains("attempts=2"),
        "decision was: {decision}"
    );
}

#[tokio::test]
async fn e2e_fails_over_when_a_200_stream_carries_only_an_error() {
    // p1 answers 200 with an error-only SSE body, p2 is healthy: the
    // non-streaming request buffers p1's frames and must still fail over.
    let broken = error_shaped_sse().await;
    let healthy = mock_upstream().await;
    let config = error_shaped_config(&broken, &healthy);
    let (router, _state) = boot_with(config, vec![]);
    let (status, headers, text) = call(
        &router,
        post_chat(r#"{"model":"m1","stream":false}"#, &[]),
    )
    .await;

    assert_eq!(status, 200, "the healthy member must serve: {text}");
    assert!(
        text.contains("\"delta\":{\"content\":\"Hel\"}"),
        "p2's answer did not arrive: {text}"
    );
    let decision = header_of(&headers, "x-ar-decision");
    assert!(
        decision.contains("outcome=ok") && decision.contains("provider=p2"),
        "decision was: {decision}"
    );
}

/// A 200 whose body the proxy inspected: the upstream answered 200 but its
/// body is a provider error object, so the stub must fail over. Priority
/// order: the error-shaped upstream first, the healthy one second.
async fn error_shaped_200() -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"id":"gen-1","error":{"code":502,"message":"Provider returned error"}}"#,
                ))
                .unwrap_or_else(|_| fallback(StatusCode::INTERNAL_SERVER_ERROR))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

/// A 200 whose stream carries only an error event: the non-streaming client's
/// attempt sees the buffered frames and must fail over to the healthy member.
async fn error_shaped_sse() -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(futures::stream::iter([
                    Ok::<Bytes, std::io::Error>(Bytes::from_static(
                        b"data: {\"error\":{\"code\":503,\"message\":\"busy\"}}\n\n",
                    )),
                    Ok(Bytes::from_static(b"data: [DONE]\n\n")),
                ])))
                .unwrap_or_else(|_| fallback(StatusCode::INTERNAL_SERVER_ERROR))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

/// Two providers: the error-shaped first, the healthy mock second.
fn error_shaped_config(broken: &str, healthy: &str) -> ServerConfig {
    let providers = vec![
        ProviderConfig::new(ProviderId::new("p1"), broken, "k").with_model("m1"),
        ProviderConfig::new(ProviderId::new("p2"), healthy, "k").with_model("m2"),
    ];
    ServerConfig::single(20128, Strategy::Priority, providers)
}

/// An upstream that always 429s with `Retry-After: 30`.
async fn always_429() -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header(header::RETRY_AFTER, "30")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error":"slow down"}"#))
                .unwrap_or_else(|_| fallback(StatusCode::INTERNAL_SERVER_ERROR))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

/// Two providers: the throttled one first, then an unreachable one.
fn two_provider_config(throttled: &str) -> ServerConfig {
    let providers = vec![
        ProviderConfig::new(ProviderId::new("p1"), throttled, "k").with_model("m1"),
        // Port 1 is reserved and refuses connections, which is the cheapest way to
        // get a deterministic transport failure.
        ProviderConfig::new(ProviderId::new("p2"), "http://127.0.0.1:1/v1", "k").with_model("m2"),
    ];
    ServerConfig::single(20128, Strategy::Priority, providers)
}

/// Wraps padding into a valid chat body.
fn big_body(pad: &str) -> String {
    format!(r#"{{"model":"m","pad":"{pad}"}}"#)
}

/// A gateway that answers in the Cline envelope: `{"success":true,
/// "data":{<completion>}}`. Provider id `cline` is what keys the unwrap —
/// `ProviderConfig::oauth_executor` parses the id — so the e2e exercises the
/// exact production arm (an API-key cline row, no OAuth connection).
async fn cline_gateway() -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"success":true,"data":{"id":"c1","choices":[{"index":0,"message":{"role":"assistant","content":"Hel"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":4}}}"#,
                ))
                .unwrap_or_else(|_| fallback(StatusCode::INTERNAL_SERVER_ERROR))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn e2e_strips_the_cline_envelope_and_counts_its_usage() {
    // MW2+MW3: the envelope must come off before the client sees the body
    // (top-level `choices`), and the usage hidden under `data` must reach the
    // token headers that read top-level `usage`. Before the unwrap both read
    // empty: the client saw `success` and the ledger saw 0 tokens.
    let upstream = cline_gateway().await;
    let config = ServerConfig::single(
        20128,
        Strategy::Priority,
        vec![ProviderConfig::new(ProviderId::new("cline"), &upstream, "k").with_model("m")],
    );
    let (router, _state) = boot_with(config, vec![]);
    let (status, headers, text) = call(
        &router,
        post_chat(r#"{"model":"cline/m","stream":false}"#, &[]),
    )
    .await;

    assert_eq!(status, 200, "body was: {text}");
    let body: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("bare json: {text}"));
    assert_eq!(
        body["choices"][0]["message"]["content"], "Hel",
        "the inner completion did not reach the client: {text}"
    );
    assert!(
        body.get("success").is_none(),
        "the envelope skin survived: {text}"
    );
    assert_eq!(
        header_of(&headers, "x-ar-tokens-in"),
        "9",
        "headers: {headers:?}"
    );
    assert_eq!(header_of(&headers, "x-ar-tokens-out"), "4");
}

/// An upstream that always answers 403 and counts the dispatches it saw, so
/// a test can prove the refusal gate stopped paying round trips (MW4).
async fn forbidden_gateway(hits: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let hits = Arc::clone(&hits);
            async move {
                hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"error":{"message":"key rejected"}}"#))
                    .unwrap_or_else(|_| fallback(StatusCode::INTERNAL_SERVER_ERROR))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn e2e_stops_paying_round_trips_when_every_key_is_auth_locked() {
    // MW4, the journal shape: opencode-zen, every key 403. The first two
    // requests each strike one key; the third must be refused by the
    // executor without the upstream ever seeing it — the hit count is the
    // whole assertion.
    use std::sync::atomic::Ordering;
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let upstream = forbidden_gateway(Arc::clone(&hits)).await;
    let config = ServerConfig::single(
        20128,
        Strategy::Priority,
        vec![ProviderConfig::with_keys(
            ProviderId::new("deadkeys"),
            &upstream,
            vec!["k1".to_owned(), "k2".to_owned()],
        )
        .with_model("m")],
    );
    let (router, _state) = boot_with(config, vec![]);

    // Two dispatches: one per credential, each earning its strike.
    for _ in 0..2 {
        let _ = call(&router, post_chat(r#"{"model":"deadkeys/m"}"#, &[])).await;
    }
    assert_eq!(
        hits.load(Ordering::Relaxed),
        2,
        "each request should have spent exactly one dispatch"
    );

    // The third request: every key is auth-locked, so it must be refused
    // without a round trip — the loop still reports a failure verdict the
    // client can see.
    let (status, _, _) = call(&router, post_chat(r#"{"model":"deadkeys/m"}"#, &[])).await;
    assert_eq!(
        hits.load(Ordering::Relaxed),
        2,
        "a fully auth-locked set must not pay another round trip"
    );
    assert_eq!(status, 502, "the refusal surfaces as a gateway failure");
}

/// A decisions-style upstream: NOT a chat collection, only the one endpoint
/// registered, so a wrongly appended `/chat/completions` is a 404 the test
/// fails on. Captures the request it saw and answers the OpenRouter Decisions
/// shape (`answers` + `usage{input_tokens,output_tokens}`, no `choices`).
async fn decisions_upstream(
    seen: Arc<std::sync::Mutex<Option<String>>>,
) -> String {
    let app = Router::new().route(
        "/api/alpha/decisions",
        post(move |body: Body| {
            let seen = Arc::clone(&seen);
            async move {
                let bytes = axum::body::to_bytes(body, 64 * 1024)
                    .await
                    .unwrap_or_default();
                *seen.lock().expect("capture") = Some(String::from_utf8_lossy(&bytes).into_owned());
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"answers":{"is_bug":{"noul":0.96,"type":"noul"}},"id":"gen-dec-1","model":"respan/span-01-lite:free","provider":"TypeSafe","usage":{"cost":0.000019992,"input_tokens":476,"output_tokens":70}}"#,
                    ))
                    .unwrap_or_else(|_| fallback(StatusCode::INTERNAL_SERVER_ERROR))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn e2e_serves_a_decisions_endpoint_through_an_endpoint_override() {
    // The Decisions shape rides the OpenAI dialect verbatim: the client posts
    // `{model, state, questions}` — no `messages` — the body crosses untouched
    // (bar the `model` rewrite), the URL is the provider's declared endpoint,
    // and the reply's Anthropic-spelled usage is counted.
    let seen = Arc::new(std::sync::Mutex::new(None));
    let upstream = decisions_upstream(Arc::clone(&seen)).await;
    let config = ServerConfig::single(
        20128,
        Strategy::Priority,
        vec![ProviderConfig::new(
            ProviderId::new("or-decisions"),
            &upstream,
            "sk-or",
        )
        .with_model("respan/span-01-lite:free")
        .with_endpoint(Some("/api/alpha/decisions".to_owned()))],
    );
    let (router, _state) = boot_with(config, vec![]);

    let (status, headers, text) = call(
        &router,
        post_chat(
            r#"{"model":"respan/span-01-lite:free","state":"My checkout page shows a blank screen after I click Pay.","questions":{"is_bug":{"type":"noul","instructions":"Is the customer reporting a software defect?","criteria":{"true":"The customer describes broken or unexpected product behavior.","false":"The customer is asking a question or requesting a feature."}}}}"#,
            // A decisions client is a JSON client: without this the e2e
            // helper's default `Accept: text/event-stream` would force the
            // streamed arm, whose usage headers stay zero by design.
            &[("accept", "application/json")],
        ),
    )
    .await;

    assert_eq!(status, 200, "the decisions endpoint must serve: {text}");
    assert!(
        text.contains("\"answers\"") && text.contains("\"noul\":0.96"),
        "the decisions reply must cross verbatim: {text}"
    );
    assert!(
        !text.contains("choices"),
        "no chat-shaped body was invented: {text}"
    );
    // Anthropic-spelled usage (input_tokens/output_tokens) is what the
    // Decisions API reports; the reader normalizes it.
    assert_eq!(
        (header_of(&headers, "x-ar-tokens-in"), header_of(&headers, "x-ar-tokens-out")),
        ("476".to_owned(), "70".to_owned()),
        "usage headers were: {headers:?}"
    );

    let sent = seen.lock().expect("capture").clone().expect("request seen");
    assert!(
        sent.contains("\"questions\"") && sent.contains("blank screen"),
        "the decisions body must arrive untouched: {sent}"
    );
    assert!(
        sent.contains("\"model\":\"respan/span-01-lite:free\""),
        "the model field must be rewritten to the provider spelling: {sent}"
    );
}

/// An upstream that answers 200 and then closes without producing a single
/// byte — the nvidia transport-drop shape (MW5): a success status wearing a
/// dead socket, before anything has been committed to the client.
async fn dead_stream_upstream() -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::empty())
                .unwrap_or_else(|_| fallback(StatusCode::INTERNAL_SERVER_ERROR))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn e2e_fails_over_when_a_streamed_200_dies_before_its_first_byte() {
    // p1 answers 200 with a zero-byte stream, p2 streams normally: the client
    // asked to stream, nothing was committed when p1 died, so the proxy must
    // hand the client p2's stream rather than a truncated empty one.
    let dead = dead_stream_upstream().await;
    let healthy = mock_upstream().await;
    let config = ServerConfig::single(
        20128,
        Strategy::Priority,
        vec![
            ProviderConfig::new(ProviderId::new("p1"), &dead, "k").with_model("m1"),
            ProviderConfig::new(ProviderId::new("p2"), &healthy, "k").with_model("m2"),
        ],
    );
    let (router, _state) = boot_with(config, vec![]);
    let (status, headers, text) = call(
        &router,
        post_chat(r#"{"model":"m1","stream":true}"#, &[]),
    )
    .await;

    assert_eq!(status, 200, "the healthy member must serve: {text}");
    assert!(
        text.contains("\"delta\":{\"content\":\"Hel\"}"),
        "p2's stream did not arrive: {text}"
    );
    assert!(text.contains("data: [DONE]"), "stream must finish cleanly: {text}");
    let decision = header_of(&headers, "x-ar-decision");
    assert!(
        decision.contains("outcome=ok") && decision.contains("provider=p2"),
        "decision was: {decision}"
    );
    assert!(
        header_of(&headers, "x-ar-usage").contains("attempts=2"),
        "decision was: {decision}"
    );
}
