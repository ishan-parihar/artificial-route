//! Bounded, zero-copy SSE framing.
//!
//! Wire shape is OmniRoute's: frames separated by a blank line, payload on a
//! `data:` line, and `data: [DONE]` terminating the stream. Splitting happens
//! here rather than in a `tokio_sse_codec` dependency because the only framing
//! rule `docs/02` needs is "payload between blank lines", and the whole decoder
//! is under 120 lines.
//!
//! # Bounded by construction
//!
//! The 00-overview RAM budget is <3MB for one in-flight stream, so a hostile or
//! broken upstream cannot grow this without limit:
//!
//! * `buf` is capped at [`DEFAULT_FRAME_CAP`]; a frame larger than that errors
//!   instead of accumulating;
//! * frames are handed out as [`Bytes::slice`] into the same allocation, so
//!   decoding a 64KB chunk costs one 64KB buffer, not one copy per payload.
//!
//! # Deliberate simplification
//!
//! Only the first `data:` line of a frame is read. SSE permits multi-line data
//! joined with `\n`, which would force a copy to reassemble; OpenAI-compatible
//! providers always emit one JSON payload per line, so the copy is avoided
//! rather than paid for a case that does not occur.
//! ponytail: multi-line `data:` reassembly -- a `Vec<u8>` join, if a provider
//! ever emits it.

use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};

/// Longest single frame this decoder will buffer before failing.
///
/// Well above any real chat delta while keeping one stream's worst case far
/// inside the <3MB budget.
pub const DEFAULT_FRAME_CAP: usize = 256 * 1024;

/// Frame terminator: `data: [DONE]`.
const DONE: &[u8] = b"[DONE]";

/// Prefix marking a payload line.
const DATA_PREFIX: &[u8] = b"data:";

/// Bytes needed *after* a `\n` to decide whether it starts a terminator.
///
/// The longest terminator this framer accepts is `\n\r\n`, so three bytes from
/// the `\n` decide it and the last two are re-examined on the next feed.
const LOOKAHEAD: usize = 2;

/// One decoded SSE payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    /// A `data:` payload with the prefix and `[DONE]` stripped.
    ///
    /// Zero-copy: the bytes share the chunk's allocation.
    Data(Bytes),
}

/// Incremental SSE framer. Feed bytes, pull events.
#[derive(Debug)]
pub(crate) struct SseDecoder {
    /// Bytes received but not yet framed.
    buf: BytesMut,
    /// Frames decoded from `buf` but not yet handed out.
    ready: VecDeque<Bytes>,
    /// `buf.len()` is known to contain no terminator *start*, so a feed never
    /// rescans more than the few bytes that just arrived.
    ///
    /// Terminates at `scanned`, not `buf.len()`: a `\n` at the end of one feed
    /// may still become a terminator once the next feed supplies its `\n` or
    /// `\r\n`, so the last [`LOOKAHEAD`] bytes are always re-examined.
    scanned: usize,
    /// Longest frame this decoder will accept.
    cap: usize,
}

impl SseDecoder {
    /// A decoder accepting frames up to `cap` bytes.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: BytesMut::new(),
            ready: VecDeque::new(),
            scanned: 0,
            cap,
        }
    }

    /// Appends `chunk` and decodes every complete frame it completes.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<(), SseError> {
        self.buf.extend_from_slice(chunk);
        while let Some((frame_len, sep_len)) = self.find_frame() {
            let frame = self.buf.split_to(frame_len + sep_len).freeze();
            self.scanned = self.scanned.saturating_sub(frame_len + sep_len);
            if let Some(payload) = frame_payload(&frame) {
                self.ready.push_back(payload);
            }
        }
        // `find_frame` ran to exhaustion, so no frame start precedes `scanned`
        // in the buffer that is left.
        //
        // ponytail: `ready` is bounded by `cap`, not by a queue limit. A frame
        // is at least `data:\n\n` (8 bytes) and `buf` is capped at `cap`, so
        // `cap / 8` entries is the hard ceiling -- ~32k * 16B handles worst
        // case, and the 256KB `buf` dominates that anyway. A queue limit would
        // be a second bound for a case the cap already covers.
        if self.buf.len() > self.cap {
            return Err(SseError::FrameTooLarge {
                len: self.buf.len(),
                cap: self.cap,
            });
        }
        Ok(())
    }

    /// Pops the next decoded payload, if any.
    pub(crate) fn next_event(&mut self) -> Option<SseEvent> {
        let payload = self.ready.pop_front()?;
        // `[DONE]` ends the stream; the caller sees `None` instead of a
        // sentinel value it would have to special-case.
        (payload.as_ref() != DONE).then_some(SseEvent::Data(payload))
    }

    /// Decodes a trailing frame that arrived without a terminating blank line.
    pub(crate) fn finish(&mut self) -> Option<SseEvent> {
        self.next_event().or_else(|| self.flush_trailing())
    }

    /// Splits off any unterminated remainder as one last frame.
    fn flush_trailing(&mut self) -> Option<SseEvent> {
        if self.buf.is_empty() {
            return None;
        }
        let frame = self.buf.split().freeze();
        let payload = frame_payload(&frame)?;
        (payload.as_ref() != DONE).then_some(SseEvent::Data(payload))
    }

    /// Finds the next frame, returning `(frame_len, separator_len)`.
    ///
    /// Both `\n\n` and `\r\n\r\n` terminate a frame. Byte-wise rather than
    /// `windows().position()` because `scanned` already keeps the rescan window
    /// to the newly arrived bytes.
    fn find_frame(&mut self) -> Option<(usize, usize)> {
        let hay = &self.buf;
        let mut i = self.scanned.min(hay.len());
        while i < hay.len() {
            if hay[i] == b'\n' && hay[i + 1..].starts_with(b"\n") {
                return Some((i, 2));
            }
            if hay[i] == b'\n' && hay[i + 1..].starts_with(b"\r\n") {
                return Some((i, 3));
            }
            i += 1;
        }
        self.scanned = hay.len().saturating_sub(LOOKAHEAD);
        None
    }
}

/// Extracts the `data:` payload from `frame`, zero-copy.
///
/// Returns `None` for a frame with no payload line (a comment, an `event:`-only
/// frame, or `[DONE]`, which the caller compares on the result).
fn frame_payload(frame: &Bytes) -> Option<Bytes> {
    let payload = frame
        .as_ref()
        .split(|&b| b == b'\n')
        .filter_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let rest = line.strip_prefix(DATA_PREFIX)?;
            // SSE strips exactly one space after the colon.
            Some(rest.strip_prefix(b" ").unwrap_or(rest))
        })
        .next()?;

    // Recover this subslice's offset inside `frame` so the payload keeps the
    // chunk's allocation instead of being copied out of it.
    let offset = payload.as_ptr() as usize - frame.as_ptr() as usize;
    debug_assert!(offset + payload.len() <= frame.len());
    Some(frame.slice(offset..offset + payload.len()))
}

/// Why an SSE byte stream could not be decoded.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SseError {
    /// A single frame exceeded the decoder's cap.
    #[error("SSE frame of {len} bytes exceeds the {cap}-byte cap")]
    FrameTooLarge {
        /// Bytes buffered when the cap tripped.
        len: usize,
        /// The configured cap.
        cap: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&str]) -> Result<Vec<String>, SseError> {
        let mut dec = SseDecoder::new(DEFAULT_FRAME_CAP);
        let mut out = Vec::new();
        for chunk in chunks {
            dec.feed(chunk.as_bytes())?;
            while let Some(SseEvent::Data(b)) = dec.next_event() {
                out.push(String::from_utf8(b.to_vec()).unwrap());
            }
        }
        Ok(out)
    }

    #[test]
    fn splits_frames_when_blanks_separate() {
        let got = decode_all(&["data: {\"a\":1}\n\ndata: {\"a\":2}\n\n"]).unwrap();
        assert_eq!(got, vec![r#"{"a":1}"#, r#"{"a":2}"#]);
    }

    #[test]
    fn reassembles_frame_split_across_chunks() {
        let got = decode_all(&["data: {\"a\"", ":1}\n", "\n"]).unwrap();
        assert_eq!(got, vec![r#"{"a":1}"#]);
    }

    #[test]
    fn terminates_on_done_sentinel() {
        let mut dec = SseDecoder::new(DEFAULT_FRAME_CAP);
        dec.feed(b"data: {\"a\":1}\n\ndata: [DONE]\n\n").unwrap();
        let first = dec.next_event();
        assert!(matches!(first, Some(SseEvent::Data(_))));
        assert!(dec.next_event().is_none());
    }

    #[test]
    fn skips_comment_frames() {
        let got = decode_all(&[": keepalive\n\ndata: {\"a\":1}\n\n"]).unwrap();
        assert_eq!(got, vec![r#"{"a":1}"#]);
    }

    #[test]
    fn errors_when_frame_exceeds_cap() {
        let mut dec = SseDecoder::new(8);
        let err = dec.feed(b"data: 0123456789").unwrap_err();
        assert_eq!(err, SseError::FrameTooLarge { len: 16, cap: 8 });
    }

    #[test]
    fn reassembles_terminator_split_across_chunks() {
        // The `\n` ending the payload and the `\n` terminating the frame arrive
        // in different feeds, so the resume cursor must re-examine the trailing
        // `\n` instead of skipping past it.
        let got = decode_all(&["data: {\"a\":1}\n", "\n"]).unwrap();
        assert_eq!(got, vec![r#"{"a":1}"#]);
    }

    #[test]
    fn reassembles_crlf_terminator_split_across_chunks() {
        let got = decode_all(&["data: {\"a\":1}\r\n", "\r\n"]).unwrap();
        assert_eq!(got, vec![r#"{"a":1}"#]);
    }

    #[test]
    fn handles_crlf_terminators() {
        let got = decode_all(&["data: {\"a\":1}\r\n\r\n"]).unwrap();
        assert_eq!(got, vec![r#"{"a":1}"#]);
    }

    #[test]
    fn flushes_unterminated_trailing_frame() {
        let mut dec = SseDecoder::new(DEFAULT_FRAME_CAP);
        dec.feed(b"data: {\"a\":1}\n\ndata: {\"a\":2}").unwrap();
        assert!(matches!(dec.next_event(), Some(SseEvent::Data(_))));
        assert!(matches!(dec.finish(), Some(SseEvent::Data(_))));
    }
}