// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;

#[test]
fn multipart_model_is_stamped_and_matches_the_bytes_path() {
    use crate::codec::framework::{MediaBlob, MediaPayload};
    let request = TranscriptionReq {
        model: "original".into(),
        audio: Some(MediaBlob {
            payload: MediaPayload::Bytes(SlabBytes::from(&b"audio"[..])),
            mime_type: "audio/wav".into(),
            pcm: None,
        }),
        ..Default::default()
    };
    let mut value_handle = TranscriptionReqHandle(request.clone());
    let mut bytes_handle = TranscriptionReqHandle(request);
    let expected = bytes_handle.write_egress_request_bytes("openai", "routed");
    match value_handle.write_egress_request(crate::codec::test_host::HOST, "openai", "routed") {
        EgressWire::Bytes(actual) => assert_eq!(actual.as_ref(), expected.as_ref()),
        _ => panic!("expected multipart bytes"),
    }
}

#[test]
fn json_string_serialization_preserves_unicode_and_escaping() {
    let value = serde_json::json!({"text": "é\n\"\\😀".repeat(8192)});
    let string = crate::codec::json::to_string(&value).unwrap();
    assert_eq!(
        string.as_bytes(),
        crate::codec::json::to_vec(&value).unwrap()
    );
    assert_eq!(serde_json::from_str::<Value>(&string).unwrap(), value);
}
