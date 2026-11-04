// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! IR MAPPING — the Bedrock reasoning ask (dropped both ways, as in 1.5.5)
//! and structured output (BED-08).

use super::*;
use serde_json::json;

#[test]
fn a_native_reasoning_ask_is_not_promoted_to_the_ir() {
    let body = json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "inferenceConfig": {"maxTokens": 20000},
        "additionalModelRequestFields": {"thinking": {"type": "enabled", "budget_tokens": 4096}}
    });
    let ir = BedrockReader.read_request(&body).expect("read");
    assert_eq!(ir.reasoning, None);
    let nova = json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "additionalModelRequestFields": {"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "high"}}
    });
    let ir = BedrockReader.read_request(&nova).expect("read");
    assert_eq!(ir.reasoning, None);

    let back = writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        back["additionalModelRequestFields"],
        json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "high"}}),
        "{back}"
    );
}

#[test]
fn a_foreign_reasoning_ask_is_dropped_on_bedrock_egress() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi")],
        }],
        max_tokens: Some(8192),
        temperature: Some(0.5),
        top_p: Some(0.9),
        top_k: Some(40),
        reasoning: Some(crate::codec::ir::IrReasoningAsk::Effort(
            crate::codec::ir::IrReasoningEffort::High,
        )),
        ..Default::default()
    };
    let out = writer().write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        out.pointer("/additionalModelRequestFields/thinking")
            .is_none(),
        "{out}"
    );
    assert_eq!(out["inferenceConfig"]["temperature"], 0.5, "{out}");
    assert_eq!(out["inferenceConfig"]["topP"], 0.9, "{out}");
    assert_eq!(out["additionalModelRequestFields"]["top_k"], 40, "{out}");
}

/// BED-08: Converse's `outputConfig.textFormat` and `toolSpec.strict` are read into the IR and
/// written back, so a JSON-schema directive and per-tool strict cross in both directions.
#[test]
fn bed08_output_config_and_tool_strict_map_both_ways() {
    let body = json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "outputConfig": {"textFormat": {"type": "json_schema", "structure": {"jsonSchema": {
            "schema": "{\"type\":\"object\"}", "name": "extract", "description": "d"}}}},
        "toolConfig": {"tools": [{"toolSpec": {"name": "f", "strict": true,
            "inputSchema": {"json": {"type": "object"}}}}]}
    });
    let ir = BedrockReader.read_request(&body).expect("read");
    let rf = ir.response_format.clone().expect("response_format");
    assert!(rf.json);
    assert_eq!(rf.schema, Some(json!({"type": "object"})));
    assert_eq!(rf.name.as_deref(), Some("extract"));
    assert_eq!(rf.description.as_deref(), Some("d"));
    assert_eq!(ir.tools[0].strict, Some(true));

    let back = writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(back["outputConfig"], body["outputConfig"], "{back}");
    assert_eq!(
        back["toolConfig"]["tools"][0]["toolSpec"]["strict"], true,
        "{back}"
    );

    let mut cross = ir.clone();
    cross.extra.clear();
    let native = LaneCaps {
        native_structured_output: true,
        ..Default::default()
    };
    let out = writer().write_request_for_lane(
        crate::codec::test_host::HOST,
        &cross,
        "anthropic.claude-opus-4-7",
        &native,
    );
    assert_eq!(
        out["outputConfig"]["textFormat"]["type"], "json_schema",
        "{out}"
    );
    assert_eq!(
        out["outputConfig"]["textFormat"]["structure"]["jsonSchema"]["name"], "extract",
        "{out}"
    );
    assert!(writer()
        .dropped_egress_controls_for_lane(&cross, &native)
        .is_empty());

    let out = writer().write_request(crate::codec::test_host::HOST, &cross);
    assert!(out.get("outputConfig").is_none(), "{out}");
    assert_eq!(
        out["toolConfig"]["tools"][0]["toolSpec"]["strict"], true,
        "{out}"
    );
    assert_eq!(
        writer().dropped_egress_controls(&cross),
        vec!["response_format"]
    );
}

/// A fresh writer per use (the const constructor carries per-stream state).
fn writer() -> BedrockWriter {
    BedrockWriter
}
