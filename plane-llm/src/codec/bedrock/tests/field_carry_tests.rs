// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! FIELD-COVERAGE CARRY instruments for the AWS Bedrock Converse dialect (`bedrock/` field ids).

use super::*;

/// `bedrock/request/inferenceConfig.stopSequences` — Bedrock nests stop sequences under
#[test]
fn bedrock_carry_request_stop_sequences() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "inferenceConfig": {"maxTokens": 50, "stopSequences": ["STOP", "END"]}
    });
    let ir = reader.read_request(&body).expect("read");

    assert_eq!(
        ir.stop,
        vec!["STOP".to_string(), "END".to_string()],
        "stopSequences must reach IrRequest.stop"
    );
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/inferenceConfig/stopSequences"),
        Some(&serde_json::json!(["STOP", "END"])),
        "stopSequences must re-emit under inferenceConfig; got {out}"
    );
}

/// `bedrock/request/toolConfig.tools` and `bedrock/request/toolConfig.toolChoice` — the tool
#[test]
fn bedrock_carry_request_tool_config() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "weather?"}]}],
        "toolConfig": {
            "tools": [{"toolSpec": {
                "name": "get_weather",
                "description": "Get weather",
                "inputSchema": {"json": {"type": "object"}}
            }}],
            "toolChoice": {"any": {}}
        }
    });
    let ir = reader.read_request(&body).expect("read");

    assert_eq!(
        ir.tools.len(),
        1,
        "toolConfig.tools must reach IrRequest.tools"
    );
    assert_eq!(ir.tools[0].name, "get_weather");

    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Required),
        "toolConfig.toolChoice `{{any:{{}}}}` must reach IrRequest.tool_choice as Required"
    );

    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/toolConfig/tools/0/toolSpec/name")
            .and_then(|v| v.as_str()),
        Some("get_weather"),
        "toolConfig.tools must re-emit; got {out}"
    );
    assert!(
        out.pointer("/toolConfig/toolChoice/any").is_some(),
        "toolConfig.toolChoice must re-emit as {{any:{{}}}}; got {out}"
    );

    if let Some(openai) = super::super::proto_codec::protocol_for("openai") {
        let oai = openai
            .writer()
            .write_request(crate::codec::test_host::HOST, &ir);
        assert_eq!(
            oai.pointer("/tool_choice").and_then(|v| v.as_str()),
            Some("required"),
            "Bedrock `{{any:{{}}}}` must cross-project to OpenAI `required`; got {oai}"
        );
    }
}

/// `bedrock/request/content[].toolUse.{toolUseId,name,input}` — an assistant turn's tool call must
#[test]
fn bedrock_carry_request_tool_use() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "assistant", "content": [
            {"toolUse": {"toolUseId": "tu_9", "name": "lookup", "input": {"q": "x"}}}
        ]}]
    });
    let ir = reader.read_request(&body).expect("read");
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::ToolUse {
            id, name, input, ..
        } => {
            assert_eq!(
                id, "tu_9",
                "toolUse.toolUseId must reach IrBlock::ToolUse.id"
            );
            assert_eq!(
                name, "lookup",
                "toolUse.name must reach IrBlock::ToolUse.name"
            );
            assert_eq!(
                input,
                &serde_json::json!({"q": "x"}),
                "toolUse.input must reach IrBlock::ToolUse.input"
            );
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/messages/0/content/0/toolUse/toolUseId")
            .and_then(|v| v.as_str()),
        Some("tu_9"),
        "toolUse.toolUseId must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/messages/0/content/0/toolUse/name")
            .and_then(|v| v.as_str()),
        Some("lookup"),
        "toolUse.name must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/messages/0/content/0/toolUse/input"),
        Some(&serde_json::json!({"q": "x"})),
        "toolUse.input must re-emit; got {out}"
    );
}

/// `bedrock/request/content[].toolResult.{toolUseId,content,status}` — a tool result must reach
#[test]
fn bedrock_carry_request_tool_result() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"toolResult": {
                "toolUseId": "tu_9",
                "content": [{"text": "boom"}],
                "status": "error"
            }}
        ]}]
    });
    let ir = reader.read_request(&body).expect("read");
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            assert_eq!(
                tool_use_id, "tu_9",
                "toolResult.toolUseId must reach IrBlock::ToolResult.tool_use_id"
            );
            assert!(
                matches!(&content[0], crate::codec::ir::IrBlock::Text { text, .. } if text == "boom"),
                "toolResult.content must reach IrBlock::ToolResult.content"
            );
            assert!(
                *is_error,
                "toolResult.status=error must reach IrBlock::ToolResult.is_error"
            );
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/messages/0/content/0/toolResult/toolUseId")
            .and_then(|v| v.as_str()),
        Some("tu_9"),
        "toolResult.toolUseId must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/messages/0/content/0/toolResult/content/0/text")
            .and_then(|v| v.as_str()),
        Some("boom"),
        "toolResult.content must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/messages/0/content/0/toolResult/status")
            .and_then(|v| v.as_str()),
        Some("error"),
        "toolResult.status must re-emit; got {out}"
    );
}

/// `bedrock/request/content[].reasoningContent.reasoningText.{text,signature}` and
#[test]
fn bedrock_carry_request_reasoning_content() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "assistant", "content": [
            {"reasoningContent": {"reasoningText": {"text": "let me think", "signature": "sig-abc"}}},
            {"reasoningContent": {"redactedContent": "ciphertext-bytes"}}
        ]}]
    });
    let ir = reader.read_request(&body).expect("read");
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::Thinking {
            text,
            signature,
            redacted,
            ..
        } => {
            assert_eq!(
                text, "let me think",
                "reasoningText.text must reach IrBlock::Thinking.text"
            );
            assert_eq!(
                signature.as_deref(),
                Some("sig-abc"),
                "reasoningText.signature must reach IrBlock::Thinking.signature"
            );
            assert!(!redacted, "a reasoningText block is not redacted");
        }
        other => panic!("expected Thinking, got {other:?}"),
    }
    match &ir.messages[0].content[1] {
        crate::codec::ir::IrBlock::Thinking { redacted, text, .. } => {
            assert!(
                *redacted,
                "redactedContent must reach IrBlock::Thinking with redacted=true"
            );
            assert_eq!(
                text, "ciphertext-bytes",
                "redacted bytes ride in Thinking.text"
            );
        }
        other => panic!("expected redacted Thinking, got {other:?}"),
    }
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/messages/0/content/0/reasoningContent/reasoningText/text")
            .and_then(|v| v.as_str()),
        Some("let me think"),
        "reasoningText.text must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/messages/0/content/0/reasoningContent/reasoningText/signature")
            .and_then(|v| v.as_str()),
        Some("sig-abc"),
        "reasoningText.signature must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/messages/0/content/1/reasoningContent/redactedContent")
            .and_then(|v| v.as_str()),
        Some("ciphertext-bytes"),
        "redactedContent must re-emit; got {out}"
    );
}

/// `bedrock/request/additionalModelRequestFields` — the model-specific escape hatch. Captured
#[test]
fn bedrock_carry_request_additional_model_request_fields() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "additionalModelRequestFields": {"custom_param": 42, "nested": {"a": true}}
    });
    let ir = reader.read_request(&body).expect("read");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/additionalModelRequestFields/custom_param")
            .and_then(|v| v.as_i64()),
        Some(42),
        "additionalModelRequestFields must round-trip verbatim; got {out}"
    );
    assert_eq!(
        out.pointer("/additionalModelRequestFields/nested/a")
            .and_then(|v| v.as_bool()),
        Some(true),
        "nested additionalModelRequestFields must round-trip; got {out}"
    );
}

/// Bedrock-specific request members with no cross-protocol analog, each captured into `extra` and
#[test]
fn bedrock_carry_request_verbatim_extras() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    for (key, value) in [
        (
            "additionalModelResponseFieldPaths",
            serde_json::json!(["/foo/bar", "/baz"]),
        ),
        (
            "promptVariables",
            serde_json::json!({"topic": {"text": "weather"}}),
        ),
        (
            "requestMetadata",
            serde_json::json!({"team": "search", "env": "prod"}),
        ),
        (
            "performanceConfig",
            serde_json::json!({"latency": "optimized"}),
        ),
    ] {
        let mut body = serde_json::json!({
            "messages": [{"role": "user", "content": [{"text": "hi"}]}]
        });
        body[key] = value.clone();
        let ir = reader.read_request(&body).expect("read");
        let out = writer.write_request(crate::codec::test_host::HOST, &ir);
        assert_eq!(
            out.get(key),
            Some(&value),
            "{key} must round-trip verbatim; got {out}"
        );
    }
}

/// `bedrock/request/content[].image.source.s3Location` — an S3-referenced image (no inline bytes).
#[test]
fn bedrock_carry_request_image_s3_location() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"image": {"format": "png", "source": {"s3Location": {"uri": "s3://b/i", "bucketOwner": "123456789012"}}}}
        ]}]
    });
    let ir = reader.read_request(&body).expect("read");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/messages/0/content/0/image/source/s3Location/uri")
            .and_then(|v| v.as_str()),
        Some("s3://b/i"),
        "image.source.s3Location.uri must round-trip; got {out}"
    );
    assert_eq!(
        out.pointer("/messages/0/content/0/image/source/s3Location/bucketOwner")
            .and_then(|v| v.as_str()),
        Some("123456789012"),
        "image.source.s3Location.bucketOwner must round-trip; got {out}"
    );
}

/// `bedrock/request/content[].document.source.s3Location`, `.document.citations`, `.document.context`
#[test]
fn bedrock_carry_request_document_provider_fields() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"text": "read this"},
            {"document": {
                "format": "pdf",
                "name": "spec",
                "source": {"s3Location": {"uri": "s3://b/spec.pdf"}},
                "citations": {"enabled": true},
                "context": "a quarterly memo"
            }}
        ]}]
    });
    let ir = reader.read_request(&body).expect("read");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let doc = out
        .pointer("/messages/0/content/1/document")
        .unwrap_or_else(|| panic!("document block must re-emit exactly once; got {out}"));
    assert_eq!(
        doc.pointer("/source/s3Location/uri")
            .and_then(|v| v.as_str()),
        Some("s3://b/spec.pdf"),
        "document.source.s3Location must round-trip; got {out}"
    );
    assert_eq!(
        doc.pointer("/citations/enabled").and_then(|v| v.as_bool()),
        Some(true),
        "document.citations must round-trip; got {out}"
    );
    assert_eq!(
        doc.pointer("/context").and_then(|v| v.as_str()),
        Some("a quarterly memo"),
        "document.context must round-trip; got {out}"
    );

    let docs = out
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter(|b| b.get("document").is_some()).count());
    assert_eq!(
        docs,
        Some(1),
        "document must be emitted exactly once; got {out}"
    );
}

/// `bedrock/request/content[].video.source.s3Location` — a Converse `video` block with an S3 source.
#[test]
fn bedrock_carry_request_video_s3_location() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"text": "watch"},
            {"video": {"format": "mp4", "source": {"s3Location": {"uri": "s3://b/clip.mp4"}}}}
        ]}]
    });
    let ir = reader.read_request(&body).expect("read");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/messages/0/content/1/video/source/s3Location/uri")
            .and_then(|v| v.as_str()),
        Some("s3://b/clip.mp4"),
        "video.source.s3Location.uri must round-trip; got {out}"
    );
}

/// `bedrock/request/content[].cachePoint.type` — an inline prompt-cache breakpoint. The native
#[test]
fn bedrock_carry_request_cache_point() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"text": "cache the prefix"},
            {"cachePoint": {"type": "default"}}
        ]}]
    });
    let ir = reader.read_request(&body).expect("read");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let content = out
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .expect("content array");
    let cp = content
        .iter()
        .find_map(|b| b.pointer("/cachePoint/type").and_then(|v| v.as_str()));
    assert_eq!(
        cp,
        Some("default"),
        "cachePoint.type must round-trip at its position; got {out}"
    );
}

/// `bedrock/request/content[].guardContent` — an inline Guardrails span. Bedrock-specific; captured
#[test]
fn bedrock_carry_request_guard_content() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"text": "hi"},
            {"guardContent": {"text": {"text": "screen me", "qualifiers": ["grounding_source"]}}}
        ]}]
    });
    let ir = reader.read_request(&body).expect("read");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let content = out
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .expect("content array");
    let gc = content.iter().find_map(|b| {
        b.pointer("/guardContent/text/text")
            .and_then(|v| v.as_str())
    });
    assert_eq!(
        gc,
        Some("screen me"),
        "guardContent must round-trip at its position; got {out}"
    );
}

/// `bedrock/response/output.message.role`, `bedrock/response/output.message.content` and
#[test]
fn bedrock_carry_response_output_and_stop_reason() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "output": {"message": {"role": "assistant", "content": [
            {"text": "on it"},
            {"toolUse": {"toolUseId": "t1", "name": "go", "input": {}}}
        ]}},
        "stopReason": "tool_use",
        "usage": {"inputTokens": 3, "outputTokens": 4, "totalTokens": 7}
    });
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");

    assert_eq!(
        resp.role,
        crate::codec::ir::IrRole::Assistant,
        "output.message.role must reach IrResponse.role"
    );

    assert_eq!(
        resp.content.len(),
        2,
        "output.message.content must reach IrResponse.content"
    );
    assert!(
        matches!(&resp.content[0], crate::codec::ir::IrBlock::Text { text, .. } if text == "on it"),
        "output.message.content[0] must reach IrResponse.content"
    );

    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::ToolUse),
        "stopReason `tool_use` must reach IrResponse.stop_reason"
    );

    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out.pointer("/output/message/role").and_then(|v| v.as_str()),
        Some("assistant"),
        "output.message.role must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/output/message/content/0/text")
            .and_then(|v| v.as_str()),
        Some("on it"),
        "output.message.content must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/stopReason").and_then(|v| v.as_str()),
        Some("tool_use"),
        "stopReason must re-emit; got {out}"
    );
}

/// `bedrock/response/usage.{inputTokens,outputTokens,totalTokens,cacheReadInputTokens,
#[test]
fn bedrock_carry_response_usage() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 100,
            "outputTokens": 25,
            "totalTokens": 175,
            "cacheReadInputTokens": 40,
            "cacheWriteInputTokens": 10
        }
    });
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");
    assert_eq!(
        resp.usage.input_tokens, 100,
        "usage.inputTokens must reach IrUsage"
    );
    assert_eq!(
        resp.usage.output_tokens, 25,
        "usage.outputTokens must reach IrUsage"
    );
    assert_eq!(
        resp.usage.cache_read_input_tokens,
        Some(40),
        "usage.cacheReadInputTokens must reach IrUsage"
    );
    assert_eq!(
        resp.usage.cache_creation_input_tokens,
        Some(10),
        "usage.cacheWriteInputTokens must reach IrUsage.cache_creation_input_tokens"
    );

    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out.pointer("/usage/inputTokens").and_then(|v| v.as_u64()),
        Some(100),
        "usage.inputTokens must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/usage/outputTokens").and_then(|v| v.as_u64()),
        Some(25),
        "usage.outputTokens must re-emit; got {out}"
    );

    assert_eq!(
        out.pointer("/usage/totalTokens").and_then(|v| v.as_u64()),
        Some(175),
        "usage.totalTokens must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/usage/cacheReadInputTokens")
            .and_then(|v| v.as_u64()),
        Some(40),
        "usage.cacheReadInputTokens must re-emit; got {out}"
    );
    assert_eq!(
        out.pointer("/usage/cacheWriteInputTokens")
            .and_then(|v| v.as_u64()),
        Some(10),
        "usage.cacheWriteInputTokens must re-emit; got {out}"
    );
}

/// `bedrock/response/metrics.latencyMs` — a native Converse response always carries the request
#[test]
fn bedrock_carry_response_metrics_latency_ms() {
    let writer = BedrockWriter;
    let mut value = serde_json::json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
        "stopReason": "end_turn",
        "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}
    });
    writer.inject_response_metrics(&mut value, Some(137));
    assert_eq!(
        value.pointer("/metrics/latencyMs").and_then(|v| v.as_u64()),
        Some(137),
        "metrics.latencyMs must be emitted from the measured elapsed time; got {value}"
    );

    let mut value2 = serde_json::json!({"output": {}});
    writer.inject_response_metrics(&mut value2, None);
    assert_eq!(
        value2
            .pointer("/metrics/latencyMs")
            .and_then(|v| v.as_u64()),
        Some(0),
        "metrics.latencyMs must still be present when timing is unavailable; got {value2}"
    );

    let mut value3 = serde_json::json!({"output": {}, "metrics": {"latencyMs": 42}});
    writer.inject_response_metrics(&mut value3, Some(137));
    assert_eq!(
        value3
            .pointer("/metrics/latencyMs")
            .and_then(|v| v.as_u64()),
        Some(42),
        "an upstream metrics.latencyMs must survive injection untouched; got {value3}"
    );
}

/// `bedrock/response/trace.guardrail` and `bedrock/response/trace.promptRouter` — Bedrock-only
#[test]
fn bedrock_carry_response_trace_documented_drop() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
        "stopReason": "end_turn",
        "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2},
        "trace": {
            "guardrail": {"modelOutput": ["blocked"]},
            "promptRouter": {"invokedModelId": "arn:aws:...:model/x"}
        }
    });
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert!(
        out.pointer("/trace").is_none(),
        "trace (guardrail + promptRouter) has no cross-protocol carrier and must drop; got {out}"
    );

    if let Some(anthropic) = super::super::proto_codec::protocol_for("anthropic") {
        let a = anthropic
            .writer()
            .write_response(crate::codec::test_host::HOST, &resp);
        assert!(
            a.to_string().find("promptRouter").is_none()
                && a.to_string().find("guardrail").is_none(),
            "trace members must not leak onto a foreign egress; got {a}"
        );
    }
}

/// Bedrock response members with no neutral-IR carrier. DOCUMENTED cross-protocol drop; the
#[test]
fn bedrock_carry_response_documented_drops() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    for (key, value) in [
        (
            "additionalModelResponseFields",
            serde_json::json!({"foo": "bar"}),
        ),
        (
            "performanceConfig",
            serde_json::json!({"latency": "optimized"}),
        ),
    ] {
        let mut body = serde_json::json!({
            "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
            "stopReason": "end_turn",
            "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}
        });
        body[key] = value;
        let resp = reader
            .read_response(crate::codec::test_host::HOST, &body)
            .expect("read");
        let out = writer.write_response(crate::codec::test_host::HOST, &resp);
        assert!(
            out.get(key).is_none(),
            "{key} has no cross-protocol carrier and must drop; got {out}"
        );
    }
}

/// `bedrock/response/stream:{messageStart,contentBlockStart,contentBlockDelta,contentBlockStop,
#[test]
fn bedrock_carry_stream_frame_types() {
    use crate::codec::ir::{IrDelta, IrStreamEvent};
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let ev = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStart", "role": "assistant"}),
        &mut state,
    );
    assert!(
        matches!(ev.first(), Some(IrStreamEvent::MessageStart { role, .. }) if *role == crate::codec::ir::IrRole::Assistant),
        "stream:messageStart must decode to IrStreamEvent::MessageStart; got {ev:?}"
    );

    let ev = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockStart",
            "contentBlockIndex": 0,
            "start": {"toolUse": {"toolUseId": "t1", "name": "go"}}
        }),
        &mut state,
    );
    assert!(
        matches!(
            ev.first(),
            Some(IrStreamEvent::BlockStart { block: crate::codec::ir::IrBlockMeta::ToolUse { name, .. }, .. }) if name == "go"
        ),
        "stream:contentBlockStart must decode to a ToolUse BlockStart; got {ev:?}"
    );

    let ev = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"toolUse": {"input": "{\"q\":"}}
        }),
        &mut state,
    );
    assert!(
        matches!(ev.first(), Some(IrStreamEvent::BlockDelta { delta: IrDelta::InputJsonDelta(s), .. }) if s == "{\"q\":"),
        "stream:contentBlockDelta must decode to a BlockDelta; got {ev:?}"
    );

    let ev = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0}),
        &mut state,
    );
    assert!(
        matches!(ev.first(), Some(IrStreamEvent::BlockStop { index: 0 })),
        "stream:contentBlockStop must decode to a BlockStop; got {ev:?}"
    );

    let ev = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStop", "stopReason": "tool_use"}),
        &mut state,
    );
    assert!(
        ev.is_empty(),
        "stream:messageStop buffers its stopReason for the metadata frame; got {ev:?}"
    );

    let ev = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "metadata", "usage": {"inputTokens": 9, "outputTokens": 3}}),
        &mut state,
    );
    assert!(
        matches!(
            ev.first(),
            Some(IrStreamEvent::MessageDelta { stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse), usage, .. })
                if usage.input_tokens == 9 && usage.output_tokens == 3
        ),
        "stream:metadata must decode to the combined MessageDelta (buffered stopReason + usage); got {ev:?}"
    );
    assert!(
        matches!(ev.get(1), Some(IrStreamEvent::MessageStop)),
        "stream:metadata must also emit the terminal MessageStop; got {ev:?}"
    );

    assert_eq!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::MessageStart {
                    role: crate::codec::ir::IrRole::Assistant,
                    usage: None,
                    id: None,
                    created: None,
                    model: None,
                }
            )
            .map(|(t, _)| t),
        Some("messageStart".to_string()),
        "IrStreamEvent::MessageStart must write a messageStart frame"
    );
    assert_eq!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStart {
                    index: 0,
                    block: crate::codec::ir::IrBlockMeta::ToolUse {
                        id: "t1".into(),
                        name: "go".into()
                    },
                    refusal: false,
                }
            )
            .map(|(t, _)| t),
        Some("contentBlockStart".to_string()),
        "a ToolUse BlockStart must write a contentBlockStart frame"
    );
    assert_eq!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockDelta {
                    index: 0,
                    delta: IrDelta::TextDelta("hi".into()),
                }
            )
            .map(|(t, _)| t),
        Some("contentBlockDelta".to_string()),
        "a BlockDelta must write a contentBlockDelta frame"
    );
    assert_eq!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStop { index: 0 }
            )
            .map(|(t, _)| t),
        Some("contentBlockStop".to_string()),
        "a BlockStop must write a contentBlockStop frame"
    );

    assert_eq!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::MessageDelta {
                    stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
                    stop_sequence: None,
                    usage: crate::codec::ir::IrUsage::default(),
                    stop_detail: None,
                }
            )
            .map(|(t, _)| t),
        Some("messageStop".to_string()),
        "a stop-only MessageDelta must write a messageStop frame"
    );
    assert_eq!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::MessageDelta {
                    stop_reason: None,
                    stop_sequence: None,
                    usage: crate::codec::ir::IrUsage {
                        input_tokens: 9,
                        output_tokens: 3,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        detail: crate::codec::ir::IrUsageDetail::default(),
                    },
                    stop_detail: None,
                }
            )
            .map(|(t, _)| t),
        Some("metadata".to_string()),
        "a usage-only MessageDelta must write a metadata frame"
    );
}
