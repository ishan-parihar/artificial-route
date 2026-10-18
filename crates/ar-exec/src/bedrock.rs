//! Bedrock's streaming reply, decoded from AWS's binary event stream.
//!
//! `converse-stream` answers `application/vnd.amazon.eventstream`, not SSE: each
//! event is a length-prefixed frame with typed headers, a JSON payload, and two
//! CRC32 checksums. This module decodes those frames and hands each event to
//! `ar_translate::BedrockStream`, which produces the OpenAI chunks the client
//! reads. Dialect semantics stay in `ar-translate`; the bytes stay here.
//!
//! # Bounded by construction
//!
//! A frame is refused before it is buffered when its declared length exceeds
//! [`FRAME_CAP`], so a hostile length prefix cannot grow the buffer. The prelude
//! checksum is verified before the rest of the frame is awaited, so a corrupt
//! length is caught without waiting for bytes that will never arrive.
//!
//! # Deliberate simplification
//!
//! Only string-typed headers are kept. The others (`:event-type` and friends are
//! always strings) are skipped by their declared width, so a frame that carries
//! one still decodes.

use bytes::{Bytes, BytesMut};
use futures::Stream;
use tokio_stream::StreamExt;

use crate::ExecError;

/// The fixed-size head: total length, header length, prelude CRC.
const PRELUDE: usize = 12;
/// The trailing message CRC.
const TRAILER: usize = 4;
/// Largest frame accepted. A Converse event is kilobytes; 1 MiB is generous and
/// keeps one hostile length prefix from reserving more.
pub const FRAME_CAP: usize = 1 << 20;

const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// IEEE 802.3 CRC32, the checksum the event stream uses on its prelude and body.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// One decoded event: its string headers and its JSON payload, zero-copy.
#[derive(Debug)]
pub struct Frame {
    headers: Vec<(String, String)>,
    payload: Bytes,
}

impl Frame {
    /// The value of a string header, if the frame carried one by that name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The event body, as written by the upstream.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// Incremental frame decoder over a byte stream that may split frames anywhere.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: BytesMut,
}

impl Decoder {
    /// Appends upstream bytes; frames are read out with [`Self::next_frame`].
    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// The next whole frame, or `None` when the buffer does not yet hold one.
    ///
    /// # Errors
    ///
    /// A length outside the frame bounds, a failed prelude or message checksum,
    /// or a header block that overruns its frame. Each is a corrupt stream, not a
    /// short read, so the caller ends the reply rather than waiting for more.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, String> {
        if self.buf.len() < PRELUDE {
            return Ok(None);
        }
        let total = be32(&self.buf[0..4]) as usize;
        let headers_len = be32(&self.buf[4..8]) as usize;
        if !(PRELUDE + TRAILER..=FRAME_CAP).contains(&total) {
            return Err(format!("frame length {total} is outside the bounds"));
        }
        if crc32(&self.buf[..8]) != be32(&self.buf[8..12]) {
            return Err("prelude checksum mismatch".to_owned());
        }
        if self.buf.len() < total {
            return Ok(None);
        }
        if PRELUDE + headers_len > total - TRAILER {
            return Err("header block overruns its frame".to_owned());
        }
        if crc32(&self.buf[..total - TRAILER]) != be32(&self.buf[total - TRAILER..total]) {
            return Err("message checksum mismatch".to_owned());
        }
        let frame = self.buf.split_to(total).freeze();
        let headers = parse_headers(&frame[PRELUDE..PRELUDE + headers_len])?;
        let payload = frame.slice(PRELUDE + headers_len..total - TRAILER);
        Ok(Some(Frame { headers, payload }))
    }
}

/// Reads a header block into its string-valued entries.
///
/// Every value type has a fixed width or a length prefix, so a non-string header
/// is stepped over rather than parsed.
fn parse_headers(mut bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
        if bytes.len() < n {
            return Err("header block is truncated".to_owned());
        }
        let (head, rest) = bytes.split_at(n);
        *bytes = rest;
        Ok(head)
    }

    let mut out = Vec::new();
    while !bytes.is_empty() {
        let name_len = take(&mut bytes, 1)?[0] as usize;
        let name = std::str::from_utf8(take(&mut bytes, name_len)?)
            .map_err(|_| "header name is not UTF-8".to_owned())?
            .to_owned();
        let kind = take(&mut bytes, 1)?[0];
        let width = match kind {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            9 => 16,
            6 | 7 => {
                let len = take(&mut bytes, 2)?;
                usize::from(u16::from_be_bytes([len[0], len[1]]))
            }
            other => return Err(format!("unknown header value type {other}")),
        };
        let value = take(&mut bytes, width)?;
        if kind == 7 {
            let text = std::str::from_utf8(value)
                .map_err(|_| "header value is not UTF-8".to_owned())?
                .to_owned();
            out.push((name, text));
        }
    }
    Ok(out)
}

/// The Converse streaming URL, with the model in the path and percent-encoded
/// there, since a model id may carry `:` (`openai.gpt-oss-120b-1:0`).
#[must_use]
pub fn converse_url(base: &str, model: &str, stream: bool) -> String {
    let stem = base.trim().trim_end_matches('/');
    let verb = if stream {
        "converse-stream"
    } else {
        "converse"
    };
    format!("{stem}/model/{}/{verb}", encode_segment(model))
}

/// RFC 3986 path-segment encoding: unreserved bytes pass, everything else is
/// `%XX`.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The HTTP status an exception frame answers with, mirroring the reference.
///
/// Matched case-insensitively: the wire names an exception in lowercase camel
/// case (`throttlingException`), and the SDK's own spelling is capitalised.
fn exception_status(kind: &str) -> u16 {
    let kind = kind.to_ascii_lowercase();
    if kind.contains("throttling") {
        429
    } else if kind.contains("validation") {
        400
    } else if kind.contains("serviceunavailable") {
        503
    } else if kind.contains("internalserver") || kind.contains("modelstream") {
        500
    } else {
        502
    }
}

/// One OpenAI SSE `data:` frame carrying `value`.
pub(crate) fn sse(value: &serde_json::Value) -> Bytes {
    let mut out = b"data: ".to_vec();
    out.extend_from_slice(&serde_json::to_vec(value).unwrap_or_default());
    out.extend_from_slice(b"\n\n");
    Bytes::from(out)
}

/// Relays a Converse event stream as OpenAI chat SSE, ending with `[DONE]`.
///
/// An exception frame becomes an in-band error object and ends the relay with no
/// `[DONE]`, so the router sees an incomplete reply. A clean end of body without
/// a `messageStop` also ends without `[DONE]`, for the same reason.
pub fn relay(
    upstream: impl Stream<Item = Result<Bytes, ExecError>> + Send + 'static,
    model: String,
) -> impl Stream<Item = Result<Bytes, ExecError>> + Send {
    async_stream::stream! {
        let mut upstream = Box::pin(upstream);
        let mut decoder = Decoder::default();
        let mut mapper = ar_translate::BedrockStream::new(&model);
        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            decoder.push(&chunk);
            loop {
                let frame = match decoder.next_frame() {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    Err(reason) => {
                        yield Err(ExecError::EventStream(reason));
                        return;
                    }
                };
                if frame.header(":message-type") == Some("exception") {
                    let kind = frame.header(":exception-type").unwrap_or("bedrock_stream_error");
                    let payload: serde_json::Value =
                        serde_json::from_slice(frame.payload()).unwrap_or_default();
                    let message = payload
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Bedrock stream failed");
                    yield Ok(sse(&serde_json::json!({
                        "error": {
                            "message": message,
                            "type": if exception_status(kind) == 429 {
                                "rate_limit_error"
                            } else {
                                "upstream_error"
                            },
                            "code": kind,
                            "status": exception_status(kind),
                        }
                    })));
                    return;
                }
                let payload: serde_json::Value =
                    serde_json::from_slice(frame.payload()).unwrap_or_default();
                for event in mapper.event(frame.header(":event-type").unwrap_or(""), &payload) {
                    yield Ok(sse(&event));
                }
            }
        }
        if let Some(last) = mapper.finish() {
            yield Ok(sse(&last));
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes one frame the way the upstream does, with string headers.
    fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut hdrs = Vec::new();
        for (name, value) in headers {
            hdrs.push(u8::try_from(name.len()).expect("short name"));
            hdrs.extend_from_slice(name.as_bytes());
            hdrs.push(7);
            hdrs.extend_from_slice(
                &u16::try_from(value.len())
                    .expect("short value")
                    .to_be_bytes(),
            );
            hdrs.extend_from_slice(value.as_bytes());
        }
        let total = PRELUDE + hdrs.len() + payload.len() + TRAILER;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&u32::try_from(total).expect("small frame").to_be_bytes());
        out.extend_from_slice(
            &u32::try_from(hdrs.len())
                .expect("small headers")
                .to_be_bytes(),
        );
        let prelude = crc32(&out[..8]);
        out.extend_from_slice(&prelude.to_be_bytes());
        out.extend_from_slice(&hdrs);
        out.extend_from_slice(payload);
        let message = crc32(&out);
        out.extend_from_slice(&message.to_be_bytes());
        out
    }

    #[test]
    fn crc32_should_match_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn next_frame_should_wait_for_a_frame_split_across_pushes() {
        let bytes = frame(
            &[(":event-type", "messageStart")],
            br#"{"role":"assistant"}"#,
        );
        let mut d = Decoder::default();
        d.push(&bytes[..10]);
        assert!(d.next_frame().expect("no error yet").is_none());
        d.push(&bytes[10..]);

        let got = d.next_frame().expect("decodes").expect("whole now");
        assert_eq!(got.header(":event-type"), Some("messageStart"));
    }

    #[test]
    fn next_frame_should_refuse_a_message_with_a_bad_checksum() {
        let mut bytes = frame(&[(":event-type", "x")], b"{}");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        let mut d = Decoder::default();
        d.push(&bytes);

        assert!(d.next_frame().is_err());
    }

    #[test]
    fn next_frame_should_refuse_a_length_past_the_cap_before_buffering_it() {
        let mut d = Decoder::default();
        let mut prelude = (u32::try_from(FRAME_CAP + 1).expect("fits"))
            .to_be_bytes()
            .to_vec();
        prelude.extend_from_slice(&[0; 8]);
        d.push(&prelude);

        assert!(d.next_frame().is_err());
    }

    #[test]
    fn converse_url_should_percent_encode_a_model_with_a_colon() {
        assert_eq!(
            converse_url(
                "https://bedrock-runtime.eu-west-2.amazonaws.com/",
                "openai.gpt-oss-120b-1:0",
                true
            ),
            "https://bedrock-runtime.eu-west-2.amazonaws.com/model/openai.gpt-oss-120b-1%3A0/converse-stream",
        );
    }

    #[tokio::test]
    async fn relay_should_turn_an_event_sequence_into_openai_sse() {
        let mut wire = Vec::new();
        wire.extend(frame(
            &[(":event-type", "messageStart")],
            br#"{"role":"assistant"}"#,
        ));
        wire.extend(frame(
            &[(":event-type", "contentBlockDelta")],
            br#"{"contentBlockIndex":0,"delta":{"text":"hi"}}"#,
        ));
        wire.extend(frame(
            &[(":event-type", "messageStop")],
            br#"{"stopReason":"end_turn"}"#,
        ));
        let chunks: Vec<Bytes> = relay(
            futures::stream::iter(vec![Ok(Bytes::from(wire))]),
            "m".to_owned(),
        )
        .map(|c| c.expect("no transport error"))
        .collect()
        .await;
        let text: String = chunks
            .iter()
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();

        assert!(text.contains(r#""content":"hi""#), "{text}");
        assert!(text.contains(r#""finish_reason":"stop""#), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    }

    #[tokio::test]
    async fn relay_should_end_without_done_on_an_exception_frame() {
        let wire = frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "throttlingException"),
            ],
            br#"{"message":"slow down"}"#,
        );
        let chunks: Vec<Bytes> = relay(
            futures::stream::iter(vec![Ok(Bytes::from(wire))]),
            "m".to_owned(),
        )
        .map(|c| c.expect("no transport error"))
        .collect()
        .await;
        let text: String = chunks
            .iter()
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();

        assert!(text.contains(r#""status":429"#), "{text}");
        assert!(!text.contains("[DONE]"), "{text}");
    }

    #[tokio::test]
    async fn post_should_stream_converse_frames_from_the_model_path_with_a_bearer_key() {
        let wire: Vec<u8> = [
            frame(
                &[(":event-type", "messageStart")],
                br#"{"role":"assistant"}"#,
            ),
            frame(
                &[(":event-type", "contentBlockDelta")],
                br#"{"contentBlockIndex":0,"delta":{"text":"ok"}}"#,
            ),
            frame(
                &[(":event-type", "messageStop")],
                br#"{"stopReason":"end_turn"}"#,
            ),
        ]
        .concat();
        let app =
            axum::Router::new().fallback(move |req: axum::http::Request<axum::body::Body>| {
                let wire = wire.clone();
                async move {
                    let path = req.uri().path().to_owned();
                    let auth = req
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    let accept = req
                        .headers()
                        .get(axum::http::header::ACCEPT)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    // The path and credential are the assertion: a wrong verb or a
                    // missing bearer answers 400 with the evidence in the body.
                    let ok = path == "/model/openai.gpt-oss-120b-1%3A0/converse-stream"
                        && auth == "Bearer key-1"
                        && accept == "application/vnd.amazon.eventstream";
                    let status = if ok { 200 } else { 400 };
                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "application/vnd.amazon.eventstream")
                        .body(axum::body::Body::from(if ok {
                            wire
                        } else {
                            format!("{path} {auth} {accept}").into_bytes()
                        }))
                        .expect("a valid response")
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let addr = listener.local_addr().expect("bound socket has an address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let core = crate::ArExec::new().expect("the client builds");
        let base = format!("http://{addr}");
        let headers = std::collections::BTreeMap::new();
        let shape = crate::Dispatch {
            base_url: &base,
            endpoint: None,
            wire_format: ar_registry::WireFormat::Bedrock,
            api_key: "key-1",
            upstream_model: "openai.gpt-oss-120b-1:0",
            stream: true,
            headers: &headers,
        };
        let canonical =
            br#"{"model":"combo","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
        let abort = tokio_util::sync::CancellationToken::new();
        let stream = core
            .post(&shape, canonical, &abort)
            .await
            .expect("the upstream answers 200");
        let text: String = stream
            .into_bedrock_sse("combo")
            .map(|c| String::from_utf8_lossy(&c.expect("no transport error")).into_owned())
            .collect::<Vec<_>>()
            .await
            .concat();

        assert!(text.contains(r#""content":"ok""#), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    }
}
