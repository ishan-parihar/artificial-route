//! The deferred-headers path: committing to `200` before the upstream answers.
//!
//! # The problem it solves
//!
//! An HTTP edge in front of this server has its own time-to-first-byte limit.
//! Cloudflare's Proxy Read Timeout is 125 s, and it is an **idle** timeout on
//! the origin response: it resets whenever the origin sends a byte. Measured
//! through a real edge with an isolated tunnel:
//!
//! | Origin behaviour | Result |
//! |---|---|
//! | headers at ~0 s, then a byte every 10 s for 300 s | **HTTP 200, total 300.9 s** |
//! | headers withheld 150 s, then a byte every 10 s | **HTTP 524 at 126.4 s** |
//!
//! This server sends nothing until its upstream returns headers, so an upstream
//! slower than the edge's limit fails at the edge — as a 524, which says
//! nothing about the upstream that was merely slow.
//!
//! # The shape of the fix
//!
//! The status code a client sees is chosen *before* the upstream answers, so
//! the choice has to be made carefully:
//!
//! - A status the upstream sends **quickly** (429, 402, 400 — measured at
//!   0.5-1.6 s for the two quota-dead providers) still reaches the client as
//!   that status, because it arrives long before the window closes. The
//!   contract clients branch on is preserved for every failure that is
//!   actually a failure.
//! - Only a request still **unresolved** when the window closes commits to
//!   `200` + `text/event-stream`. From that point the keepalives are real
//!   bytes the edge is holding the connection open on, and a later failure
//!   arrives as an in-band frame ([`Dialect::deferred_error`]) rather than a
//!   status code that has already been sent.
//!
//! # What it does not fix
//!
//! A **non-streaming** request. Its body is one JSON chunk, and a comment
//! injected into it would corrupt the JSON, so there is no way to keep it
//! alive without changing its shape. Non-streaming requests keep the deadline
//! and get the typed 502 they get today.
//!
//! # What it costs
//!
//! A client that treats `200` as success and ignores a trailing error frame
//! sees a success-shaped failure. That is a real risk, and the mitigation is
//! that the frame carries the same `error` object shape the JSON path
//! serializes — a client already parsing an error from a body sees the same
//! shape it has always seen.

use crate::routes::Dialect;

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::time::Duration;

/// How long a wait may go before a keepalive goes out.
///
/// The same interval the relay uses for an idle upstream
/// ([`crate::routes::KEEPALIVE_INTERVAL`]). Ten seconds is inside every
/// client watchdog that exists and is far below the 125 s edge limit, so the
/// edge never sees an idle origin.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// The outcome of the window: either the upstream answered in time, or it did
/// not.
pub enum Commit<T> {
    /// The upstream answered within the window. The real status is intact.
    Resolved(T),
    /// The window closed with the request still unresolved. The response must
    /// commit to `200` now, and the outcome arrives later as a frame.
    Deferred,
}

/// Races the attempt loop against the commit window.
///
/// `None` for the window means the feature is off, and the future is awaited
/// directly — no `select!`, no timer, and the exact behaviour that predates
/// this module.
pub async fn race_window<F, T>(window: Option<Duration>, fut: F) -> (Commit<T>, Option<Pin<Box<F>>>)
where
    F: std::future::Future<Output = T>,
{
    let Some(window) = window else {
        return (Commit::Resolved(fut.await), None);
    };
    let mut fut = Box::pin(fut);
    tokio::select! {
        biased;
        done = &mut fut => (Commit::Resolved(done), None),
        () = tokio::time::sleep(window) => (Commit::Deferred, Some(fut)),
    }
}

/// Builds the `200` response that a deferred request commits to.
///
/// The body is a stream that opens with keepalives — which is what keeps the
/// edge's idle timer from firing — and then relays whatever the attempt loop
/// eventually produced.
///
/// `resolved` is the eventual outcome. A success relays its chunks; a failure
/// is named in-band with [`Dialect::deferred_error`], because the status that
/// would have carried it has already been sent.
pub(crate) fn deferred_response<S>(
    dialect: Dialect,
    keepalive: Option<&'static str>,
    resolved: S,
) -> Response
where
    S: Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static,
{
    let failure = dialect.deferred_error();
    let body = Body::from_stream(async_stream::stream! {
        // The keepalive loop runs until the attempt loop produces something.
        // It is the byte the edge needs: without it the edge's idle timer runs
        // out and the whole request fails with a 524 — the exact failure this
        // path exists to prevent.
        let mut ticker = tokio::time::interval(KEEPALIVE_INTERVAL);
        // `interval` fires immediately on its first tick, and the first tick is
        // the one that must not be skipped: it is the byte that proves to the
        // edge that the origin is alive.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut resolved = Box::pin(resolved);
        // Whether the upstream actually answered. The failure frame is only
        // correct when it did NOT: a success that ends without a terminator is
        // a different failure (a cut), and the relay's own completion check
        // owns that one.
        let mut answered = false;
        loop {
            tokio::select! {
                biased;
                chunk = resolved.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            answered = true;
                            yield Ok::<Bytes, std::convert::Infallible>(bytes);
                        }
                        Some(Err(never)) => match never {},
                        None => break,
                    }
                }
                _ = ticker.tick() => {
                    if let Some(frame) = keepalive {
                        yield Ok(Bytes::from_static(frame.as_bytes()));
                    }
                }
            }
        }
        // The attempt loop finished. If it finished with a failure, name it
        // in-band: the status that would have carried it is already gone. A
        // success is not a failure, and saying otherwise would tell every
        // client its own answer was an error.
        if !answered
            && let Some(frame) = failure
        {
            yield Ok(Bytes::from_static(frame.as_bytes()));
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .expect("a static status and an empty body always build")
        })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod e2e;
