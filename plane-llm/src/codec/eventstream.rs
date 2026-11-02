// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The LLM plane's own AWS event-stream (`application/vnd.amazon.eventstream`) frame codec (#83a
//! SD-3): only the Bedrock dialect and its stream translator speak it. CRC32 is `crc32fast`
//! (CRC-32/ISO-HDLC); the drift test beside this module holds every encoded frame byte-identical to
//! the frames the host's (now deleted) copy wrote.
//!
//! `drain_frames_signaled` is the production decoder: it pulls events and explicit header-only
//! errors out of ConverseStream and InvokeModel responses. `drain_frames_checked` retains the
//! event/payload projection for consumers needing only those bytes. Incremental (a trailing partial
//! frame stays buffered); CRCs are
//! not validated on decode (a client consuming well-formed AWS frames). `drain_frames` is a
//! test-only wrapper that discards the consumed-byte count.
//!
//! The `event_type` is normally the `:event-type` header. A mid-stream modeled exception instead
//! carries `:message-type: exception` and `:exception-type: <Name>` with no `:event-type`; for those
//! the exception name is normalized to the Smithy union-member form (first letter lowercased, e.g.
//! `internalServerException`) so it matches the reader's exception arms and surfaces as an error
//! rather than a dropped typeless frame.
//!
//! [`encode_frame`](crate::codec::eventstream::encode_frame) is the production encoder (exact
//! inverse of the decoder) for Bedrock ingress streaming: a native AWS SDK client needs byte-exact
//! frames with valid CRC32.
//! Frame layout:
//! ```text
//!   [total_len: u32 BE][headers_len: u32 BE][prelude_crc: u32 BE]
//!   [headers: headers_len bytes]
//!   [payload: total_len - headers_len - 16 bytes]
//!   [message_crc: u32 BE]
//! ```
//! Header: `[name_len: u8][name][value_type: u8][value]`. Bedrock uses string headers (type 7):
//! `[value_len: u16 BE][value]`.

/// Upper bound on one event-stream frame. ConverseStream frames are small JSON deltas, so a larger

pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

use crate::codec::diagnostics::{
    EVENTSTREAM_EVENTTYPE_HEADER_OVERSIZE, EVENTSTREAM_EXCEPTIONTYPE_HEADER_OVERSIZE,
    EVENTSTREAM_FRAME_OVERSIZE,
};
use busbar_contract::diag_debug;

/// Why a [`drain_frames_checked`] pass stopped. The egress reassembler (`StreamTranslate::feed`)
/// keys off this explicit abandonment signal; inferring an abort from an emptied buffer is fragile,
/// since a clean full drain also leaves it empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainStatus {
    /// Every complete frame was consumed and the decoder stopped cleanly (buffer empty or holding a
    /// trailing partial frame). The stream is healthy; feed more bytes and call again.
    Ok,
    /// A malformed prelude (out-of-range `total_len`, or `headers_len` too large for the frame). The
    /// stream is unrecoverable: the buffer is cleared and the caller must abandon it.
    MalformedPrelude,
}

/// Drain every complete frame from `buf`, returning `(event_type, payload_bytes)` pairs and a

pub fn drain_frames_checked(
    buf: &mut Vec<u8>,
    consumed_sink: Option<&mut Vec<u8>>,
) -> (Vec<(String, Vec<u8>)>, DrainStatus, usize) {
    drain_frames_counted(buf, consumed_sink, &mut DrainWork::default())
}

/// A decoded frame retaining an explicit AWS error independently of its optional JSON payload.
pub(crate) struct DecodedFrame {
    pub event_type: String,
    pub payload: Vec<u8>,
    pub error: Option<FrameError>,
}

/// `:message-type: error` is a failure even when its code is unknown or absent.
pub(crate) struct FrameError {
    pub code: Option<String>,
    pub message: Option<String>,
}

/// The production decoder, retaining the header-only error signal for both stream consumers.
pub(crate) fn drain_frames_signaled(
    buf: &mut Vec<u8>,
    consumed_sink: Option<&mut Vec<u8>>,
) -> (Vec<DecodedFrame>, DrainStatus, usize) {
    drain_frames_signaled_counted(buf, consumed_sink, &mut DrainWork::default())
}

/// The work one drain pass did on its buffer, counted where done: the measure the complexity test
/// holds the drain to, instead of wall-clock time (Law 8: count work units, not time).
///
/// `scanned` is bytes walked as complete frames; `edits` buffer edits; `shifted` bytes those edits
/// moved (a front `drain` memmoves the tail). Every edit goes through [`consume_front`] /
/// [`clear_all`], so a per-frame editor shows `edits == frames` and `shifted` quadratic in frames.

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DrainWork {
    pub frames: usize,
    pub scanned: usize,
    pub edits: usize,
    pub shifted: usize,
}

impl DrainWork {
    /// Every byte the pass touched: walked as frames plus moved by an edit.
    #[cfg(test)]
    pub(crate) fn total(&self) -> usize {
        self.scanned + self.shifted
    }
}

/// Remove the first `n` bytes of `buf`, counting the edit and the tail bytes it moves.
fn consume_front(buf: &mut Vec<u8>, n: usize, work: &mut DrainWork) {
    work.edits += 1;
    work.shifted += buf.len() - n;
    buf.drain(..n);
}

/// Empty `buf`, counting the edit (it moves nothing).
fn clear_all(buf: &mut Vec<u8>, work: &mut DrainWork) {
    work.edits += 1;
    buf.clear();
}

/// [`drain_frames_checked`], counting its buffer work into `work`.
pub(crate) fn drain_frames_counted(
    buf: &mut Vec<u8>,
    consumed_sink: Option<&mut Vec<u8>>,
    work: &mut DrainWork,
) -> (Vec<(String, Vec<u8>)>, DrainStatus, usize) {
    let (frames, status, consumed) = drain_frames_signaled_counted(buf, consumed_sink, work);
    (
        frames
            .into_iter()
            .map(|f| (f.event_type, f.payload))
            .collect(),
        status,
        consumed,
    )
}

fn drain_frames_signaled_counted(
    buf: &mut Vec<u8>,
    mut consumed_sink: Option<&mut Vec<u8>>,
    work: &mut DrainWork,
) -> (Vec<DecodedFrame>, DrainStatus, usize) {
    let mut out = Vec::new();
    let mut status = DrainStatus::Ok;

    let mut pos = 0usize;
    loop {
        let rem = &buf[pos..];
        if rem.len() < PRELUDE_LEN {
            break;
        }
        let total_len = u32::from_be_bytes([rem[0], rem[1], rem[2], rem[3]]) as usize;
        let headers_len = u32::from_be_bytes([rem[4], rem[5], rem[6], rem[7]]) as usize;

        if !(MIN_FRAME_BYTES..=MAX_FRAME_BYTES).contains(&total_len)
            || headers_len > total_len - MIN_FRAME_BYTES
        {
            status = DrainStatus::MalformedPrelude;
            break;
        }
        if rem.len() < total_len {
            break;
        }

        let headers = &rem[PRELUDE_LEN..PRELUDE_LEN + headers_len];
        let parsed = parse_frame_headers(headers);
        let event_type = event_type_from_headers(&parsed);
        let error =
            (parsed.message_type.as_deref() == Some(MSG_TYPE_ERROR)).then_some(FrameError {
                code: parsed.error_code,
                message: parsed.error_message,
            });
        let payload = rem[PRELUDE_LEN + headers_len..total_len - CRC_BYTES].to_vec();
        out.push(DecodedFrame {
            event_type,
            payload,
            error,
        });

        if let Some(sink) = consumed_sink.as_deref_mut() {
            sink.extend_from_slice(&rem[..total_len]);
        }
        pos += total_len;
        work.frames += 1;
        work.scanned += total_len;
    }

    match status {
        DrainStatus::Ok => consume_front(buf, pos, work),
        DrainStatus::MalformedPrelude => clear_all(buf, work),
    }
    (out, status, pos)
}

/// Drain every complete frame from `buf`, returning `(event_type, payload_bytes)` per frame and

#[cfg(test)]
pub fn drain_frames(buf: &mut Vec<u8>) -> Vec<(String, Vec<u8>)> {
    drain_frames_checked(buf, None).0
}

/// The framing headers `drain_frames_checked` reads: `:event-type`, plus `:message-type` and
/// `:exception-type` that a modeled-exception frame carries instead. All optional strings.
#[derive(Default)]
struct FrameHeaders {
    event_type: Option<String>,
    message_type: Option<String>,
    exception_type: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
}

/// Resolve the event-type token `drain_frames_checked` returns for a frame: `:event-type` verbatim

#[cfg(test)]
fn event_type_for_frame(headers: &[u8]) -> String {
    event_type_from_headers(&parse_frame_headers(headers))
}

fn event_type_from_headers(parsed: &FrameHeaders) -> String {
    if parsed.message_type.as_deref() == Some(MSG_TYPE_ERROR) {
        if let Some(code) = &parsed.error_code {
            return normalize_error_token(code);
        }
    }
    if parsed.message_type.as_deref() == Some(MSG_TYPE_EXCEPTION) {
        if let Some(exc) = &parsed.exception_type {
            return normalize_error_token(exc);
        }
    }
    parsed.event_type.clone().unwrap_or_default()
}

/// Normalize an AWS failure-name header (`:exception-type` or `:error-code`) to the reader's

fn normalize_error_token(raw: &str) -> String {
    let bare = raw
        .rsplit(['#', '/'])
        .find(|s| !s.is_empty())
        .unwrap_or(raw);
    lowercase_first(bare)
}

/// Lowercase only the first character (`InternalServerException` to `internalServerException`),

fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Scan the header block for the `:event-type`, `:message-type` and `:exception-type` string

fn parse_frame_headers(mut h: &[u8]) -> FrameHeaders {
    let mut found = FrameHeaders::default();
    while !h.is_empty() {
        let Some(&name_len_byte) = h.first() else {
            break;
        };
        let name_len = name_len_byte as usize;
        if h.len() < 1 + name_len + 1 {
            break;
        }
        let name = &h[1..1 + name_len];
        let value_type = h[1 + name_len];
        let mut p = 1 + name_len + 1;

        let fixed_width: Option<usize> = match value_type {
            0 | 1 => Some(0),
            2 => Some(1),
            3 => Some(2),
            4 => Some(4),
            5 => Some(8),
            8 => Some(8),
            9 => Some(16),
            _ => None,
        };
        let value: Option<&[u8]> = match value_type {
            6 | HDR_TYPE_STRING => {
                if h.len() < p + 2 {
                    break;
                }
                let vlen = u16::from_be_bytes([h[p], h[p + 1]]) as usize;
                p += 2;
                if h.len() < p + vlen {
                    break;
                }
                let v = &h[p..p + vlen];
                p += vlen;
                Some(v)
            }
            _ => match fixed_width {
                Some(w) => {
                    if h.len() < p + w {
                        break;
                    }
                    p += w;
                    None
                }

                None => break,
            },
        };

        if let Some(v) = value.and_then(|v| std::str::from_utf8(v).ok()) {
            match name {
                n if n == HDR_EVENT_TYPE.as_bytes() => found.event_type = Some(v.to_string()),
                n if n == HDR_MESSAGE_TYPE.as_bytes() => found.message_type = Some(v.to_string()),
                n if n == HDR_EXCEPTION_TYPE.as_bytes() => {
                    found.exception_type = Some(v.to_string())
                }
                n if n == HDR_ERROR_CODE.as_bytes() => found.error_code = Some(v.to_string()),
                n if n == HDR_ERROR_MESSAGE.as_bytes() => found.error_message = Some(v.to_string()),
                _ => {}
            }
        }
        h = &h[p..];
    }
    found
}

/// Append one `[name_len:u8][name][value_type:u8 = 7][value_len:u16 BE][value]` string header. The

#[must_use]
fn push_string_header(headers: &mut Vec<u8>, name: &str, value: &str) -> bool {
    if name.len() > u8::MAX as usize || value.len() > u16::MAX as usize {
        return false;
    }
    headers.push(name.len() as u8);
    headers.extend_from_slice(name.as_bytes());
    headers.push(HDR_TYPE_STRING);
    headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
    headers.extend_from_slice(value.as_bytes());
    true
}

/// Encode one AWS event-stream message, the exact inverse of one [`drain_frames_checked`]
/// iteration, with real CRC32 (SDK clients validate both CRCs).
///
/// Wire layout:
/// ```text
///   [total_len:u32 BE][headers_len:u32 BE][prelude_crc:u32 BE = CRC32(first 8 bytes)]
///   [headers][payload]
///   [message_crc:u32 BE = CRC32(byte 0 .. end of payload)]
/// ```
/// A ConverseStream frame carries three string headers: `:event-type`, `:content-type`
/// (`application/json`) and `:message-type` (`event`). On the streaming hot path: arithmetic is
/// `u64`-widened and bounded by `MAX_FRAME_BYTES`, so no cast wraps and it never panics.
pub fn encode_frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut frame = frame_open();

    if !push_string_header(&mut frame, HDR_EVENT_TYPE, event_type)
        || !push_string_header(
            &mut frame,
            HDR_CONTENT_TYPE,
            busbar_contract::protocol::APPLICATION_JSON,
        )
        || !push_string_header(&mut frame, HDR_MESSAGE_TYPE, MSG_TYPE_EVENT)
    {
        diag_debug!(
            EVENTSTREAM_EVENTTYPE_HEADER_OVERSIZE,
            event_type_len = event_type.len(),
            "event-stream :event-type header exceeds the type-7 string cap; dropping frame"
        );
        return Vec::new();
    }
    frame_close(frame, payload)
}

/// Encode a modeled-exception event-stream message for a native Bedrock SDK client: AWS signals a

pub fn encode_exception_frame(exception_type: &str, message: &str) -> Vec<u8> {
    let payload = crate::codec::json::to_vec(&serde_json::json!({ "message": message }))
        .unwrap_or_else(|_| b"{\"message\":\"An internal server error occurred.\"}".to_vec());

    let exception_type = lowercase_first(exception_type);
    let exception_type = exception_type.as_str();
    let mut frame = frame_open();
    if !push_string_header(&mut frame, HDR_EXCEPTION_TYPE, exception_type)
        || !push_string_header(
            &mut frame,
            HDR_CONTENT_TYPE,
            busbar_contract::protocol::APPLICATION_JSON,
        )
        || !push_string_header(&mut frame, HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION)
    {
        diag_debug!(
            EVENTSTREAM_EXCEPTIONTYPE_HEADER_OVERSIZE,
            exception_type_len = exception_type.len(),
            "event-stream :exception-type header exceeds the type-7 string cap; dropping frame"
        );
        return Vec::new();
    }
    frame_close(frame, &payload)
}

/// Length of the fixed event-stream prelude: `total_len:u32 + headers_len:u32 + prelude_crc:u32`.
const PRELUDE_LEN: usize = 12;

/// Length of the trailing CRC32 message checksum appended to every frame.
const CRC_BYTES: usize = 4;

/// Minimum valid frame size: prelude + message CRC, with zero-length headers and payload.
const MIN_FRAME_BYTES: usize = PRELUDE_LEN + CRC_BYTES;

/// AWS event-stream header name for the event type (normal frames).
const HDR_EVENT_TYPE: &str = ":event-type";

/// AWS event-stream header name for the content MIME type.
const HDR_CONTENT_TYPE: &str = ":content-type";

/// AWS event-stream header name for the message type discriminator (`event` or `exception`).
const HDR_MESSAGE_TYPE: &str = ":message-type";

/// AWS event-stream header name for the modeled-exception type (exception frames only).
const HDR_EXCEPTION_TYPE: &str = ":exception-type";

/// `:message-type` value for a normal Bedrock event frame.
const MSG_TYPE_EVENT: &str = "event";

/// `:message-type` value for an AWS mid-stream modeled-exception frame.
const MSG_TYPE_EXCEPTION: &str = "exception";

/// `:message-type` value for an AWS mid-stream ERROR frame — the UNMODELED sibling of an exception

const MSG_TYPE_ERROR: &str = "error";

/// AWS event-stream header name for the error code (error frames only).
const HDR_ERROR_CODE: &str = ":error-code";

/// The message on a header-only unmodeled failure.
const HDR_ERROR_MESSAGE: &str = ":error-message";

/// AWS event-stream value-type byte for a UTF-8 string header (type 7 per the spec).
const HDR_TYPE_STRING: u8 = 7;

/// Open a frame buffer with the 12-byte prelude (`total_len`, `headers_len`, `prelude_crc`) reserved

fn frame_open() -> Vec<u8> {
    vec![0u8; PRELUDE_LEN]
}

/// Close a frame from [`frame_open`] after its headers are appended: backfill the prelude

fn frame_close(mut frame: Vec<u8>, payload: &[u8]) -> Vec<u8> {
    let headers_len = (frame.len() - PRELUDE_LEN) as u64;

    let total_len = PRELUDE_LEN as u64 + headers_len + payload.len() as u64 + CRC_BYTES as u64;
    if total_len > MAX_FRAME_BYTES as u64 {
        diag_debug!(
            EVENTSTREAM_FRAME_OVERSIZE,
            total_len,
            cap = MAX_FRAME_BYTES,
            "event-stream frame exceeds MAX_FRAME_BYTES; dropping"
        );
        return Vec::new();
    }

    frame.reserve(payload.len() + CRC_BYTES);

    frame[0..4].copy_from_slice(&(total_len as u32).to_be_bytes());
    frame[4..8].copy_from_slice(&(headers_len as u32).to_be_bytes());

    let prelude_crc = crc32fast::hash(&frame[..8]);
    frame[8..12].copy_from_slice(&prelude_crc.to_be_bytes());

    frame.extend_from_slice(payload);

    let message_crc = crc32fast::hash(&frame);
    frame.extend_from_slice(&message_crc.to_be_bytes());

    frame
}

#[cfg(test)]
#[path = "tests/eventstream_drift_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/eventstream_tests.rs"]
mod unit_tests;

#[cfg(test)]
#[path = "tests/eventstream_member_tests.rs"]
mod member_tests;
