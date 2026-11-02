use super::*;
use crate::codec::ir::IrUsage;
use crate::codec_diagnostics::{diagnostic_texts, warnings};

#[test]
fn stop_reason_reverse_never_leaks_foreign_tokens() {
    use crate::codec::ir::IrStopReason as S;
    assert_eq!(stop_reason_reverse(S::EndTurn), "end_turn");
    assert_eq!(stop_reason_reverse(S::ToolUse), "tool_use");
    assert_eq!(stop_reason_reverse(S::Safety), "content_filtered");

    assert_eq!(stop_reason_map("guardrail_intervened"), S::Safety);

    assert_eq!(stop_reason_reverse(S::Refusal), "content_filtered");
    assert_eq!(stop_reason_reverse(S::Error), "end_turn");
    assert_eq!(stop_reason_reverse(S::Other), "end_turn");
}

#[test]
fn test_bedrock_sigv4_sign_request_structure() {
    let Some(busbar_contract::protocol::EgressScheme::SigV4 {
        service,
        region_of_host,
        default_region,
        content_type,
    }) = super::DECL.egress_scheme
    else {
        panic!("the signing dialect declares a SigV4 scheme");
    };
    assert_eq!(service, "bedrock");
    assert_eq!(content_type, "application/json");
    assert_eq!(default_region, "us-east-1");
    assert_eq!(
        region_of_host("bedrock-runtime.us-east-1.amazonaws.com"),
        Some("us-east-1"),
        "scope/region derived from host"
    );
    assert!(
        super::DECL.egress_auth_headers.is_none(),
        "the plane holds no signer"
    );
}

#[test]
fn test_bedrock_sigv4_session_token() {
    let Some(busbar_contract::protocol::EgressScheme::SigV4 {
        service,
        region_of_host,
        default_region,
        content_type,
    }) = super::DECL.egress_scheme
    else {
        panic!("the signing dialect declares a SigV4 scheme");
    };
    let _ = (service, default_region, content_type);
    assert_eq!(
        region_of_host("bedrock-runtime.eu-west-1.amazonaws.com"),
        Some("eu-west-1")
    );
}

fn bedrock_rich_fixture() -> serde_json::Value {
    serde_json::json!({
        "system": [{"text": "You are a helpful assistant."}],
        "messages": [
            {"role": "user", "content": [{"text": "What is the weather in San Francisco?"}]},
            {"role": "assistant", "content": [{"toolUse": {"toolUseId": "tool_123", "name": "get_weather", "input": {"city": "San Francisco"}}}]},
            {"role": "user", "content": [{"toolResult": {"toolUseId": "tool_123", "content": [{"text": "Sunny, 72°F"}], "status": "success"}}]}
        ],
        "inferenceConfig": {"maxTokens": 1024, "temperature": 0.7},
        "toolConfig": {
            "tools": [{
                "toolSpec": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "inputSchema": {"json": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}
                }
            }]
        },
        "top_p": 0.95
    })
}

#[test]
fn test_write_request() {
    let ir = crate::codec::ir::IrRequest {
        system: vec![crate::codec::ir::IrBlock::text(
            "You are a helpful assistant.".to_string(),
        )],
        messages: vec![
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::text(
                    "What is the weather in San Francisco?".to_string(),
                )],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Assistant,
                content: vec![crate::codec::ir::IrBlock::ToolUse {
                    thought_signature: None,
                    id: "tool_123".to_string(),
                    name: "get_weather".to_string(),
                    input: serde_json::json!({"city": "San Francisco"}),
                    cache_control: None,
                }],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::ToolResult {
                    tool_use_id: "tool_123".to_string(),
                    content: vec![crate::codec::ir::IrBlock::text("Sunny, 72°F".to_string())],
                    is_error: false,
                    cache_control: None,
                }],
            },
        ],
        tools: vec![crate::codec::ir::IrTool {
            name: "get_weather".to_string(),
            description: Some("Get weather for a city".to_string()),
            input_schema: serde_json::json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
            cache_control: None,

            hosted: None,

            strict: None,
        }],
        max_tokens: Some(1024),
        temperature: Some(0.7_f64),
        ..Default::default()
    };

    let writer = BedrockWriter;
    let json = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(
        json.get("system")
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str()),
        Some("You are a helpful assistant.")
    );
    assert_eq!(
        json.get("messages")
            .and_then(|m| m.as_array())
            .and_then(|a| a.first())
            .and_then(|msg| msg.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str()),
        Some("What is the weather in San Francisco?")
    );
    assert_eq!(
        json.get("messages")
            .and_then(|m| m.as_array())
            .and_then(|a| a.get(1))
            .and_then(|msg| msg.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|b| b.get("toolUse"))
            .and_then(|tu| tu.get("toolUseId"))
            .and_then(|id| id.as_str()),
        Some("tool_123")
    );
    assert_eq!(
        json.get("messages")
            .and_then(|m| m.as_array())
            .and_then(|a| a.get(1))
            .and_then(|msg| msg.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|b| b.get("toolUse"))
            .and_then(|tu| tu.get("name"))
            .and_then(|n| n.as_str()),
        Some("get_weather")
    );
    assert_eq!(
        json.get("messages")
            .and_then(|m| m.as_array())
            .and_then(|a| a.get(1))
            .and_then(|msg| msg.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|b| b.get("toolUse"))
            .and_then(|tu| tu.get("input"))
            .and_then(|i| i.get("city"))
            .and_then(|c| c.as_str()),
        Some("San Francisco")
    );
    assert_eq!(
        json.get("messages")
            .and_then(|m| m.as_array())
            .and_then(|a| a.get(2))
            .and_then(|msg| msg.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|b| b.get("toolResult"))
            .and_then(|tr| tr.get("status"))
            .and_then(|s| s.as_str()),
        Some("success")
    );
    assert_eq!(
        json.get("inferenceConfig")
            .and_then(|ic| ic.get("maxTokens"))
            .and_then(|m| m.as_u64()),
        Some(1024)
    );
    assert_eq!(
        json.get("inferenceConfig")
            .and_then(|ic| ic.get("temperature"))
            .and_then(|t| t.as_f64()),
        Some(0.7)
    );
    assert_eq!(
        json.get("toolConfig")
            .and_then(|tc| tc.get("tools"))
            .and_then(|ts| ts.as_array())
            .and_then(|arr| arr.first())
            .and_then(|t| t.get("toolSpec"))
            .and_then(|spec| spec.get("name"))
            .and_then(|n| n.as_str()),
        Some("get_weather")
    );
}

#[test]
fn test_read_request() {
    let reader = BedrockReader;
    let j = bedrock_rich_fixture();
    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");

    assert!(!ir.system.is_empty());
    if let crate::codec::ir::IrBlock::Text { text, .. } = &ir.system[0] {
        assert_eq!(text, "You are a helpful assistant.");
    } else {
        panic!("system[0] should be Text block");
    }

    assert_eq!(ir.messages.len(), 3);

    if let crate::codec::ir::IrBlock::Text { text, .. } = &ir.messages[0].content[0] {
        assert_eq!(text, "What is the weather in San Francisco?");
    } else {
        panic!("messages[0].content[0] should be Text block");
    }

    if let crate::codec::ir::IrBlock::ToolUse {
        id, name, input, ..
    } = &ir.messages[1].content[0]
    {
        assert_eq!(id, "tool_123");
        assert_eq!(name, "get_weather");
        match input {
            serde_json::Value::Object(obj) => {
                assert_eq!(obj.get("city"), Some(&serde_json::json!("San Francisco")));
            }
            _ => panic!("input should be Object"),
        }
    } else {
        panic!("messages[1].content[0] should be ToolUse block");
    }

    if let crate::codec::ir::IrBlock::ToolResult {
        tool_use_id,
        content,
        is_error,
        ..
    } = &ir.messages[2].content[0]
    {
        assert_eq!(tool_use_id, "tool_123");
        assert!(!is_error);
        if let crate::codec::ir::IrBlock::Text { text, .. } = &content[0] {
            assert_eq!(text, "Sunny, 72°F");
        } else {
            panic!("toolResult content[0] should be Text block");
        }
    } else {
        panic!("messages[2].content[0] should be ToolResult block");
    }

    assert_eq!(ir.max_tokens, Some(1024));
    assert_eq!(ir.temperature, Some(0.7_f64));
    assert_eq!(ir.tools.len(), 1);
    let crate::codec::ir::IrTool {
        ref name,
        ref description,
        ..
    } = ir.tools[0];
    assert_eq!(name, "get_weather");
    assert_eq!(description.as_deref(), Some("Get weather for a city"));
}

#[test]
fn test_roundtrip() {
    let reader = BedrockReader;
    let writer = BedrockWriter;

    let wire = serde_json::json!({
        "system": [{"text": "You are helpful."}],
        "messages": [{"role": "user", "content": [{"text": "Hello!"}]}],
        "inferenceConfig": {"maxTokens": 512, "temperature": 0.7}
    });

    let ir = reader
        .read_request(&wire)
        .expect("read round-trip should succeed");
    let wire_after = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(
        wire, wire_after,
        "same-protocol wire round-trip must be byte-identical"
    );
}

#[test]
fn test_temperature_fidelity() {
    let j = serde_json::json!({"inferenceConfig": {"temperature": 0.7}, "messages": [{"role": "user", "content": [{"text": "hi"}]}]});
    let reader = BedrockReader;
    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");
    assert_eq!(ir.temperature, Some(0.7_f64));
}

#[test]
fn test_read_response_decode() {
    let j = serde_json::json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [
                    {"text": "Let me check the weather for you."},
                    {"toolUse": {"toolUseId": "tu_1", "name": "get_weather", "input": {"city": "SF"}}}
                ]
            }
        },
        "stopReason": "tool_use",
        "usage": {
            "inputTokens": 42,
            "outputTokens": 15,
            "totalTokens": 57
        }
    });

    let reader = BedrockReader;
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &j)
        .expect("read_response should succeed");

    assert_eq!(resp.role, crate::codec::ir::IrRole::Assistant);
    assert_eq!(resp.content.len(), 2);

    if let crate::codec::ir::IrBlock::Text { text, .. } = &resp.content[0] {
        assert_eq!(text, "Let me check the weather for you.");
    } else {
        panic!("content[0] should be Text block");
    }

    if let crate::codec::ir::IrBlock::ToolUse {
        id, name, input, ..
    } = &resp.content[1]
    {
        assert_eq!(id, "tu_1");
        assert_eq!(name, "get_weather");
        match input {
            serde_json::Value::Object(obj) => {
                assert_eq!(obj.get("city"), Some(&serde_json::json!("SF")));
            }
            _ => panic!("input should be Object"),
        }
    } else {
        panic!("content[1] should be ToolUse block");
    }

    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::ToolUse)
    );
    assert_eq!(resp.usage.input_tokens, 42);
    assert_eq!(resp.usage.output_tokens, 15);
}

#[test]
fn test_read_write_response_roundtrip() {
    let j = serde_json::json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [{"text": "Hello, world!"}]
            }
        },
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 10,
            "outputTokens": 5,
            "totalTokens": 15
        }
    });

    let reader = BedrockReader;
    let writer = BedrockWriter;

    let resp = reader
        .read_response(crate::codec::test_host::HOST, &j)
        .expect("read_response should succeed");
    let written = writer.write_response(crate::codec::test_host::HOST, &resp);

    assert_eq!(
        written, j,
        "round-trip must be byte-identical for text-only response"
    );
}

#[test]
fn test_stream_decode_sequence() {
    use crate::codec::ir::IrStreamEvent;

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let events: Vec<_> = vec![
        (serde_json::json!({"type": "messageStart", "role": "assistant"})),
        (serde_json::json!({
            "type": "contentBlockStart",
            "contentBlockIndex": 0,
            "start": {}
        })),
        (serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"text": "Hello"}
        })),
        (serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"text": ", world!"}
        })),
        (serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0})),
        (serde_json::json!({
            "type": "messageStop",
            "stopReason": "end_turn"
        })),
        (serde_json::json!({
            "type": "metadata",
            "usage": {"inputTokens": 10, "outputTokens": 5}
        })),
    ]
    .into_iter()
    .flat_map(|data| {
        reader.read_response_events(crate::codec::test_host::HOST, "", &data, &mut state)
    })
    .collect();

    assert_eq!(events.len(), 7);

    match &events[0] {
        IrStreamEvent::MessageStart { role, usage, .. } => {
            assert_eq!(*role, crate::codec::ir::IrRole::Assistant);
            assert!(usage.is_none());
        }
        _ => panic!("event[0] should be MessageStart"),
    }

    match &events[1] {
        IrStreamEvent::BlockStart {
            index,
            block,
            refusal: _,
        } => {
            assert_eq!(*index, 0);
            assert!(matches!(block, crate::codec::ir::IrBlockMeta::Text));
        }
        _ => panic!("event[1] should be BlockStart"),
    }

    match &events[2] {
        IrStreamEvent::BlockDelta { index, delta } => {
            assert_eq!(*index, 0);
            if let crate::codec::ir::IrDelta::TextDelta(text) = delta {
                assert_eq!(text, "Hello");
            } else {
                panic!("event[2] should be TextDelta");
            }
        }
        _ => panic!("event[2] should be BlockDelta"),
    }

    match &events[3] {
        IrStreamEvent::BlockDelta { index, delta } => {
            assert_eq!(*index, 0);
            if let crate::codec::ir::IrDelta::TextDelta(text) = delta {
                assert_eq!(text, ", world!");
            } else {
                panic!("event[3] should be TextDelta");
            }
        }
        _ => panic!("event[3] should be BlockDelta"),
    }

    match &events[4] {
        IrStreamEvent::BlockStop { index } => assert_eq!(*index, 0),
        _ => panic!("event[4] should be BlockStop"),
    }

    match &events[5] {
        IrStreamEvent::MessageDelta {
            stop_reason, usage, ..
        } => {
            assert_eq!(stop_reason, &Some(crate::codec::ir::IrStopReason::EndTurn));
            assert_eq!(usage.input_tokens, 10);
            assert_eq!(usage.output_tokens, 5);
        }
        _ => {
            panic!("event[5] should be the combined MessageDelta carrying stop_reason + usage")
        }
    }

    match &events[6] {
        IrStreamEvent::MessageStop => {}
        _ => panic!("event[6] should be the terminal MessageStop"),
    }
}

#[test]
fn test_write_response_event() {
    let writer = BedrockWriter;

    let delta_ev = IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
    };

    if let Some((event_type, payload)) =
        writer.write_response_event(crate::codec::test_host::HOST, &delta_ev)
    {
        assert_eq!(event_type, "contentBlockDelta");
        assert_eq!(
            payload.get("contentBlockIndex").and_then(|i| i.as_u64()),
            Some(0)
        );
        assert_eq!(
            payload
                .get("delta")
                .and_then(|d| d.as_object())
                .and_then(|o| o.get("text"))
                .and_then(|t| t.as_str()),
            Some("hi")
        );
    } else {
        panic!("write_response_event should return Some for BlockDelta");
    }

    let delta_ev2 = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        stop_sequence: None,
        usage: IrUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
        stop_detail: None,
    };

    if let Some((event_type, payload)) =
        writer.write_response_event(crate::codec::test_host::HOST, &delta_ev2)
    {
        assert_eq!(event_type, "messageStop");
        assert_eq!(
            payload.get("stopReason").and_then(|s| s.as_str()),
            Some("tool_use")
        );
    } else {
        panic!("write_response_event should return Some for MessageDelta with tool_use");
    }
}

/// Regression: `extract_error` must read the machine-readable error type from the AWS `__type`
#[test]
fn test_extract_error_structured_type_from_type_field() {
    let reader = BedrockReader;
    let body = br#"{"__type":"ThrottlingException","message":"Rate exceeded"}"#;
    let raw = reader.extract_error(StatusCode::TOO_MANY_REQUESTS, body);
    assert_eq!(raw.http_status, 429);
    assert_eq!(raw.provider_code.as_deref(), Some("Rate exceeded"));
    assert_eq!(
        raw.structured_type.as_deref(),
        Some("ThrottlingException"),
        "structured_type must come from __type, not the message"
    );
}

/// `__type` is sometimes serialised as a shape ARN suffix
#[test]
fn test_extract_error_strips_type_arn_prefix() {
    let reader = BedrockReader;
    let body =
        br#"{"__type":"com.amazon.coral.service#ValidationException","message":"bad input"}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(raw.provider_code.as_deref(), Some("bad input"));
    assert_eq!(raw.structured_type.as_deref(), Some("ValidationException"));
}

/// When `__type` is absent, `structured_type` is None (no longer duplicated from `message`).
#[test]
fn test_extract_error_no_type_field_yields_none_structured_type() {
    let reader = BedrockReader;
    let body = br#"{"message":"something went wrong"}"#;
    let raw = reader.extract_error(StatusCode::INTERNAL_SERVER_ERROR, body);
    assert_eq!(raw.provider_code.as_deref(), Some("something went wrong"));
    assert!(
        raw.structured_type.is_none(),
        "structured_type must NOT be duplicated from message"
    );
}

/// A non-JSON body parses gracefully to (None, None) — single parse, no panic.
#[test]
fn test_extract_error_non_json_body() {
    let reader = BedrockReader;
    let raw = reader.extract_error(StatusCode::BAD_GATEWAY, b"<html>502</html>");
    assert_eq!(raw.http_status, 502);
    assert!(raw.provider_code.is_none());
    assert!(raw.structured_type.is_none());
}

/// A ConverseStream that ends after `messageStop` WITHOUT a trailing `metadata` event
#[test]
fn test_stream_metadata_less_defers_terminator() {
    use crate::codec::ir::IrStreamEvent;

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let events: Vec<_> = vec![
        serde_json::json!({"type": "messageStart", "role": "assistant"}),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"text": "Hi"}
        }),
        serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0}),
        serde_json::json!({"type": "messageStop", "stopReason": "end_turn"}),
        // NOTE: no `metadata` event — the upstream truncated here.
    ]
    .into_iter()
    .flat_map(|data| {
        reader.read_response_events(crate::codec::test_host::HOST, "", &data, &mut state)
    })
    .collect();

    assert!(
            !events
                .iter()
                .any(|e| matches!(e, IrStreamEvent::MessageStop)),
            "no terminal MessageStop is emitted on a metadata-less stream (deferred to metadata); got: {events:?}"
        );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, IrStreamEvent::MessageDelta { .. })),
        "no combined MessageDelta is emitted without metadata; got: {events:?}"
    );

    assert_eq!(
        state.pending_stop_reason,
        Some(crate::codec::ir::IrStopReason::EndTurn)
    );
}

/// Exactly one terminal MessageStop is emitted across the full happy-path sequence
#[test]
fn test_stream_emits_single_message_stop_with_metadata() {
    use crate::codec::ir::IrStreamEvent;

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let events: Vec<_> = vec![
        serde_json::json!({"type": "messageStart", "role": "assistant"}),
        serde_json::json!({"type": "messageStop", "stopReason": "end_turn"}),
        serde_json::json!({"type": "metadata", "usage": {"inputTokens": 3, "outputTokens": 1}}),
    ]
    .into_iter()
    .flat_map(|data| {
        reader.read_response_events(crate::codec::test_host::HOST, "", &data, &mut state)
    })
    .collect();

    let stop_count = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::MessageStop))
        .count();
    assert_eq!(
        stop_count, 1,
        "exactly one terminal MessageStop expected; got: {events:?}"
    );
}

/// The native Bedrock Converse error envelope is a flat `{"__type", "message"}` body (the exact
#[test]
fn test_write_error_native_bedrock_shape() {
    let writer = BedrockWriter;
    let v = writer.write_error(
        crate::codec::test_host::HOST,
        400,
        "invalid_request_error",
        "bad input",
    );
    assert_eq!(
        v.get("message").and_then(|m| m.as_str()),
        Some("bad input"),
        "human text must be in lowercase `message`"
    );
    assert_eq!(
        v.get("__type").and_then(|t| t.as_str()),
        Some("ValidationException"),
        "generic kind must map to a native Converse exception name in `__type`"
    );
    assert!(
        v.get("error").is_none(),
        "must NOT carry the generic `{{\"error\":...}}` envelope (non-native tell)"
    );

    let s = serde_json::to_string(&v).expect("error envelope must serialize");
    assert!(s.contains("\"__type\""));
}

#[test]
fn a_spent_budget_is_400_service_quota_exceeded_not_a_retryable_throttle() {
    assert_eq!(super::DECL.quota_exceeded_status, 400);
    for kind in [
        ERR_TYPE_INSUFFICIENT_QUOTA,
        "quota_exceeded",
        "service_quota_exceeded",
    ] {
        assert_eq!(
            error_kind_to_bedrock_type(kind),
            "ServiceQuotaExceededException",
            "{kind}"
        );
    }

    assert_eq!(
        error_kind_to_bedrock_type("rate_limit_error"),
        "ThrottlingException"
    );

    let table: serde_json::Value =
        serde_json::from_str(super::REFUSALS_JSON).expect("refusals.json is JSON");
    let row = &table["refusals"][0];
    assert_eq!(row["exception"], "ServiceQuotaExceededException");
    assert_eq!(
        row["status"].as_u64(),
        Some(u64::from(super::DECL.quota_exceeded_status))
    );
    assert!(row["kinds"]
        .as_array()
        .is_some_and(|k| k.iter().any(|k| k == ERR_TYPE_INSUFFICIENT_QUOTA)));
}

/// Kind → Bedrock exception-name mapping covers the common categories and falls back to a real
#[test]
fn test_error_kind_to_bedrock_type_mapping() {
    assert_eq!(
        error_kind_to_bedrock_type("rate_limit_error"),
        "ThrottlingException"
    );
    assert_eq!(error_kind_to_bedrock_type("auth"), "AccessDeniedException");
    assert_eq!(
        error_kind_to_bedrock_type("not_found"),
        "ResourceNotFoundException"
    );
    assert_eq!(
        error_kind_to_bedrock_type(ERR_TYPE_OVERLOADED),
        "ServiceUnavailableException"
    );

    assert_eq!(
        error_kind_to_bedrock_type(busbar_contract::protocol::KIND_OVERLOADED),
        "ServiceUnavailableException"
    );
    assert_eq!(
        error_kind_to_bedrock_type("api_error"),
        "InternalServerException"
    );

    assert_eq!(
        error_kind_to_bedrock_type("some_future_kind"),
        "ValidationException"
    );
}

/// The native error envelope round-trips back through `extract_error`: a Bedrock SDK (and the
#[test]
fn test_write_error_roundtrips_through_extract_error() {
    let writer = BedrockWriter;
    let reader = BedrockReader;
    let v = writer.write_error(
        crate::codec::test_host::HOST,
        429,
        "rate_limit_error",
        "Rate exceeded",
    );
    let body = serde_json::to_vec(&v).expect("serialize");
    let raw = reader.extract_error(StatusCode::TOO_MANY_REQUESTS, &body);
    assert_eq!(raw.provider_code.as_deref(), Some("Rate exceeded"));
    assert_eq!(raw.structured_type.as_deref(), Some("ThrottlingException"));
}

/// Same-protocol passthrough fidelity: reading a native Converse response and writing it back
#[test]
fn test_response_identity_same_protocol_roundtrip_no_synth() {
    let reader = BedrockReader;
    let writer = BedrockWriter;

    let j = serde_json::json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [{"text": "Hello, world!"}]
            }
        },
        "stopReason": "end_turn",
        "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}
    });

    let resp = reader
        .read_response(crate::codec::test_host::HOST, &j)
        .expect("read_response");

    assert_eq!(resp.id, None, "Converse body has no id to capture");
    assert_eq!(
        resp.created, None,
        "Converse body has no created to capture"
    );
    assert_eq!(resp.system_fingerprint, None);
    assert_eq!(resp.stop_sequence, None);

    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::EndTurn)
    );
    assert_eq!(resp.usage.input_tokens, 10);
    assert_eq!(resp.usage.output_tokens, 5);

    let written = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        written, j,
        "same-protocol round-trip must be byte-identical"
    );

    assert!(written.get("id").is_none(), "native body must carry no id");
    assert!(
        written.get("created").is_none(),
        "native body must carry no created"
    );
}

/// A stream MessageDelta with `stop_reason = None` (the usage-only trailing
#[test]
fn test_write_response_event_usage_delta_is_metadata_frame() {
    let writer = BedrockWriter;

    let usage_only = IrStreamEvent::MessageDelta {
        stop_reason: None,
        stop_sequence: None,
        usage: IrUsage {
            input_tokens: 11,
            output_tokens: 7,
            ..Default::default()
        },
        stop_detail: None,
    };
    let (et, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &usage_only)
        .expect("usage-only delta must emit a frame");
    assert_eq!(
        et, "metadata",
        "usage-only delta must be a `metadata` frame, not messageStop"
    );
    assert_eq!(
        payload
            .pointer("/usage/inputTokens")
            .and_then(|v| v.as_u64()),
        Some(11)
    );
    assert_eq!(
        payload
            .pointer("/usage/outputTokens")
            .and_then(|v| v.as_u64()),
        Some(7)
    );
    assert_eq!(
        payload
            .pointer("/usage/totalTokens")
            .and_then(|v| v.as_u64()),
        Some(18),
        "totalTokens must be inputTokens + outputTokens"
    );

    let stop = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        stop_sequence: None,
        usage: IrUsage::default(),
        stop_detail: None,
    };
    let (et2, payload2) = writer
        .write_response_event(crate::codec::test_host::HOST, &stop)
        .expect("stop delta must emit a frame");
    assert_eq!(et2, "messageStop");
    assert_eq!(
        payload2.get("stopReason").and_then(|s| s.as_str()),
        Some("tool_use")
    );
}

/// A text BlockStart must emit NO frame. AWS Bedrock ConverseStream's
#[test]
fn test_write_response_event_text_block_start_emits_no_frame() {
    let writer = BedrockWriter;
    let ev = IrStreamEvent::BlockStart {
        index: 0,
        block: crate::codec::ir::IrBlockMeta::Text,
        refusal: false,
    };
    assert!(
        writer
            .write_response_event(crate::codec::test_host::HOST, &ev)
            .is_none(),
        "a text BlockStart must emit NO contentBlockStart frame; the block opens implicitly on \
         its first contentBlockDelta"
    );

    let stop = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 0 },
        )
        .expect("a text BlockStop must still emit a contentBlockStop");
    assert_eq!(stop.0, "contentBlockStop");
    assert_eq!(
        stop.1.get("contentBlockIndex").and_then(|i| i.as_u64()),
        Some(0)
    );
}

/// A mid-stream Bedrock exception event (`internalServerException` etc.)
#[test]
fn test_stream_decode_surfaces_midstream_exception() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let events = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "internalServerException",
            "message": "the model is on fire"
        }),
        &mut state,
    );
    assert_eq!(
        events.len(),
        1,
        "exactly one Error event expected; got {events:?}"
    );
    match &events[0] {
        IrStreamEvent::Error(err) => {
            assert_eq!(err.class, StatusClass::ServerError);
            assert_eq!(err.provider_signal.as_deref(), Some("the model is on fire"));
        }
        other => panic!("expected IrStreamEvent::Error, got {other:?}"),
    }

    let throttle = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "throttlingException"}),
        &mut state,
    );
    match throttle.as_slice() {
        [IrStreamEvent::Error(err)] => {
            assert_eq!(err.class, StatusClass::RateLimit);
            assert_eq!(err.provider_signal.as_deref(), Some("throttlingException"));
        }
        other => panic!("expected a single RateLimit Error; got {other:?}"),
    }

    let unknown = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "someFutureEvent"}),
        &mut state,
    );
    assert!(
        unknown.is_empty(),
        "unknown event types must be skipped; got {unknown:?}"
    );

    let model_timeout = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "modelTimeoutException", "message": "slow"}),
        &mut state,
    );
    assert!(
        model_timeout.is_empty(),
        "modelTimeoutException is not a ConverseStream output-union member; must be skipped, \
             got {model_timeout:?}"
    );
}

/// `bedrock_image_block` must never emit `format: ""`, and never relabel an image as a format it
#[test]
fn test_bedrock_image_block_empty_subtype_is_dropped() {
    assert!(
        bedrock_image_block(&crate::codec::ir::IrImageSource::Base64 {
            media_type: "image/".to_string(),
            data: ("QQ==").to_string(),
        })
        .is_none(),
        "an empty subtype has no Converse format: dropped, never png"
    );

    let jpeg = bedrock_image_block(&crate::codec::ir::IrImageSource::Base64 {
        media_type: "image/jpeg".to_string(),
        data: ("QQ==").to_string(),
    })
    .expect("jpeg must emit a block");
    assert_eq!(
        jpeg.pointer("/format").and_then(|f| f.as_str()),
        Some("jpeg")
    );

    assert!(
        bedrock_image_block(&crate::codec::ir::IrImageSource::Base64 {
            media_type: "png".to_string(),
            data: ("QQ==").to_string(),
        })
        .is_none(),
        "a bare media_type is dropped, never relabelled"
    );

    assert!(
        bedrock_image_block(&crate::codec::ir::IrImageSource::Url(
            ("https://example.com/x.png").to_string()
        ))
        .is_none(),
        "URL-source image must be dropped, not emitted as a base64 block"
    );
}

/// The injected `stream` flag on a Bedrock-INGRESS converse-stream request
#[test]
fn test_read_request_honors_injected_stream_flag() {
    let reader = BedrockReader;

    let streaming = serde_json::json!({
        "stream": true,
        "messages": [{"role": "user", "content": [{"text": "hi"}]}]
    });
    let ir = reader.read_request(&streaming).expect("read_request");
    assert!(
        ir.stream,
        "injected `stream: true` must be read into the IR"
    );

    let buffered = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}]
    });
    let ir2 = reader.read_request(&buffered).expect("read_request");
    assert!(
        !ir2.stream,
        "absent `stream` defaults to false (native egress)"
    );
}

/// A System-role message that escapes the caller's system extraction is
#[test]
fn test_write_request_skips_system_role_message() {
    let writer = BedrockWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::System,
                content: vec![crate::codec::ir::IrBlock::text(
                    "leaked system text".to_string(),
                )],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Tool,
                content: vec![crate::codec::ir::IrBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: vec![crate::codec::ir::IrBlock::text("ok".to_string())],
                    is_error: false,
                    cache_control: None,
                }],
            },
        ],
        ..Default::default()
    };

    let json = writer.write_request(crate::codec::test_host::HOST, &req);
    let msgs = json
        .get("messages")
        .and_then(|m| m.as_array())
        .expect("messages array");
    assert_eq!(
        msgs.len(),
        1,
        "the System-role message must be dropped; got {msgs:?}"
    );
    assert_eq!(
        msgs[0].get("role").and_then(|r| r.as_str()),
        Some("user"),
        "the surviving Tool-role message maps to a user turn"
    );

    let wire = serde_json::to_string(&json).unwrap();
    assert!(
        !wire.contains("leaked system text"),
        "system text must not leak onto the wire; got {wire}"
    );
}

/// A non-Text block inside a ToolResult must be re-encoded faithfully
#[test]
fn test_write_request_tool_result_preserves_non_text_content() {
    let writer = BedrockWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: vec![crate::codec::ir::IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Base64 {
                        media_type: "image/png".to_string(),
                        data: "BASE64DATA".to_string(),
                    },
                    cache_control: None,
                    detail: None,
                }],
                is_error: false,
                cache_control: None,
            }],
        }],
        ..Default::default()
    };

    let json = writer.write_request(crate::codec::test_host::HOST, &req);
    let inner = json
        .pointer("/messages/0/content/0/toolResult/content/0")
        .expect("tool result inner content block");
    assert_eq!(
        inner.pointer("/image/format").and_then(|v| v.as_str()),
        Some("png"),
        "image inner block must be a native Bedrock image block; got {inner}"
    );
    assert_eq!(
        inner
            .pointer("/image/source/bytes")
            .and_then(|v| v.as_str()),
        Some("BASE64DATA")
    );

    let wire = serde_json::to_string(&json).unwrap();
    assert!(
        !wire.contains(r#"{"text":"{}"}"#),
        "must not emit the `{{}}` placeholder; got {wire}"
    );
}

/// A stream Error event names a REAL Converse exception (mapped from the IR
#[test]
fn test_write_response_event_error_names_real_exception() {
    let writer = BedrockWriter;

    let throttle = IrStreamEvent::Error(busbar_contract::protocol::IrError {
        class: StatusClass::RateLimit,
        provider_signal: Some("slow down".to_string()),
        retry_after: None,
    });
    let (et, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &throttle)
        .expect("error event must emit a frame");
    assert_eq!(
        et, "ThrottlingException",
        "event-type token must be a real Converse exception name, not `error`"
    );
    assert_eq!(
        payload.get("message").and_then(|m| m.as_str()),
        Some("slow down")
    );

    let server = IrStreamEvent::Error(busbar_contract::protocol::IrError {
        class: StatusClass::ServerError,
        provider_signal: None,
        retry_after: None,
    });
    let (et2, payload2) = writer
        .write_response_event(crate::codec::test_host::HOST, &server)
        .expect("error event must emit a frame");
    assert_eq!(et2, "InternalServerException");
    assert_eq!(
        payload2.get("message").and_then(|m| m.as_str()),
        Some("InternalServerException")
    );
}

/// Unmodeled top-level request fields must be collected into `extra` so a
#[test]
fn test_read_request_collects_unmodeled_fields_into_extra() {
    let reader = BedrockReader;
    let j = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "inferenceConfig": {"maxTokens": 10},
        "system": [{"text": "sys"}],
        "toolConfig": {"tools": []},
        "stream": true,
        "topP": 0.95,
        "topK": 40,
        "stopSequences": ["STOP"],
        "guardrailConfig": {"guardrailIdentifier": "gr-1", "guardrailVersion": "1"},
        "additionalModelRequestFields": {"foo": "bar"}
    });
    let ir = reader.read_request(&j).expect("read_request");

    assert_eq!(ir.extra.get("topP"), Some(&serde_json::json!(0.95)));
    assert_eq!(ir.extra.get("topK"), Some(&serde_json::json!(40)));
    assert_eq!(
        ir.extra.get("stopSequences"),
        Some(&serde_json::json!(["STOP"]))
    );
    assert_eq!(
        ir.extra.get("guardrailConfig"),
        Some(&serde_json::json!({"guardrailIdentifier": "gr-1", "guardrailVersion": "1"}))
    );
    assert_eq!(
        ir.extra.get("additionalModelRequestFields"),
        Some(&serde_json::json!({"foo": "bar"}))
    );

    assert_eq!(
        ir.extra.get("inferenceConfig"),
        Some(&serde_json::json!({"maxTokens": 10})),
        "inferenceConfig must be captured into extra verbatim"
    );

    assert_eq!(
        ir.extra.get("toolConfig"),
        Some(&serde_json::json!({"tools": []})),
        "toolConfig must be captured into extra verbatim"
    );

    for k in ["system", "messages", "stream"] {
        assert!(
            ir.extra.get(k).is_none(),
            "modeled key `{k}` must not leak into extra; got {:?}",
            ir.extra
        );
    }

    assert!(
        ir.stream,
        "injected stream flag still captured structurally"
    );
}

/// A full passthrough — read a native Converse request carrying
#[test]
fn test_request_passthrough_preserves_unmodeled_fields() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let j = bedrock_rich_fixture();
    let ir = reader.read_request(&j).expect("read_request");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("top_p").and_then(|v| v.as_f64()),
        Some(0.95),
        "unmodeled `top_p` must survive a Bedrock->Bedrock passthrough; got {out}"
    );
}

/// `toolConfig.toolChoice` (the force-tool-use control:
#[test]
fn test_request_passthrough_preserves_tool_choice() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let j = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "weather?"}]}],
        "toolConfig": {
            "tools": [{
                "toolSpec": {
                    "name": "get_weather",
                    "inputSchema": {"json": {"type": "object"}}
                }
            }],
            "toolChoice": {"any": {}}
        }
    });
    let ir = reader.read_request(&j).expect("read_request");

    assert_eq!(ir.tools.len(), 1);
    assert_eq!(ir.tools[0].name, "get_weather");

    assert_eq!(
        ir.extra
            .get("toolConfig")
            .and_then(|tc| tc.get("toolChoice")),
        Some(&serde_json::json!({"any": {}})),
        "raw toolChoice must be captured into extra; got {:?}",
        ir.extra
    );

    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let tc = out
        .get("toolConfig")
        .and_then(|v| v.as_object())
        .expect("toolConfig must be re-emitted");

    assert_eq!(
        tc.get("toolChoice"),
        Some(&serde_json::json!({"any": {}})),
        "toolChoice must survive a Bedrock->Bedrock passthrough; got {out}"
    );

    assert_eq!(
        tc.get("tools").and_then(|t| t.as_array()).map(|a| a.len()),
        Some(1),
        "rebuilt tools array must be present; got {out}"
    );
}

/// On the CROSS-protocol seam `extra` is cleared, so a writer driven
#[test]
fn test_write_request_tool_config_cross_protocol_and_empty() {
    let writer = BedrockWriter;

    let ir_tools = crate::codec::ir::IrRequest {
        tools: vec![crate::codec::ir::IrTool {
            name: "f".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: None,

            hosted: None,

            strict: None,
        }],
        ..Default::default()
    };
    let out = writer.write_request(crate::codec::test_host::HOST, &ir_tools);
    let tc = out
        .get("toolConfig")
        .and_then(|v| v.as_object())
        .expect("toolConfig must be emitted from typed tools alone");
    assert_eq!(
        tc.get("tools").and_then(|t| t.as_array()).map(|a| a.len()),
        Some(1)
    );
    assert!(
        tc.get("toolChoice").is_none(),
        "no toolChoice should appear cross-protocol (IR has no field for it); got {out}"
    );

    let ir_empty = crate::codec::ir::IrRequest::default();
    let out_empty = writer.write_request(crate::codec::test_host::HOST, &ir_empty);
    assert!(
        out_empty.get("toolConfig").is_none(),
        "no toolConfig must be emitted when there are no tools; got {out_empty}"
    );
}

/// A text block that opens at index > 0 (after a preceding tool-use block,
#[test]
fn test_stream_text_block_after_tool_not_dropped() {
    use crate::codec::ir::IrStreamEvent;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let events: Vec<_> = vec![
        serde_json::json!({"type": "messageStart", "role": "assistant"}),
        // tool-use block at index 0
        serde_json::json!({
            "type": "contentBlockStart",
            "contentBlockIndex": 0,
            "start": {"toolUse": {"toolUseId": "t1", "name": "f"}}
        }),
        serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0}),
        // text block at index 1 (start has no `start` object → text)
        serde_json::json!({"type": "contentBlockStart", "contentBlockIndex": 1}),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 1,
            "delta": {"text": "first"}
        }),
        serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 1}),
        // a SECOND text block at index 2 — must still open (flag was cleared at idx 1 stop)
        serde_json::json!({"type": "contentBlockStart", "contentBlockIndex": 2}),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 2,
            "delta": {"text": "second"}
        }),
        serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 2}),
    ]
    .into_iter()
    .flat_map(|d| reader.read_response_events(crate::codec::test_host::HOST, "", &d, &mut state))
    .collect();

    let text_starts: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: crate::codec::ir::IrBlockMeta::Text,
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(
        text_starts,
        vec![1, 2],
        "both text blocks (idx 1 and idx 2) must emit a BlockStart; got {events:?}"
    );

    let deltas: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockDelta {
                delta: crate::codec::ir::IrDelta::TextDelta(t),
                ..
            } => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["first".to_string(), "second".to_string()]);
}

/// A `contentBlockStart` whose `start` object carries an UNRECOGNIZED key
#[test]
fn test_stream_unrecognized_start_does_not_open_text() {
    use crate::codec::ir::IrStreamEvent;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStart", "role": "assistant"}),
        &mut state,
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockStart",
            "contentBlockIndex": 0,
            "start": {"reasoningContent": {"foo": "bar"}}
        }),
        &mut state,
    );
    assert!(
        !evs.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                block: crate::codec::ir::IrBlockMeta::Text,
                ..
            }
        )),
        "an unrecognized `start` key must not open a Text block; got {evs:?}"
    );
    assert!(
        !state.text_block_open,
        "text_block_open must remain false for an unrecognized start shape"
    );

    let evs2 = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStart", "contentBlockIndex": 0, "start": {}}),
        &mut state,
    );
    assert!(
        evs2.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                block: crate::codec::ir::IrBlockMeta::Text,
                ..
            }
        )),
        "an empty `start: {{}}` must still open a Text block; got {evs2:?}"
    );
}

/// A usage-only delta's `metadata` frame carries the real `usage` but does
#[test]
fn test_write_response_event_metadata_no_fabricated_metrics() {
    let writer = BedrockWriter;
    let usage_only = IrStreamEvent::MessageDelta {
        stop_reason: None,
        stop_sequence: None,
        usage: IrUsage {
            input_tokens: 3,
            output_tokens: 2,
            ..Default::default()
        },
        stop_detail: None,
    };
    let (et, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &usage_only)
        .expect("usage-only delta emits a metadata frame");
    assert_eq!(et, "metadata");
    assert!(
        payload.pointer("/metrics").is_none(),
        "writer must NOT fabricate a `metrics` object (latency is injected by StreamTranslate \
             or omitted); got {payload}"
    );

    assert_eq!(
        payload
            .pointer("/usage/totalTokens")
            .and_then(|v| v.as_u64()),
        Some(5)
    );
}

/// `totalTokens` is computed with a saturating
#[test]
fn test_write_response_event_total_tokens_saturates() {
    let writer = BedrockWriter;
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: None,
        stop_sequence: None,
        usage: IrUsage {
            input_tokens: u64::MAX,
            output_tokens: 1,
            ..Default::default()
        },
        stop_detail: None,
    };
    let (et, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("usage-only delta emits a metadata frame");
    assert_eq!(et, "metadata");

    assert_eq!(
        payload
            .pointer("/usage/totalTokens")
            .and_then(|v| v.as_u64()),
        Some(u64::MAX),
        "totalTokens must saturate, not wrap; got {payload}"
    );

    assert_eq!(
        payload
            .pointer("/usage/inputTokens")
            .and_then(|v| v.as_u64()),
        Some(u64::MAX)
    );
    assert_eq!(
        payload
            .pointer("/usage/outputTokens")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
}

/// `bedrock/writer.rs`'s `BlockStop` arm is unconditional even though `BlockStart` suppresses the
#[test]
fn bedrock_writer_image_block_stop_is_suppressed() {
    let writer = BedrockWriter;
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStart {
                    index: 0,
                    block: crate::codec::ir::IrBlockMeta::Image,
                    refusal: false,
                }
            )
            .is_none(),
        "Image BlockStart must stay suppressed"
    );
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStop { index: 0 }
            )
            .is_none(),
        "a BlockStop for an index whose BlockStart was suppressed must ALSO be suppressed, \
         not emit an orphan contentBlockStop"
    );
}

/// `bedrock_response_to_eventstream` (buffered 2xx → synthesized
#[test]
fn eventstream_emits_reasoning_content_for_thinking_block() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![
            crate::codec::ir::IrBlock::Thinking {
                text: "let me think".to_string(),
                signature: Some("sigblob".to_string()),
                redacted: false,
                cache_control: None,
                kind: None,
                signature_origin: None,
            },
            crate::codec::ir::IrBlock::text("answer".to_string()),
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let bytes = bedrock_response_to_eventstream(crate::codec::test_host::HOST, &resp, Some(5));
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("reasoningContent"),
        "a Thinking block must synthesize a reasoningContent frame, not be dropped"
    );
    assert!(
        text.contains("let me think"),
        "the thinking text must ride a reasoningContent delta"
    );
    assert!(
        text.contains("sigblob"),
        "the reasoning signature must ride a signature delta"
    );
}

/// `bedrock_response_to_eventstream` derives `contentBlockIndex` from `enumerate()` over ALL
#[test]
fn eventstream_content_block_index_is_contiguous_when_a_block_is_skipped() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![
            crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: "aGk=".to_string(),
                },
                cache_control: None,
                detail: None,
            },
            crate::codec::ir::IrBlock::text("answer".to_string()),
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bytes = bedrock_response_to_eventstream(crate::codec::test_host::HOST, &resp, Some(5));
    let frames = crate::codec::eventstream::drain_frames(&mut bytes);

    let text_delta = frames
        .iter()
        .find(|(et, _)| et == "contentBlockDelta")
        .map(|(_, payload)| serde_json::from_slice::<serde_json::Value>(payload).unwrap())
        .expect("Text block must synthesize a contentBlockDelta frame");
    assert_eq!(
        text_delta.get("contentBlockIndex").and_then(|i| i.as_u64()),
        Some(0),
        "the Text block is the only one that emits frames; it must land at index 0, \
         not the enumerate()-derived index 1: {text_delta:?}"
    );

    assert!(
        !frames.iter().any(|(et, _)| et == "contentBlockStart"),
        "neither an Image nor a Text block emits a contentBlockStart frame; got {:?}",
        frames.iter().map(|(t, _)| t).collect::<Vec<_>>()
    );
}

/// REGRESSION PROOF: no orphaned start or stop. Only `ToolUse` projects a `contentBlockStart`
#[test]
fn eventstream_every_content_block_start_has_exactly_one_stop() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![
            crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: "aGk=".to_string(),
                },
                cache_control: None,
                detail: None,
            },
            crate::codec::ir::IrBlock::Thinking {
                text: "let me think".to_string(),
                signature: Some("sigblob".to_string()),
                redacted: false,
                cache_control: None,
                kind: None,
                signature_origin: None,
            },
            crate::codec::ir::IrBlock::text("answer".to_string()),
            crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "toolu_1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
                cache_control: None,
            },
            crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: vec![crate::codec::ir::IrBlock::text("sunny".to_string())],
                is_error: false,
                cache_control: None,
            },
            crate::codec::ir::IrBlock::Json(serde_json::json!({"k": "v"})),
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bytes = bedrock_response_to_eventstream(crate::codec::test_host::HOST, &resp, Some(5));
    let frames = crate::codec::eventstream::drain_frames(&mut bytes);

    let mut starts: Vec<u64> = Vec::new();
    let mut stops: Vec<u64> = Vec::new();
    for (event_type, payload) in &frames {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
            continue;
        };
        let Some(idx) = v.get("contentBlockIndex").and_then(|i| i.as_u64()) else {
            continue;
        };
        match event_type.as_str() {
            "contentBlockStart" => starts.push(idx),
            "contentBlockStop" => stops.push(idx),
            _ => {}
        }
    }
    starts.sort_unstable();
    stops.sort_unstable();
    assert!(!stops.is_empty(), "expected at least one emitted block");

    {
        let mut uniq_starts = starts.clone();
        uniq_starts.dedup();
        assert_eq!(
            uniq_starts, starts,
            "a contentBlockStart index was emitted twice: {starts:?}"
        );
        let mut uniq_stops = stops.clone();
        uniq_stops.dedup();
        assert_eq!(
            uniq_stops, stops,
            "a contentBlockStop index was emitted twice: {stops:?}"
        );
    }

    for s in &starts {
        assert!(
            stops.contains(s),
            "orphan contentBlockStart with no matching contentBlockStop: index {s}; starts={starts:?} stops={stops:?}"
        );
    }

    assert!(
        !starts.is_empty(),
        "the ToolUse block must emit exactly one contentBlockStart: starts={starts:?}"
    );
}

/// The buffered Converse `totalTokens`
#[test]
fn test_write_response_total_tokens_saturates() {
    let writer = BedrockWriter;
    let resp = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: u64::MAX - 1,
            output_tokens: 100,
            ..Default::default()
        },
        ..Default::default()
    };
    let body = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        body.pointer("/usage/totalTokens").and_then(|v| v.as_u64()),
        Some(u64::MAX),
        "totalTokens must saturate, not wrap; got {body}"
    );

    let normal = crate::codec::ir::IrResponse {
        usage: IrUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
        ..resp
    };
    let body = writer.write_response(crate::codec::test_host::HOST, &normal);
    assert_eq!(
        body.pointer("/usage/totalTokens").and_then(|v| v.as_u64()),
        Some(15)
    );
}

/// An assistant response carrying
#[test]
fn test_write_response_projects_image_block() {
    let writer = BedrockWriter;
    let resp = crate::codec::ir::IrResponse {
        content: vec![
            crate::codec::ir::IrBlock::text("see image".to_string()),
            crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: "aGVsbG8=".to_string(),
                },
                cache_control: None,
                detail: None,
            },
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let body = writer.write_response(crate::codec::test_host::HOST, &resp);
    let content = body
        .pointer("/output/message/content")
        .and_then(|v| v.as_array())
        .expect("content array present");

    assert_eq!(content.len(), 2, "image block must not be dropped: {body}");
    let image = content
        .iter()
        .find(|b| b.get("image").is_some())
        .expect("a native image block must be present");
    assert_eq!(
        image.pointer("/image/format").and_then(|v| v.as_str()),
        Some("png"),
        "image format derived from MIME subtype: {body}"
    );
    assert_eq!(
        image
            .pointer("/image/source/bytes")
            .and_then(|v| v.as_str()),
        Some("aGVsbG8="),
        "base64 image data carried as `bytes` source: {body}"
    );
}

/// A response whose blocks are ALL
#[test]
fn test_write_response_empty_content_emits_placeholder() {
    let writer = BedrockWriter;
    let resp = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::ToolResult {
            tool_use_id: "tu_1".to_string(),
            content: Vec::new(),
            is_error: false,
            cache_control: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let body = writer.write_response(crate::codec::test_host::HOST, &resp);
    let content = body
        .pointer("/output/message/content")
        .and_then(|v| v.as_array())
        .expect("content array present");
    assert!(
        !content.is_empty(),
        "content array must never be empty (Bedrock rejects it): {body}"
    );
    assert_eq!(
        content.len(),
        1,
        "exactly one placeholder block when all blocks are non-representable: {body}"
    );
    assert_eq!(
        content[0].get("text").and_then(|v| v.as_str()),
        Some(""),
        "placeholder is a minimal empty-text block (mirrors write_request): {body}"
    );
}

/// An `inferenceConfig` carrying sub-fields this reader
#[test]
fn test_inference_config_passthrough_preserves_unmodeled_subfields() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let wire = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "inferenceConfig": {
            "maxTokens": 256,
            "temperature": 0.3,
            "topP": 0.9,
            "topK": 50,
            "stopSequences": ["\n\nHuman:", "END"]
        }
    });

    let ir = reader.read_request(&wire).expect("read_request");

    assert_eq!(ir.max_tokens, Some(256));
    assert_eq!(ir.temperature, Some(0.3));

    assert!(ir.extra.contains_key("inferenceConfig"));

    let out = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(
        out.pointer("/inferenceConfig/maxTokens")
            .and_then(|v| v.as_u64()),
        Some(256)
    );
    assert_eq!(
        out.pointer("/inferenceConfig/temperature")
            .and_then(|v| v.as_f64()),
        Some(0.3)
    );
    assert_eq!(
        out.pointer("/inferenceConfig/topP")
            .and_then(|v| v.as_f64()),
        Some(0.9),
        "topP must survive passthrough; got {out}"
    );
    assert_eq!(
        out.pointer("/inferenceConfig/topK")
            .and_then(|v| v.as_u64()),
        Some(50),
        "topK must survive passthrough; got {out}"
    );
    assert_eq!(
        out.pointer("/inferenceConfig/stopSequences"),
        Some(&serde_json::json!(["\n\nHuman:", "END"])),
        "stopSequences must survive passthrough; got {out}"
    );

    assert_eq!(out, wire, "full body must round-trip byte-identically");
}

/// `top_k` (a first-class IR field) must reach a Bedrock egress via
#[test]
fn test_top_k_reaches_bedrock_via_additional_model_request_fields() {
    let writer = BedrockWriter;

    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        max_tokens: Some(64),
        top_k: Some(40),
        ..Default::default()
    };
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/additionalModelRequestFields/top_k")
            .and_then(|v| v.as_u64()),
        Some(40),
        "top_k must be emitted under additionalModelRequestFields.top_k; got {out}"
    );

    let reader = BedrockReader;
    let wire = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "inferenceConfig": {"maxTokens": 64},
        "additionalModelRequestFields": {"top_k": 40}
    });
    let ir2 = reader.read_request(&wire).expect("read_request");
    assert_eq!(ir2.top_k, Some(40), "reader must promote top_k into the IR");
    let out2 = writer.write_request(crate::codec::test_host::HOST, &ir2);
    assert_eq!(
        out2.pointer("/additionalModelRequestFields/top_k")
            .and_then(|v| v.as_u64()),
        Some(40),
        "top_k must survive the Bedrock round-trip; got {out2}"
    );
    assert_eq!(
        out2, wire,
        "native Bedrock body must round-trip byte-identically"
    );
}

/// Losslessness (top_k spelling preservation): a native Bedrock->Bedrock passthrough whose body
#[test]
fn test_top_k_camel_spelling_round_trips_and_cross_protocol_stays_snake() {
    let reader = BedrockReader;
    let writer = BedrockWriter;

    let wire = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "inferenceConfig": {"maxTokens": 64},
        "additionalModelRequestFields": {"topK": 40}
    });
    let ir = reader.read_request(&wire).expect("read_request");
    assert_eq!(ir.top_k, Some(40), "reader must promote topK into the IR");
    assert!(
        ir.extra.contains_key(TOP_K_CAMEL_SENTINEL),
        "reader must stamp the camel-spelling sentinel when source used topK"
    );
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/additionalModelRequestFields/topK")
            .and_then(|v| v.as_u64()),
        Some(40),
        "topK source spelling must be preserved; got {out}"
    );
    assert!(
        out.pointer("/additionalModelRequestFields/top_k").is_none(),
        "must NOT also emit snake_case top_k; got {out}"
    );

    assert!(
        out.get(TOP_K_CAMEL_SENTINEL).is_none(),
        "the source-spelling sentinel must never reach the wire; got {out}"
    );
    assert_eq!(
        out, wire,
        "Bedrock->Bedrock topK body must round-trip byte-identically"
    );

    let ir_cross = crate::codec::ir::IrRequest {
        top_k: Some(40),
        extra: serde_json::Map::new(),
        ..ir.clone()
    };
    let out_cross = writer.write_request(crate::codec::test_host::HOST, &ir_cross);
    assert_eq!(
        out_cross
            .pointer("/additionalModelRequestFields/top_k")
            .and_then(|v| v.as_u64()),
        Some(40),
        "cross-protocol egress (no sentinel) must emit canonical top_k; got {out_cross}"
    );
    assert!(
        out_cross
            .pointer("/additionalModelRequestFields/topK")
            .is_none(),
        "cross-protocol egress must NOT emit camelCase topK; got {out_cross}"
    );
}

/// PF (non-silent clamp): the Bedrock temperature clamp helper signals when it changes the value
#[test]
fn test_clamp_temperature_for_bedrock_signals_on_change() {
    assert_eq!(crate::codec::carry::clamp(1.8, 0.0, 1.0), (1.0, true));
    assert_eq!(crate::codec::carry::clamp(2.0, 0.0, 1.0), (1.0, true));
    assert_eq!(crate::codec::carry::clamp(-0.5, 0.0, 1.0), (0.0, true));

    assert_eq!(crate::codec::carry::clamp(0.7, 0.0, 1.0), (0.7, false));
    assert_eq!(crate::codec::carry::clamp(0.0, 0.0, 1.0), (0.0, false));
    assert_eq!(crate::codec::carry::clamp(1.0, 0.0, 1.0), (1.0, false));
}

/// The typed IR fields WIN over a same-named raw `inferenceConfig` entry
#[test]
fn test_inference_config_typed_fields_override_raw_and_cross_protocol() {
    let writer = BedrockWriter;

    let mut extra = serde_json::Map::new();
    extra.insert(
        "inferenceConfig".to_string(),
        serde_json::json!({"maxTokens": 1, "topP": 0.5}),
    );
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        max_tokens: Some(999),
        extra,
        ..Default::default()
    };
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/inferenceConfig/maxTokens")
            .and_then(|v| v.as_u64()),
        Some(999),
        "typed maxTokens must override the raw extra value; got {out}"
    );
    assert_eq!(
        out.pointer("/inferenceConfig/topP")
            .and_then(|v| v.as_f64()),
        Some(0.5),
        "unmodeled topP from raw config still survives"
    );

    let ir2 = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        max_tokens: Some(42),
        temperature: Some(0.1),
        ..Default::default()
    };
    let out2 = writer.write_request(crate::codec::test_host::HOST, &ir2);
    assert_eq!(
        out2.pointer("/inferenceConfig/maxTokens")
            .and_then(|v| v.as_u64()),
        Some(42)
    );
    assert_eq!(
        out2.pointer("/inferenceConfig/temperature")
            .and_then(|v| v.as_f64()),
        Some(0.1)
    );

    assert!(out2.pointer("/inferenceConfig/topP").is_none());
}

/// A mid-stream `IrError` mapped for the ConverseStream output union must
#[test]
fn test_stream_exception_only_emits_converse_stream_union_members() {
    let writer = BedrockWriter;
    const STREAM_UNION: [&str; 5] = [
        "InternalServerException",
        "ModelStreamErrorException",
        "ValidationException",
        "ThrottlingException",
        "ServiceUnavailableException",
    ];

    let cases = [
        (StatusClass::RateLimit, "ThrottlingException"),
        (StatusClass::Overloaded, "ServiceUnavailableException"),
        (StatusClass::ClientError, "ValidationException"),
        (StatusClass::ContextLength, "ValidationException"),
        (StatusClass::Timeout, "ModelStreamErrorException"),
        (StatusClass::Auth, "InternalServerException"),
        (StatusClass::Billing, "InternalServerException"),
        (StatusClass::ServerError, "InternalServerException"),
        (StatusClass::Network, "InternalServerException"),
    ];

    for (class, expected) in cases {
        let err = busbar_contract::protocol::IrError {
            class,
            provider_signal: Some("upstream detail".to_string()),
            retry_after: None,
        };

        let (exc, msg) = writer
            .write_response_exception(crate::codec::test_host::HOST, &err)
            .expect("write_response_exception must map every class");
        assert_eq!(
            exc, expected,
            "class {class:?} must map to {expected} on the exception frame"
        );
        assert!(
            STREAM_UNION.contains(&exc.as_str()),
            "{exc} is not a ConverseStream output-union member"
        );
        assert_eq!(msg, "upstream detail", "message prefers provider_signal");

        let ev = IrStreamEvent::Error(busbar_contract::protocol::IrError {
            class,
            provider_signal: None,
            retry_after: None,
        });
        let (et, payload) = writer
            .write_response_event(crate::codec::test_host::HOST, &ev)
            .expect("error event must emit a frame");
        assert_eq!(
            et, expected,
            "event-arm class {class:?} must also map to {expected}"
        );
        assert!(
            STREAM_UNION.contains(&et.as_str()),
            "{et} is not a ConverseStream output-union member"
        );

        assert_eq!(
            payload.get("message").and_then(|m| m.as_str()),
            Some(expected)
        );
    }

    for class in [
        StatusClass::Timeout,
        StatusClass::Auth,
        StatusClass::Billing,
    ] {
        let err = busbar_contract::protocol::IrError {
            class,
            provider_signal: None,
            retry_after: None,
        };
        let (exc, _) = writer
            .write_response_exception(crate::codec::test_host::HOST, &err)
            .unwrap();
        assert_ne!(exc, "ModelTimeoutException");
        assert_ne!(exc, "AccessDeniedException");
        assert_ne!(exc, "ServiceQuotaExceededException");
    }
}

/// A cross-protocol ingress
#[test]
fn test_write_request_url_sentinel_image_not_emitted_as_base64() {
    let writer = BedrockWriter;

    let url = "https://example.com/cat.png";
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![
                crate::codec::ir::IrBlock::text("look".to_string()),
                crate::codec::ir::IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Url(url.to_string()),
                    cache_control: None,
                    detail: None,
                },
            ],
        }],
        ..Default::default()
    };
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    let wire = serde_json::to_string(&out).unwrap();
    assert!(
        !wire.contains(url),
        "URL must NOT be emitted as base64 image bytes; got {wire}"
    );
    assert!(
        !wire.contains("\"image\""),
        "no image block must be emitted for a URL sentinel; got {wire}"
    );

    assert_eq!(
        out.pointer("/messages/0/content/0/text")
            .and_then(|v| v.as_str()),
        Some("look")
    );

    let req2 = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: "QkFTRTY0".to_string(),
                },
                cache_control: None,
                detail: None,
            }],
        }],
        ..req.clone()
    };
    let out2 = writer.write_request(crate::codec::test_host::HOST, &req2);
    assert_eq!(
        out2.pointer("/messages/0/content/0/image/format")
            .and_then(|v| v.as_str()),
        Some("png")
    );
    assert_eq!(
        out2.pointer("/messages/0/content/0/image/source/bytes")
            .and_then(|v| v.as_str()),
        Some("QkFTRTY0")
    );
}

/// A user/assistant turn whose blocks are ALL non-representable on the
#[test]
fn test_write_request_all_nonrepresentable_turn_kept_with_placeholder() {
    let writer = BedrockWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::text("hello".to_string())],
            },
            // Assistant turn carrying ONLY a thinking block (now re-emitted as reasoningContent).
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Assistant,
                content: vec![crate::codec::ir::IrBlock::Thinking {
                    text: "internal reasoning".to_string(),
                    signature: None,
                    redacted: false,
                    cache_control: None,
                    kind: None,
                    signature_origin: None,
                }],
            },
            // User turn carrying ONLY a URL-sentinel image (also non-representable here).
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Url(
                        "https://example.com/cat.png".to_string(),
                    ),
                    cache_control: None,
                    detail: None,
                }],
            },
        ],
        ..Default::default()
    };
    let out = writer.write_request(crate::codec::test_host::HOST, &req);

    let msgs = out
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array present");
    assert_eq!(msgs.len(), 3, "no turn may be dropped; got {out:?}");

    assert_eq!(
        msgs[0].pointer("/role").and_then(|v| v.as_str()),
        Some("user")
    );
    assert_eq!(
        msgs[1].pointer("/role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(
        msgs[2].pointer("/role").and_then(|v| v.as_str()),
        Some("user")
    );

    assert_eq!(
        msgs[1]
            .pointer("/content/0/reasoningContent/reasoningText/text")
            .and_then(|v| v.as_str()),
        Some("internal reasoning"),
        "thinking-only assistant turn must carry its reasoningContent block"
    );
    assert_eq!(
        msgs[1]
            .pointer("/content")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(1)
    );

    assert_eq!(
        msgs[2].pointer("/content/0/text").and_then(|v| v.as_str()),
        Some(""),
        "image-only (URL-sentinel) user turn must carry a placeholder text block"
    );
}

/// The region is parsed robustly from the
#[test]
fn test_derive_sigv4_region_shapes() {
    assert_eq!(
        derive_sigv4_region("bedrock-runtime.us-east-1.amazonaws.com"),
        Some("us-east-1")
    );
    assert_eq!(
        derive_sigv4_region("bedrock-runtime.ap-southeast-2.amazonaws.com"),
        Some("ap-southeast-2")
    );

    assert_eq!(
        derive_sigv4_region("bedrock-runtime-fips.eu-west-1.amazonaws.com"),
        Some("eu-west-1")
    );

    assert_eq!(
        derive_sigv4_region("bedrock-runtime.eu-central-1.vpce.amazonaws.com"),
        Some("eu-central-1")
    );
    assert_eq!(
        derive_sigv4_region(
            "vpce-0a1b2c3d4e5f-9zyxw.bedrock-runtime.ca-central-1.vpce.amazonaws.com"
        ),
        Some("ca-central-1")
    );

    assert_eq!(
        derive_sigv4_region("bedrock.us-west-2.amazonaws.com"),
        Some("us-west-2")
    );

    assert_eq!(
        derive_sigv4_region("bedrock-runtime.us-gov-west-1.amazonaws.com"),
        Some("us-gov-west-1")
    );
    assert_eq!(
        derive_sigv4_region("bedrock-runtime.us-gov-east-1.amazonaws.com"),
        Some("us-gov-east-1")
    );
    assert_eq!(
        derive_sigv4_region("bedrock-runtime-fips.us-gov-west-1.amazonaws.com"),
        Some("us-gov-west-1")
    );
    assert_eq!(
        derive_sigv4_region("bedrock-runtime.us-iso-east-1.c2s.ic.gov"),
        Some("us-iso-east-1")
    );
    assert_eq!(
        derive_sigv4_region("bedrock-runtime.us-isob-east-1.sc2s.sgov.gov"),
        Some("us-isob-east-1")
    );

    assert_eq!(derive_sigv4_region("my-cname-front.example.com"), None);
    assert_eq!(derive_sigv4_region("10.0.0.5"), None);
    assert_eq!(derive_sigv4_region("localhost"), None);

    assert_eq!(
        derive_sigv4_region("bedrock-runtime.internal.corp.example.com"),
        None
    );

    assert_eq!(
        derive_sigv4_region("bedrock-runtime.us-east.amazonaws.com"),
        None
    );
    assert_eq!(
        derive_sigv4_region("bedrock-runtime.us-gov-west-foo.amazonaws.com"),
        None
    );
    assert_eq!(
        derive_sigv4_region("bedrock-runtime.1-gov-west-1.amazonaws.com"),
        None
    );
}

/// A FIPS host in a non-us-east-1 region signs for THAT region's
#[test]
fn test_bedrock_sigv4_fips_host_derives_correct_region() {
    let Some(busbar_contract::protocol::EgressScheme::SigV4 {
        service,
        region_of_host,
        default_region,
        content_type,
    }) = super::DECL.egress_scheme
    else {
        panic!("the signing dialect declares a SigV4 scheme");
    };
    let _ = (service, content_type);
    let region = region_of_host("bedrock-runtime-fips.eu-west-1.amazonaws.com");
    assert_eq!(
        region,
        Some("eu-west-1"),
        "FIPS host must derive eu-west-1 scope"
    );
    assert_ne!(
        region,
        Some(default_region),
        "must NOT fall back to the us-east-1 default"
    );
}

/// A non-derivable host falls back to us-east-1 (signing still
#[test]
fn test_bedrock_sigv4_undecodable_host_falls_back_to_us_east_1() {
    let Some(busbar_contract::protocol::EgressScheme::SigV4 {
        service,
        region_of_host,
        default_region,
        content_type,
    }) = super::DECL.egress_scheme
    else {
        panic!("the signing dialect declares a SigV4 scheme");
    };
    let _ = (service, content_type);
    assert_eq!(region_of_host("my-cname-front.example.com"), None);
    assert_eq!(default_region, "us-east-1");
}

/// A `metadata` frame that lacks a `usage` key
#[test]
fn test_stream_metadata_without_usage_still_emits_delta_with_stop_reason() {
    use crate::codec::ir::IrStreamEvent;

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let events: Vec<_> = vec![
        serde_json::json!({"type": "messageStart", "role": "assistant"}),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"text": "Hi"}
        }),
        serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0}),
        serde_json::json!({"type": "messageStop", "stopReason": "end_turn"}),
        // `metadata` arrives but carries NO `usage` key (mock backend).
        serde_json::json!({"type": "metadata", "metrics": {"latencyMs": 12}}),
    ]
    .into_iter()
    .flat_map(|data| {
        reader.read_response_events(crate::codec::test_host::HOST, "", &data, &mut state)
    })
    .collect();

    let delta_idx = events
        .iter()
        .position(|e| matches!(e, IrStreamEvent::MessageDelta { .. }))
        .expect("a combined MessageDelta must be emitted even without usage");
    match &events[delta_idx] {
        IrStreamEvent::MessageDelta {
            stop_reason, usage, ..
        } => {
            assert_eq!(
                stop_reason,
                &Some(crate::codec::ir::IrStopReason::EndTurn),
                "stop_reason buffered from messageStop must survive a usage-less metadata"
            );
            assert_eq!(usage.input_tokens, 0);
            assert_eq!(usage.output_tokens, 0);
        }
        other => panic!("expected MessageDelta, got {other:?}"),
    }

    let stop_idx = events
        .iter()
        .position(|e| matches!(e, IrStreamEvent::MessageStop))
        .expect("a terminal MessageStop must be emitted");
    assert!(
        delta_idx < stop_idx,
        "MessageDelta must precede MessageStop; got {events:?}"
    );

    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, IrStreamEvent::MessageStop))
            .count(),
        1
    );
    assert!(
        state.pending_stop_reason.is_none(),
        "buffered stop_reason must be consumed by the delta"
    );
}

/// The same URL-sentinel guard
#[test]
fn test_write_request_tool_result_url_sentinel_image_dropped() {
    let writer = BedrockWriter;
    let url = "https://example.com/in-tool.png";
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Tool,
            content: vec![crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: vec![
                    crate::codec::ir::IrBlock::text("result".to_string()),
                    crate::codec::ir::IrBlock::Image {
                        source: crate::codec::ir::IrImageSource::Url(url.to_string()),
                        cache_control: None,
                        detail: None,
                    },
                ],
                is_error: false,
                cache_control: None,
            }],
        }],
        ..Default::default()
    };
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    let wire = serde_json::to_string(&out).unwrap();
    assert!(
        !wire.contains(url),
        "URL sentinel inside toolResult must not be emitted as base64; got {wire}"
    );

    assert_eq!(
        out.pointer("/messages/0/content/0/toolResult/content/0/text")
            .and_then(|v| v.as_str()),
        Some("result")
    );

    assert_eq!(
        out.pointer("/messages/0/content/0/toolResult/content")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(1),
        "URL-sentinel image inner block must be dropped; got {out}"
    );
}

/// A `maxTokens` value above u32::MAX must be
#[test]
fn test_read_request_max_tokens_overflow_dropped_not_truncated() {
    let reader = BedrockReader;

    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "inferenceConfig": {"maxTokens": 5_000_000_000u64}
    });
    let ir = reader.read_request(&body).unwrap();
    assert_eq!(
        ir.max_tokens, None,
        "maxTokens above u32::MAX must drop to None, not wrap; got {:?}",
        ir.max_tokens
    );

    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "inferenceConfig": {"maxTokens": u32::MAX as u64}
    });
    let ir = reader.read_request(&body).unwrap();
    assert_eq!(ir.max_tokens, Some(u32::MAX));

    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "inferenceConfig": {"maxTokens": 0}
    });
    let ir = reader.read_request(&body).unwrap();
    assert_eq!(ir.max_tokens, None);
}

/// A message-level `image` block whose source is
#[test]
fn test_read_request_image_s3_location_captured() {
    let reader = BedrockReader;
    let body = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [{
                "image": {
                    "format": "jpeg",
                    "source": {
                        "s3Location": {
                            "uri": "s3://my-bucket/img.jpg",
                            "bucketOwner": "123456789012"
                        }
                    }
                }
            }]
        }]
    });
    let ir = reader.read_request(&body).expect("read_request");
    let block = &ir.messages[0].content[0];
    match block {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Vendor { vendor, value },
            ..
        } => {
            assert_eq!(
                *vendor, "bedrock",
                "S3-source image is a bedrock vendor reference"
            );
            assert_eq!(
                value.pointer("/s3Location/uri").and_then(|v| v.as_str()),
                Some("s3://my-bucket/img.jpg")
            );
            assert_eq!(
                value
                    .pointer("/s3Location/bucketOwner")
                    .and_then(|v| v.as_str()),
                Some("123456789012")
            );
            assert_eq!(
                value.pointer("/format").and_then(|v| v.as_str()),
                Some("jpeg")
            );
        }
        other => panic!("expected Image(Vendor) block, got {other:?}"),
    }
}

/// A Bedrock body carrying an S3-sourced image must
#[test]
fn test_image_s3_location_round_trip() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [{
                "image": {
                    "format": "png",
                    "source": {
                        "s3Location": {
                            "uri": "s3://b/k.png",
                            "bucketOwner": "999988887777"
                        }
                    }
                }
            }]
        }]
    });
    let ir = reader.read_request(&body).expect("read_request");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let img = out
        .pointer("/messages/0/content/0/image")
        .expect("image block must be re-emitted, not dropped");
    assert_eq!(
        img.pointer("/format").and_then(|v| v.as_str()),
        Some("png"),
        "format must round-trip; got {img}"
    );
    assert_eq!(
        img.pointer("/source/s3Location/uri")
            .and_then(|v| v.as_str()),
        Some("s3://b/k.png"),
        "s3Location.uri must round-trip; got {img}"
    );
    assert_eq!(
        img.pointer("/source/s3Location/bucketOwner")
            .and_then(|v| v.as_str()),
        Some("999988887777"),
        "s3Location.bucketOwner must round-trip; got {img}"
    );

    let wire = serde_json::to_string(&out).unwrap();
    assert!(
        !wire.contains("image_s3"),
        "the image_s3 sentinel must not appear on the wire; got {wire}"
    );

    assert!(
        img.pointer("/source/bytes").is_none(),
        "an S3 image must not be emitted with a bytes source; got {img}"
    );
}

/// An `image` block inside a `toolResult.content` array
#[test]
fn test_read_request_tool_result_decodes_image() {
    let reader = BedrockReader;
    let body = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [{
                "toolResult": {
                    "toolUseId": "t1",
                    "status": "success",
                    "content": [
                        {"text": "see image"},
                        {"image": {"format": "png", "source": {"bytes": "QQ=="}}}
                    ]
                }
            }]
        }]
    });
    let ir = reader.read_request(&body).expect("read_request");
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::ToolResult { content, .. } => {
            assert_eq!(
                content.len(),
                2,
                "both inner blocks must decode; got {content:?}"
            );
            match &content[1] {
                crate::codec::ir::IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Base64 { media_type, data },
                    ..
                } => {
                    assert_eq!(media_type, "image/png");
                    assert_eq!(data, "QQ==");
                }
                other => panic!("expected inner Image block, got {other:?}"),
            }
        }
        other => panic!("expected ToolResult block, got {other:?}"),
    }
}

/// An S3-sourced image inside a toolResult survives
#[test]
fn test_tool_result_image_s3_round_trip() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [{
                "toolResult": {
                    "toolUseId": "t9",
                    "status": "success",
                    "content": [
                        {"image": {"format": "gif", "source": {"s3Location": {"uri": "s3://x/y.gif"}}}}
                    ]
                }
            }]
        }]
    });
    let ir = reader.read_request(&body).expect("read_request");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let inner = out
        .pointer("/messages/0/content/0/toolResult/content/0/image")
        .expect("toolResult image must round-trip, not be dropped");
    assert_eq!(
        inner.pointer("/format").and_then(|v| v.as_str()),
        Some("gif")
    );
    assert_eq!(
        inner
            .pointer("/source/s3Location/uri")
            .and_then(|v| v.as_str()),
        Some("s3://x/y.gif"),
        "toolResult s3Location.uri must round-trip; got {inner}"
    );
}

/// `bedrock_image_block` re-emits `source.s3Location` for a Bedrock-produced
#[test]
fn test_bedrock_image_block_s3_vendor() {
    let source = crate::codec::ir::IrImageSource::Vendor {
        vendor: "bedrock",
        value: serde_json::json!({
            "format": "png",
            "s3Location": { "uri": "s3://bk/i.png", "bucketOwner": "111122223333" }
        }),
    };
    let block = bedrock_image_block(&source).expect("bedrock vendor ref must emit a block");
    assert_eq!(
        block.pointer("/format").and_then(|f| f.as_str()),
        Some("png")
    );
    assert_eq!(
        block
            .pointer("/source/s3Location/uri")
            .and_then(|v| v.as_str()),
        Some("s3://bk/i.png")
    );
    assert_eq!(
        block
            .pointer("/source/s3Location/bucketOwner")
            .and_then(|v| v.as_str()),
        Some("111122223333")
    );

    let foreign = crate::codec::ir::IrImageSource::Vendor {
        vendor: "responses",
        value: serde_json::json!({ "file_id": "x" }),
    };
    assert!(
        bedrock_image_block(&foreign).is_none(),
        "a foreign vendor ref must be dropped"
    );
}

/// A Converse response `usage` carrying
#[test]
fn test_read_response_plumbs_cache_tokens() {
    let reader = BedrockReader;
    let j = serde_json::json!({
        "output": { "message": { "role": "assistant", "content": [{"text": "hi"}] } },
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 10,
            "outputTokens": 5,
            "totalTokens": 15,
            "cacheReadInputTokens": 64,
            "cacheWriteInputTokens": 128
        }
    });
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &j)
        .expect("read_response");
    assert_eq!(
        resp.usage.cache_read_input_tokens,
        Some(64),
        "cacheReadInputTokens must map to cache_read_input_tokens (was hardcoded None)"
    );
    assert_eq!(
        resp.usage.cache_creation_input_tokens,
        Some(128),
        "cacheWriteInputTokens must map to cache_creation_input_tokens (was hardcoded None)"
    );
}

/// An absent cache field stays `None` (not `Some(0)`), so a no-cache
#[test]
fn test_read_response_absent_cache_tokens_are_none() {
    let reader = BedrockReader;
    let j = serde_json::json!({
        "output": { "message": { "role": "assistant", "content": [{"text": "hi"}] } },
        "stopReason": "end_turn",
        "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}
    });
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &j)
        .expect("read_response");
    assert_eq!(resp.usage.cache_read_input_tokens, None);
    assert_eq!(resp.usage.cache_creation_input_tokens, None);
}

/// The `metadata` event's `usage` cache fields must surface on
#[test]
fn test_read_stream_metadata_plumbs_cache_tokens() {
    use crate::codec::ir::IrStreamEvent;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;
    let events: Vec<IrStreamEvent> = [
        serde_json::json!({"type": "messageStop", "stopReason": "end_turn"}),
        serde_json::json!({
            "type": "metadata",
            "usage": {
                "inputTokens": 3,
                "outputTokens": 1,
                "cacheReadInputTokens": 9,
                "cacheWriteInputTokens": 17
            }
        }),
    ]
    .into_iter()
    .flat_map(|data| {
        reader.read_response_events(crate::codec::test_host::HOST, "", &data, &mut state)
    })
    .collect();

    let usage = events
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::MessageDelta { usage, .. } => Some(usage),
            _ => None,
        })
        .expect("a combined MessageDelta must be emitted");
    assert_eq!(usage.cache_read_input_tokens, Some(9));
    assert_eq!(usage.cache_creation_input_tokens, Some(17));
}

/// IR usage cache fields must be emitted as
#[test]
fn test_write_response_emits_cache_tokens_roundtrip() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let j = serde_json::json!({
        "output": { "message": { "role": "assistant", "content": [{"text": "hi"}] } },
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 10,
            "outputTokens": 5,
            // AWS's cache-INCLUSIVE total: 10 + 5 + 64 + 128. A fixture whose total omitted the
            // cache buckets could only "round-trip" against a writer making the same omission;
            // this pins the round-trip against a body an AWS backend would actually send.
            "totalTokens": 207,
            "cacheReadInputTokens": 64,
            "cacheWriteInputTokens": 128
        }
    });
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &j)
        .expect("read_response");
    let written = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        written
            .pointer("/usage/cacheReadInputTokens")
            .and_then(|v| v.as_u64()),
        Some(64),
        "writer must emit cacheReadInputTokens (was omitted)"
    );
    assert_eq!(
        written
            .pointer("/usage/cacheWriteInputTokens")
            .and_then(|v| v.as_u64()),
        Some(128),
        "writer must emit cacheWriteInputTokens (was omitted)"
    );
    assert_eq!(
        written, j,
        "cache-bearing round-trip must be byte-identical"
    );
}

/// A `None` cache field is OMITTED, not serialized as `0` — so a no-cache
#[test]
fn test_write_response_omits_absent_cache_tokens() {
    let writer = BedrockWriter;
    let resp = crate::codec::ir::IrResponse {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let written = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert!(
        written.pointer("/usage/cacheReadInputTokens").is_none(),
        "absent cache_read must not be emitted as 0"
    );
    assert!(
        written.pointer("/usage/cacheWriteInputTokens").is_none(),
        "absent cache_creation must not be emitted as 0"
    );
}

/// A usage-only MessageDelta carrying cache fields must emit
#[test]
fn test_write_stream_metadata_emits_cache_tokens() {
    let writer = BedrockWriter;
    let usage_only = IrStreamEvent::MessageDelta {
        stop_reason: None,
        stop_sequence: None,
        usage: IrUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_creation_input_tokens: Some(40),
            cache_read_input_tokens: Some(20),
            ..Default::default()
        },
        stop_detail: None,
    };
    let (et, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &usage_only)
        .expect("usage-only delta must emit a frame");
    assert_eq!(et, "metadata");
    assert_eq!(
        payload
            .pointer("/usage/cacheReadInputTokens")
            .and_then(|v| v.as_u64()),
        Some(20)
    );
    assert_eq!(
        payload
            .pointer("/usage/cacheWriteInputTokens")
            .and_then(|v| v.as_u64()),
        Some(40)
    );

    assert_eq!(
        payload
            .pointer("/usage/inputTokens")
            .and_then(|v| v.as_u64()),
        Some(11)
    );

    assert_eq!(
        payload
            .pointer("/usage/totalTokens")
            .and_then(|v| v.as_u64()),
        Some(78)
    );
}

/// Native Converse `cachePoint` blocks (the prompt-cache markers) that
#[test]
fn test_cache_point_round_trip() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let wire = serde_json::json!({
        "system": [
            {"text": "you are a helpful assistant with a long static preamble"},
            {"cachePoint": {"type": "default"}}
        ],
        "messages": [
            {"role": "user", "content": [
                {"text": "first big doc"},
                {"cachePoint": {"type": "default"}},
                {"text": "now the question"}
            ]},
            {"role": "assistant", "content": [
                {"text": "an answer"}
            ]}
        ]
    });

    let ir = reader.read_request(&wire).expect("read_request");

    assert!(
        ir.extra.contains_key(CACHE_POINTS_SENTINEL),
        "cachePoint markers must be captured into extra; got {:?}",
        ir.extra
    );

    let out = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert!(
        out.get(CACHE_POINTS_SENTINEL).is_none(),
        "the cachePoint sentinel must not appear on the wire; got {out}"
    );

    assert_eq!(
        out, wire,
        "cachePoint markers must survive the round-trip at their original positions; got {out}"
    );
}

/// A `cachePoint` (or `guardContent`) BEFORE a `document` must not make the document be sent twice.
#[test]
fn a_cache_point_before_a_document_does_not_double_the_document() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let wire = serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"text": "here is the static preamble"},
            {"cachePoint": {"type": "default"}},
            {"document": {
                "format": "pdf",
                "name": "spec",
                "source": {"s3Location": {"uri": "s3://b/spec.pdf"}}
            }},
            {"text": "now answer from it"}
        ]}]
    });

    let ir = reader.read_request(&wire).expect("read_request");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);

    let content = out
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("message content must be an array; got {out}"));
    let docs = content
        .iter()
        .filter(|b| b.get("document").is_some())
        .count();
    assert_eq!(
        docs, 1,
        "the document must be sent EXACTLY once, not once modelled and once spliced; got {out}"
    );
    assert_eq!(
        out, wire,
        "the cachePoint+document round-trip must be byte-identical; got {out}"
    );
}

/// A message whose ONLY content block is a `cachePoint` (no
#[test]
fn test_cache_point_only_message_round_trip() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let wire = serde_json::json!({
        "messages": [
            {"role": "user", "content": [
                {"text": "context block"}
            ]},
            {"role": "user", "content": [
                {"cachePoint": {"type": "default"}}
            ]}
        ]
    });

    let ir = reader.read_request(&wire).expect("read_request");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(
            out,
            serde_json::json!({
                "messages": [
                    {"role": "user", "content": [
                        {"text": "context block"},
                        {"cachePoint": {"type": "default"}}
                    ]}
                ]
            }),
            "consecutive user turns must coalesce (alternation) while preserving the cachePoint; got {out}"
        );

    assert_eq!(
        out.pointer("/messages/0/content/1/cachePoint"),
        Some(&serde_json::json!({"type": "default"})),
        "the merged turn must carry the cachePoint marker; got {out}"
    );
}

/// A request that NEVER used prompt caching must not gain a stray sentinel key, and its body must
#[test]
fn test_no_cache_point_no_sentinel() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let wire = serde_json::json!({
        "system": [{"text": "plain system"}],
        "messages": [{"role": "user", "content": [{"text": "hi"}]}]
    });

    let ir = reader.read_request(&wire).expect("read_request");
    assert!(
        !ir.extra.contains_key(CACHE_POINTS_SENTINEL),
        "no cachePoint marker → no sentinel key; got {:?}",
        ir.extra
    );
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out, wire,
        "cache-free body must round-trip byte-identically"
    );
}

/// `splice_cache_points` must NOT panic on a stale/foreign index (e.g. an `extra` that survived
#[test]
fn test_splice_cache_points_out_of_range_does_not_panic() {
    let mut arr = vec![serde_json::json!({"text": "only block"})];
    let entries = vec![
        serde_json::json!({"i": 999, "block": {"cachePoint": {"type": "default"}}}),
        // Missing fields are skipped, not panicked on.
        serde_json::json!({"block": {"cachePoint": {"type": "default"}}}),
        serde_json::json!({"i": 0}),
    ];
    splice_cache_points(&mut arr, &entries);

    assert_eq!(arr.len(), 2);
    assert_eq!(
        arr[1].pointer("/cachePoint"),
        Some(&serde_json::json!({"type": "default"}))
    );
}

/// `extract_error` must synthesize the canonical
#[test]
fn test_extract_error_synthesizes_context_length_exceeded() {
    let reader = BedrockReader;

    let body = br#"{"__type":"ValidationException","message":"Input is longer than the maximum number of tokens allowed (200000) for this model."}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "an oversized-context ValidationException must surface the canonical \
             context_length_exceeded code; got {raw:?}"
    );

    let body2 = br#"{"__type":"ValidationException","message":"The maximum-tokens limit was exceeded: 250000 requested."}"#;
    let raw2 = reader.extract_error(StatusCode::BAD_REQUEST, body2);
    assert_eq!(
            raw2.provider_code.as_deref(),
            Some("context_length_exceeded"),
            "the maximum-tokens/requested phrasing must also map to context_length_exceeded; got {raw2:?}"
        );

    let body3 =
        br#"{"__type":"ValidationException","message":"malformed request: unknown field foo"}"#;
    let raw3 = reader.extract_error(StatusCode::BAD_REQUEST, body3);
    assert_eq!(
        raw3.provider_code.as_deref(),
        Some("malformed request: unknown field foo"),
        "a non-context-length validation error must keep its message; got {raw3:?}"
    );
}

/// The `contentBlockStart` toolUse arm must honor the
#[test]
fn test_stream_tool_block_start_before_message_start_is_dropped() {
    use crate::codec::ir::IrStreamEvent;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockStart",
            "contentBlockIndex": 0,
            "start": {"toolUse": {"toolUseId": "t1", "name": "f"}}
        }),
        &mut state,
    );
    assert!(
        !evs.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                block: crate::codec::ir::IrBlockMeta::ToolUse { .. },
                ..
            }
        )),
        "a tool BlockStart must not precede MessageStart; got {evs:?}"
    );

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStart", "role": "assistant"}),
        &mut state,
    );
    let evs2 = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockStart",
            "contentBlockIndex": 0,
            "start": {"toolUse": {"toolUseId": "t1", "name": "f"}}
        }),
        &mut state,
    );
    assert!(
        evs2.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                block: crate::codec::ir::IrBlockMeta::ToolUse { .. },
                ..
            }
        )),
        "after messageStart, a toolUse start must emit a ToolUse BlockStart; got {evs2:?}"
    );
}

/// `read_response_events`'s `ET_CONTENT_BLOCK_STOP` arm must NEVER emit a `BlockStop` unless a
#[test]
fn test_stream_block_stop_before_message_start_is_dropped() {
    use crate::codec::ir::IrStreamEvent;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0}),
        &mut state,
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStop { .. })),
        "a contentBlockStop before messageStart must not emit a BlockStop; got {evs:?}"
    );
}

/// A `contentBlockStop` for a tool-use index that never had a matching `contentBlockStart` (e.g. a
#[test]
fn test_stream_block_stop_for_unopened_tool_index_is_dropped() {
    use crate::codec::ir::IrStreamEvent;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStart", "role": "assistant"}),
        &mut state,
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 3}),
        &mut state,
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStop { .. })),
        "a contentBlockStop for an index with no matching contentBlockStart must not emit a \
         BlockStop; got {evs:?}"
    );
}

/// Regression guard on the happy path: a well-formed toolUse `contentBlockStart` followed by its
#[test]
fn test_stream_block_stop_for_opened_tool_index_still_emits() {
    use crate::codec::ir::IrStreamEvent;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStart", "role": "assistant"}),
        &mut state,
    );
    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockStart",
            "contentBlockIndex": 2,
            "start": {"toolUse": {"toolUseId": "t1", "name": "f"}}
        }),
        &mut state,
    );
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 2}),
        &mut state,
    );
    assert_eq!(
        evs,
        vec![IrStreamEvent::BlockStop { index: 2 }],
        "a contentBlockStop matching a real prior contentBlockStart must still emit exactly one \
         BlockStop at that index; got {evs:?}"
    );
}

/// Two concurrently open tool-use blocks (indices 0 and 1) must each close independently: a STOP
#[test]
fn test_stream_block_stop_closes_concurrent_tool_blocks_independently() {
    use crate::codec::ir::IrStreamEvent;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStart", "role": "assistant"}),
        &mut state,
    );
    for idx in [0u64, 1u64] {
        let _ = reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &serde_json::json!({
                "type": "contentBlockStart",
                "contentBlockIndex": idx,
                "start": {"toolUse": {"toolUseId": format!("t{idx}"), "name": "f"}}
            }),
            &mut state,
        );
    }

    let evs1 = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 1}),
        &mut state,
    );
    assert_eq!(evs1, vec![IrStreamEvent::BlockStop { index: 1 }]);

    let evs0 = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0}),
        &mut state,
    );
    assert_eq!(
        evs0,
        vec![IrStreamEvent::BlockStop { index: 0 }],
        "index 0's tool block must still be open (and independently closeable) after index 1's \
         STOP; got {evs0:?}"
    );

    let evs_dup = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 1}),
        &mut state,
    );
    assert!(
        evs_dup.is_empty(),
        "a duplicate contentBlockStop for an already-closed index must be dropped; got \
         {evs_dup:?}"
    );
}

/// Adversarial-input guard: on a MALFORMED stream that opens a
#[test]
fn test_stream_block_stop_prefers_indexed_tool_over_stale_text_flag() {
    use crate::codec::ir::IrStreamEvent;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStart", "role": "assistant"}),
        &mut state,
    );

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStart", "contentBlockIndex": 0}),
        &mut state,
    );

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockStart",
            "contentBlockIndex": 1,
            "start": {"toolUse": {"toolUseId": "t1", "name": "f"}}
        }),
        &mut state,
    );

    let evs1 = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 1}),
        &mut state,
    );
    assert_eq!(
        evs1,
        vec![IrStreamEvent::BlockStop { index: 1 }],
        "the tool's STOP must close its own index, not misattribute to the text flag; got {evs1:?}"
    );

    let evs0 = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0}),
        &mut state,
    );
    assert_eq!(
        evs0,
        vec![IrStreamEvent::BlockStop { index: 0 }],
        "the text block's own later STOP must still emit, not be silently dropped; got {evs0:?}"
    );
}

/// A native Converse `{"json": <value>}` block
#[test]
fn test_tool_result_json_block_round_trip() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [{
                "toolResult": {
                    "toolUseId": "t1",
                    "status": "success",
                    "content": [
                        {"json": {"temperature": 72, "unit": "F", "nested": {"ok": true}}}
                    ]
                }
            }]
        }]
    });
    let ir = reader.read_request(&body).expect("read_request");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let inner = out
        .pointer("/messages/0/content/0/toolResult/content/0/json")
        .expect("toolResult json block must round-trip as `json`, not be collapsed to text");
    assert_eq!(
        inner,
        &serde_json::json!({"temperature": 72, "unit": "F", "nested": {"ok": true}}),
        "the json tool-result value must round-trip verbatim; got {inner}"
    );

    assert!(
        out.pointer("/messages/0/content/0/toolResult/content/0/text")
            .is_none(),
        "a json tool-result block must not degrade to a text block; got {out}"
    );
}

/// The upstream-controlled `contentBlockIndex` is
#[test]
fn test_stream_huge_content_block_index_is_clamped() {
    use crate::codec::ir::IrStreamEvent;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStart", "role": "assistant"}),
        &mut state,
    );

    let huge = u64::MAX;
    let expected = MAX_CONTENT_BLOCK_INDEX as usize;

    let start = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockStart",
            "contentBlockIndex": huge,
            "start": {}
        }),
        &mut state,
    );
    let start_idx = start.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart { index, .. } => Some(*index),
        _ => None,
    });
    assert_eq!(
        start_idx,
        Some(expected),
        "a huge contentBlockIndex on contentBlockStart must be clamped to \
             MAX_CONTENT_BLOCK_INDEX; got {start:?}"
    );

    let delta = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": huge,
            "delta": {"text": "hi"}
        }),
        &mut state,
    );
    let delta_idx = delta.iter().find_map(|e| match e {
        IrStreamEvent::BlockDelta { index, .. } => Some(*index),
        _ => None,
    });
    assert_eq!(
        delta_idx,
        Some(expected),
        "a huge contentBlockIndex on contentBlockDelta must be clamped; got {delta:?}"
    );

    let stop = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockStop",
            "contentBlockIndex": huge
        }),
        &mut state,
    );
    let stop_idx = stop.iter().find_map(|e| match e {
        IrStreamEvent::BlockStop { index } => Some(*index),
        _ => None,
    });
    assert_eq!(
        stop_idx,
        Some(expected),
        "a huge contentBlockIndex on contentBlockStop must be clamped; got {stop:?}"
    );
}

/// The `#[cfg(test)]` `classify`
#[test]
fn test_classify_third_context_length_pattern_matches_extract_error() {
    let reader = BedrockReader;

    let body = br#"{"__type":"ValidationException","message":"The request exceeds the maximum context length for this model."}"#;

    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "extract_error must recognize the `exceeds the maximum` phrasing; got {raw:?}"
    );

    let signal = reader.classify(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        signal.class,
        StatusClass::ContextLength,
        "classify must map the `exceeds the maximum` phrasing to ContextLength, in lockstep \
             with extract_error; got {signal:?}"
    );
    assert_eq!(
        signal.provider_signal.as_deref(),
        Some("context_length_exceeded"),
        "classify must surface the canonical context_length_exceeded signal; got {signal:?}"
    );

    let body_tok = br#"{"__type":"ValidationException","message":"Prompt exceeds the maximum number of input tokens."}"#;
    let signal_tok = reader.classify(StatusCode::BAD_REQUEST, body_tok);
    assert_eq!(
        signal_tok.class,
        StatusClass::ContextLength,
        "classify must match the token variant of the third pattern; got {signal_tok:?}"
    );
}

/// The `extract_error` context-length
#[test]
fn test_extract_error_5xx_context_phrasing_not_reclassified() {
    let reader = BedrockReader;

    let body = br#"{"__type":"InternalServerException","message":"Input is longer than the maximum number of tokens allowed (200000) for this model."}"#;

    let raw = reader.extract_error(StatusCode::INTERNAL_SERVER_ERROR, body);
    assert_ne!(
        raw.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "a 5xx body must NEVER be reclassified as context_length_exceeded; got {raw:?}"
    );
    assert_eq!(
        raw.http_status, 500,
        "the 5xx status must be preserved; got {raw:?}"
    );

    let raw_400 = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw_400.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "a 400 with context-length phrasing must still surface the canonical code; got {raw_400:?}"
    );

    let signal = reader.classify(StatusCode::INTERNAL_SERVER_ERROR, body);
    assert_eq!(
        signal.class,
        StatusClass::ServerError,
        "classify must map a 5xx context-phrasing body to ServerError, in lockstep with \
             extract_error; got {signal:?}"
    );
}

/// The `read_response` content loop must
#[test]
fn test_read_response_carries_image_block() {
    let reader = BedrockReader;

    let body = serde_json::json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [
                    {"text": "here is the chart"},
                    {"image": {"format": "png", "source": {"bytes": "AAAA"}}}
                ]
            }
        },
        "stopReason": "end_turn",
        "usage": {"inputTokens": 1, "outputTokens": 2}
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    let img = ir.content.iter().find_map(|b| match b {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Base64 { media_type, data },
            ..
        } => Some((media_type, data)),
        _ => None,
    });
    assert_eq!(
        img,
        Some((&"image/png".to_string(), &"AAAA".to_string())),
        "a base64 response image must be carried into IR as an Image block; got {:?}",
        ir.content
    );

    let body_s3 = serde_json::json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [
                    {"image": {"format": "jpeg", "source": {"s3Location": {"uri": "s3://b/k"}}}}
                ]
            }
        },
        "stopReason": "end_turn",
        "usage": {"inputTokens": 1, "outputTokens": 2}
    });
    let ir_s3 = reader
        .read_response(crate::codec::test_host::HOST, &body_s3)
        .expect("read_response s3");
    let has_s3 = ir_s3.content.iter().any(|b| {
        matches!(
            b,
            crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Vendor {
                    vendor: "bedrock",
                    ..
                },
                ..
            }
        )
    });
    assert!(
        has_s3,
        "an s3Location response image must be carried into IR under the image_s3 sentinel; \
             got {:?}",
        ir_s3.content
    );
}

/// A native Converse `reasoningContent` (extended-thinking) block in a
#[test]
fn test_request_reasoning_content_round_trips() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"reasoningContent": {"reasoningText": {"text": "let me think", "signature": "sig-abc"}}},
                    {"text": "the answer is 42"}
                ]
            }
        ]
    });
    let ir = reader.read_request(&body).expect("read_request");

    let thinking = ir.messages[0].content.iter().find_map(|b| match b {
        crate::codec::ir::IrBlock::Thinking {
            text, signature, ..
        } => Some((text, signature)),
        _ => None,
    });
    assert_eq!(
        thinking,
        Some((&"let me think".to_string(), &Some("sig-abc".to_string()))),
        "reasoningContent must be carried into IR as a Thinking block; got {:?}",
        ir.messages[0].content
    );
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(
        out.pointer("/messages/0/content/0/reasoningContent/reasoningText/text")
            .and_then(|v| v.as_str()),
        Some("let me think"),
        "reasoningContent text must survive the round-trip; got {out}"
    );
    assert_eq!(
        out.pointer("/messages/0/content/0/reasoningContent/reasoningText/signature")
            .and_then(|v| v.as_str()),
        Some("sig-abc"),
        "reasoningContent signature must survive the round-trip; got {out}"
    );
    assert_eq!(
        out, body,
        "a reasoningContent-bearing request must round-trip byte-identically; got {out}"
    );
}

/// A `reasoningContent` block in a RESPONSE message's `content[]` was
#[test]
fn test_response_reasoning_content_round_trips() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [
                    {"reasoningContent": {"reasoningText": {"text": "reasoning", "signature": "rs-1"}}},
                    {"text": "final"}
                ]
            }
        },
        "stopReason": "end_turn",
        "usage": {"inputTokens": 3, "outputTokens": 4, "totalTokens": 7}
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert!(
        ir.content.iter().any(|b| matches!(
            b,
            crate::codec::ir::IrBlock::Thinking { text, signature , .. }
                if text == "reasoning" && signature.as_deref() == Some("rs-1")
        )),
        "response reasoningContent must be carried into IR as a Thinking block; got {:?}",
        ir.content
    );
    let out = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out, body,
        "a reasoningContent-bearing response must round-trip byte-identically; got {out}"
    );

    let redacted_body = serde_json::json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [
                    {"reasoningContent": {"redactedContent": "RVhBTVBMRQ=="}},
                    {"text": "ok"}
                ]
            }
        },
        "stopReason": "end_turn",
        "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}
    });
    let ir_r = reader
        .read_response(crate::codec::test_host::HOST, &redacted_body)
        .expect("read redacted");
    assert!(
        ir_r.content.iter().any(|b| matches!(
            b,
            crate::codec::ir::IrBlock::Thinking { text, redacted: true, .. }
                if text == "RVhBTVBMRQ=="
        )),
        "redactedContent must be carried under the redacted-signature sentinel; got {:?}",
        ir_r.content
    );
    let out_r = writer.write_response(crate::codec::test_host::HOST, &ir_r);
    assert_eq!(
            out_r.pointer("/output/message/content/0/reasoningContent/redactedContent")
                .and_then(|v| v.as_str()),
            Some("RVhBTVBMRQ=="),
            "a redacted reasoning block must re-emit as redactedContent, not reasoningText; got {out_r}"
        );
    assert!(
        out_r
            .pointer("/output/message/content/0/reasoningContent/reasoningText")
            .is_none(),
        "a redacted reasoning block must NOT leak as a plaintext reasoningText; got {out_r}"
    );
}

/// STREAMING extended-thinking (`reasoningContent` deltas on the
#[test]
fn test_stream_reasoning_content_round_trips() {
    use crate::codec::ir::{IrBlockMeta, IrDelta, IrStreamEvent};

    let reader = BedrockReader;
    let writer = BedrockWriter;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let frames = vec![
        serde_json::json!({"type": "messageStart", "role": "assistant"}),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"reasoningContent": {"text": "let me "}}
        }),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"reasoningContent": {"text": "think"}}
        }),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"reasoningContent": {"signature": "sig-xyz"}}
        }),
        serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0}),
        serde_json::json!({"type": "contentBlockStart", "contentBlockIndex": 1, "start": {}}),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 1,
            "delta": {"text": "the answer"}
        }),
        serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 1}),
    ];

    let events: Vec<IrStreamEvent> = frames
        .into_iter()
        .flat_map(|f| {
            reader.read_response_events(crate::codec::test_host::HOST, "", &f, &mut state)
        })
        .collect();

    assert!(
            events.iter().any(|e| matches!(
                e,
                IrStreamEvent::BlockStart { index: 0, block: IrBlockMeta::Thinking { .. }, refusal: _, }
            )),
            "a Thinking BlockStart must be lazily opened on the first reasoningContent delta; got {events:?}"
        );
    let thinking_text: String = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockDelta {
                delta: IrDelta::ThinkingDelta(t),
                ..
            } => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        thinking_text, "let me think",
        "the streamed reasoning text must be carried as ThinkingDeltas; got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockDelta { delta: IrDelta::SignatureDelta(s), .. } if s == "sig-xyz"
        )),
        "the reasoning signature must be carried as a SignatureDelta; got {events:?}"
    );

    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStart {
                    index: 0,
                    block: IrBlockMeta::Thinking { kind: None },
                    refusal: false,
                }
            )
            .is_none(),
        "a Thinking BlockStart must emit NO contentBlockStart frame; reasoning is streamed via \
         contentBlockDelta.reasoningContent"
    );

    let think_stop = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 0 },
        )
        .expect("a Thinking BlockStop must still emit a contentBlockStop");
    assert_eq!(think_stop.0, "contentBlockStop");

    let text_delta = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::ThinkingDelta("hello".to_string()),
            },
        )
        .expect("ThinkingDelta must produce a frame");
    assert_eq!(text_delta.0, "contentBlockDelta");
    assert_eq!(
        text_delta
            .1
            .pointer("/delta/reasoningContent/text")
            .and_then(|v| v.as_str()),
        Some("hello"),
        "a ThinkingDelta must re-emit as reasoningContent.text; got {}",
        text_delta.1
    );

    let sig_delta = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::SignatureDelta("sig-xyz".to_string()),
            },
        )
        .expect("SignatureDelta must produce a frame");
    assert_eq!(sig_delta.0, "contentBlockDelta");
    assert_eq!(
        sig_delta
            .1
            .pointer("/delta/reasoningContent/signature")
            .and_then(|v| v.as_str()),
        Some("sig-xyz"),
        "a SignatureDelta must re-emit as reasoningContent.signature; got {}",
        sig_delta.1
    );

    assert!(
        sig_delta
            .1
            .pointer("/delta/reasoningContent/redactedContent")
            .is_none(),
        "a real signature must not re-emit as redactedContent; got {}",
        sig_delta.1
    );
}

/// The STREAMING redacted-reasoning case. A `reasoningContent`
#[test]
fn test_stream_reasoning_redacted_round_trips() {
    use crate::codec::ir::{IrBlockMeta, IrDelta, IrStreamEvent};

    let reader = BedrockReader;
    let writer = BedrockWriter;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let frames = vec![
        serde_json::json!({"type": "messageStart", "role": "assistant"}),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"reasoningContent": {"redactedContent": "RVhBTVBMRQ=="}}
        }),
        serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0}),
    ];

    let events: Vec<IrStreamEvent> = frames
        .into_iter()
        .flat_map(|f| {
            reader.read_response_events(crate::codec::test_host::HOST, "", &f, &mut state)
        })
        .collect();

    assert!(
        events.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 0,
                block: IrBlockMeta::RedactedThinking, refusal: _,
            }
        )),
        "a RedactedThinking BlockStart must open for a streamed redactedContent delta; got {events:?}"
    );
    let redacted_delta = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockDelta {
            delta: IrDelta::RedactedReasoningDelta(s),
            ..
        } => Some(s.clone()),
        _ => None,
    });
    assert_eq!(
        redacted_delta.as_deref(),
        Some("RVhBTVBMRQ=="),
        "redacted reasoning bytes must ride on a typed RedactedReasoningDelta; got {events:?}"
    );

    assert!(
        !events.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockDelta {
                delta: IrDelta::ThinkingDelta(_),
                ..
            }
        )),
        "a redacted reasoning block must NOT emit a plaintext ThinkingDelta; got {events:?}"
    );

    let frame = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::RedactedReasoningDelta(
                    redacted_delta.expect("redacted delta present"),
                ),
            },
        )
        .expect("RedactedReasoningDelta must produce a frame");
    assert_eq!(frame.0, "contentBlockDelta");
    assert_eq!(
        frame
            .1
            .pointer("/delta/reasoningContent/redactedContent")
            .and_then(|v| v.as_str()),
        Some("RVhBTVBMRQ=="),
        "a RedactedReasoningDelta must re-emit the bytes as redactedContent; got {}",
        frame.1
    );
    assert!(
        frame
            .1
            .pointer("/delta/reasoningContent/signature")
            .is_none(),
        "a redacted reasoning delta must NOT leak as a plaintext signature; got {}",
        frame.1
    );
    assert!(
        frame.1.pointer("/delta/reasoningContent/text").is_none(),
        "a redacted reasoning delta must NOT leak as a plaintext text; got {}",
        frame.1
    );
}

/// Native Converse `guardContent` (inline Guardrails) blocks that appear
#[test]
fn test_guard_content_round_trips() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "system": [
            {"text": "be safe"},
            {"guardContent": {"text": {"text": "ground truth", "qualifiers": ["grounding_source"]}}}
        ],
        "messages": [
            {
                "role": "user",
                "content": [
                    {"guardContent": {"text": {"text": "user query", "qualifiers": ["query"]}}},
                    {"text": "hello"}
                ]
            }
        ]
    });
    let ir = reader.read_request(&body).expect("read_request");
    assert!(
        ir.extra.contains_key(GUARD_CONTENT_SENTINEL),
        "guardContent markers must be captured into extra; got {:?}",
        ir.extra
    );
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        out.get(GUARD_CONTENT_SENTINEL).is_none(),
        "the guardContent sentinel must not appear on the wire; got {out}"
    );
    assert_eq!(
        out, body,
        "guardContent markers must survive the round-trip at their original positions; got {out}"
    );
}

/// When BOTH `cachePoint` and `guardContent` markers occupy DISTINCT
#[test]
fn test_guard_content_and_cache_point_interleave_preserved() {
    let reader = BedrockReader;
    let writer = BedrockWriter;
    let body = serde_json::json!({
        "messages": [
            {
                "role": "user",
                "content": [
                    {"guardContent": {"text": {"text": "q", "qualifiers": ["query"]}}},
                    {"text": "a"},
                    {"cachePoint": {"type": "default"}},
                    {"text": "b"}
                ]
            }
        ]
    });
    let ir = reader.read_request(&body).expect("read_request");
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out, body,
        "interleaved guardContent + cachePoint markers must round-trip at their original \
             positions; got {out}"
    );
}

/// A `reasoningContent` block carrying neither known member (a hypothetical future union member)
#[test]
fn test_unknown_reasoning_member_does_not_panic() {
    let reader = BedrockReader;
    let body = serde_json::json!({
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"reasoningContent": {"futureMember": {"foo": "bar"}}},
                    {"text": "ok"}
                ]
            }
        ]
    });
    let ir = reader.read_request(&body).expect("read_request");

    assert!(
        !ir.messages[0]
            .content
            .iter()
            .any(|b| matches!(b, crate::codec::ir::IrBlock::Thinking { .. })),
        "an unknown reasoningContent member must not be mis-mapped to a Thinking block; got {:?}",
        ir.messages[0].content
    );
}

/// Helper: a minimal IR request carrying only a tool_choice (and one tool so toolConfig is valid).
#[test]
fn consecutive_user_turns_coalesce_for_alternation() {
    let text_msg = |role, t: &str| crate::codec::ir::IrMessage {
        role,
        content: vec![crate::codec::ir::IrBlock::Text {
            text: t.to_string(),
            cache_control: None,
            citations: vec![],
            refusal: false,
        }],
    };
    let req = crate::codec::ir::IrRequest {
        messages: vec![
            text_msg(crate::codec::ir::IrRole::User, "q"),
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Assistant,
                content: vec![crate::codec::ir::IrBlock::ToolUse {
                    thought_signature: None,
                    id: "t1".to_string(),
                    name: "get".to_string(),
                    input: serde_json::json!({}),
                    cache_control: None,
                }],
            },
            // A Tool-result turn (maps to "user") immediately followed by a real User turn:
            // assistant, user, user on the wire without coalescing.
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Tool,
                content: vec![crate::codec::ir::IrBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: vec![crate::codec::ir::IrBlock::Text {
                        text: "42".to_string(),
                        cache_control: None,
                        citations: vec![],
                        refusal: false,
                    }],
                    is_error: false,
                    cache_control: None,
                }],
            },
            text_msg(crate::codec::ir::IrRole::User, "thanks"),
        ],
        ..Default::default()
    };
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    let msgs = out
        .get("messages")
        .and_then(|m| m.as_array())
        .expect("messages array");

    let roles: Vec<&str> = msgs
        .iter()
        .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
        .collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "user"],
        "consecutive user turns must coalesce so roles alternate: {out}"
    );

    let last = msgs.last().expect("last message");
    let last_content = last
        .get("content")
        .and_then(|c| c.as_array())
        .expect("content array");
    assert!(
        last_content.iter().any(|b| b.get("toolResult").is_some()),
        "merged turn must keep the toolResult block: {out}"
    );
    assert!(
        last_content
            .iter()
            .any(|b| b.get("text").and_then(|t| t.as_str()) == Some("thanks")),
        "merged turn must keep the trailing user text: {out}"
    );
}

fn tool_choice_req(tc: Option<crate::codec::ir::IrToolChoice>) -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        tools: vec![crate::codec::ir::IrTool {
            name: "get_weather".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: None,

            hosted: None,

            strict: None,
        }],
        tool_choice: tc,
        ..Default::default()
    }
}

/// F3 conformance: a `tool_choice` with NO accompanying tools must NOT produce a `toolConfig`.
#[test]
fn tool_choice_without_tools_emits_no_tool_config() {
    for tc in [
        crate::codec::ir::IrToolChoice::Auto,
        crate::codec::ir::IrToolChoice::Required,
        crate::codec::ir::IrToolChoice::Tool {
            name: "get_weather".to_string(),
        },
    ] {
        let mut req = tool_choice_req(Some(tc.clone()));
        req.tools = vec![];
        let writer = BedrockWriter;
        let out = writer.write_request(crate::codec::test_host::HOST, &req);
        assert!(
            out.get("toolConfig").is_none(),
            "toolConfig must be omitted entirely when tool_choice={tc:?} has no tools: {out}"
        );
    }
}

/// F5 conformance: the common `image/jpg` media_type (and casing variants) must normalize to
#[test]
fn image_format_jpg_normalizes_to_jpeg() {
    for mt in ["image/jpg", "image/JPG", "image/Jpeg", "image/jpeg"] {
        let block = bedrock_image_block(&crate::codec::ir::IrImageSource::Base64 {
            media_type: mt.to_string(),
            data: "QUJD".to_string(),
        })
        .expect("image block");
        assert_eq!(
            block.get("format").and_then(|f| f.as_str()),
            Some("jpeg"),
            "media_type {mt:?} must emit Bedrock format=jpeg"
        );
    }

    for (mt, want) in [
        ("image/png", "png"),
        ("image/gif", "gif"),
        ("image/webp", "webp"),
    ] {
        let block = bedrock_image_block(&crate::codec::ir::IrImageSource::Base64 {
            media_type: mt.to_string(),
            data: "QUJD".to_string(),
        })
        .expect("image block");
        assert_eq!(block.get("format").and_then(|f| f.as_str()), Some(want));
    }
}

/// Bedrock `toolChoice:{any:{}}` (must call SOME tool) round-trips through the IR's
#[test]
fn test_bedrock_tool_choice_any_required_roundtrips() {
    let reader = BedrockReader;
    let j = serde_json::json!({
        "messages": [],
        "toolConfig": {
            "tools": [{"toolSpec": {"name": "get_weather", "inputSchema": {"json": {}}}}],
            "toolChoice": {"any": {}}
        }
    });
    let ir = reader.read_request(&j).expect("read ok");
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Required)
    );

    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let tc = out
        .get("toolConfig")
        .and_then(|t| t.get("toolChoice"))
        .expect("toolChoice emitted");
    assert_eq!(tc, &serde_json::json!({"any": {}}));
}

/// Bedrock `toolChoice:{tool:{name}}` (forced specific tool) round-trips through the IR's
#[test]
fn test_bedrock_tool_choice_specific_tool() {
    let reader = BedrockReader;
    let j = serde_json::json!({
        "messages": [],
        "toolConfig": {
            "tools": [{"toolSpec": {"name": "get_weather", "inputSchema": {"json": {}}}}],
            "toolChoice": {"tool": {"name": "get_weather"}}
        }
    });
    let ir = reader.read_request(&j).expect("read ok");
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Tool {
            name: "get_weather".to_string()
        })
    );

    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let tc = out
        .get("toolConfig")
        .and_then(|t| t.get("toolChoice"))
        .expect("toolChoice emitted");
    assert_eq!(tc, &serde_json::json!({"tool": {"name": "get_weather"}}));
}

/// `toolChoice.tool` (force a SPECIFIC tool) is documented Anthropic-Claude-only on
#[test]
fn bedrock_specific_tool_choice_warns_it_is_claude_only() {
    let req = tool_choice_req(Some(crate::codec::ir::IrToolChoice::Tool {
        name: "get_weather".to_string(),
    }));
    let writer = BedrockWriter;

    let (cap, cap_subscriber) = warnings();
    let subscriber = cap_subscriber.clone();
    let out = tracing::dispatcher::with_default(&subscriber, || {
        writer.write_request(crate::codec::test_host::HOST, &req)
    });

    let tc = out
        .get("toolConfig")
        .and_then(|t| t.get("toolChoice"))
        .expect("toolChoice still emitted");
    assert_eq!(tc, &serde_json::json!({"tool": {"name": "get_weather"}}));

    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("Claude-only")),
        "emitting toolChoice.tool must warn that it is Claude-only on Bedrock: {:?}",
        diagnostic_texts(&cap)
    );
}

/// `IrToolChoice::None` has no native Bedrock representation, so the writer must omit
#[test]
fn test_bedrock_tool_choice_none_omitted_on_write() {
    let req = tool_choice_req(Some(crate::codec::ir::IrToolChoice::None));
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    let tool_config = out
        .get("toolConfig")
        .expect("toolConfig with tools emitted");
    assert!(
        tool_config.get("toolChoice").is_none(),
        "Bedrock has no native 'none' — toolChoice must be omitted; got {tool_config:?}"
    );
    assert!(
        tool_config.get("tools").is_some(),
        "tools must still be emitted"
    );
}

/// A request with no native `toolChoice` reads back as `tool_choice == None` (no spurious
#[test]
fn test_bedrock_tool_choice_absent_is_none() {
    let reader = BedrockReader;
    let j = serde_json::json!({
        "messages": [],
        "toolConfig": {"tools": [{"toolSpec": {"name": "f", "inputSchema": {"json": {}}}}]}
    });
    let ir = reader.read_request(&j).expect("read ok");
    assert_eq!(ir.tool_choice, None);
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        out.get("toolConfig")
            .and_then(|t| t.get("toolChoice"))
            .is_none(),
        "no toolChoice should be emitted when none was provided"
    );
}

#[test]
fn test_bedrock_writer_clamps_temperature_above_one() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        temperature: Some(1.8),
        ..Default::default()
    };
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/inferenceConfig/temperature")
            .and_then(|v| v.as_f64()),
        Some(1.0),
        "an OpenAI-ingress temperature of 1.8 must clamp to 1.0 on the Bedrock writer; got {out}"
    );
}

/// Helper: a bare IR request with the given system / messages / tools and an EMPTY `extra`
fn cache_ctrl_req(
    system: Vec<crate::codec::ir::IrBlock>,
    messages: Vec<crate::codec::ir::IrMessage>,
    tools: Vec<crate::codec::ir::IrTool>,
) -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        system,
        messages,
        tools,
        ..Default::default()
    }
}

fn ephemeral() -> Option<crate::codec::ir::CacheControl> {
    Some(crate::codec::ir::CacheControl {
        kind: crate::codec::ir::CacheKind::Ephemeral,
    })
}

/// Write: an IR message Text block carrying `cache_control` must emit a Bedrock `cachePoint`
#[test]
fn test_cache_control_on_message_block_emits_cache_point() {
    let req = cache_ctrl_req(
        vec![],
        vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![
                crate::codec::ir::IrBlock::Text {
                    text: "cache me".to_string(),
                    cache_control: ephemeral(),
                    citations: vec![],
                    refusal: false,
                },
                crate::codec::ir::IrBlock::Text {
                    text: "but not this".to_string(),
                    cache_control: None,
                    citations: vec![],
                    refusal: false,
                },
            ],
        }],
        vec![],
    );
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);

    assert_eq!(
        out.pointer("/messages/0/content/0/text")
            .and_then(|v| v.as_str()),
        Some("cache me"),
    );
    assert_eq!(
        out.pointer("/messages/0/content/1/cachePoint"),
        Some(&serde_json::json!({"type": "default"})),
        "a cache_control block must emit a trailing cachePoint; got {out}"
    );
    assert_eq!(
        out.pointer("/messages/0/content/2/text")
            .and_then(|v| v.as_str()),
        Some("but not this"),
        "the non-cached block must follow, with no extra cachePoint; got {out}"
    );

    let count = out
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter(|b| b.get("cachePoint").is_some()).count())
        .unwrap_or(0);
    assert_eq!(count, 1, "exactly one cachePoint expected; got {out}");
}

/// Write: a system Text block carrying `cache_control` emits a trailing cachePoint in the
#[test]
fn test_cache_control_on_system_block_emits_cache_point() {
    let req = cache_ctrl_req(
        vec![crate::codec::ir::IrBlock::Text {
            text: "long static preamble".to_string(),
            cache_control: ephemeral(),
            citations: vec![],
            refusal: false,
        }],
        vec![],
        vec![],
    );
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        out.pointer("/system/0/text").and_then(|v| v.as_str()),
        Some("long static preamble"),
    );
    assert_eq!(
        out.pointer("/system/1/cachePoint"),
        Some(&serde_json::json!({"type": "default"})),
        "a system cache_control block must emit a trailing cachePoint; got {out}"
    );
}

/// Write: a tool definition carrying `cache_control` emits a `cachePoint` element in the
#[test]
fn test_cache_control_on_tool_emits_cache_point() {
    let req = cache_ctrl_req(
        vec![],
        vec![],
        vec![crate::codec::ir::IrTool {
            name: "get_weather".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: ephemeral(),

            hosted: None,

            strict: None,
        }],
    );
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        out.pointer("/toolConfig/tools/0/toolSpec/name")
            .and_then(|v| v.as_str()),
        Some("get_weather"),
    );
    assert_eq!(
        out.pointer("/toolConfig/tools/1/cachePoint"),
        Some(&serde_json::json!({"type": "default"})),
        "a tool cache_control must emit a trailing cachePoint in toolConfig.tools; got {out}"
    );
}

/// Read: a Bedrock `cachePoint` following a content block must map onto the preceding IR
#[test]
fn test_cache_point_maps_onto_preceding_block_cache_control() {
    let reader = BedrockReader;
    let wire = serde_json::json!({
        "system": [
            {"text": "preamble"},
            {"cachePoint": {"type": "default"}}
        ],
        "messages": [
            {"role": "user", "content": [
                {"text": "doc"},
                {"cachePoint": {"type": "default"}},
                {"text": "tail"}
            ]}
        ],
        "toolConfig": {
            "tools": [
                {"toolSpec": {"name": "t", "inputSchema": {"json": {}}}},
                {"cachePoint": {"type": "default"}}
            ]
        }
    });
    let ir = reader.read_request(&wire).expect("read_request");

    match &ir.system[0] {
        crate::codec::ir::IrBlock::Text { cache_control, .. } => {
            assert!(
                cache_control.is_some(),
                "system block must carry cache_control"
            );
        }
        other => panic!("expected Text, got {other:?}"),
    }

    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::Text {
            text,
            cache_control,
            ..
        } => {
            assert_eq!(text, "doc");
            assert!(
                cache_control.is_some(),
                "preceding block must carry cache_control"
            );
        }
        other => panic!("expected Text, got {other:?}"),
    }
    match &ir.messages[0].content[1] {
        crate::codec::ir::IrBlock::Text {
            text,
            cache_control,
            ..
        } => {
            assert_eq!(text, "tail");
            assert!(
                cache_control.is_none(),
                "trailing block must NOT carry cache_control"
            );
        }
        other => panic!("expected Text, got {other:?}"),
    }

    assert!(
        ir.tools[0].cache_control.is_some(),
        "the tool preceding a tools-array cachePoint must carry cache_control"
    );
}

/// Round-trip, cross-protocol shape: reading a cachePoint-bearing body into the IR and then
#[test]
fn test_cache_control_round_trip_via_field_only() {
    let reader = BedrockReader;
    let wire = serde_json::json!({
        "messages": [
            {"role": "user", "content": [
                {"text": "doc"},
                {"cachePoint": {"type": "default"}}
            ]}
        ]
    });
    let mut ir = reader.read_request(&wire).expect("read_request");

    ir.extra.clear();
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/messages/0/content/0/text")
            .and_then(|v| v.as_str()),
        Some("doc"),
    );
    assert_eq!(
            out.pointer("/messages/0/content/1/cachePoint"),
            Some(&serde_json::json!({"type": "default"})),
            "the cachePoint must be re-derived from cache_control after the stash is cleared; got {out}"
        );
}

/// No double-emit: on the SAME-protocol path the positional stash AND the first-class
#[test]
fn test_same_protocol_cache_point_no_double_emit() {
    let reader = BedrockReader;
    let wire = serde_json::json!({
        "messages": [
            {"role": "user", "content": [
                {"text": "doc"},
                {"cachePoint": {"type": "default"}},
                {"text": "tail"}
            ]}
        ]
    });
    let ir = reader.read_request(&wire).expect("read_request");
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let count = out
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter(|b| b.get("cachePoint").is_some()).count())
        .unwrap_or(0);
    assert_eq!(
        count, 1,
        "same-protocol path must emit exactly one cachePoint; got {out}"
    );
    assert_eq!(
        out, wire,
        "same-protocol body must round-trip byte-identically; got {out}"
    );
}

/// Warn path: a malformed image media_type (an empty subtype, an unprefixed word) or one outside
#[test]
fn test_malformed_media_type_warns_and_drops() {
    for mt in ["image/", "garbage", "image/bmp", "image/svg+xml"] {
        let (cap, cap_subscriber) = warnings();
        let block = tracing::dispatcher::with_default(&cap_subscriber, || {
            bedrock_image_block(&crate::codec::ir::IrImageSource::Base64 {
                media_type: mt.to_string(),
                data: ("QQ==").to_string(),
            })
        });
        assert!(block.is_none(), "{mt:?} must be dropped, got {block:?}");
        assert_eq!(
            diagnostic_texts(&cap)
                .iter()
                .filter(|event| event.contains("dropping image block on Bedrock egress"))
                .count(),
            1,
            "{mt:?}: one warn: {:?}",
            diagnostic_texts(&cap)
        );
    }

    let jpeg = bedrock_image_block(&crate::codec::ir::IrImageSource::Base64 {
        media_type: "image/jpeg".to_string(),
        data: ("QQ==").to_string(),
    })
    .expect("jpeg must emit a block");
    assert_eq!(
        jpeg.pointer("/format").and_then(|v| v.as_str()),
        Some("jpeg")
    );
}

/// A cross-protocol IR carrying a JSON-schema `response_format` reaching a Bedrock lane that declares
#[test]
fn test_write_request_response_format_projects_output_config() {
    let writer = BedrockWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        response_format: Some(crate::codec::ir::IrResponseFormat {
            json: true,
            schema: Some(serde_json::json!({"type": "object"})),
            name: Some("s".to_string()),
            strict: None,
            description: None,
        }),
        ..Default::default()
    };
    let default_lane = writer.write_request(crate::codec::test_host::HOST, &req);
    assert!(default_lane.get("outputConfig").is_none(), "{default_lane}");
    let out = writer.write_request_for_lane(
        crate::codec::test_host::HOST,
        &req,
        "anthropic.claude-opus-4-7",
        &LaneCaps {
            native_structured_output: true,
            ..Default::default()
        },
    );
    let wire = serde_json::to_string(&out).unwrap();
    assert!(
        !wire.contains("response_format"),
        "no foreign response_format key may reach the Bedrock wire; got {wire}"
    );
    assert!(
        out.get("response_format").is_none(),
        "no top-level response_format key may be present; got {out}"
    );
    let tf = &out["outputConfig"]["textFormat"];
    assert_eq!(tf["type"], "json_schema", "{out}");
    assert_eq!(tf["structure"]["jsonSchema"]["name"], "s", "{out}");
    let schema: serde_json::Value = serde_json::from_str(
        tf["structure"]["jsonSchema"]["schema"]
            .as_str()
            .expect("schema is a string"),
    )
    .expect("schema string parses");
    assert_eq!(schema, serde_json::json!({"type": "object"}), "{out}");
}

#[test]
fn bedrock_stream_framing_emits_one_metadata_delta_then_guards_duplicate() {
    use super::StreamFraming;
    let usage = crate::codec::ir::IrUsage {
        input_tokens: 5,
        output_tokens: 2,
        ..Default::default()
    };
    let mut framing = BedrockStreamFraming::default();

    fn metadata_delta_count(events: &[crate::codec::ir::IrStreamEvent]) -> usize {
        events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::codec::ir::IrStreamEvent::MessageDelta {
                        stop_reason: None,
                        ..
                    }
                )
            })
            .count()
    }

    let first = framing
        .on_combined_stop_delta(crate::codec::ir::IrStopReason::EndTurn, None, &usage)
        .expect("first stop-delta returns events");
    assert_eq!(
        metadata_delta_count(&first),
        1,
        "first call must emit exactly ONE usage-only metadata delta; got {first:?}"
    );

    let second = framing
        .on_combined_stop_delta(crate::codec::ir::IrStopReason::EndTurn, None, &usage)
        .expect("second stop-delta returns events");
    assert_eq!(
            metadata_delta_count(&second),
            0,
            "second call must emit ZERO metadata deltas (the !emitted guard suppresses the duplicate); got {second:?}"
        );
}

/// A CACHE-ONLY stop-delta (`input_tokens == 0 && output_tokens == 0`
#[test]
fn cache_only_usage_emits_metadata_inline() {
    fn metadata_delta_count(events: &[crate::codec::ir::IrStreamEvent]) -> usize {
        events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::codec::ir::IrStreamEvent::MessageDelta {
                        stop_reason: None,
                        ..
                    }
                )
            })
            .count()
    }
    let usage = crate::codec::ir::IrUsage {
        cache_read_input_tokens: Some(4096),

        detail: crate::codec::ir::IrUsageDetail::default(),
        ..Default::default()
    };
    let mut framing = BedrockStreamFraming::default();
    let evs = framing
        .on_combined_stop_delta(crate::codec::ir::IrStopReason::EndTurn, None, &usage)
        .expect("stop-delta returns events");
    assert_eq!(
            metadata_delta_count(&evs),
            1,
            "a cache-only stop-delta must emit ONE metadata frame inline with the real cache tokens, not defer a zero-usage frame; got {evs:?}"
        );
}

/// F4 coalescing (headline Bedrock behavior): Converse requires STRICTLY ALTERNATING
#[test]
fn consecutive_tool_result_turns_coalesce_into_single_user_message() {
    let req = crate::codec::ir::IrRequest {
        messages: vec![
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::text(
                    "run both tools".to_string(),
                )],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Assistant,
                content: vec![crate::codec::ir::IrBlock::ToolUse {
                    thought_signature: None,
                    id: "t1".to_string(),
                    name: "a".to_string(),
                    input: serde_json::json!({}),
                    cache_control: None,
                }],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Tool,
                content: vec![crate::codec::ir::IrBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: vec![crate::codec::ir::IrBlock::text("r1".to_string())],
                    is_error: false,
                    cache_control: None,
                }],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Tool,
                content: vec![crate::codec::ir::IrBlock::ToolResult {
                    tool_use_id: "t2".to_string(),
                    content: vec![crate::codec::ir::IrBlock::text("r2".to_string())],
                    is_error: false,
                    cache_control: None,
                }],
            },
        ],
        ..Default::default()
    };
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    let msgs = out
        .get("messages")
        .and_then(|m| m.as_array())
        .expect("messages array");

    let roles: Vec<&str> = msgs
        .iter()
        .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
        .collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "user"],
        "consecutive same-role (tool→user) turns must coalesce to keep strict alternation: {out}"
    );

    let last_content = msgs
        .last()
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .expect("last content array");
    assert_eq!(
        last_content.len(),
        2,
        "both tool_result blocks must land in the single coalesced user turn: {out}"
    );
}

/// An UNKNOWN native `stopReason` maps to `IrStopReason::Other` on read (ir.rs:186), and `Other`
#[test]
fn unknown_stop_reason_maps_to_other_and_degrades_to_end_turn() {
    assert_eq!(
        stop_reason_map("some_future_reason"),
        crate::codec::ir::IrStopReason::Other
    );
    let body = serde_json::json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
        "stopReason": "some_future_reason",
        "usage": {"inputTokens": 3, "outputTokens": 1, "totalTokens": 4}
    });
    let resp = BedrockReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::Other)
    );
    let writer = BedrockWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out.get("stopReason").and_then(|v| v.as_str()),
        Some("end_turn"),
        "Other must degrade to end_turn, never leak the foreign token into Converse's enum"
    );
}

/// Bedrock cache tokens are ALREADY ADDITIVE (ir.rs:457): unlike OpenAI/Gemini (which subtract the
#[test]
fn cache_usage_is_additive_input_not_reduced() {
    let body = serde_json::json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 10,
            "outputTokens": 5,
            "totalTokens": 15,
            "cacheReadInputTokens": 1000,
            "cacheWriteInputTokens": 200
        }
    });
    let resp = BedrockReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        resp.usage.input_tokens, 10,
        "Bedrock inputTokens stored AS-IS (additive convention), NOT reduced by cache reads"
    );
    assert_eq!(resp.usage.cache_read_input_tokens, Some(1000));
    assert_eq!(resp.usage.cache_creation_input_tokens, Some(200));

    assert_eq!(resp.usage.billable_tokens(), 1215);

    let writer = BedrockWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out.pointer("/usage/cacheReadInputTokens")
            .and_then(|v| v.as_u64()),
        Some(1000)
    );
    assert_eq!(
        out.pointer("/usage/cacheWriteInputTokens")
            .and_then(|v| v.as_u64()),
        Some(200)
    );
    assert_eq!(
        out.pointer("/usage/inputTokens").and_then(|v| v.as_u64()),
        Some(10)
    );
}

/// BEDROCK ADDITIVE-CACHE billing guard: the neutral projection the MONEY PATH actually bills from
#[test]
fn bedrock_input_tier_excludes_cache_at_billing_boundary() {
    let body = serde_json::json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 10,
            "outputTokens": 5,
            "totalTokens": 15,
            "cacheReadInputTokens": 1000,
            "cacheWriteInputTokens": 200
        }
    });
    let resp = BedrockReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");

    let tu = resp.usage.to_token_usage();
    let tier = crate::codec::usage_census::tier_usage(&tu);
    let u = |k: &str| tier.usage_units.get(k).copied().unwrap_or(0);
    assert_eq!(
        u(busbar_contract::records::UNIT_INPUT),
        10,
        "the INPUT tier must be Bedrock's raw uncached inputTokens (10), NOT input+cache — cache \
         folded into input would over-bill the input rate on cache tokens"
    );
    assert_eq!(
        u(busbar_contract::records::UNIT_CACHE_READ),
        1000,
        "cache-read tokens must bill under the cache_read tier, not input"
    );
    assert_eq!(
        u(busbar_contract::records::UNIT_CACHE_WRITE),
        200,
        "cache-write tokens must bill under the cache_write tier, not input"
    );
    assert_eq!(u(busbar_contract::records::UNIT_OUTPUT), 5);

    assert_ne!(
        u(busbar_contract::records::UNIT_INPUT),
        10 + 1000 + 200,
        "inputTokens must never be treated as cache-INCLUSIVE for Bedrock (additive convention)"
    );
}

/// AWS Converse's `usage.totalTokens` is the TOTAL of every token the call consumed, cache tokens
#[test]
fn total_tokens_includes_cache_tokens_on_both_write_paths() {
    let wire_total = 10u64 + 5 + 1000 + 200;
    let body = serde_json::json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 10,
            "outputTokens": 5,
            "totalTokens": wire_total,
            "cacheReadInputTokens": 1000,
            "cacheWriteInputTokens": 200
        }
    });
    let resp = BedrockReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    let writer = BedrockWriter;

    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out.pointer("/usage/totalTokens").and_then(|v| v.as_u64()),
        Some(wire_total),
        "buffered totalTokens must be inputTokens + outputTokens + cacheRead + cacheWrite, the \
         AWS arithmetic — a round-trip that drops the cache tokens under-reports the call"
    );

    let part = |k: &str| {
        out.pointer(&format!("/usage/{k}"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    assert_eq!(
        part("inputTokens")
            + part("outputTokens")
            + part("cacheReadInputTokens")
            + part("cacheWriteInputTokens"),
        part("totalTokens"),
        "the emitted parts must sum to the emitted totalTokens"
    );

    let usage_only = IrStreamEvent::MessageDelta {
        stop_reason: None,
        stop_sequence: None,
        usage: resp.usage.clone(),
        stop_detail: None,
    };
    let (et, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &usage_only)
        .expect("usage-only delta must emit a frame");
    assert_eq!(et, "metadata");
    assert_eq!(
        payload
            .pointer("/usage/totalTokens")
            .and_then(|v| v.as_u64()),
        Some(wire_total),
        "the streamed metadata frame must report the same total as the buffered body for the \
         same usage — a stream/buffered split is a second, disagreeing answer"
    );
}

/// REGRESSION: The native AWS Bedrock ConverseStream `ContentBlockStart$start`
#[test]
fn test_stream_text_delta_lazily_opens_block_start() {
    use crate::codec::ir::IrStreamEvent;

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = BedrockReader;

    let events: Vec<_> = vec![
        serde_json::json!({"type": "messageStart", "role": "assistant"}),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"text": "Hello"}
        }),
        serde_json::json!({
            "type": "contentBlockDelta",
            "contentBlockIndex": 0,
            "delta": {"text": ", world!"}
        }),
        serde_json::json!({"type": "contentBlockStop", "contentBlockIndex": 0}),
        serde_json::json!({"type": "messageStop", "stopReason": "end_turn"}),
        serde_json::json!({
            "type": "metadata",
            "usage": {"inputTokens": 10, "outputTokens": 5}
        }),
    ]
    .into_iter()
    .flat_map(|data| {
        reader.read_response_events(crate::codec::test_host::HOST, "", &data, &mut state)
    })
    .collect();

    let first_block_start = events.iter().position(|e| {
        matches!(
            e,
            IrStreamEvent::BlockStart {
                block: crate::codec::ir::IrBlockMeta::Text,
                ..
            }
        )
    });
    let first_text_delta = events.iter().position(|e| {
        matches!(
            e,
            IrStreamEvent::BlockDelta {
                delta: crate::codec::ir::IrDelta::TextDelta(_),
                ..
            }
        )
    });
    let bs = first_block_start.expect("a Text BlockStart must be emitted for lazy text open");
    let bd = first_text_delta.expect("a TextDelta must be present");
    assert!(
        bs < bd,
        "the Text BlockStart (idx {bs}) must precede the first TextDelta (idx {bd}); \
         an orphaned content_block_delta with no BlockStart is the defect"
    );

    if let IrStreamEvent::BlockStart { index, .. } = &events[bs] {
        assert_eq!(*index, 0, "lazily-opened Text block must be at index 0");
    }
}

/// REGRESSION: The AWS Bedrock Converse `ContentBlock` union includes top-level
#[test]
fn test_roundtrip_preserves_document_and_video_blocks() {
    let reader = BedrockReader;
    let writer = BedrockWriter;

    let wire = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [
                {"text": "Summarize the attachment."},
                {"document": {
                    "format": "pdf",
                    "name": "report",
                    "source": {"bytes": "JVBERi0xLjQK"}
                }},
                {"text": "and describe the clip"},
                {"video": {
                    "format": "mp4",
                    "source": {"s3Location": {"uri": "s3://bucket/clip.mp4"}}
                }}
            ]
        }],
        "inferenceConfig": {"maxTokens": 256}
    });

    let ir = reader
        .read_request(&wire)
        .expect("read round-trip should succeed");
    let wire_after = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(
        wire, wire_after,
        "same-protocol wire round-trip must preserve document/video blocks at their original \
         positions, byte-identically"
    );

    let content = wire_after["messages"][0]["content"].as_array().unwrap();
    assert!(
        content.iter().any(|b| b.get("document").is_some()),
        "the document block must be present on Bedrock egress"
    );
    assert!(
        content.iter().any(|b| b.get("video").is_some()),
        "the video block must be present on Bedrock egress"
    );
}

#[test]
fn bedrock_orphan_content_block_delta_before_start_is_dropped() {
    use crate::codec::ir::IrStreamEvent;
    let reader = BedrockReader;

    let mut st = crate::codec::ir::StreamDecodeState::default();
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockDelta", "contentBlockIndex": 0, "delta": {"text": "orphan"}
        }),
        &mut st,
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, IrStreamEvent::BlockDelta { .. })),
        "a text delta before message start must not emit an orphan BlockDelta: {evs:?}"
    );

    let mut st2 = crate::codec::ir::StreamDecodeState::default();
    let evs2 = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "contentBlockDelta", "contentBlockIndex": 0,
            "delta": {"toolUse": {"input": "{\"a\":1}"}}
        }),
        &mut st2,
    );
    assert!(
        !evs2
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockDelta { .. })),
        "a toolUse delta before its block opened must not emit an orphan BlockDelta: {evs2:?}"
    );
}

/// A same-dialect buffered answer is relayed verbatim in every dialect, Bedrock included: no
#[test]
fn a_same_dialect_buffered_answer_gets_no_translator_in_any_dialect() {
    for d in [
        "anthropic",
        "bedrock",
        "cohere",
        "gemini",
        "openai",
        "responses",
    ] {
        assert!(
            crate::codec::proto_stream::new_stream_translator_on(
                d,
                d,
                false,
                busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT
            )
            .is_none(),
            "{d}: a same-dialect buffered answer is relayed verbatim"
        );
    }
    assert!(crate::codec::proto_stream::new_stream_translator_on(
        "bedrock",
        "anthropic",
        false,
        busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT
    )
    .is_none());
}

#[test]
fn streaming_metadata_metrics_is_always_present_and_never_overwrites_upstream() {
    let framing = BedrockStreamFraming::default();

    let mut data =
        serde_json::json!({"usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}});
    framing.inject_streaming_metrics(ET_METADATA, &mut data, None);
    assert_eq!(
        data.pointer("/metrics/latencyMs").and_then(|v| v.as_u64()),
        Some(0)
    );

    let mut data = serde_json::json!({"usage": {}, "metrics": {"latencyMs": 31}});
    framing.inject_streaming_metrics(ET_METADATA, &mut data, Some(0));
    assert_eq!(
        data.pointer("/metrics/latencyMs").and_then(|v| v.as_u64()),
        Some(31)
    );

    let mut other = serde_json::json!({"stopReason": "end_turn"});
    framing.inject_streaming_metrics(ET_MESSAGE_STOP, &mut other, Some(0));
    assert!(other.get("metrics").is_none());
}

#[test]
fn buffered_to_eventstream_metadata_carries_metrics_even_without_timing() {
    let ir = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hi")],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 3,
            output_tokens: 4,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bytes = bedrock_response_to_eventstream(crate::codec::test_host::HOST, &ir, None);
    let frames = crate::codec::eventstream::drain_frames(&mut bytes);
    let (_, payload) = frames
        .iter()
        .find(|(et, _)| et == ET_METADATA)
        .expect("a metadata frame");
    let v: serde_json::Value = serde_json::from_slice(payload).unwrap();
    assert_eq!(
        v.pointer("/metrics/latencyMs").and_then(|m| m.as_u64()),
        Some(0)
    );
    assert_eq!(v["usage"]["totalTokens"], 7);
}

/// A two-citation delta driven straight at `write_response_events` must emit BOTH citations. The
#[test]
fn bedrock_writer_emits_every_citation_in_a_multi_citation_delta() {
    let cit = |title: &str| crate::codec::ir::IrCitation {
        domain: None,
        kind: Some("web_search_result_location".to_string()),
        cited_text: Some("quoted".to_string()),
        title: Some(title.to_string()),
        url: Some("https://example.invalid/a".to_string()),
        document_index: None,
        start_index: Some(0),
        end_index: Some(6),
        encrypted_index: None,
        raw: None,
        ..Default::default()
    };
    let writer = BedrockWriter;
    let frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::CitationsDelta(vec![cit("first"), cit("second")]),
        },
    );

    let titles: Vec<Option<&str>> = frames
        .iter()
        .filter(|(_, d)| d.pointer("/delta/citation").is_some())
        .map(|(_, d)| d.pointer("/delta/citation/title").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        titles,
        vec![Some("first"), Some("second")],
        "both citations must reach the wire, in order, got {frames:?}"
    );
}

#[test]
fn status_word_golden() {
    let golden: [(StatusClass, &str); 9] = [
        (StatusClass::RateLimit, "ThrottlingException"),
        (StatusClass::Overloaded, "ServiceUnavailableException"),
        (StatusClass::ServerError, "InternalServerException"),
        (StatusClass::Timeout, "ModelStreamErrorException"),
        (StatusClass::Network, "InternalServerException"),
        (StatusClass::Auth, "InternalServerException"),
        (StatusClass::Billing, "InternalServerException"),
        (StatusClass::ClientError, "ValidationException"),
        (StatusClass::ContextLength, "ValidationException"),
    ];
    for (class, word) in golden {
        let err = CanonicalSignal {
            class,
            provider_signal: None,
            retry_after: None,
        };
        let (name, message) = bedrock_stream_exception_for(&err);
        assert_eq!(name, word, "class={class:?}");
        assert_eq!(message, word, "an absent signal falls back to the name");
    }
}

#[test]
fn the_served_tier_is_read_and_written() {
    let answer = serde_json::json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
        "stopReason": "end_turn",
        "usage": {"inputTokens": 3, "outputTokens": 1, "totalTokens": 4},
        "serviceTier": {"type": "priority"}
    });
    let resp = BedrockReader
        .read_response(crate::codec::test_host::HOST, &answer)
        .expect("reads");
    assert_eq!(resp.usage.detail.service_tier.as_deref(), Some("priority"));
    let writer = BedrockWriter;
    let written = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(written["serviceTier"]["type"], "priority");

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events: Vec<_> = [
        serde_json::json!({"type": "messageStop", "stopReason": "end_turn"}),
        serde_json::json!({"type": "metadata", "usage": {"inputTokens": 3, "outputTokens": 1},
            "serviceTier": {"type": "flex"}}),
    ]
    .into_iter()
    .flat_map(|data| {
        BedrockReader.read_response_events(crate::codec::test_host::HOST, "", &data, &mut state)
    })
    .collect();
    let tier = events.iter().find_map(|e| match e {
        crate::codec::ir::IrStreamEvent::MessageDelta { usage, .. } => {
            usage.detail.service_tier.clone()
        }
        _ => None,
    });
    assert_eq!(tier.as_deref(), Some("flex"));
}

#[test]
fn the_tails_sigv4_parameters_restate_the_signing_scheme() {
    let Some(busbar_contract::protocol::EgressScheme::SigV4 {
        service,
        region_of_host,
        default_region,
        content_type,
    }) = super::DECL.egress_scheme
    else {
        panic!("the signing dialect declares a SigV4 scheme");
    };
    let params: serde_json::Value =
        serde_json::from_str(crate::dialect::SIGNED_PARAMS).expect("the parameters are JSON");
    assert_eq!(params["service"], serde_json::json!(service));
    assert_eq!(params["content_type"], serde_json::json!(content_type));
    assert_eq!(
        params["region"]["default"],
        serde_json::json!(default_region)
    );
    let labels: Vec<&str> = params["region"]["host_label_after"]
        .as_array()
        .expect("the region is read after the service labels")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();

    for label in &labels {
        assert_eq!(
            region_of_host(&format!("{label}.eu-west-3.amazonaws.com")),
            Some("eu-west-3"),
            "{label}"
        );
    }
    assert_eq!(region_of_host("runtime.eu-west-3.amazonaws.com"), None);
    assert!(params["region"]["unread"]
        .as_str()
        .is_some_and(|w| w.contains("defaulting SigV4 scope to us-east-1")));
}

/// The production request translate steps (reader → egress seam → writer → shim strip), with the
fn rehome_xreq(
    ingress: &'static str,
    egress: &str,
    body: &serde_json::Value,
) -> (crate::codec::ir::IrRequest, serde_json::Value) {
    let ingress_p = crate::codec::proto_codec::protocol_for(ingress).expect("ingress");
    let egress_p = crate::codec::proto_codec::protocol_for(egress).expect("egress");
    let mut req = ingress_p.reader().read_request(body).expect("read_request");
    crate::codec::chat_handle::chat_prepare_for_egress(
        &mut req,
        &crate::codec::egress_prep::EgressPrep {
            thought_signature_fill: false,
            ingress_protocol: ingress,
            egress_requires_max_tokens: egress_p.decl().is_some_and(|d| d.requires_max_tokens),
            lane_default_max_tokens: None,
            global_default_max_tokens: 4096,
            reasoning_allowed: true,
            reasoning_budgets: crate::codec::ir::REASONING_BUDGET_DEFAULTS,
            prompt_caching_allowed: true,
            cache_control_cap: None,
            lane_caps: Default::default(),
        },
    );
    let mut out = egress_p
        .writer()
        .write_request(crate::codec::test_host::HOST, &req);
    crate::codec::wire_shim::strip_router_shim_keys(&mut out, egress);
    (req, out)
}

#[test]
fn openai_tool_choice_none_onto_bedrock_forwards_and_reports_the_drop() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{
            "type": "function",
            "function": {"name": "get_weather", "parameters": {"type": "object"}}
        }],
        "tool_choice": "none"
    });
    let (req, out) = rehome_xreq("openai", "bedrock", &body);
    assert!(out.as_object().is_some_and(|o| !o.is_empty()), "{out}");
    assert_eq!(
        out.pointer("/toolConfig/tools/0/toolSpec/name"),
        Some(&serde_json::json!("get_weather")),
        "{out}"
    );
    assert!(out.pointer("/toolConfig/toolChoice").is_none(), "{out}");
    let dropped = crate::codec::proto_codec::protocol_for("bedrock")
        .expect("bedrock")
        .writer()
        .dropped_egress_controls(&req);
    assert!(dropped.contains(&"tool_choice=none"), "{dropped:?}");
}
