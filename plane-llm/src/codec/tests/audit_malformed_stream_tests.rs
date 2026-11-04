// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::StreamTranslate;
use crate::codec::test_host::HOST;

#[test]
fn malformed_sse_aborts_before_later_content_and_success() {
    // Exercise both borrowed input and a malformed frame split across feeds.
    for split in [false, true] {
        let mut stream = StreamTranslate::new("anthropic", "openai").unwrap();
        let opening = stream.feed(HOST, br#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"visible prefix"},"finish_reason":null}]}

"#);
        assert!(String::from_utf8(opening)
            .unwrap()
            .contains("visible prefix"));
        if split {
            assert!(stream.feed(HOST, b"data: {broken").is_empty());
        }
        let mut input = if split {
            b"\n\n".to_vec()
        } else {
            b"data: {broken\n\n".to_vec()
        };
        input.extend_from_slice(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"must not appear\"},\"finish_reason\":\"stop\"}]}\n\n");
        assert!(stream.feed(HOST, &input).is_empty());
        assert!(stream.aborted());
        assert!(stream.buf.is_empty());
        assert!(stream.feed(HOST, b"data: {}\n\n").is_empty());
        let tail = String::from_utf8(stream.finish(HOST)).unwrap();
        assert!(tail.contains("error"), "{tail}");
        assert!(!tail.contains("message_stop"), "{tail}");
        assert!(!tail.contains("must not appear"), "{tail}");
    }
}

#[test]
fn malformed_eventstream_json_aborts_before_later_success() {
    let mut stream = StreamTranslate::new("anthropic", "bedrock").unwrap();
    let mut bytes = crate::codec::eventstream::encode_frame("contentBlockDelta", b"{broken");
    bytes.extend_from_slice(&crate::codec::eventstream::encode_frame(
        "messageStop",
        br#"{"stopReason":"end_turn"}"#,
    ));
    assert!(stream.feed(HOST, &bytes).is_empty());
    assert!(stream.aborted());
    let tail = String::from_utf8(stream.finish(HOST)).unwrap();
    assert!(tail.contains("error"), "{tail}");
    assert!(!tail.contains("message_stop"), "{tail}");
}

#[test]
fn comments_and_empty_sse_data_are_not_malformed_json() {
    let mut stream = StreamTranslate::new("anthropic", "openai").unwrap();
    assert!(stream
        .feed(HOST, b": keepalive\n\ndata: \n\ndata: [DONE]\n\n")
        .is_empty());
    assert!(!stream.aborted());
}
