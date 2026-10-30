// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The LLM plane's own AWS event-stream (`application/vnd.amazon.eventstream`) frame codec (#83a
//! SD-3): only the Bedrock dialect and its stream translator speak it. CRC32 is `crc32fast`
//! (CRC-32/ISO-HDLC); the drift test beside this module holds every encoded frame byte-identical to
//! the frames the host's (now deleted) copy wrote.
//!
//! [`drain_frames_checked`](crate::codec::eventstream::drain_frames_checked) is the production
//! decoder: it pulls `(event_type, payload)` pairs out of ConverseStream responses for the Bedrock
//! reader's `read_response_events`. Incremental (a trailing partial frame stays buffered); CRCs are
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
/// declared `total_len` is a malformed or hostile prelude; the cap stops it driving unbounded
/// buffering. Keep equal to `StreamTranslate::MAX_BUF` (the reassembly abort threshold) so any frame
/// the decoder will assemble can also be buffered to completion upstream; between the two caps a
/// frame would be aborted before `drain_frames_checked` saw it.
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
/// [`DrainStatus`]. [`DrainStatus::MalformedPrelude`] is the explicit abort signal: the buffer is
/// cleared (unrecoverable). A clean pass (emptied, or a trailing partial frame left) is
/// [`DrainStatus::Ok`]. The third element counts bytes consumed as complete valid frames from the
/// front of `buf`, excluding any cleared malformed remainder; the same-protocol verbatim path uses
/// `consumed_sink` instead and discards it.
/// `consumed_sink`, when `Some`, receives the verbatim bytes of each complete valid frame in order.
/// The same-protocol bedrock-to-bedrock re-emit uses it to forward the original frames without
/// cloning the whole reassembly buffer per chunk (O(chunks x buf) cumulative allocation, a memory
/// DoS). It collects only consumed bytes (nothing on a chunk completing no frame) and never the
/// malformed remainder. Pass `None` cross-protocol, which re-encodes.
pub fn drain_frames_checked(
    buf: &mut Vec<u8>,
    consumed_sink: Option<&mut Vec<u8>>,
) -> (Vec<(String, Vec<u8>)>, DrainStatus, usize) {
    drain_frames_counted(buf, consumed_sink, &mut DrainWork::default())
}

/// The work one drain pass did on its buffer, counted where done: the measure the complexity test
/// holds the drain to, instead of wall-clock time (Law 8: count work units, not time).
///
/// `scanned` is bytes walked as complete frames; `edits` buffer edits; `shifted` bytes those edits
/// moved (a front `drain` memmoves the tail). Every edit goes through [`consume_front`] /
/// [`clear_all`], so a per-frame editor shows `edits == frames` and `shifted` quadratic in frames.
// Production discards the count; only the complexity test reads it.
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
    mut consumed_sink: Option<&mut Vec<u8>>,
    work: &mut DrainWork,
) -> (Vec<(String, Vec<u8>)>, DrainStatus, usize) {
    let mut out = Vec::new();
    let mut status = DrainStatus::Ok;
    // Index marking how much of the front is consumed as complete valid frames. One buffer edit per
    // pass, not per frame: a front `Vec::drain` memmoves the tail, costing O(frames x bytes) on a
    // chunk of many small deltas. Nothing else observes `buf` mid-loop, so one `drain`/`clear` at the
    // end is behavior-identical.
    let mut pos = 0usize;
    loop {
        let rem = &buf[pos..];
        if rem.len() < PRELUDE_LEN {
            break; // need the full prelude
        }
        let total_len = u32::from_be_bytes([rem[0], rem[1], rem[2], rem[3]]) as usize;
        let headers_len = u32::from_be_bytes([rem[4], rem[5], rem[6], rem[7]]) as usize;
        // `total_len` is upstream-controlled (up to ~4 GiB). Reject a frame above MAX_FRAME_BYTES
        // before waiting for `rem.len() >= total_len`, or a crafted prelude declaring an enormous
        // consistent length would make the caller buffer toward a frame that never arrives (DoS).
        // Treated like any malformed prelude: abandon the stream.
        if !(MIN_FRAME_BYTES..=MAX_FRAME_BYTES).contains(&total_len)
            || headers_len > total_len - MIN_FRAME_BYTES
        {
            status = DrainStatus::MalformedPrelude; // distinct propagated signal, not length-inferred
            break;
        }
        if rem.len() < total_len {
            break; // partial frame — wait for more bytes
        }
        // Read the frame in place via slices into `rem` (one payload copy) and advance `pos`, not a
        // per-frame throwaway Vec (a second allocation per frame on the hot path).
        let headers = &rem[PRELUDE_LEN..PRELUDE_LEN + headers_len];
        let event_type = event_type_for_frame(headers);
        let payload = rem[PRELUDE_LEN + headers_len..total_len - CRC_BYTES].to_vec();
        out.push((event_type, payload));
        // Capture the frame's verbatim bytes for the same-proto re-emit.
        if let Some(sink) = consumed_sink.as_deref_mut() {
            sink.extend_from_slice(&rem[..total_len]);
        }
        pos += total_len;
        work.frames += 1;
        work.scanned += total_len;
    }
    // Malformed: the stream is unrecoverable, so the valid prefix and the remainder both go;
    // `valid_consumed` reports only the prefix.
    match status {
        DrainStatus::Ok => consume_front(buf, pos, work),
        DrainStatus::MalformedPrelude => clear_all(buf, work),
    }
    (out, status, pos)
}

/// Drain every complete frame from `buf`, returning `(event_type, payload_bytes)` per frame and
/// leaving a trailing partial frame buffered. A malformed prelude clears the buffer rather than
/// looping. Thin wrapper over [`drain_frames_checked`] that discards the [`DrainStatus`]; only
/// tests call it (production uses the checked form), so it is test-gated to avoid a dead-code warning.
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
}

/// Resolve the event-type token `drain_frames_checked` returns for a frame: `:event-type` verbatim
/// for a normal event; for a modeled exception frame (`:message-type: exception`, `:exception-type`,
/// no `:event-type`) the name with the first letter lowercased (`InternalServerException` to
/// `internalServerException`) so it matches the reader's exception arms. Empty when neither header
/// is present (a typeless or malformed frame).
fn event_type_for_frame(headers: &[u8]) -> String {
    let parsed = parse_frame_headers(headers);
    // An exception frame (`:message-type: exception`): AWS sets no `:event-type`, so use its
    // `:exception-type` normalized to the reader's union-member token (such frames once yielded `""`
    // and were dropped). An unmodeled failure arrives as `:message-type: error` with `:error-code` /
    // `:error-message` and no `:event-type`, the same typeless shape; without this arm a mid-flight
    // failure looked like a stream that simply stopped. Its code goes through the same
    // namespace-strip and first-letter lowercase, so `internalFailure` reaches the error arms like
    // an exception name.
    if parsed.message_type.as_deref() == Some(MSG_TYPE_ERROR) {
        if let Some(code) = parsed.error_code {
            return normalize_error_token(&code);
        }
    }
    if parsed.message_type.as_deref() == Some(MSG_TYPE_EXCEPTION) {
        if let Some(exc) = parsed.exception_type {
            // AWS may qualify `:exception-type` with a Smithy namespace / shape ARN prefix (e.g.
            // `com.amazon.coral.service#ThrottlingException`). Keep only the trailing bare name,
            // mirroring `extract_error`'s `rsplit(['#', '/'])` in proto/bedrock.rs, so it matches the
            // reader's exception arms. Use the last non-empty token, not `.next()`: a value ending in
            // a delimiter (`ThrottlingException#`) makes `rsplit` yield an empty first token, dropping
            // the classification to `""`. `unwrap_or(&exc)` covers the all-delimiter case (`"#"`).
            return normalize_error_token(&exc);
        }
    }
    parsed.event_type.unwrap_or_default()
}

/// Normalize an AWS failure-name header (`:exception-type` or `:error-code`) to the reader's
/// union-member token: drop namespace / shape-ARN qualification, lowercase the first letter.
///
/// Uses the last non-empty token, not `.next()`: a value ending in a delimiter
/// (`ThrottlingException#`, `aws.bedrock/`) makes `rsplit` yield an empty first token, dropping the
/// classification to `""` and sinking the error into the no-op arm. `unwrap_or(raw)` covers the
/// all-delimiter case (`"#"`), falling back to the raw value.
fn normalize_error_token(raw: &str) -> String {
    let bare = raw
        .rsplit(['#', '/'])
        .find(|s| !s.is_empty())
        .unwrap_or(raw);
    lowercase_first(bare)
}

/// Lowercase only the first character (`InternalServerException` to `internalServerException`),
/// mapping the PascalCase `:exception-type` to the Smithy union-member token the reader keys on.
/// ASCII-only (Converse exception names are ASCII identifiers).
fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Scan the header block for the `:event-type`, `:message-type` and `:exception-type` string
/// headers. String/bytes values (types 7/6) are read; the AWS-spec fixed-width types
/// (bool/byte/short/int/long/timestamp/uuid) are skipped by their width, so a non-string header
/// before the wanted ones does not abort the scan. It stops early only on a truncated block or a
/// value-type byte with no defined width, so a future AWS framing header does not drop the
/// recognized ones before it.
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
        // Fixed-width types have no length prefix and are skipped by advancing `p`; string/bytes
        // (6/7) carry a u16 length prefix.
        let fixed_width: Option<usize> = match value_type {
            0 | 1 => Some(0), // bool true / bool false — value is encoded in the type byte itself
            2 => Some(1),     // byte
            3 => Some(2),     // short
            4 => Some(4),     // int
            5 => Some(8),     // long
            8 => Some(8),     // timestamp
            9 => Some(16),    // uuid
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
                // Unknown value-type byte with no defined width: malformed, bail.
                None => break,
            },
        };
        // These framing headers are always type-7 strings; a fixed-width value has none to record.
        if let Some(v) = value.and_then(|v| std::str::from_utf8(v).ok()) {
            match name {
                n if n == HDR_EVENT_TYPE.as_bytes() => found.event_type = Some(v.to_string()),
                n if n == HDR_MESSAGE_TYPE.as_bytes() => found.message_type = Some(v.to_string()),
                n if n == HDR_EXCEPTION_TYPE.as_bytes() => {
                    found.exception_type = Some(v.to_string())
                }
                n if n == HDR_ERROR_CODE.as_bytes() => found.error_code = Some(v.to_string()),
                _ => {}
            }
        }
        h = &h[p..];
    }
    found
}

/// Append one `[name_len:u8][name][value_type:u8 = 7][value_len:u16 BE][value]` string header. The
/// spec caps a name at 255 bytes and a string value at 65535; current callers pass short ASCII, so
/// the limits never fire in practice.
///
/// Returns `false` and pushes nothing when `name` or `value` exceeds its limit, rather than
/// byte-truncating: a cut could split a multibyte UTF-8 sequence and emit a CRC-valid frame with an
/// invalid-UTF-8 header a strict SDK rejects (the outcome `encode_with_headers` avoids for
/// payloads). The encoder drops the whole frame on `false`, as for an oversized payload: graceful
/// and panic-free on the streaming path in every build profile (no `debug_assert`).
#[must_use]
fn push_string_header(headers: &mut Vec<u8>, name: &str, value: &str) -> bool {
    if name.len() > u8::MAX as usize || value.len() > u16::MAX as usize {
        return false; // oversized — drop rather than emit a truncated/corrupt header
    }
    headers.push(name.len() as u8);
    headers.extend_from_slice(name.as_bytes());
    headers.push(HDR_TYPE_STRING); // value_type 7 = UTF-8 string
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
    // Build headers directly into the frame buffer after a 12-byte prelude placeholder, avoiding a
    // second per-frame allocation on the hot path. `frame_close` backfills the prelude and CRCs.
    let mut frame = frame_open();
    // Drop the frame if a header is oversized rather than emit a corrupt one (see
    // push_string_header). `:event-type` is the only caller-supplied value.
    if !push_string_header(&mut frame, HDR_EVENT_TYPE, event_type)
        || !push_string_header(
            &mut frame,
            HDR_CONTENT_TYPE,
            busbar_contract::protocol::APPLICATION_JSON,
        )
        || !push_string_header(&mut frame, HDR_MESSAGE_TYPE, MSG_TYPE_EVENT)
    {
        // An oversized header dropped the frame. Unreachable for a real Bedrock event name but
        // logged, not silent. `:event-type` is the only caller-supplied value; the others are literals.
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
/// mid-stream error with `:message-type: exception` and an `:exception-type` naming the Converse
/// exception (e.g. `InternalServerException`, `ModelStreamErrorException`); the payload is the JSON
/// `{"message": ...}` body. A Bedrock-ingress stream must emit this on a mid-stream upstream failure,
/// not SSE `event: error` text, which makes an undecodable prelude/CRC for the SDK.
pub fn encode_exception_frame(exception_type: &str, message: &str) -> Vec<u8> {
    // Fallback only if serializing `{"message": <string>}` fails (effectively unreachable). Uses
    // AWS's generic phrasing, not busbar vocabulary like "upstream", which a native exception frame
    // would never carry (a protocol tell; mirrors the scrub in
    // proto::gemini::GeminiJsonArrayFramer::finish_with_error).
    let payload = crate::codec::json::to_vec(&serde_json::json!({ "message": message }))
        .unwrap_or_else(|_| b"{\"message\":\"An internal server error occurred.\"}".to_vec());
    // Headers go straight into the single frame buffer (see `encode_frame`).
    //
    // `:exception-type` names a member of the ConverseStreamOutput union (botocore bedrock-runtime
    // service-2.json), which is lowerCamel (`internalServerException`), so a PascalCase shape name
    // is spelled as the member on the wire (owner-approved fidelity fix, 2026-10-26; v1.5.5 sent the
    // shape name). Idempotent for already-lowerCamel names.
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
        // `:exception-type` is caller-supplied; an oversized one drops the frame. Logged so a swallowed
        // mid-stream error signal is observable.
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
/// frame. It carries `:error-code` / `:error-message` instead of `:exception-type`, and no
/// `:event-type` at all.
const MSG_TYPE_ERROR: &str = "error";

/// AWS event-stream header name for the error code (error frames only).
const HDR_ERROR_CODE: &str = ":error-code";

/// AWS event-stream value-type byte for a UTF-8 string header (type 7 per the spec).
const HDR_TYPE_STRING: u8 = 7;

/// Open a frame buffer with the 12-byte prelude (`total_len`, `headers_len`, `prelude_crc`) reserved
/// as a zeroed placeholder. Callers append headers after it, then call [`frame_close`], keeping the
/// whole frame in one allocation.
fn frame_open() -> Vec<u8> {
    vec![0u8; PRELUDE_LEN]
}

/// Close a frame from [`frame_open`] after its headers are appended: backfill the prelude
/// (`total_len`, `headers_len`, real `prelude_crc`), append `payload`, then the real `message_crc`.
/// Shared by [`encode_frame`] and [`encode_exception_frame`].
///
/// Frames are always well under `MAX_FRAME_BYTES`. If header+payload would exceed it, the frame is
/// dropped (empty `Vec`) rather than truncating the payload: a CRC-valid frame with unparseable JSON
/// is worse for an SDK than none. The caller appends the result, so an empty return emits nothing.
fn frame_close(mut frame: Vec<u8>, payload: &[u8]) -> Vec<u8> {
    // At entry `frame` is [12-byte prelude placeholder][headers].
    let headers_len = (frame.len() - PRELUDE_LEN) as u64;
    // total_len = prelude(12) + headers + payload + message_crc(4), widened to u64 so it cannot
    // overflow, then bounded by MAX_FRAME_BYTES.
    let total_len = PRELUDE_LEN as u64 + headers_len + payload.len() as u64 + CRC_BYTES as u64;
    if total_len > MAX_FRAME_BYTES as u64 {
        // Oversized: drop the frame rather than emit truncated JSON. Unreachable for a real
        // ConverseStream delta; guards a pathological multi-MiB event.
        diag_debug!(
            EVENTSTREAM_FRAME_OVERSIZE,
            total_len,
            cap = MAX_FRAME_BYTES,
            "event-stream frame exceeds MAX_FRAME_BYTES; dropping"
        );
        return Vec::new();
    }

    // Reserve payload + CRC trailer up front.
    frame.reserve(payload.len() + CRC_BYTES);

    // Backfill total_len + headers_len (u32 BE); bounded above, so the casts are exact.
    frame[0..4].copy_from_slice(&(total_len as u32).to_be_bytes());
    frame[4..8].copy_from_slice(&(headers_len as u32).to_be_bytes());

    // prelude_crc = CRC32 of the first 8 bytes (the two length fields).
    let prelude_crc = crc32fast::hash(&frame[..8]);
    frame[8..12].copy_from_slice(&prelude_crc.to_be_bytes());

    frame.extend_from_slice(payload);

    // message_crc = CRC32 of the whole frame so far (prelude, prelude_crc, headers, payload).
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
