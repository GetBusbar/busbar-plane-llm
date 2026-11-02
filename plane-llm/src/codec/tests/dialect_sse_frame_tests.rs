//! The SSE line-terminator grammar the dialect probes and parser walk: the contract's frame

use super::*;
use busbar_contract::protocol::find_frame_terminator;

/// The event-stream grammar names three line terminators — CRLF, a lone LF, a lone CR — and a
#[test]
fn every_spec_line_terminator_pairing_ends_a_frame() {
    assert_eq!(find_frame_terminator(b"data: a\r\rrest"), Some((7, 2)));
    assert_eq!(find_frame_terminator(b"data: a\n\rrest"), Some((7, 2)));
    assert_eq!(find_frame_terminator(b"data: a\r\n\rrest"), Some((7, 3)));
    assert_eq!(find_frame_terminator(b"data: a\n\r\nrest"), Some((7, 3)));
    assert_eq!(find_frame_terminator(b"data: a\r\n\n"), Some((7, 3)));
    assert_eq!(find_frame_terminator(b"data: a\n\r\n"), Some((7, 3)));

    assert_eq!(find_frame_terminator(b"data: a\r\nrest"), None);

    assert_eq!(find_frame_terminator(b"a\r\nb\r\n\r\nc"), Some((4, 4)));

    assert_eq!(find_frame_terminator(b"data: a\r"), None);

    assert_eq!(find_frame_terminator(b"data: a\n\nrest"), Some((7, 2)));
    assert_eq!(find_frame_terminator(b"data: a\r\n\r\nrest"), Some((7, 4)));
    assert_eq!(find_frame_terminator(b"data: a"), None);
}

/// `parse_sse_frame` used `str::lines()`, which does not split on a bare CR — a bare-CR frame
#[test]
fn parse_sse_frame_splits_on_bare_cr() {
    assert_eq!(
        parse_sse_frame(b"event: message\rdata: {\"a\":1}"),
        Some(("message", "{\"a\":1}".into()))
    );
    assert_eq!(
        parse_sse_frame(b"data: line1\rdata: line2"),
        Some(("", "line1\nline2".into()))
    );

    assert_eq!(
        parse_sse_frame(b"event: message\r\ndata: {\"a\":1}\r\n"),
        Some(("message", "{\"a\":1}".into()))
    );
    assert_eq!(
        parse_sse_frame(b"data: line1\ndata: line2"),
        Some(("", "line1\nline2".into()))
    );
}

/// The cheap `event:`-name probe split on LF alone, so a bare-CR frame came back as the WHOLE
#[test]
fn sse_event_type_splits_on_bare_cr() {
    assert_eq!(
        sse_event_type(b"event: message_delta\rdata: {\"x\":1}\r\r"),
        "message_delta"
    );
    assert_eq!(
        sse_event_type(b"event: message_start\rdata: {}"),
        "message_start"
    );

    assert_eq!(sse_event_type(b"event: a\revent: b\rdata: {}"), "b");

    assert_eq!(
        sse_event_type(b"event: message_delta\ndata: {}\n\n"),
        "message_delta"
    );
    assert_eq!(
        sse_event_type(b"event: message_delta\r\ndata: {}\r\n\r\n"),
        "message_delta"
    );
    assert_eq!(sse_event_type(b"data: {}\n\n"), "");
    assert_eq!(sse_event_type(b""), "");
}
