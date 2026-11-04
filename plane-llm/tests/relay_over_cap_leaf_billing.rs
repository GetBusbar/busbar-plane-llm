// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

mod common;

use busbar_contract::operation::OpVerb;
use busbar_plane_llm::exchange::reply::relay::{End, Relay, RelayCtx};

fn delivered(protocol: &str, operation: OpVerb, body: &[u8], cap: usize, chunk: usize) -> End {
    let handler = busbar_plane_llm::codec::DECLS
        .iter()
        .find(|d| d.name == protocol)
        .unwrap()
        .handler
        .unwrap()
        .operation_handler(operation)
        .unwrap();
    let mut relay = Relay::new_for_operation(
        RelayCtx {
            ingress: protocol,
            egress: protocol,
            far_is_stream: false,
            json_array: false,
            client_include_usage: false,
            request: None,
            handler,
            meter: true,
            translate_cap: cap,
        },
        operation,
    );
    for piece in body.chunks(chunk) {
        let _ = relay.feed(&common::TestHost, piece);
    }
    relay.end(&common::TestHost)
}

#[test]
fn capped_transcription_recovers_reported_audio_duration_and_tokens() {
    for (usage, audio_ms, input) in [
        (r#"{"type":"duration","seconds":1.25}"#, Some(1250), None),
        (
            r#"{"type":"tokens","input_tokens":5,"output_tokens":2}"#,
            None,
            Some(5),
        ),
    ] {
        let body = format!(r#"{{"text":"{}","usage":{usage}}}"#, "x".repeat(2000));
        for chunk in [1, 7, 128, body.len()] {
            let end = delivered("openai", OpVerb::TRANSCRIPTION, body.as_bytes(), 128, chunk);
            assert_eq!(end.open_units.get("audio_ms").copied(), audio_ms);
            assert_eq!(end.usage.map(|u| u.input), input);
        }
    }
}

#[test]
fn capped_image_counts_match_uncapped_without_retaining_payloads() {
    let payload = "x".repeat(2000);
    for (protocol, body) in [
        (
            "openai",
            format!(
                r#"{{"data":[{{"b64_json":"{payload}"}},{{"url":"https://example.test/image"}}]}}"#
            ),
        ),
        (
            "gemini",
            format!(
                r#"{{"predictions":[{{"bytesBase64Encoded":"{payload}"}},{{"bytesBase64Encoded":"{payload}"}}]}}"#
            ),
        ),
        (
            "bedrock",
            format!(r#"{{"images":["{payload}","{payload}"]}}"#),
        ),
    ] {
        let baseline = delivered(protocol, OpVerb::IMAGE, body.as_bytes(), body.len() + 1, 7);
        assert_eq!(baseline.open_units.get("images"), Some(&2));
        for chunk in [1, 7, 128, body.len()] {
            let end = delivered(protocol, OpVerb::IMAGE, body.as_bytes(), 128, chunk);
            assert_eq!(
                end.open_units, baseline.open_units,
                "{protocol}, chunk={chunk}"
            );
            assert_eq!(end.usage, baseline.usage);
        }
    }
}

#[test]
fn image_usage_takes_priority_over_array_counts_even_when_oversized() {
    for (protocol, array, usage_key, usage) in [
        (
            "openai",
            "data",
            "usage",
            r#"{"input_tokens":5,"output_tokens":2}"#,
        ),
        (
            "gemini",
            "predictions",
            "usageMetadata",
            r#"{"promptTokenCount":5,"candidatesTokenCount":2}"#,
        ),
    ] {
        for usage in [
            usage.to_string(),
            format!(r#"{{"padding":"{}"}}"#, "x".repeat(1000)),
            "null".into(),
        ] {
            let body = format!(
                r#"{{"{array}":[{{"padding":"{}"}}],"{usage_key}":{usage}}}"#,
                "x".repeat(2000)
            );
            for chunk in [1, 17, body.len()] {
                let end = delivered(protocol, OpVerb::IMAGE, body.as_bytes(), 128, chunk);
                assert!(end.open_units.get("images").is_none());
                if usage.contains("TokenCount") || usage.contains("input_tokens") {
                    let tokens = end
                        .usage
                        .expect("reported image counts retained as residuals");
                    assert_eq!((tokens.input, tokens.output), (0, 0));
                    assert_eq!(tokens.residual_units.get("image.input_tokens"), Some(&5));
                }
            }
        }
    }
}

#[test]
fn image_count_ignores_spoofs_and_obeys_last_root_member() {
    let padding = "x".repeat(1000);
    for (body, expected) in [
        (
            format!(r#"{{"images":["{padding}"],"images":["a","b"]}}"#),
            Some(2),
        ),
        (format!(r#"{{"images":["{padding}"],"images":null}}"#), None),
        (format!(r#"{{"images":["{padding}",42]}}"#), None),
        (format!(r#"{{"nested":{{"images":["{padding}"]}}}}"#), None),
        (format!(r#"{{"images":["{padding}"],"bad":[1,,2]}}"#), None),
    ] {
        for chunk in [1, 17, body.len()] {
            let end = delivered("bedrock", OpVerb::IMAGE, body.as_bytes(), 128, chunk);
            assert_eq!(end.open_units.get("images").copied(), expected);
        }
    }
}

#[test]
fn image_readers_refuse_missing_payload_but_recover_reported_residual_tokens() {
    for (protocol, array, usage) in [
        (
            "openai",
            "data",
            r#""usage":{"input_tokens":5,"output_tokens":2}"#,
        ),
        (
            "gemini",
            "predictions",
            r#""usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2}"#,
        ),
    ] {
        let handler = busbar_plane_llm::codec::DECLS
            .iter()
            .find(|d| d.name == protocol)
            .unwrap()
            .handler
            .unwrap()
            .operation_handler(OpVerb::IMAGE)
            .unwrap();
        for shape in [
            None,
            Some("null"),
            Some("[null]"),
            Some("[{}]"),
            Some("[42]"),
        ] {
            let member = shape
                .map(|shape| format!(r#""{array}":{shape},"#))
                .unwrap_or_default();
            let body = format!(r#"{{{member}{usage},"padding":"{}"}}"#, "x".repeat(1000));
            assert!(handler
                .read_response(&common::TestHost, body.as_bytes())
                .is_err());
            let recovered = handler.recover_response_billing(&common::TestHost, body.as_bytes());
            let Some(busbar_contract::billing::Billing::Tokens(tokens)) = recovered else {
                panic!("independent image usage must survive malformed payload");
            };
            assert_eq!((tokens.input, tokens.output), (0, 0));
            assert_eq!(tokens.residual_units.get("image.input_tokens"), Some(&5));
            for cap in [128, body.len() + 1] {
                let end = delivered(protocol, OpVerb::IMAGE, body.as_bytes(), cap, 7);
                assert_eq!(end.usage.as_ref(), Some(&tokens));
                assert!(end.open_units.is_empty());
            }
        }
        let empty = format!(r#"{{"{array}":[]}}"#);
        assert!(handler
            .read_response(&common::TestHost, empty.as_bytes())
            .is_ok());
    }
}

#[test]
fn imagen_storage_and_filtered_predictions_preserve_only_delivered_image_count() {
    let body = format!(
        r#"{{"predictions":[{{"gcsUri":"gs://bucket/image"}},{{"raiFilteredReason":"filtered","padding":"{}"}}]}}"#,
        "x".repeat(1000)
    );
    for cap in [128, body.len() + 1] {
        for chunk in [1, 17, body.len()] {
            let end = delivered("gemini", OpVerb::IMAGE, body.as_bytes(), cap, chunk);
            assert_eq!(end.open_units.get("images"), Some(&1));
        }
    }
}

#[test]
fn capped_image_item_payload_requires_direct_last_member_string() {
    let padding = "x".repeat(1000);
    for (protocol, array, items, expected) in [
        (
            "openai",
            "data",
            r#"{"b64_json":"yes","b64_json":null}"#,
            None,
        ),
        (
            "openai",
            "data",
            r#"{"b64_json":null,"url":"https://example.test/image"}"#,
            Some(1),
        ),
        (
            "openai",
            "data",
            r#"{"nested":{"url":"https://example.test/image"}}"#,
            None,
        ),
        (
            "gemini",
            "predictions",
            r#"{"bytesBase64Encoded":"yes","bytesBase64Encoded":null}"#,
            None,
        ),
        (
            "gemini",
            "predictions",
            r#"{"raiFilteredReason":"filtered","raiFilteredReason":null}"#,
            None,
        ),
        (
            "gemini",
            "predictions",
            r#"{"gcsUri":"gs://bucket/image"},{"raiFilteredReason":"filtered"}"#,
            Some(1),
        ),
    ] {
        let body = format!(r#"{{"{array}":[{items}],"padding":"{padding}"}}"#);
        for chunk in [1, 17, body.len()] {
            let end = delivered(protocol, OpVerb::IMAGE, body.as_bytes(), 128, chunk);
            assert_eq!(
                end.open_units.get("images").copied(),
                expected,
                "{protocol}: {items}"
            );
        }
    }
}
