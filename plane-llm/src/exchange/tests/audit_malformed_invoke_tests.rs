// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::{chunk_event, Transcoder};

fn chunk(event: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "bytes": busbar_contract::media::base64_encode(event)
    }))
    .unwrap()
}

#[test]
fn malformed_single_line_invoke_chunk_ends_with_error() {
    let frame = crate::codec::eventstream::encode_frame("chunk", &chunk(b"{broken"));
    let mut transcoder = Transcoder::new();
    let output = String::from_utf8(transcoder.feed(&frame)).unwrap();
    assert!(output.starts_with("event: error\n"), "{output}");
    assert!(output.contains("malformed InvokeModel stream chunk"));
    assert!(!output.contains("{broken"));
    let stop =
        crate::codec::eventstream::encode_frame("chunk", &chunk(br#"{"type":"message_stop"}"#));
    assert!(transcoder.feed(&stop).is_empty());
}

#[test]
fn invoke_cr_json_is_compacted_and_valid_single_line_bytes_are_preserved() {
    let multiline = b"{\r\"type\": \"message_stop\"\r}";
    let event = chunk_event(&chunk(multiline)).unwrap();
    assert!(!event.contains(&b'\r'));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&event).unwrap()["type"],
        "message_stop"
    );
    let single = br#"{ "type" : "message_stop" }"#;
    assert_eq!(chunk_event(&chunk(single)).unwrap(), single);
}
