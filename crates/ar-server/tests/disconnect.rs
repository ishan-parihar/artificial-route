//! R7: a client that hangs up before the response headers.
//!
//! The executor is a mock whose `post_chat` sleeps before it answers, so the
//! request is still inside the pre-header wait when the client leaves. The
//! counters say what the proxy did with that wait: a call that ran to the end
//! of its sleep kept spending upstream time for nobody.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use ar_route::{ArExec, CanonicalRequest, ExecError, MediaReply, ProviderId, Upstream};
use ar_server::{Components, ModelCard, ServerConfig, server};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// What the mock executor saw. `cancelled` counts calls whose future was
/// dropped before the executor finished its wait.
#[derive(Default)]
struct Probe {
    started: AtomicUsize,
    finished: AtomicUsize,
    cancelled: AtomicUsize,
}

/// Counts a drop as a cancellation unless the wait already completed.
struct Guard(Arc<Probe>);

impl Drop for Guard {
    fn drop(&mut self) {
        if self.0.finished.load(SeqCst) == 0 {
            self.0.cancelled.fetch_add(1, SeqCst);
        }
    }
}

struct SlowExec(Arc<Probe>);

impl ArExec for SlowExec {
    fn post_chat<'a>(
        &'a self,
        _provider: &'a ProviderId,
        _canonical: &'a CanonicalRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Upstream, ExecError>> + Send + 'a>> {
        let probe = self.0.clone();
        Box::pin(async move {
            probe.started.fetch_add(1, SeqCst);
            let _guard = Guard(probe.clone());
            tokio::time::sleep(Duration::from_millis(800)).await;
            probe.finished.fetch_add(1, SeqCst);
            let chunks = vec![Bytes::from_static(b"data: [DONE]\n\n")];
            Ok(Upstream::success(Box::pin(futures::stream::iter(chunks))))
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
        Box::pin(async move { Err(ExecError::Transport("no media in this fixture".to_owned())) })
    }
}

#[tokio::test]
async fn a_disconnected_client_should_cancel_its_upstream_wait() {
    hang_up_before_headers(false).await;
}

#[tokio::test]
async fn a_disconnected_stream_should_cancel_its_upstream_wait() {
    hang_up_before_headers(true).await;
}

async fn hang_up_before_headers(stream: bool) {
    let probe = Arc::new(Probe::default());
    let config = ServerConfig::from_provider(
        None,
        Some("http://127.0.0.1:9".to_owned()),
        Some("mock-model".to_owned()),
        Some("sk-test-key".to_owned()),
        Some("mockprov".to_owned()),
        Some("priority".to_owned()),
        Some("0.15".to_owned()),
    );
    let app = server(Components {
        extra_models: vec![ModelCard::new("mockprov", "mock-model")],
        ..Components::with_exec(config, Arc::new(SlowExec(probe.clone())) as Arc<dyn ArExec>)
    })
    .router;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move { axum::serve(listener, app).await });

    let body = format!(
        r#"{{"model":"mock-model","stream":{stream},"messages":[{{"role":"user","content":"hi"}}]}}"#
    );
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut sock = TcpStream::connect(addr).await.expect("connect");
    sock.write_all(request.as_bytes()).await.expect("write");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        probe.started.load(SeqCst),
        1,
        "the call reached the executor"
    );
    drop(sock);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    println!(
        "started={} finished={} cancelled={}",
        probe.started.load(SeqCst),
        probe.finished.load(SeqCst),
        probe.cancelled.load(SeqCst)
    );
    assert_eq!(
        probe.cancelled.load(SeqCst),
        1,
        "the disconnect cancelled the wait"
    );
}
