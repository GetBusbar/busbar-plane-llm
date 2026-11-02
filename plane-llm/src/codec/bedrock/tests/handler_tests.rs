// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `crates/busbar/src/handlers/bedrock.rs`.

use super::*;

#[test]
fn invoke_rerank_not_misclassified_by_inputtext_substring() {
    let h = BedrockRequestHandler;
    let body = br#"{"query":"how does inputText work?","documents":["textToImageParams too"]}"#;
    assert_eq!(
        h.resolve_operation("/model/cohere.rerank-v3-5:0/invoke", body),
        Some(OpVerb::RERANK),
    );

    assert_eq!(
        h.resolve_operation(
            "/model/amazon.titan-embed-text-v2:0/invoke",
            br#"{"inputText":"hello"}"#,
        ),
        Some(OpVerb::EMBEDDINGS),
    );

    assert_eq!(
        h.resolve_operation(
            "/model/amazon.titan-image-generator-v1/invoke",
            br#"{"textToImageParams":{"text":"a cat"}}"#,
        ),
        Some(OpVerb::IMAGE),
    );

    assert_eq!(
        h.resolve_operation("/model/anthropic.claude/converse", b"{}"),
        Some(OpVerb::CHAT),
    );
}

#[test]
fn image_read_request_captures_prompt_and_count() {
    let body =
        br#"{"textToImageParams":{"text":"a cat"},"imageGenerationConfig":{"numberOfImages":2}}"#;
    let ir =
        super::super::super::leaf_codec::image_read_request("bedrock", body, "application/json")
            .expect("valid titan image body");
    let r = ir;
    assert_eq!(r.prompt.as_deref(), Some("a cat"));
    assert_eq!(r.n, Some(2));
}

#[test]
fn image_response_bills_per_image() {
    let wire = br#"{"images":["AAAA","BBBB","CCCC"]}"#;
    let resp = super::read_image_response(wire).unwrap();
    match resp.billing() {
        Some(busbar_contract::billing::Billing::Images { count, .. }) => {
            assert_eq!(count, 3)
        }
        other => panic!("Titan per-image response must bill Images, got {other:?}"),
    }
}

#[test]
fn embeddings_read_request_captures_input_text() {
    let ir = super::super::super::leaf_codec::embeddings_read_request(
        "bedrock",
        br#"{"inputText":"hello"}"#,
        "application/json",
    )
    .expect("valid titan embeddings body");
    let r = ir;
    assert_eq!(r.input, EmbInput::Text(vec!["hello".to_string()]));
}

#[test]
fn embeddings_write_request_warns_on_dropped_non_text_input() {
    use crate::codec::ir::embeddings::EmbeddingsReq;
    use busbar_test_kit::warn_capture::WarnCapture;

    let req = EmbeddingsReq {
        input: EmbInput::Tokens(vec![vec![1, 2, 3]]),
        ..Default::default()
    };
    let cap = WarnCapture::default();
    let subscriber = cap.clone();
    let out = tracing::subscriber::with_default(subscriber, || {
        super::super::super::leaf_codec::embeddings_write_request("bedrock", &req)
    });

    let body: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(body["inputText"], json!(""));

    assert!(
        cap.contains("dropping a non-text embeddings input"),
        "a dropped non-text embeddings input must warn: {:?}",
        cap.messages()
    );
}

#[test]
fn embeddings_read_request_without_input_text_is_bad_request() {
    let err = super::super::super::leaf_codec::embeddings_read_request(
        "bedrock",
        br#"{"dimensions":256}"#,
        "application/json",
    )
    .expect_err("missing inputText must reject");
    assert!(matches!(err, IngressReject::BadRequest(_)));
}

#[test]
fn embeddings_write_read_roundtrip_preserves_input_text() {
    let req = crate::codec::ir::embeddings::EmbeddingsReq {
        input: EmbInput::Text(vec!["roundtrip".to_string()]),
        encoding_formats: vec![EncFmt::Float],
        ..Default::default()
    };
    let wire = super::super::super::leaf_codec::embeddings_write_request("bedrock", &req);
    let back = super::super::super::leaf_codec::embeddings_read_request(
        "bedrock",
        &wire,
        "application/json",
    )
    .expect("emitted body reparses");
    let r = back;
    assert_eq!(r.input, EmbInput::Text(vec!["roundtrip".to_string()]));
}

#[test]
fn bedrock_titan_image_drops_size_and_quality_as_1_5_5_did() {
    use busbar_test_kit::warn_capture::WarnCapture;
    let ir = crate::codec::ir::image::ImageReq {
        model: "amazon.titan-image-generator-v1".into(),
        prompt: Some("a cat".into()),
        size: Some(crate::codec::ir::image::ImageSize::Wh {
            width: 1024,
            height: 768,
        }),
        quality: Some("hd".into()),
        ..Default::default()
    };
    let cap = WarnCapture::default();
    let out = tracing::subscriber::with_default(cap.clone(), || {
        super::super::super::leaf_codec::image_write_request("bedrock", &ir)
    });
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v,
        json!({
            "taskType": "TEXT_IMAGE",
            "textToImageParams": {"text": "a cat"},
            "imageGenerationConfig": {"numberOfImages": 1}
        }),
        "exactly v1.5.5's Titan body: {v}"
    );
    assert!(
        cap.contains("dropping `size`") && cap.contains("dropping `quality`"),
        "each drop is warned: {:?}",
        cap.messages()
    );
}

#[test]
fn converse_resolves_to_chat_unconditionally() {
    let h = BedrockRequestHandler;
    for body in [
        b"".as_slice(),
        b"{}",
        b"garbage-not-json",
        br#"{"query":"x","documents":["y"]}"#,
    ] {
        assert_eq!(
            h.resolve_operation("/model/anthropic.claude-3/converse", body),
            Some(OpVerb::CHAT),
            "converse must resolve to CHAT regardless of body"
        );
        assert_eq!(
            h.resolve_operation("/model/anthropic.claude-3/converse-stream", body),
            Some(OpVerb::CHAT),
            "converse-stream must resolve to CHAT regardless of body"
        );
    }
}
