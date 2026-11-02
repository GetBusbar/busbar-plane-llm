// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `crates/busbar/src/proto/stream.rs`.

use super::rewrite_frame_strip_usage;

/// Byte-stripper fallback indistinguishability: when the fast byte-splice path declines and
#[test]
fn fallback_preserves_key_order_and_crlf() {
    let payload = r#"{"id":"chatcmpl-x","object":"chat.completion.chunk","choices":[{"delta":{"content":"hi"}}],"usage":null}"#;

    let raw_frame = b"data: <multiline-join-not-verbatim>\r\n\r\n";
    let mut out = Vec::new();
    rewrite_frame_strip_usage(&mut out, raw_frame, payload);
    let out_str = std::str::from_utf8(&out).unwrap();

    assert!(
        out_str.ends_with("\r\n\r\n"),
        "CRLF terminator must be preserved, got {out_str:?}"
    );

    assert!(
        !out_str.contains("usage"),
        "usage must be stripped: {out_str:?}"
    );

    let id_at = out_str.find("\"id\"").expect("id present");
    let object_at = out_str.find("\"object\"").expect("object present");
    let choices_at = out_str.find("\"choices\"").expect("choices present");
    assert!(
        id_at < object_at && object_at < choices_at,
        "original key order (id, object, choices) must be preserved, got {out_str:?}"
    );
}

/// The fallback keeps an LF-only terminator as LF (it must not upgrade LF to CRLF either).
#[test]
fn fallback_preserves_lf_terminator() {
    let payload = r#"{"id":"x","object":"chat.completion.chunk","usage":null,"choices":[]}"#;
    let raw_frame = b"data: <not-verbatim>\n\n";
    let mut out = Vec::new();
    rewrite_frame_strip_usage(&mut out, raw_frame, payload);
    let out_str = std::str::from_utf8(&out).unwrap();
    assert!(
        out_str.ends_with("\n\n"),
        "LF terminator preserved: {out_str:?}"
    );
    assert!(!out_str.contains("\r"), "no CR introduced: {out_str:?}");
    assert!(!out_str.contains("usage"), "usage stripped: {out_str:?}");
}
