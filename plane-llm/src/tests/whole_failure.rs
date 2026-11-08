// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Buffered upstream failures preserve their classification and reported work.

use crate::common;

use crate::exchange::reply::whole::{translate, WholeCtx, WholeEnd};
use busbar_contract::{billing::Billing, operation::OpVerb};
use common::SevensHost;
use serde_json::{json, Value};

fn context(egress: &str, wants_stream: bool) -> WholeCtx<'_> {
    WholeCtx {
        ingress: "openai",
        egress,
        operation: OpVerb::CHAT,
        model: "m",
        wants_stream,
        json_array: false,
        request: None,
        now_s: 1,
        elapsed_ms: None,
        translate_cap: 1024 * 1024,
    }
}

#[test]
fn failed_responses_return_502_and_keep_normalized_reported_tokens() {
    for wants_stream in [false, true] {
        for error in [
            Value::Null,
            json!({"code":"server_error"}),
            json!({"type":"rate_limit_exceeded"}),
        ] {
            let body = json!({"status":"failed", "error":error, "usage":{
                "input_tokens":17,"output_tokens":3,"input_tokens_details":{"cached_tokens":5}
            }});
            let answer = translate(
                &SevensHost,
                &context("responses", wants_stream),
                200,
                &serde_json::to_vec(&body).unwrap(),
            );
            assert_eq!(answer.end, WholeEnd::FailedGeneration);
            assert_eq!(answer.answer.status, 502);
            let Some(Billing::Tokens(usage)) = answer.usage else {
                panic!("reported tokens must survive");
            };
            assert_eq!(usage.input, 12);
            assert_eq!(usage.output, 3);
            assert!(!answer.synthesized);
            assert!(answer.dropped.is_empty());
        }
    }
}

#[test]
fn failed_response_without_reported_usage_does_not_invent_tokens() {
    let answer = translate(
        &SevensHost,
        &context("responses", false),
        200,
        br#"{"status":"failed","error":{"code":"server_error"}}"#,
    );
    assert_eq!(answer.end, WholeEnd::FailedGeneration);
    assert!(answer.usage.is_none());
}

#[test]
fn malformed_completed_response_remains_a_local_translation_failure() {
    let answer = translate(
        &SevensHost,
        &context("responses", false),
        200,
        br#"{"status":"completed","output":null}"#,
    );
    assert_eq!(answer.end, WholeEnd::NotTranslatable);
    assert_eq!(answer.answer.status, 500);
    assert!(answer.refused.is_some());
}

#[test]
fn completed_response_still_delivers_text_and_usage_as_json_or_stream() {
    let body = br#"{"id":"resp_1","status":"completed","model":"m","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":7,"output_tokens":2}}"#;
    for wants_stream in [false, true] {
        let answer = translate(&SevensHost, &context("responses", wants_stream), 200, body);
        assert_eq!(answer.end, WholeEnd::Delivered);
        assert_eq!(answer.answer.status, 200);
        assert_eq!(answer.synthesized, wants_stream);
        assert!(String::from_utf8(answer.answer.body)
            .unwrap()
            .contains("hello"));
        let Some(Billing::Tokens(usage)) = answer.usage else {
            panic!("expected usage");
        };
        assert_eq!(usage.input, 7);
        assert_eq!(usage.output, 2);
    }
}
