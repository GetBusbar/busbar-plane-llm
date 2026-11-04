// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The framing codec's unit suite: decode, encode, the header guards and their coded drops, the
//! checked drain's malformed-prelude signal and its linear scaling.

use super::*;
use crate::codec_diagnostics::{debug_diagnostics, diagnostic_texts, warnings};

/// AWS event-stream header name for the human-readable error text an error frame carries alongside
/// its code.
const HDR_ERROR_MESSAGE: &str = ":error-message";

#[test]
fn test_decode_single_frame() {
    let mut buf = encode_frame("contentBlockDelta", br#"{"delta":{"text":"hi"}}"#);
    let frames = drain_frames(&mut buf);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, "contentBlockDelta");
    assert_eq!(frames[0].1, br#"{"delta":{"text":"hi"}}"#);
    assert!(buf.is_empty(), "fully-consumed buffer");
}

#[test]
fn test_decode_multiple_and_partial() {
    let mut buf = encode_frame("messageStart", br#"{"role":"assistant"}"#);
    buf.extend(encode_frame("messageStop", br#"{"stopReason":"end_turn"}"#));

    let partial = encode_frame("metadata", br#"{"usage":{}}"#);
    buf.extend_from_slice(&partial[..partial.len() - 5]);

    let frames = drain_frames(&mut buf);
    assert_eq!(frames.len(), 2, "two complete frames decoded");
    assert_eq!(frames[0].0, "messageStart");
    assert_eq!(frames[1].0, "messageStop");
    assert!(!buf.is_empty(), "partial third frame remains buffered");

    buf.extend_from_slice(&partial[partial.len() - 5..]);
    let more = drain_frames(&mut buf);
    assert_eq!(more.len(), 1);
    assert_eq!(more[0].0, "metadata");
    assert!(buf.is_empty());
}

#[test]
fn test_oversized_total_len_is_abandoned_not_buffered() {
    let mut buf = Vec::new();
    let huge: u32 = u32::MAX;
    buf.extend_from_slice(&huge.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&[0, 0, 0, 0]);
    buf.extend_from_slice(b"trailing junk");

    let frames = drain_frames(&mut buf);
    assert!(frames.is_empty(), "no frame should be emitted");
    assert!(
        buf.is_empty(),
        "oversized frame must clear the buffer, not buffer toward total_len"
    );
}

/// `drain_frames(encode_frame(x)) == [x]` for a spread of event types + payload sizes, including
/// empty and large payloads.
#[test]
fn test_encode_decode_round_trip() {
    let cases: &[(&str, Vec<u8>)] = &[
        ("messageStart", br#"{"role":"assistant"}"#.to_vec()),
        ("contentBlockDelta", br#"{"delta":{"text":"hi"}}"#.to_vec()),
        ("messageStop", br#"{"stopReason":"end_turn"}"#.to_vec()),
        (
            "metadata",
            br#"{"usage":{"inputTokens":3,"outputTokens":5}}"#.to_vec(),
        ),
        ("contentBlockStop", Vec::new()),
        ("contentBlockDelta", vec![b'x'; 64 * 1024]),
    ];
    for (event_type, payload) in cases {
        let mut buf = encode_frame(event_type, payload);
        let frames = drain_frames(&mut buf);
        assert_eq!(frames.len(), 1, "exactly one frame for {event_type}");
        assert_eq!(&frames[0].0, event_type, "event type round-trips");
        assert_eq!(
            &frames[0].1, payload,
            "payload round-trips for {event_type}"
        );
        assert!(buf.is_empty(), "buffer fully consumed for {event_type}");
    }
}

/// The encoder writes REAL CRC32s (not the `[0,0,0,0]` placeholders the old test helper used).
#[test]
fn test_encode_crcs_are_real() {
    let frame = encode_frame("contentBlockDelta", br#"{"delta":{"text":"hi"}}"#);
    let total_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    assert_eq!(
        total_len,
        frame.len(),
        "total_len matches the bytes written"
    );

    let prelude_crc = u32::from_be_bytes([frame[8], frame[9], frame[10], frame[11]]);
    let expected_prelude = crc32fast::hash(&frame[..8]);
    assert_eq!(
        prelude_crc, expected_prelude,
        "prelude CRC is the real CRC32"
    );
    assert_ne!(prelude_crc, 0, "prelude CRC is not the zero placeholder");

    let len = frame.len();
    let message_crc = u32::from_be_bytes([
        frame[len - 4],
        frame[len - 3],
        frame[len - 2],
        frame[len - 1],
    ]);
    let expected_message = crc32fast::hash(&frame[..len - 4]);
    assert_eq!(
        message_crc, expected_message,
        "message CRC is the real CRC32"
    );
    assert_ne!(message_crc, 0, "message CRC is not the zero placeholder");
}

/// Build a header block with one type-7 string header `[name][value]`.
fn string_header(name: &str, value: &str) -> Vec<u8> {
    let mut h = Vec::new();
    h.push(name.len() as u8);
    h.extend_from_slice(name.as_bytes());
    h.push(HDR_TYPE_STRING);
    h.extend_from_slice(&(value.len() as u16).to_be_bytes());
    h.extend_from_slice(value.as_bytes());
    h
}

/// `event_type_for_frame` returns `""` (rather than panic or misread) when it meets a header
/// whose value-type byte is genuinely unknown / has no defined width before any recognized
/// header.
#[test]
fn test_event_type_unknown_value_type_yields_empty() {
    let mut h = Vec::new();
    h.push(1u8);
    h.extend_from_slice(b"x");
    h.push(200u8);
    assert_eq!(event_type_for_frame(&h), "");
}

/// A fixed-width header (e.g. a `timestamp`, type 8) appearing BEFORE `:event-type` must be
/// skipped by advancing the correct number of bytes, not abort the scan — so the event type is
/// still recovered.
#[test]
fn test_event_type_skips_fixed_width_header() {
    let mut h = Vec::new();

    h.push(3u8);
    h.extend_from_slice(b":ts");
    h.push(8u8);
    h.extend_from_slice(&0u64.to_be_bytes());

    h.extend_from_slice(&string_header(HDR_EVENT_TYPE, "messageStart"));
    assert_eq!(event_type_for_frame(&h), "messageStart");
}

/// A zero-length `:event-type` string value yields `""` — a present-but-empty event type is
/// indistinguishable from absent at the `drain_frames` boundary, which is fine (the reader
/// treats both as a no-op frame).
#[test]
fn test_event_type_empty_value() {
    let h = string_header(HDR_EVENT_TYPE, "");
    assert_eq!(event_type_for_frame(&h), "");
}

/// An AWS modeled-exception frame carries
/// `:message-type: exception` + `:exception-type: <Name>` and NO `:event-type`.
#[test]
fn test_event_type_exception_frame_returns_normalized_exception_name() {
    let mut h = string_header(HDR_EXCEPTION_TYPE, "InternalServerException");
    h.extend_from_slice(&string_header(
        HDR_CONTENT_TYPE,
        crate::codec::protocol::APPLICATION_JSON,
    ));
    h.extend_from_slice(&string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION));
    assert_eq!(event_type_for_frame(&h), "internalServerException");

    let mut h2 = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION);
    h2.extend_from_slice(&string_header(HDR_EXCEPTION_TYPE, "ThrottlingException"));
    assert_eq!(event_type_for_frame(&h2), "throttlingException");
}

/// The UNMODELED sibling of an exception frame: `:message-type: error` carrying `:error-code` /
/// `:error-message` and NO `:event-type`.
#[test]
fn test_event_type_error_frame_returns_normalized_error_code() {
    let mut h = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_ERROR);
    h.extend_from_slice(&string_header(HDR_ERROR_CODE, "InternalFailure"));
    h.extend_from_slice(&string_header(
        HDR_ERROR_MESSAGE,
        "upstream terminated the stream",
    ));
    assert_eq!(event_type_for_frame(&h), "internalFailure");

    let mut h2 = string_header(
        HDR_ERROR_CODE,
        "com.amazon.coral.service#ThrottlingException",
    );
    h2.extend_from_slice(&string_header(HDR_MESSAGE_TYPE, MSG_TYPE_ERROR));
    assert_eq!(event_type_for_frame(&h2), "throttlingException");

    let h3 = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_ERROR);
    assert_eq!(event_type_for_frame(&h3), "");
}

/// AWS may qualify the `:exception-type`
#[test]
fn test_event_type_exception_strips_namespace_prefix() {
    let mut h = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION);
    h.extend_from_slice(&string_header(
        HDR_EXCEPTION_TYPE,
        "com.amazon.coral.service#ThrottlingException",
    ));
    assert_eq!(
        event_type_for_frame(&h),
        "throttlingException",
        "namespace prefix stripped before lowercasing the bare exception name"
    );

    let mut h2 = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION);
    h2.extend_from_slice(&string_header(
        HDR_EXCEPTION_TYPE,
        "aws.service/InternalServerException",
    ));
    assert_eq!(event_type_for_frame(&h2), "internalServerException");

    let mut h3 = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION);
    h3.extend_from_slice(&string_header(
        HDR_EXCEPTION_TYPE,
        "ModelStreamErrorException",
    ));
    assert_eq!(event_type_for_frame(&h3), "modelStreamErrorException");
}

/// An `:exception-type` value
/// that ENDS with a Smithy/ARN delimiter (`ThrottlingException#`, `aws.bedrock/`) made
/// `rsplit(['#', '/']).next()` return the empty LEADING token, dropping the classification to
/// `""` — re-sinking the mid-stream error into the no-op arm the namespace fix was meant to
/// prevent.
#[test]
fn test_event_type_exception_trailing_delimiter_recovers_name() {
    let mut h = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION);
    h.extend_from_slice(&string_header(HDR_EXCEPTION_TYPE, "ThrottlingException#"));
    assert_eq!(
        event_type_for_frame(&h),
        "throttlingException",
        "a trailing `#` must not drop the exception classification to empty"
    );

    let mut h2 = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION);
    h2.extend_from_slice(&string_header(HDR_EXCEPTION_TYPE, "ThrottlingException/"));
    assert_eq!(
        event_type_for_frame(&h2),
        "throttlingException",
        "a trailing `/` must not drop the exception classification to empty"
    );

    let mut h3 = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION);
    h3.extend_from_slice(&string_header(
        HDR_EXCEPTION_TYPE,
        "com.amazon.coral.service#ThrottlingException",
    ));
    assert_eq!(event_type_for_frame(&h3), "throttlingException");

    let mut h4 = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION);
    h4.extend_from_slice(&string_header(HDR_EXCEPTION_TYPE, "#"));
    assert_eq!(
        event_type_for_frame(&h4),
        "#",
        "an all-delimiter value falls back to the raw value, not empty/panic"
    );
}

/// An exception-typed frame
/// (`:message-type: exception`) that carries NO `:exception-type` header must fall through to the
/// empty string — never panic and never misreport.
#[test]
fn test_event_type_exception_without_exception_type_yields_empty() {
    let h = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION);
    assert_eq!(
        event_type_for_frame(&h),
        "",
        "exception frame missing :exception-type falls through to empty, no panic"
    );

    let mut h2 = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EXCEPTION);
    h2.extend_from_slice(&string_header(
        HDR_CONTENT_TYPE,
        crate::codec::protocol::APPLICATION_JSON,
    ));
    assert_eq!(
        event_type_for_frame(&h2),
        "",
        "exception frame with only :message-type + :content-type is still empty"
    );
}

/// A frame with `:message-type: event` (the normal case) must still report its `:event-type`,
/// never an exception name, even if a stray `:exception-type` somehow rode along.
#[test]
fn test_event_type_event_message_type_prefers_event_type() {
    let mut h = string_header(HDR_MESSAGE_TYPE, MSG_TYPE_EVENT);
    h.extend_from_slice(&string_header(HDR_EVENT_TYPE, "contentBlockDelta"));
    assert_eq!(event_type_for_frame(&h), "contentBlockDelta");
}

/// End-to-end through `drain_frames`: a real binary exception frame (built by the production
/// `encode_exception_frame`) decodes to the normalized exception event-type, so the egress
/// decode path (`StreamTranslate::feed`) folds a matchable `type` into the JSON and the reader
/// surfaces an error instead of dropping a typeless frame.
#[test]
fn test_drain_frames_surfaces_exception_event_type() {
    let mut buf =
        encode_exception_frame("ServiceUnavailableException", "upstream temporarily down");
    let frames = drain_frames(&mut buf);
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].0, "serviceUnavailableException",
        "exception frame decodes to the normalized union-member token"
    );
    let payload: serde_json::Value = serde_json::from_slice(&frames[0].1).unwrap();
    assert_eq!(payload["message"], "upstream temporarily down");
    assert!(buf.is_empty());
}

/// A modeled-exception frame is a valid event-stream message: real CRC32s, and a header block
/// carrying `:message-type: exception` + `:exception-type` + the JSON `{"message":...}` payload.
#[test]
fn test_encode_exception_frame_is_valid() {
    let frame = encode_exception_frame("InternalServerException", "upstream stream error");

    let total_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    assert_eq!(total_len, frame.len(), "total_len matches frame bytes");

    let prelude_crc = u32::from_be_bytes([frame[8], frame[9], frame[10], frame[11]]);
    assert_eq!(prelude_crc, crc32fast::hash(&frame[..8]));

    let len = frame.len();
    let msg_crc = u32::from_be_bytes([
        frame[len - CRC_BYTES],
        frame[len - CRC_BYTES + 1],
        frame[len - CRC_BYTES + 2],
        frame[len - CRC_BYTES + 3],
    ]);
    assert_eq!(msg_crc, crc32fast::hash(&frame[..len - CRC_BYTES]));

    let headers_len = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
    let headers = String::from_utf8_lossy(&frame[PRELUDE_LEN..PRELUDE_LEN + headers_len]);
    assert!(headers.contains(":message-type"));
    assert!(headers.contains("exception"));
    assert!(headers.contains(":exception-type"));
    assert!(headers.contains("internalServerException"));

    let payload = &frame[PRELUDE_LEN + headers_len..len - CRC_BYTES];
    let v: serde_json::Value = serde_json::from_slice(payload).unwrap();
    assert_eq!(v["message"], "upstream stream error");

    assert!(!frame.starts_with(b"event:"));
}

/// THE CAP, FROM BOTH SIDES. A frame whose total length is EXACTLY `MAX_FRAME_BYTES` is encoded
/// whole; one byte more is DROPPED (empty frame), never emitted as a CRC-valid frame carrying
/// byte-truncated, unparseable JSON.
#[test]
fn test_encode_frame_cap_admits_the_boundary_and_drops_one_past_it() {
    const EVENT: &str = "contentBlockDelta";
    let probe = encode_frame(EVENT, b"x");
    assert!(!probe.is_empty(), "the probe frame must encode");
    let overhead = probe.len() - 1;

    let at_cap = vec![b'x'; MAX_FRAME_BYTES - overhead];
    let frame = encode_frame(EVENT, &at_cap);
    assert_eq!(
        frame.len(),
        MAX_FRAME_BYTES,
        "a frame of exactly MAX_FRAME_BYTES is legal and must be encoded whole"
    );
    assert_eq!(
        u32::from_be_bytes(frame[0..4].try_into().unwrap()) as usize,
        MAX_FRAME_BYTES,
        "and its prelude must declare exactly the cap"
    );

    let one_over = vec![b'x'; MAX_FRAME_BYTES - overhead + 1];
    assert!(
        encode_frame(EVENT, &one_over).is_empty(),
        "one byte past the cap must drop the frame, not truncate JSON into a CRC-valid corrupt frame"
    );
}

/// `drain_frames` must abandon (clear) the buffer on a frame whose `total_len` is in range but
/// whose `headers_len` exceeds the space remaining after the 16-byte overhead — the second half
/// of the prelude-validation guard, previously untested.
#[test]
fn test_headers_len_overflow_abandoned() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&20u32.to_be_bytes());
    buf.extend_from_slice(&5u32.to_be_bytes());
    buf.extend_from_slice(&[0, 0, 0, 0]);
    buf.extend_from_slice(b"junk extra bytes");

    let frames = drain_frames(&mut buf);
    assert!(
        frames.is_empty(),
        "no frame emitted for headers_len overflow"
    );
    assert!(
        buf.is_empty(),
        "headers_len overflow must abandon (clear) the buffer, not slice OOB"
    );
}

/// An oversized header NAME or VALUE must DROP the whole frame (empty `Vec`) rather than silently
/// byte-truncate the string — a truncation could split a multi-byte UTF-8 sequence and emit a
/// CRC-valid frame carrying an invalid-UTF-8 type-7 header a strict AWS SDK rejects.
#[test]
fn test_oversized_header_value_drops_frame() {
    let huge_value = "x".repeat(u16::MAX as usize + 1);
    let frame = encode_exception_frame(&huge_value, "msg");
    assert!(
        frame.is_empty(),
        "an oversized exception-type header must drop the frame, not truncate the string"
    );

    let ok = encode_exception_frame("InternalServerException", "msg");
    assert!(!ok.is_empty());
}

/// `encode_frame` must DROP the whole frame (empty `Vec`) when the caller-supplied `:event-type`
#[test]
fn test_encode_frame_oversized_event_type_drops_frame() {
    let huge_event_type = "e".repeat(u16::MAX as usize + 1);
    let frame = encode_frame(&huge_event_type, br#"{"x":1}"#);
    assert!(
        frame.is_empty(),
        "an oversized :event-type header must drop the frame, not truncate the string"
    );

    let ok = encode_frame("contentBlockDelta", br#"{"x":1}"#);
    assert!(!ok.is_empty());
}

/// The encoder carries the three Bedrock framing headers (`:event-type`, `:content-type`,
/// `:message-type`); `parse_event_type` must skip past the others and still find the event name.
#[test]
fn test_encode_carries_three_headers() {
    let frame = encode_frame("messageStart", br#"{"role":"assistant"}"#);
    let headers_len = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
    let headers = &frame[PRELUDE_LEN..PRELUDE_LEN + headers_len];

    let hs = String::from_utf8_lossy(headers);
    assert!(hs.contains(":event-type"));
    assert!(hs.contains(":content-type"));
    assert!(hs.contains("application/json"));
    assert!(hs.contains(":message-type"));
    assert!(hs.contains("event"));
}

/// The SMALLEST valid frame —
/// `total_len == 16` (12-byte prelude + 0-byte headers + 0-byte payload + 4-byte message CRC) —
/// must decode cleanly.
#[test]
fn test_drain_frames_minimum_valid_frame() {
    let mut frame = Vec::with_capacity(16);
    frame.extend_from_slice(&16u32.to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes());
    let prelude_crc = crc32fast::hash(&frame[..8]);
    frame.extend_from_slice(&prelude_crc.to_be_bytes());

    let message_crc = crc32fast::hash(&frame);
    frame.extend_from_slice(&message_crc.to_be_bytes());
    assert_eq!(
        frame.len(),
        16, // golden wire-contract literal (kept bare on purpose)
        "hand-crafted frame is exactly the minimum size"
    );

    let mut buf = frame;
    let frames = drain_frames(&mut buf);
    assert_eq!(
        frames.len(),
        1,
        "the minimum 16-byte frame decodes to one frame"
    );
    assert_eq!(frames[0].0, "", "no :event-type header → empty event type");
    assert!(frames[0].1.is_empty(), "empty payload round-trips");
    assert!(buf.is_empty(), "minimum frame is fully consumed");
}

/// The single-buffer
/// encoder must be BYTE-FOR-BYTE identical to the prior two-Vec (`headers` + `frame`) encoding.
#[test]
fn test_encode_frame_byte_for_byte_matches_reference() {
    let event_type = "contentBlockDelta";
    let payload = br#"{"delta":{"text":"hi"}}"#;
    let got = encode_frame(event_type, payload);

    let mut headers = Vec::new();
    headers.extend_from_slice(&string_header(":event-type", event_type));
    headers.extend_from_slice(&string_header(":content-type", "application/json"));
    headers.extend_from_slice(&string_header(":message-type", "event"));

    let headers_len = headers.len();
    let total_len = 12 + headers_len + payload.len() + 4;

    let mut want = Vec::new();
    want.extend_from_slice(&(total_len as u32).to_be_bytes());
    want.extend_from_slice(&(headers_len as u32).to_be_bytes());
    let prelude_crc = crc32fast::hash(&want[..8]);
    want.extend_from_slice(&prelude_crc.to_be_bytes());
    want.extend_from_slice(&headers);
    want.extend_from_slice(payload);
    let message_crc = crc32fast::hash(&want);
    want.extend_from_slice(&message_crc.to_be_bytes());

    assert_eq!(
        got, want,
        "single-buffer encode_frame must be byte-for-byte identical to the reference encoding"
    );
}

/// When an oversized `:event-type` makes `push_string_header` reject the header, `encode_frame`
#[test]
fn test_encode_frame_oversized_event_type_is_a_coded_debug_drop() {
    use crate::codec::diagnostics::EVENTSTREAM_EVENTTYPE_HEADER_OVERSIZE as DIAG;

    let huge_event_type = "e".repeat(u16::MAX as usize + 1);

    let (debug_cap, debug_cap_subscriber) = debug_diagnostics();
    let frame = tracing::dispatcher::with_default(&debug_cap_subscriber, || {
        encode_frame(&huge_event_type, br#"{"x":1}"#)
    });
    let (warn_cap, warn_cap_subscriber) = warnings();
    tracing::dispatcher::with_default(&warn_cap_subscriber, || {
        encode_frame(&huge_event_type, br#"{"x":1}"#)
    });

    assert!(
        frame.is_empty(),
        "oversized :event-type still drops the frame"
    );
    let banner = DIAG.banner().to_string();
    assert!(
        diagnostic_texts(&debug_cap)
            .iter()
            .any(|event| event.contains(&banner)),
        "the drop must carry diag={banner} so an operator can look it up; captured: {:?}",
        diagnostic_texts(&debug_cap)
    );
    let warned = diagnostic_texts(&warn_cap);
    assert!(
        warned.is_empty(),
        "and it must stay at debug, never an unlatched per-frame WARN, got: {warned:?}"
    );
}

/// The same guarantee for `encode_exception_frame` — an oversized `:exception-type` drops the frame,
/// and the drop carries `EVENTSTREAM_EXCEPTIONTYPE_HEADER_OVERSIZE`'s code at `debug!` (a swallowed
/// mid-stream error signal, near-unreachable per request) rather than spamming per frame.
#[test]
fn test_encode_exception_frame_oversized_type_is_a_coded_debug_drop() {
    use crate::codec::diagnostics::EVENTSTREAM_EXCEPTIONTYPE_HEADER_OVERSIZE as DIAG;

    let huge = "x".repeat(u16::MAX as usize + 1);

    let (debug_cap, debug_cap_subscriber) = debug_diagnostics();
    let frame = tracing::dispatcher::with_default(&debug_cap_subscriber, || {
        encode_exception_frame(&huge, "msg")
    });
    let (warn_cap, warn_cap_subscriber) = warnings();
    tracing::dispatcher::with_default(&warn_cap_subscriber, || {
        encode_exception_frame(&huge, "msg")
    });

    assert!(
        frame.is_empty(),
        "oversized :exception-type still drops the frame"
    );
    let banner = DIAG.banner().to_string();
    assert!(
        diagnostic_texts(&debug_cap)
            .iter()
            .any(|event| event.contains(&banner)),
        "the drop must carry diag={banner}; captured: {:?}",
        diagnostic_texts(&debug_cap)
    );
    let warned = diagnostic_texts(&warn_cap);
    assert!(
        warned.is_empty(),
        "and it must stay at debug, never an unlatched per-frame WARN, got: {warned:?}"
    );
}

/// A malformed prelude must abandon
/// the stream via a DISTINCT propagated status (`DrainStatus::MalformedPrelude`), not be inferred
/// from the buffer being emptied.
#[test]
fn test_drain_frames_checked_signals_malformed_prelude_distinctly() {
    let mut bad = Vec::new();
    bad.extend_from_slice(&u32::MAX.to_be_bytes());
    bad.extend_from_slice(&0u32.to_be_bytes());
    bad.extend_from_slice(&[0, 0, 0, 0]);
    bad.extend_from_slice(b"trailing junk");
    let (frames, status, valid_consumed) = drain_frames_checked(&mut bad, None);
    assert!(
        frames.is_empty(),
        "no frame emitted for a malformed prelude"
    );
    assert!(bad.is_empty(), "malformed prelude clears the buffer");
    assert_eq!(
        valid_consumed, 0,
        "a malformed prelude at the front consumes ZERO valid frame bytes — the same-proto \
             verbatim emit must forward none of the cleared remainder"
    );
    assert_eq!(
        status,
        DrainStatus::MalformedPrelude,
        "malformed prelude must propagate the DISTINCT abort signal, not be length-inferred"
    );

    let mut bad2 = Vec::new();
    bad2.extend_from_slice(&20u32.to_be_bytes());
    bad2.extend_from_slice(&5u32.to_be_bytes());
    bad2.extend_from_slice(&[0, 0, 0, 0]);
    bad2.extend_from_slice(b"junk extra bytes");
    let (frames2, status2, _) = drain_frames_checked(&mut bad2, None);
    assert!(frames2.is_empty());
    assert!(bad2.is_empty());
    assert_eq!(status2, DrainStatus::MalformedPrelude);

    let mut good = encode_frame("contentBlockDelta", br#"{"delta":{"text":"hi"}}"#);
    let good_len = good.len();
    let (frames3, status3, valid_consumed3) = drain_frames_checked(&mut good, None);
    assert_eq!(frames3.len(), 1);
    assert_eq!(
        valid_consumed3, good_len,
        "a clean full drain reports the whole consumed frame length"
    );
    assert!(
        good.is_empty(),
        "a clean full drain also empties the buffer"
    );
    assert_eq!(
        status3,
        DrainStatus::Ok,
        "an empty buffer after a clean full drain must NOT be read as an abort"
    );

    let full = encode_frame("messageStop", br#"{"stopReason":"end_turn"}"#);
    let mut partial = full[..full.len() - 4].to_vec();
    let (frames4, status4, _) = drain_frames_checked(&mut partial, None);
    assert!(frames4.is_empty(), "no complete frame yet");
    assert!(!partial.is_empty(), "partial frame stays buffered");
    assert_eq!(
        status4,
        DrainStatus::Ok,
        "a buffered partial is not an abort"
    );

    let mut buf = encode_frame("messageStart", br#"{"role":"assistant"}"#);
    let only_frames = drain_frames(&mut buf);
    assert_eq!(only_frames.len(), 1);
    assert_eq!(only_frames[0].0, "messageStart");
}

/// The smallest frame with a NON-empty
/// payload that carries no headers — `total_len == 18` (12 prelude + 0 headers + 2 payload + 4
/// CRC).
#[test]
fn test_drain_frames_two_byte_payload_no_headers() {
    let payload = b"hi";

    let total_len = PRELUDE_LEN as u32 + payload.len() as u32 + CRC_BYTES as u32;
    let mut frame = Vec::with_capacity(total_len as usize);
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes());
    let prelude_crc = crc32fast::hash(&frame[..8]);
    frame.extend_from_slice(&prelude_crc.to_be_bytes());
    frame.extend_from_slice(payload);
    let message_crc = crc32fast::hash(&frame);
    frame.extend_from_slice(&message_crc.to_be_bytes());
    assert_eq!(frame.len(), 18);

    let mut buf = frame;
    let frames = drain_frames(&mut buf);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, "", "no :event-type header → empty event type");
    assert_eq!(frames[0].1, payload, "two-byte payload round-trips");
    assert!(buf.is_empty());
}

/// Regression proof (byte-identical behavior before and after the O(frames²)→O(bytes) rewrite
/// of `drain_frames_checked`'s per-frame buffer advance): many frames, a trailing partial, a
/// `consumed_sink` populated in order, exact `valid_consumed`.
#[test]
fn drain_frames_checked_is_byte_identical_after_the_index_rewrite() {
    let mut buf = Vec::new();
    let mut expected: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..50 {
        let payload = format!("{{\"i\":{i}}}");
        let frame = encode_frame("contentBlockDelta", payload.as_bytes());
        buf.extend_from_slice(&frame);
        expected.push(("contentBlockDelta".to_string(), payload.into_bytes()));
    }

    let partial_full = encode_frame("messageStop", br#"{"stopReason":"end_turn"}"#);
    let partial = &partial_full[..partial_full.len() - 3];
    buf.extend_from_slice(partial);

    let mut sink = Vec::new();
    let (frames, status, valid_consumed) = drain_frames_checked(&mut buf, Some(&mut sink));
    assert_eq!(status, DrainStatus::Ok);
    assert_eq!(frames, expected);
    assert_eq!(
        valid_consumed,
        sink.len(),
        "valid_consumed must equal the verbatim bytes captured in the sink"
    );
    assert_eq!(
        buf, partial,
        "the trailing partial frame remains buffered, byte-identical"
    );

    let mut expected_sink = Vec::new();
    for i in 0..50 {
        let payload = format!("{{\"i\":{i}}}");
        expected_sink.extend_from_slice(&encode_frame("contentBlockDelta", payload.as_bytes()));
    }
    assert_eq!(sink, expected_sink);
}

/// Extends `test_drain_frames_checked_signals_malformed_prelude_distinctly` with valid frames
/// BEFORE the malformed one — the case the index-tracking rewrite is most likely to break,
/// since it is the first test to exercise `pos` advancing across multiple frames before hitting
/// the `buf.clear()` / status-abort arm.
#[test]
fn a_malformed_prelude_after_valid_frames_still_clears_and_reports_the_prefix() {
    let mut buf = Vec::new();
    let mut prefix_len = 0usize;
    for i in 0..5 {
        let payload = format!("{{\"i\":{i}}}");
        let frame = encode_frame("contentBlockDelta", payload.as_bytes());
        prefix_len += frame.len();
        buf.extend_from_slice(&frame);
    }

    buf.extend_from_slice(&u32::MAX.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&[0, 0, 0, 0]);
    buf.extend_from_slice(b"trailing junk");

    let (frames, status, valid_consumed) = drain_frames_checked(&mut buf, None);
    assert_eq!(
        frames.len(),
        5,
        "the 5 valid frames before the malformed one are returned"
    );
    assert_eq!(
        valid_consumed, prefix_len,
        "valid_consumed counts only the valid prefix, not the cleared malformed remainder"
    );
    assert_eq!(status, DrainStatus::MalformedPrelude);
    assert!(buf.is_empty(), "the WHOLE buffer clears, prefix included");
}

/// A buffer of `n` complete small frames.
fn frames_buffer(n: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    for i in 0..n {
        let payload = format!("{{\"i\":{i}}}");
        buf.extend_from_slice(&encode_frame("contentBlockDelta", payload.as_bytes()));
    }
    buf
}

/// The drain's work grew linearly from `n` to `4n` frames: under 8× — the log-scale midpoint of
/// linear's ~4× and quadratic's ~16× — in COUNTED work units, so no scheduler, cache or neighbour
/// load can move the verdict (Law 8).
fn scales_linearly(small: DrainWork, big: DrainWork) -> bool {
    big.total() < small.total() * 8
}

/// The drain as it was before the index-tracking rewrite: one front `drain` PER FRAME, each
/// memmoving the whole remaining tail.
fn per_frame_drain(buf: &mut Vec<u8>) -> DrainWork {
    let mut work = DrainWork::default();
    while buf.len() >= PRELUDE_LEN {
        let total_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if buf.len() < total_len {
            break;
        }
        work.frames += 1;
        work.scanned += total_len;
        consume_front(buf, total_len, &mut work);
    }
    work
}

/// Complexity regression: `drain_frames_checked` used `Vec::drain(..total_len)` PER FRAME, which
/// memmoves the entire remaining tail on every call — O(frames × bytes) for a buffer of many small
/// frames.
#[test]
fn drain_frames_checked_scales_linearly_in_frame_count() {
    let run = |n: usize| {
        let mut buf = frames_buffer(n);
        let bytes = buf.len();
        let mut work = DrainWork::default();
        let (frames, status, consumed) = drain_frames_counted(&mut buf, None, &mut work);
        assert_eq!(status, DrainStatus::Ok);
        assert_eq!(frames.len(), n);
        assert_eq!(consumed, bytes);
        assert!(buf.is_empty());
        assert_eq!(work.frames, n);
        assert_eq!(work.scanned, bytes, "every byte is walked once");
        assert_eq!(work.edits, 1, "one buffer edit for the whole pass");
        assert_eq!(work.shifted, 0, "consuming whole frames moves nothing");
        work
    };
    let (small, big) = (run(1_000), run(4_000));
    assert!(
        scales_linearly(small, big),
        "drain_frames_checked did {} work units for 1k frames and {} for 4k (ratio {:.1}x; linear \
         predicts ~4x, quadratic ~16x)",
        small.total(),
        big.total(),
        big.total() as f64 / small.total() as f64
    );

    let mut buf = frames_buffer(1_000);
    buf.extend_from_slice(&frames_buffer(1)[..PRELUDE_LEN + 2]);
    let mut work = DrainWork::default();
    let (_, status, _) = drain_frames_counted(&mut buf, None, &mut work);
    assert_eq!(status, DrainStatus::Ok);
    assert_eq!(work.edits, 1);
    assert_eq!(work.shifted, PRELUDE_LEN + 2, "only the partial tail moves");
}

#[test]
fn the_linearity_check_rejects_a_per_frame_drain() {
    let small = per_frame_drain(&mut frames_buffer(1_000));
    let big = per_frame_drain(&mut frames_buffer(4_000));
    assert_eq!((small.edits, big.edits), (1_000, 4_000));
    assert!(
        !scales_linearly(small, big),
        "a per-frame drain must fail the check: {} vs {} work units",
        small.total(),
        big.total()
    );
}
