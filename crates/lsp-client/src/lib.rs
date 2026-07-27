//! Generic LSP 3.17 client building blocks (#100 S2), ported from
//! `src/intentdiff/lsp/client.py`.
//!
//! Slice 1 is **sans-IO**: the Content-Length framing codec and the JSON-RPC 2.0 message
//! helpers, with the #88 reader caps enforced exactly as the Python client does. The codec
//! is a pull-based state machine (`feed` bytes in, `next_event` frames out) so the same
//! logic serves any transport (tokio stdio subprocess, TCP) without duplication — the
//! transports arrive in the next slice.
//!
//! Cap semantics mirrored from the Python `_read_loop`:
//! - a header line over [`MAX_HEADER_LINE`] bytes is **fatal** (Python: length check /
//!   `LimitOverrunError` → connection closed);
//! - a header section over [`MAX_HEADERS_TOTAL`] bytes is **fatal**;
//! - a `Content-Length` over [`MAX_BODY_BYTES`] is **fatal**;
//! - a non-numeric `Content-Length` is **fatal** (Python: `int()` raises out of the loop);
//! - a missing/zero/negative `Content-Length` **skips** to the next header block;
//! - a body that fails JSON parsing yields [`DecodeEvent::MalformedFrame`] and decoding
//!   **continues** (Python: warning + `continue`).

use serde_json::{json, Value};

pub mod client;
pub mod enrich;
pub mod specs;
pub use client::{env_with_path_prepend, resolve_command, LspClient, LspError};
pub use enrich::{enrich_targets, HoverTarget, MAX_CONCURRENT_HOVERS};
pub use specs::{
    known_server_specs, load_lsp_servers_json, resolve_launch, LaunchPlan, ServerEntry,
    ServerSpec, Transport,
};

/// Max bytes for a single header line (`_MAX_HEADER_LINE`).
pub const MAX_HEADER_LINE: usize = 4_096;
/// Max bytes for the whole header section of one message (`_MAX_HEADERS_TOTAL`).
pub const MAX_HEADERS_TOTAL: usize = 16_384;
/// Max bytes for one message body (`_MAX_BODY_BYTES`, 64 MiB).
pub const MAX_BODY_BYTES: usize = 67_108_864;
/// Max bytes retained from a server's stderr (`_STDERR_RING_BYTES`, 64 KiB) — used by the
/// transport slice's stderr ring buffer; defined here so the cap lives with its siblings.
pub const STDERR_RING_BYTES: usize = 65_536;

/// Encode a JSON-RPC message with LSP `Content-Length` framing (`_encode`).
pub fn encode_message(msg: &Value) -> Vec<u8> {
    let body = msg.to_string().into_bytes();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// A JSON-RPC 2.0 request (`_request`).
pub fn request(method: &str, params: Value, id: i64) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// A JSON-RPC 2.0 notification — no `id` (`_notification`).
pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// A successfully decoded item, or a recoverable per-frame problem.
#[derive(Debug, PartialEq)]
pub enum DecodeEvent {
    /// A complete, JSON-parsed message.
    Frame(Value),
    /// The body arrived but was not valid JSON — the frame is dropped, decoding continues.
    MalformedFrame,
}

/// A fatal framing violation: the connection must be closed (Python closes its read loop).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DecodeError {
    /// One header line exceeded [`MAX_HEADER_LINE`] bytes.
    HeaderLineTooLong,
    /// The header section exceeded [`MAX_HEADERS_TOTAL`] bytes.
    HeadersTooLarge,
    /// `Content-Length` was present but not a valid integer.
    InvalidContentLength,
    /// `Content-Length` exceeded [`MAX_BODY_BYTES`].
    BodyTooLarge,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HeaderLineTooLong => {
                write!(f, "LSP header line exceeds {MAX_HEADER_LINE} bytes")
            }
            Self::HeadersTooLarge => {
                write!(f, "LSP header section exceeds {MAX_HEADERS_TOTAL} bytes")
            }
            Self::InvalidContentLength => write!(f, "LSP Content-Length is not a valid integer"),
            Self::BodyTooLarge => write!(f, "LSP Content-Length exceeds {MAX_BODY_BYTES} bytes"),
        }
    }
}

impl std::error::Error for DecodeError {}

enum DecodeState {
    /// Reading header lines; `header_bytes` counts the section (incl. line terminators).
    Headers { header_bytes: usize, content_length: Option<usize> },
    /// Headers done; waiting for `length` body bytes.
    Body { length: usize },
    /// A fatal error was reported; the decoder refuses further work.
    Poisoned(DecodeError),
}

/// Sans-IO incremental decoder for LSP-framed JSON-RPC streams.
pub struct FrameDecoder {
    buf: Vec<u8>,
    state: DecodeState,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            state: DecodeState::Headers { header_bytes: 0, content_length: None },
        }
    }

    /// Append transport bytes. Call [`Self::next_event`] until it returns `Ok(None)`.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pull the next decoded event, if a complete one is buffered.
    ///
    /// `Ok(None)` means "need more bytes". A `DecodeError` is fatal and sticky: every
    /// subsequent call returns the same error (mirroring the Python client, which closes
    /// the connection on these).
    pub fn next_event(&mut self) -> Result<Option<DecodeEvent>, DecodeError> {
        loop {
            match &mut self.state {
                DecodeState::Poisoned(err) => return Err(*err),
                DecodeState::Headers { header_bytes, content_length } => {
                    // Find the next complete header line.
                    let Some(term) = find_crlf(&self.buf) else {
                        // No terminator yet: a partial line already over the cap can never
                        // become valid (Python's reader hits its limit the same way).
                        if self.buf.len() > MAX_HEADER_LINE {
                            return Err(self.poison(DecodeError::HeaderLineTooLong));
                        }
                        return Ok(None);
                    };
                    let raw_len = term + 2; // include \r\n, as the Python byte counts do
                    if raw_len > MAX_HEADER_LINE {
                        return Err(self.poison(DecodeError::HeaderLineTooLong));
                    }
                    *header_bytes += raw_len;
                    if *header_bytes > MAX_HEADERS_TOTAL {
                        return Err(self.poison(DecodeError::HeadersTooLarge));
                    }
                    let line: Vec<u8> = self.buf.drain(..raw_len).collect();
                    let line = String::from_utf8_lossy(&line[..line.len() - 2]).into_owned();
                    if line.is_empty() {
                        // Blank line ends the header section.
                        let length = content_length.take();
                        *header_bytes = 0;
                        match length {
                            Some(len) if len > MAX_BODY_BYTES => {
                                return Err(self.poison(DecodeError::BodyTooLarge));
                            }
                            Some(len) if len > 0 => {
                                self.state = DecodeState::Body { length: len };
                            }
                            // Missing/zero Content-Length: skip to the next header block
                            // (Python: `if length <= 0: continue`).
                            _ => {}
                        }
                        continue;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.trim().eq_ignore_ascii_case("content-length") {
                            match value.trim().parse::<i64>() {
                                Ok(n) => {
                                    *content_length = Some(n.max(0) as usize);
                                }
                                Err(_) => {
                                    return Err(self.poison(DecodeError::InvalidContentLength));
                                }
                            }
                        }
                    }
                    // Lines without ':' are ignored, as in the Python parser.
                }
                DecodeState::Body { length } => {
                    if self.buf.len() < *length {
                        return Ok(None);
                    }
                    let body: Vec<u8> = self.buf.drain(..*length).collect();
                    self.state = DecodeState::Headers { header_bytes: 0, content_length: None };
                    return match serde_json::from_slice::<Value>(&body) {
                        Ok(msg) => Ok(Some(DecodeEvent::Frame(msg))),
                        Err(_) => Ok(Some(DecodeEvent::MalformedFrame)),
                    };
                }
            }
        }
    }

    fn poison(&mut self, err: DecodeError) -> DecodeError {
        self.state = DecodeState::Poisoned(err);
        err
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(dec: &mut FrameDecoder) -> Vec<DecodeEvent> {
        let mut events = Vec::new();
        while let Ok(Some(ev)) = dec.next_event() {
            events.push(ev);
        }
        events
    }

    #[test]
    fn encode_decode_round_trip() {
        let msg = request("textDocument/hover", json!({"a": 1}), 7);
        let mut dec = FrameDecoder::new();
        dec.feed(&encode_message(&msg));
        assert_eq!(drain(&mut dec), vec![DecodeEvent::Frame(msg)]);
    }

    #[test]
    fn byte_at_a_time_feeding_yields_the_same_frame() {
        let msg = notification("initialized", json!({}));
        let bytes = encode_message(&msg);
        let mut dec = FrameDecoder::new();
        let mut events = Vec::new();
        for b in bytes {
            dec.feed(&[b]);
            events.extend(drain(&mut dec));
        }
        assert_eq!(events, vec![DecodeEvent::Frame(msg)]);
    }

    #[test]
    fn two_frames_in_one_feed_decode_in_order() {
        let a = request("initialize", json!({}), 1);
        let b = request("shutdown", Value::Null, 2);
        let mut dec = FrameDecoder::new();
        let mut bytes = encode_message(&a);
        bytes.extend(encode_message(&b));
        dec.feed(&bytes);
        assert_eq!(drain(&mut dec), vec![DecodeEvent::Frame(a), DecodeEvent::Frame(b)]);
    }

    #[test]
    fn extra_headers_and_case_insensitive_content_length_are_accepted() {
        let mut dec = FrameDecoder::new();
        dec.feed(b"Content-Type: application/vscode-jsonrpc\r\ncontent-LENGTH: 2\r\n\r\n{}");
        assert_eq!(drain(&mut dec), vec![DecodeEvent::Frame(json!({}))]);
    }

    #[test]
    fn missing_or_zero_content_length_skips_to_the_next_block() {
        let mut dec = FrameDecoder::new();
        // A header block with no Content-Length, then a well-formed frame.
        dec.feed(b"X-Noise: yes\r\n\r\n");
        dec.feed(b"Content-Length: 0\r\n\r\n");
        dec.feed(&encode_message(&json!({"ok": true})));
        assert_eq!(drain(&mut dec), vec![DecodeEvent::Frame(json!({"ok": true}))]);
    }

    #[test]
    fn malformed_json_body_is_skipped_and_decoding_continues() {
        let mut dec = FrameDecoder::new();
        dec.feed(b"Content-Length: 5\r\n\r\n{oops");
        dec.feed(&encode_message(&json!({"next": 1})));
        assert_eq!(
            drain(&mut dec),
            vec![DecodeEvent::MalformedFrame, DecodeEvent::Frame(json!({"next": 1}))]
        );
    }

    #[test]
    fn header_line_over_cap_is_fatal_and_sticky() {
        let mut dec = FrameDecoder::new();
        let long = format!("X-Big: {}\r\n", "a".repeat(MAX_HEADER_LINE));
        dec.feed(long.as_bytes());
        assert_eq!(dec.next_event(), Err(DecodeError::HeaderLineTooLong));
        assert_eq!(dec.next_event(), Err(DecodeError::HeaderLineTooLong));
    }

    #[test]
    fn partial_header_line_over_cap_is_fatal_without_a_terminator() {
        let mut dec = FrameDecoder::new();
        dec.feed("a".repeat(MAX_HEADER_LINE + 1).as_bytes());
        assert_eq!(dec.next_event(), Err(DecodeError::HeaderLineTooLong));
    }

    #[test]
    fn header_section_over_total_cap_is_fatal() {
        let mut dec = FrameDecoder::new();
        // Many individually-legal lines whose sum exceeds the section cap.
        let line = format!("X-Filler: {}\r\n", "b".repeat(1_000));
        for _ in 0..20 {
            dec.feed(line.as_bytes());
        }
        assert_eq!(dec.next_event(), Err(DecodeError::HeadersTooLarge));
    }

    #[test]
    fn body_over_cap_is_fatal() {
        let mut dec = FrameDecoder::new();
        dec.feed(format!("Content-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1).as_bytes());
        assert_eq!(dec.next_event(), Err(DecodeError::BodyTooLarge));
    }

    #[test]
    fn non_numeric_content_length_is_fatal() {
        let mut dec = FrameDecoder::new();
        dec.feed(b"Content-Length: banana\r\n\r\n");
        assert_eq!(dec.next_event(), Err(DecodeError::InvalidContentLength));
    }

    #[test]
    fn negative_content_length_is_treated_as_skip() {
        // Python `int("-5")` parses, then `length <= 0` skips the block.
        let mut dec = FrameDecoder::new();
        dec.feed(b"Content-Length: -5\r\n\r\n");
        dec.feed(&encode_message(&json!({"after": true})));
        assert_eq!(drain(&mut dec), vec![DecodeEvent::Frame(json!({"after": true}))]);
    }

    #[test]
    fn request_and_notification_shapes_match_the_python_client() {
        let req = request("m", json!([1]), 3);
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 3);
        let note = notification("n", json!({}));
        assert_eq!(note["jsonrpc"], "2.0");
        assert!(note.get("id").is_none());
    }
}
