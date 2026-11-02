use super::*;

#[test]
fn cohere_stop_reason_codec_round_trips_and_never_leaks() {
    use crate::codec::ir::IrStopReason as S;

    assert_eq!(read_cohere_stop_reason(COHERE_FINISH_ERROR), S::Error);
    assert_eq!(write_cohere_stop_reason(S::Error), "ERROR");
    assert_eq!(
        read_cohere_stop_reason(COHERE_FINISH_ERROR_TOXIC),
        S::Safety
    );

    assert_eq!(write_cohere_stop_reason(S::Safety), "COMPLETE");

    assert_eq!(read_cohere_stop_reason("TIMEOUT"), S::Other);

    assert_eq!(read_cohere_stop_reason("ERROR_LIMIT"), S::Other);
    assert_eq!(write_cohere_stop_reason(S::Refusal), "COMPLETE");
    assert_eq!(write_cohere_stop_reason(S::Other), "COMPLETE");
}

#[test]
fn test_write_request() {
    let ir = crate::codec::ir::IrRequest {
        system: vec![crate::codec::ir::IrBlock::text(
            "You are helpful.".to_string(),
        )],
        messages: vec![
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Assistant,
                content: vec![crate::codec::ir::IrBlock::ToolUse {
                    thought_signature: None,
                    id: "t1".to_string(),
                    name: "f".to_string(),
                    input: serde_json::json!({"x": 1}),
                    cache_control: None,
                }],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Tool,
                content: vec![crate::codec::ir::IrBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: vec![crate::codec::ir::IrBlock::text("result text".to_string())],
                    is_error: false,
                    cache_control: None,
                }],
            },
        ],
        tools: vec![crate::codec::ir::IrTool {
            name: "f".to_string(),
            description: Some("..".to_string()),
            input_schema: serde_json::json!({}),
            cache_control: None,
            hosted: None,
            strict: None,
        }],
        max_tokens: Some(1024),
        temperature: Some(0.7),
        ..Default::default()
    };

    let writer = CohereWriter;
    let json = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert!(json.get("messages").is_some());
    let msgs = json.get("messages").unwrap().as_array().unwrap();

    assert_eq!(msgs[0].get("role"), Some(&serde_json::json!("system")));
    assert_eq!(
        msgs[0].get("content"),
        Some(&serde_json::json!("You are helpful."))
    );
    assert_eq!(msgs[1].get("role"), Some(&serde_json::json!("user")));
    assert_eq!(msgs[2].get("role"), Some(&serde_json::json!("assistant")));

    let tool_calls = msgs[2].get("tool_calls").unwrap().as_array().unwrap();
    assert_eq!(
        tool_calls[0]
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str()),
        Some("f")
    );

    let tools_arr = json.get("tools").unwrap().as_array().unwrap();
    assert_eq!(
        tools_arr[0]
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str()),
        Some("f")
    );

    assert_eq!(json.get("max_tokens"), Some(&serde_json::json!(1024)));
    assert_eq!(json.get("temperature"), Some(&serde_json::json!(0.7)));
}

#[test]
fn test_read_request_roundtrip() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::text("user msg".to_string())],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Assistant,
                content: vec![crate::codec::ir::IrBlock::text("assistant msg".to_string())],
            },
        ],
        max_tokens: Some(512),
        temperature: Some(0.7),
        stream: true,
        ..Default::default()
    };

    let writer = CohereWriter;
    let reader = CohereReader;
    let json = writer.write_request(crate::codec::test_host::HOST, &ir);
    let ir2 = reader
        .read_request(&json)
        .expect("read_request should succeed");

    assert_eq!(ir, ir2);
}

#[test]
fn test_read_response() {
    let json = serde_json::json!({
        "id": "msg_123",
        "finish_reason": COHERE_FINISH_TOOL_CALL,
        "message": {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"location": "SF"}}
            ]
        },
        "usage": {"tokens": {"input_tokens": 10, "output_tokens": 5}}
    });

    let reader = CohereReader;
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read_response should succeed");

    assert_eq!(resp.role, crate::codec::ir::IrRole::Assistant);
    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::ToolUse)
    );
    assert_eq!(resp.usage.input_tokens, 10);

    assert_eq!(resp.id.as_deref(), Some("msg_123"));
}

#[test]
fn test_write_response_roundtrip() {
    let json = serde_json::json!({
        "id": "c14c80c3-18eb-4519-9460-6c92edd8cfb4",
        "finish_reason": COHERE_FINISH_COMPLETE,
        "message": {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
        "usage": {"tokens": {"input_tokens": 10, "output_tokens": 5}}
    });

    let reader = CohereReader;
    let writer = CohereWriter;
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read_response should succeed");
    let json2 = writer.write_response(crate::codec::test_host::HOST, &resp);

    assert_eq!(json, json2);
}

#[test]
fn test_stream_fanout() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_MESSAGE_START, "delta": {"message": {"role": "assistant"}}}),
        &mut state,
    );
    assert_eq!(evs.len(), 1);
    assert!(matches!(
        evs[0],
        crate::codec::ir::IrStreamEvent::MessageStart { .. }
    ));

    let evs = reader.read_response_events(crate::codec::test_host::HOST, "", &serde_json::json!({"type": ET_CONTENT_START, "index": 0, "delta": {"message": {"content": {"type": "text", "text": ""}}}}), &mut state);
    assert_eq!(evs.len(), 1);
    assert!(matches!(
        evs[0],
        crate::codec::ir::IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: _,
        }
    ));

    let evs = reader.read_response_events(crate::codec::test_host::HOST, "", &serde_json::json!({"type": ET_CONTENT_DELTA, "index": 0, "delta": {"message": {"content": "he"}}}), &mut state);
    assert_eq!(evs.len(), 1);
    let crate::codec::ir::IrStreamEvent::BlockDelta {
        index,
        delta: crate::codec::ir::IrDelta::TextDelta(ref t),
    } = &evs[0]
    else {
        panic!(
            "content-delta must decode to a TEXT delta, got {:?}",
            evs[0]
        );
    };
    assert_eq!(
        *index, 0,
        "the delta must carry the block index cohere sent"
    );
    assert_eq!(t, "he");

    let evs = reader.read_response_events(crate::codec::test_host::HOST, "", &serde_json::json!({"type": ET_CONTENT_DELTA, "index": 0, "delta": {"message": {"content": "llo"}}}), &mut state);
    assert_eq!(evs.len(), 1);
    let crate::codec::ir::IrStreamEvent::BlockDelta {
        index,
        delta: crate::codec::ir::IrDelta::TextDelta(ref t),
    } = &evs[0]
    else {
        panic!(
            "content-delta must decode to a TEXT delta, got {:?}",
            evs[0]
        );
    };
    assert_eq!(
        *index, 0,
        "the delta must carry the block index cohere sent"
    );
    assert_eq!(t, "llo");

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_CONTENT_END, "index": 0}),
        &mut state,
    );
    assert_eq!(evs.len(), 1);
    assert!(matches!(
        evs[0],
        crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
    ));

    let evs = reader.read_response_events(crate::codec::test_host::HOST, "", &serde_json::json!({"type": ET_MESSAGE_END, "delta": {"finish_reason": COHERE_FINISH_COMPLETE, "usage": {"tokens": {"input_tokens": 10, "output_tokens": 5}}}}), &mut state);
    assert_eq!(evs.len(), 2);
    let crate::codec::ir::IrStreamEvent::MessageDelta {
        stop_reason,
        ref usage,
        ..
    } = &evs[0]
    else {
        panic!(
            "message-end must emit a MessageDelta first, got {:?}",
            evs[0]
        );
    };
    assert_eq!(*stop_reason, Some(crate::codec::ir::IrStopReason::EndTurn));
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 5);
    assert!(matches!(
        evs[1],
        crate::codec::ir::IrStreamEvent::MessageStop
    ));
}

#[test]
fn test_cross_protocol_system_prompt_preserved_to_cohere() {
    let anthropic_body = serde_json::json!({
        "model": "x",
        "system": "You are terse.",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 10
    });
    let ir = super::super::anthropic::AnthropicReader
        .read_request(&anthropic_body)
        .expect("anthropic read_request");
    assert!(
        !ir.system.is_empty(),
        "anthropic system must land in IrRequest.system"
    );
    let writer = CohereWriter;
    let cohere = writer.write_request(crate::codec::test_host::HOST, &ir);
    let msgs = cohere.get("messages").unwrap().as_array().unwrap();
    assert_eq!(
        msgs[0].get("role").and_then(|r| r.as_str()),
        Some("system"),
        "Cohere must emit the system prompt as a leading system message"
    );
    assert_eq!(
        msgs[0].get("content").and_then(|c| c.as_str()),
        Some("You are terse.")
    );
    assert_eq!(msgs[1].get("role").and_then(|r| r.as_str()), Some("user"));
}

/// Regression: a response carrying several parallel `ToolUse` blocks must surface ALL of them
#[test]
fn test_write_response_preserves_parallel_tool_calls() {
    let resp = crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        content: vec![
            crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "t1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
                cache_control: None,
            },
            crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "t2".to_string(),
                name: "get_time".to_string(),
                input: serde_json::json!({"tz": "PST"}),
                cache_control: None,
            },
            crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "t3".to_string(),
                name: "get_news".to_string(),
                input: serde_json::json!({}),
                cache_control: None,
            },
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: None,
        id: None,
        created: None,
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };

    let writer = CohereWriter;
    let json = writer.write_response(crate::codec::test_host::HOST, &resp);

    let tool_calls = json
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|v| v.as_array())
        .expect("tool_calls array must be present under message");
    assert_eq!(tool_calls.len(), 3, "all parallel tool calls must survive");
    let ids: Vec<&str> = tool_calls
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(ids, ["t1", "t2", "t3"]);
}

/// Regression: an assistant message whose only block is a `ToolUse` (surfaced via `tool_calls`)
#[test]
fn test_write_request_sole_tooluse_omits_empty_content() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "t1".to_string(),
                name: "f".to_string(),
                input: serde_json::json!({"x": 1}),
                cache_control: None,
            }],
        }],
        max_tokens: Some(64),
        ..Default::default()
    };

    let writer = CohereWriter;
    let json = writer.write_request(crate::codec::test_host::HOST, &ir);
    let msgs = json.get("messages").unwrap().as_array().unwrap();
    let assistant = &msgs[0];
    assert!(
        assistant.get("content").is_none(),
        "sole-ToolUse message must omit content rather than emit []"
    );
    assert!(
        assistant.get("tool_calls").is_some(),
        "the tool call must still be present"
    );
}

/// Multiple text blocks in one message must serialize as a text-part array (not be collapsed),
#[test]
fn test_write_request_text_block_shapes() {
    let single = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        ..Default::default()
    };
    let writer = CohereWriter;
    let j = writer.write_request(crate::codec::test_host::HOST, &single);
    assert_eq!(
        j.get("messages").unwrap().as_array().unwrap()[0].get("content"),
        Some(&serde_json::json!("hi"))
    );

    let multi = crate::codec::ir::IrRequest {
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![
                crate::codec::ir::IrBlock::text("a".to_string()),
                crate::codec::ir::IrBlock::text("b".to_string()),
            ],
        }],
        ..single
    };
    let j = writer.write_request(crate::codec::test_host::HOST, &multi);
    let content = j.get("messages").unwrap().as_array().unwrap()[0]
        .get("content")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].get("text").and_then(|t| t.as_str()), Some("a"));
    assert_eq!(content[1].get("text").and_then(|t| t.as_str()), Some("b"));
}

/// `read_request` must not allocate a temporary empty Vec when `tools` is absent, and must
#[test]
fn test_read_request_missing_tools() {
    let json = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let ir = CohereReader
        .read_request(&json)
        .expect("read_request should succeed");
    assert!(ir.tools.is_empty());
}

/// A native Cohere request with a `role: "system"` message must parse successfully (the "system"
#[test]
fn test_read_request_role_system_maps_to_ir_system_not_a_message() {
    let json = serde_json::json!({
        "model": "command",
        "messages": [
            {"role": "system", "content": "be nice"},
            {"role": "user", "content": "hi"}
        ]
    });
    let ir = CohereReader
        .read_request(&json)
        .expect("a role:system message must parse, not error");
    assert_eq!(
        ir.messages.len(),
        1,
        "the system message must not appear in .messages"
    );
    assert_eq!(ir.messages[0].role, crate::codec::ir::IrRole::User);
    assert_eq!(ir.system.len(), 1);
    assert!(matches!(
        &ir.system[0],
        crate::codec::ir::IrBlock::Text { text, .. } if text == "be nice"
    ));
}

/// System content given as an ARRAY of blocks only picks up blocks whose `"type"` is exactly
#[test]
fn test_read_request_system_content_array_only_collects_text_typed_blocks() {
    let json = serde_json::json!({
        "model": "command",
        "messages": [
            {"role": "system", "content": [
                {"type": "text", "text": "first"},
                {"type": "not_text", "text": "must be skipped"},
                {"type": "text", "text": "second"}
            ]},
            {"role": "user", "content": "hi"}
        ]
    });
    let ir = CohereReader
        .read_request(&json)
        .expect("read_request should succeed");
    let texts: Vec<&str> = ir
        .system
        .iter()
        .map(|b| match b {
            crate::codec::ir::IrBlock::Text { text, .. } => text.as_str(),
            _ => panic!("expected only Text blocks"),
        })
        .collect();
    assert_eq!(texts, vec!["first", "second"]);
}

/// `tool_calls` on a message is only processed when that message's role is Assistant — a
#[test]
fn test_read_request_tool_calls_only_processed_for_assistant_role() {
    let stray_tool_calls = serde_json::json!([{
        "id": "call_1",
        "function": {"name": "lookup", "arguments": "{}"}
    }]);
    let json = serde_json::json!({
        "model": "command",
        "messages": [
            {"role": "user", "content": "hi", "tool_calls": stray_tool_calls},
        ]
    });
    let ir = CohereReader
        .read_request(&json)
        .expect("read_request should succeed");
    assert_eq!(ir.messages.len(), 1);
    assert!(
        ir.messages[0]
            .content
            .iter()
            .all(|b| !matches!(b, crate::codec::ir::IrBlock::ToolUse { .. })),
        "a user message's tool_calls field must be ignored, not decoded: {:?}",
        ir.messages[0].content
    );

    let json = serde_json::json!({
        "model": "command",
        "messages": [
            {"role": "assistant", "content": "ok", "tool_calls": stray_tool_calls},
        ]
    });
    let ir = CohereReader
        .read_request(&json)
        .expect("read_request should succeed");
    assert!(
        ir.messages[0]
            .content
            .iter()
            .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolUse { .. })),
        "an assistant message's tool_calls field must be decoded: {:?}",
        ir.messages[0].content
    );
}

/// Once a text block is closed (`ET_CONTENT_END`), `state.text_block_closed` latches — a LATER
#[test]
fn test_stream_content_start_after_close_does_not_reopen_the_text_block() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;

    let evs = reader.read_response_events(crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_CONTENT_START, "index": 0, "delta": {"message": {"content": {"type": "text", "text": ""}}}}),
        &mut state,
    );
    assert_eq!(evs.len(), 1);
    assert!(matches!(
        evs[0],
        crate::codec::ir::IrStreamEvent::BlockStart { .. }
    ));

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_CONTENT_END, "index": 0}),
        &mut state,
    );
    assert_eq!(evs.len(), 1);
    assert!(matches!(
        evs[0],
        crate::codec::ir::IrStreamEvent::BlockStop { .. }
    ));

    let evs = reader.read_response_events(crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_CONTENT_START, "index": 1, "delta": {"message": {"content": {"type": "text", "text": ""}}}}),
        &mut state,
    );
    assert!(
        evs.is_empty(),
        "a text block reopened after close must be dropped, not re-emit BlockStart: {evs:?}"
    );
}

/// The array-form `read_response_events` content-delta path only emits a `TextDelta` for a block
#[test]
fn test_stream_content_delta_array_form_only_emits_text_typed_blocks() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;

    let _ = reader.read_response_events(crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_CONTENT_START, "index": 0, "delta": {"message": {"content": {"type": "text", "text": ""}}}}),
        &mut state,
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_CONTENT_DELTA, "index": 0, "delta": {"message": {"content": [
            {"type": "not_text", "text": "must be skipped"},
            {"type": "text", "text": "real"}
        ]}}}),
        &mut state,
    );
    let texts: Vec<&str> = evs
        .iter()
        .map(|e| match e {
            crate::codec::ir::IrStreamEvent::BlockDelta {
                delta: crate::codec::ir::IrDelta::TextDelta(t),
                ..
            } => t.as_str(),
            other => panic!("expected only TextDelta events, got {other:?}"),
        })
        .collect();
    assert_eq!(
        texts,
        vec!["real"],
        "only the type:text block must emit a delta"
    );
}

/// The NATIVE Cohere v2 error envelope is a bare `{"message": <detail>}` — NOT the generic
#[test]
fn test_write_error_native_cohere_envelope() {
    let writer = CohereWriter;
    let v = writer.write_error(
        crate::codec::test_host::HOST,
        404,
        "not_found",
        "model 'x' not found",
    );

    let serialized = serde_json::to_string(&v).expect("write_error output must serialize");
    let reparsed: serde_json::Value =
        serde_json::from_str(&serialized).expect("write_error output must be valid JSON");

    assert_eq!(
        reparsed.get("message").and_then(|m| m.as_str()),
        Some("model 'x' not found"),
        "native Cohere error carries the detail under top-level `message`"
    );
    assert!(
        reparsed.get("error").is_none(),
        "must NOT use the generic `error` wrapper"
    );
    assert!(
        reparsed.get("type").is_none() && reparsed.get("code").is_none(),
        "Cohere conveys the error category via HTTP status, not a typed body field"
    );
    assert!(
        reparsed.get("id").is_none(),
        "real Cohere error bodies carry no synthesized id"
    );

    assert_eq!(
        reparsed.as_object().map(|o| o.len()),
        Some(1),
        "native Cohere error body is a bare {{\"message\": ...}}"
    );
}

/// Same-protocol (Cohere → Cohere) passthrough must preserve the upstream response `id` exactly
#[test]
fn test_same_protocol_roundtrip_preserves_id() {
    let upstream_id = "c14c80c3-18eb-4519-9460-6c92edd8cfb4";
    let json = serde_json::json!({
        "id": upstream_id,
        "finish_reason": COHERE_FINISH_COMPLETE,
        "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        "usage": {"tokens": {"input_tokens": 3, "output_tokens": 1}}
    });

    let resp = CohereReader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read_response should succeed");
    assert_eq!(
        resp.id.as_deref(),
        Some(upstream_id),
        "upstream id captured verbatim into the IR"
    );

    let writer = CohereWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out.get("id").and_then(|i| i.as_str()),
        Some(upstream_id),
        "the same id must be re-emitted on write (same-protocol fidelity)"
    );
}

/// Same-protocol stream passthrough preserves the message-start `id`.
#[test]
fn test_same_protocol_stream_roundtrip_preserves_id() {
    let upstream_id = "c14c80c3-18eb-4519-9460-6c92edd8cfb4";
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let evs = CohereReader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": upstream_id,
            "type": ET_MESSAGE_START,
            "delta": {"message": {"role": "assistant"}}
        }),
        &mut state,
    );
    assert_eq!(evs.len(), 1);
    let captured = match &evs[0] {
        crate::codec::ir::IrStreamEvent::MessageStart { id, .. } => id.clone(),
        other => panic!("expected MessageStart, got {other:?}"),
    };
    assert_eq!(captured.as_deref(), Some(upstream_id));

    let writer = CohereWriter;
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &evs[0])
        .expect("message-start must serialize");
    assert_eq!(
        frame.get("id").and_then(|i| i.as_str()),
        Some(upstream_id),
        "stream message-start id must round-trip verbatim"
    );
}

/// Cross-protocol write (the backend supplied NO id — `IrResponse.id == None`) must SYNTHESIZE a
#[test]
fn test_cross_protocol_write_synthesizes_valid_id() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hello".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    };

    let writer = CohereWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    let id = out
        .get("id")
        .and_then(|i| i.as_str())
        .expect("synthesized id must be present as a string");
    assert!(!id.is_empty(), "synthesized id must be non-empty");
    assert!(
        is_uuid_shaped(id),
        "synthesized id must be a bare UUID (no `cohere-` prefix), got {id}"
    );
}

/// Test helper: validate the 8-4-4-4-12 lowercase-hex UUID layout that native Cohere ids use.
fn is_uuid_shaped(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    let expected_lens = [8usize, 4, 4, 4, 12];
    groups.len() == 5
        && groups.iter().zip(expected_lens.iter()).all(|(g, &len)| {
            g.len() == len
                && g.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
}

/// Test helper: validate that a UUID string is a proper RFC-4122 UUIDv4 — version nibble `4`
fn is_uuid_v4(s: &str) -> bool {
    if !is_uuid_shaped(s) {
        return false;
    }
    let groups: Vec<&str> = s.split('-').collect();
    let version_ok = groups[2].starts_with('4');
    let variant_ok = matches!(
        groups[3].bytes().next(),
        Some(b'8') | Some(b'9') | Some(b'a') | Some(b'b')
    );
    version_ok && variant_ok
}

/// The synthesized id must be a bare UUID (8-4-4-4-12 hex),
#[test]
fn test_synthesized_id_is_uuid_shaped() {
    let id =
        synthesize_cohere_id(crate::codec::test_host::HOST).expect("host entropy is available");
    assert!(
        is_uuid_shaped(&id),
        "synthesized id must match the UUID layout, got {id}"
    );
    assert!(
        !id.starts_with("cohere-"),
        "synthesized id must NOT carry a literal prefix, got {id}"
    );
}

/// The synthesized id must be a PROPER RFC-4122 UUIDv4 — version
#[test]
fn test_synthesized_id_is_valid_uuid_v4() {
    for _ in 0..1000 {
        let id =
            synthesize_cohere_id(crate::codec::test_host::HOST).expect("host entropy is available");
        assert!(
            is_uuid_v4(&id),
            "synthesized id must be a valid RFC-4122 UUIDv4 (version nibble 4, variant 10xx), \
                 got {id}"
        );
    }
}

/// The synthesized id must NOT embed the server clock. The previous
#[test]
fn test_synthesized_id_does_not_leak_timestamp() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let leaked_prefix = format!("{:08x}", secs as u32);

    let mut matched = 0u32;
    for _ in 0..256 {
        let id =
            synthesize_cohere_id(crate::codec::test_host::HOST).expect("host entropy is available");
        let first_group = id.split('-').next().unwrap_or("");
        if first_group == leaked_prefix {
            matched += 1;
        }
    }
    assert_eq!(
        matched, 0,
        "first UUID group must not deterministically equal the unix-second hex {leaked_prefix} \
             (server-clock leak)"
    );
}

/// Synthesized ids are unique across a burst by virtue of CSPRNG entropy alone — `synthesize_
#[test]
fn test_synthesized_ids_are_unique() {
    const N: usize = 4096;
    let mut seen = std::collections::HashSet::with_capacity(N);
    for _ in 0..N {
        let id =
            synthesize_cohere_id(crate::codec::test_host::HOST).expect("host entropy is available");
        assert!(
            is_uuid_v4(&id),
            "each synthesized id must be a well-formed UUIDv4, got {id}"
        );
        assert!(
            seen.insert(id.clone()),
            "CSPRNG-seeded synthesized ids must be unique across a burst; collision on {id}"
        );
    }
}

/// A well-formed credential produces a single `Authorization: Bearer <key>` header.
#[test]
fn test_auth_headers_valid_key_emits_bearer() {
    let Some(busbar_contract::protocol::EgressScheme::Static {
        families,
        own,
        passthrough,
    }) = crate::codec::cohere::DECL.egress_scheme
    else {
        panic!("cohere declares a static credential scheme");
    };
    assert!(families.is_empty());
    assert_eq!(own, busbar_contract::protocol::CredentialHeader::Bearer);
    assert_eq!(
        passthrough,
        busbar_contract::protocol::CredentialHeader::Bearer
    );
    assert!(
        crate::codec::cohere::DECL.egress_auth_headers.is_none(),
        "no credential passes through the plane"
    );
}

/// `ERROR_TOXIC` is a v1 Generate-API finish token, not a member of Cohere v2's
#[test]
fn test_safety_finish_reason_writes_complete_non_stream() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("moderated".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::Safety),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        id: Some("r1".to_string()),
        ..Default::default()
    };
    let writer = CohereWriter;
    let body = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        body.get("finish_reason").and_then(|v| v.as_str()),
        Some("COMPLETE"), // golden wire-contract literal (kept bare on purpose)
        "IR safety must write back as the v2-legal COMPLETE (a served answer, COH-15), never the \
         v1-only ERROR_TOXIC and never the ERROR that reads as an infrastructure failure"
    );

    let back = CohereReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read self-written body");
    assert_eq!(
        back.stop_reason,
        Some(crate::codec::ir::IrStopReason::EndTurn)
    );
}

#[test]
fn test_safety_finish_reason_writes_complete_stream() {
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::Safety),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage::default(),
        stop_detail: None,
    };
    let writer = CohereWriter;
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("message-end must serialize");
    assert_eq!(
        frame
            .get("delta")
            .and_then(|d| d.get("finish_reason"))
            .and_then(|v| v.as_str()),
        Some("COMPLETE"), // golden wire-contract literal (kept bare on purpose)
        "streamed safety stop must emit the v2-legal COMPLETE (COH-15), never ERROR_TOXIC or ERROR"
    );
}

/// The readers must NOT fold the generic `ERROR` finish_reason into the
#[test]
fn test_generic_error_does_not_fold_into_safety_and_round_trips() {
    let reader = CohereReader;

    let err_body = serde_json::json!({
        "finish_reason": COHERE_FINISH_ERROR,
        "message": { "content": [] },
        "usage": { "tokens": { "input_tokens": 1, "output_tokens": 1 } }
    });
    let err_ir = reader
        .read_response(crate::codec::test_host::HOST, &err_body)
        .expect("read ERROR body");
    assert_eq!(
        err_ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::Error),
        "generic ERROR must read back as IR `error`, not `safety`"
    );

    let toxic_body = serde_json::json!({
        "finish_reason": COHERE_FINISH_ERROR_TOXIC,
        "message": { "content": [] },
        "usage": { "tokens": { "input_tokens": 1, "output_tokens": 1 } }
    });
    let toxic_ir = reader
        .read_response(crate::codec::test_host::HOST, &toxic_body)
        .expect("read ERROR_TOXIC body");
    assert_eq!(
        toxic_ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::Safety),
        "ERROR_TOXIC must still read back as IR `safety`"
    );

    let writer = CohereWriter;
    let err_resp = crate::codec::ir::IrResponse {
        stop_reason: Some(crate::codec::ir::IrStopReason::Error),
        id: Some("e1".to_string()),
        ..Default::default()
    };
    let err_out = writer.write_response(crate::codec::test_host::HOST, &err_resp);
    assert_eq!(
        err_out.get("finish_reason").and_then(|v| v.as_str()),
        Some("ERROR"), // golden wire-contract literal (kept bare on purpose)
        "IR `error` must write back as the native generic `ERROR`, not `ERROR_TOXIC`"
    );

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let err_frame = serde_json::json!({
        "type": ET_MESSAGE_END,
        "delta": { "finish_reason": COHERE_FINISH_ERROR, "usage": { "tokens": {} } }
    });
    let evs =
        reader.read_response_events(crate::codec::test_host::HOST, "", &err_frame, &mut state);
    assert!(
        evs.iter().any(|e| matches!(
            e,
            IrStreamEvent::MessageDelta { stop_reason, .. }
                if stop_reason == &Some(crate::codec::ir::IrStopReason::Error)
        )),
        "streamed generic ERROR must decode to IR `error`, not `safety`, got {evs:?}"
    );
    let mut state2 = crate::codec::ir::StreamDecodeState::default();
    let toxic_frame = serde_json::json!({
        "type": ET_MESSAGE_END,
        "delta": { "finish_reason": COHERE_FINISH_ERROR_TOXIC, "usage": { "tokens": {} } }
    });
    let toxic_evs =
        reader.read_response_events(crate::codec::test_host::HOST, "", &toxic_frame, &mut state2);
    assert!(
        toxic_evs.iter().any(|e| matches!(
            e,
            IrStreamEvent::MessageDelta { stop_reason, .. }
                if stop_reason == &Some(crate::codec::ir::IrStopReason::Safety)
        )),
        "streamed ERROR_TOXIC must still decode to IR `safety`, got {toxic_evs:?}"
    );
}

/// An upstream-controlled stream tool-call frame `index` of `usize::MAX`
#[test]
fn test_huge_tool_frame_index_clamped_below_sentinel() {
    let reader = CohereReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let huge = u64::MAX;
    let start = serde_json::json!({
        "type": ET_TOOL_CALL_START,
        "index": huge,
        "delta": { "message": { "tool_calls": {
            "id": "call_huge",
            "type": "function",
            "function": { "name": "f", "arguments": "{}" }
        }}}
    });
    let evs = reader.read_response_events(crate::codec::test_host::HOST, "", &start, &mut state);

    assert!(
        !state.open_tools.contains(&TEXT_BLOCK_SEEN_SENTINEL),
        "a huge wire index must never be recorded as the sentinel value"
    );
    assert!(
        state
            .open_tools
            .iter()
            .filter(|&&e| e != TEXT_BLOCK_SEEN_SENTINEL)
            .all(|&e| e <= MAX_TOOL_FRAME_INDEX as usize),
        "every recorded tool frame index must be clamped to MAX_TOOL_FRAME_INDEX, got {:?}",
        state.open_tools
    );

    let starts = evs
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::BlockStart { .. }))
        .count();
    assert_eq!(
        starts, 1,
        "a clamped huge-index tool-call-start must open exactly one block, got {evs:?}"
    );

    let delta = serde_json::json!({
        "type": ET_TOOL_CALL_DELTA,
        "index": huge,
        "delta": { "message": { "tool_calls": {
            "function": { "arguments": "more" }
        }}}
    });
    let delta_evs =
        reader.read_response_events(crate::codec::test_host::HOST, "", &delta, &mut state);
    assert_eq!(
        delta_evs
            .iter()
            .filter(|e| matches!(e, IrStreamEvent::BlockDelta { .. }))
            .count(),
        1,
        "a clamped tool-call-delta must forward to the open block, got {delta_evs:?}"
    );
    let end = serde_json::json!({ "type": ET_TOOL_CALL_END, "index": huge });
    let end_evs = reader.read_response_events(crate::codec::test_host::HOST, "", &end, &mut state);
    assert_eq!(
        end_evs
            .iter()
            .filter(|e| matches!(e, IrStreamEvent::BlockStop { .. }))
            .count(),
        1,
        "a clamped tool-call-end must close the open block, got {end_evs:?}"
    );
}

/// A mid-stream `IrStreamEvent::Error` on a Cohere-ingress
#[test]
fn test_stream_error_emits_native_message_end_not_error_event() {
    let writer = CohereWriter;

    let infra = IrStreamEvent::Error(busbar_contract::protocol::IrError {
        class: busbar_contract::upstream::StatusClass::ServerError,
        provider_signal: Some("internal_server_error".to_string()),
        retry_after: None,
    });
    let (event_type, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &infra)
        .expect("Error must serialize to a native frame");

    assert_eq!(event_type, "");
    assert_eq!(
        frame.get("type").and_then(|v| v.as_str()),
        Some("message-end"), // golden wire-contract literal (kept bare on purpose)
        "Cohere v2 has no `type: error` event; a mid-stream error terminates with message-end"
    );
    assert_ne!(
        frame.get("type").and_then(|v| v.as_str()),
        Some("error"),
        "the non-native `type: error` frame must not be emitted"
    );

    assert!(
        frame.get("message").is_none(),
        "error message-end must not carry a top-level `message` field (proxy fingerprint), \
             got {frame:?}"
    );
    assert_eq!(
        frame.get("delta").and_then(|d| d.as_object()).map(|d| {
            let mut keys: Vec<&str> = d.keys().map(String::as_str).collect();
            keys.sort_unstable();
            keys
        }),
        Some(vec!["finish_reason", "usage"]),
        "error message-end `delta` must carry exactly `finish_reason` and `usage`, \
             mirroring the native MessageDelta shape"
    );

    let err_tokens = frame
        .get("delta")
        .and_then(|d| d.get("usage"))
        .and_then(|u| u.get("tokens"))
        .expect("error message-end must carry delta.usage.tokens");
    assert_eq!(
        err_tokens.get("input_tokens").and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        err_tokens.get("output_tokens").and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        frame
            .get("delta")
            .and_then(|d| d.get("finish_reason"))
            .and_then(|v| v.as_str()),
        Some("ERROR"), // golden wire-contract literal (kept bare on purpose)
        "an infrastructure error maps to the native ERROR finish_reason"
    );

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let decoded =
        CohereReader.read_response_events(crate::codec::test_host::HOST, "", &frame, &mut state);
    assert!(
        decoded.iter().any(|e| matches!(
            e,
            IrStreamEvent::MessageDelta { stop_reason, .. }
                if stop_reason == &Some(crate::codec::ir::IrStopReason::Error)
        )),
        "emitted message-end must decode back to a generic `error` stop, got {decoded:?}"
    );

    let toxic = IrStreamEvent::Error(busbar_contract::protocol::IrError {
        class: busbar_contract::upstream::StatusClass::ClientError,
        provider_signal: Some("content_filter_safety".to_string()),
        retry_after: None,
    });
    let (_, toxic_frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &toxic)
        .expect("Error must serialize");
    assert_eq!(
        toxic_frame
            .get("delta")
            .and_then(|d| d.get("finish_reason"))
            .and_then(|v| v.as_str()),
        Some("ERROR"),
        "a genuine error uses the v2 ERROR finish_reason regardless of its detail"
    );
    assert!(
        toxic_frame.get("message").is_none(),
        "toxic error message-end must not carry a top-level `message` field"
    );

    let bare = IrStreamEvent::Error(busbar_contract::protocol::IrError {
        class: busbar_contract::upstream::StatusClass::ServerError,
        provider_signal: None,
        retry_after: None,
    });
    let (_, bare_frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &bare)
        .expect("Error must serialize");
    assert_eq!(
        bare_frame.get("type").and_then(|v| v.as_str()),
        Some("message-end") // golden wire-contract literal (kept bare on purpose)
    );
    assert_eq!(
        bare_frame
            .get("delta")
            .and_then(|d| d.get("finish_reason"))
            .and_then(|v| v.as_str()),
        Some("ERROR") // golden wire-contract literal (kept bare on purpose)
    );
    assert!(
        bare_frame.get("message").is_none(),
        "bare error message-end must not carry a top-level `message` field"
    );
}

/// `read_response` must tolerate a missing `usage` object,
#[test]
fn test_read_response_missing_usage_defaults_to_zero() {
    let json = serde_json::json!({
        "id": "c14c80c3-18eb-4519-9460-6c92edd8cfb4",
        "finish_reason": COHERE_FINISH_COMPLETE,
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}]
        }
        // NOTE: no `usage` key at all.
    });
    let resp = CohereReader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("missing usage must not hard-error (zero-usage fallback)");
    assert_eq!(resp.usage.input_tokens, 0);
    assert_eq!(resp.usage.output_tokens, 0);
    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::EndTurn)
    );

    let json_empty_usage = serde_json::json!({
        "finish_reason": COHERE_FINISH_COMPLETE,
        "message": { "role": "assistant", "content": [{"type": "text", "text": "hi"}] },
        "usage": {}
    });
    let resp2 = CohereReader
        .read_response(crate::codec::test_host::HOST, &json_empty_usage)
        .expect("empty usage object must not hard-error");
    assert_eq!(resp2.usage.input_tokens, 0);
    assert_eq!(resp2.usage.output_tokens, 0);
}

/// MONEY BUG regression (buffered path): Cohere's API *specs* usage counts as JSON numbers, but its
#[test]
fn test_read_response_usage_float_token_counts_survive_as_integers() {
    let json = serde_json::json!({
        "id": "c14c80c3-18eb-4519-9460-6c92edd8cfb4",
        "finish_reason": COHERE_FINISH_COMPLETE,
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}]
        },
        "usage": {
            "tokens": {"input_tokens": 27.0, "output_tokens": 9.0},
            "billed_units": {
                "input_tokens": 25.0,
                "output_tokens": 9.0,
                "search_units": 3.0,
                "classifications": 1.0
            }
        }
    });
    let resp = CohereReader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read_response should succeed");
    assert_eq!(
        resp.usage.input_tokens, 27,
        "a float-encoded input_tokens count must survive as the exact integer, not 0"
    );
    assert_eq!(
        resp.usage.output_tokens, 9,
        "a float-encoded output_tokens count must survive as the exact integer, not 0"
    );
    assert_eq!(resp.usage.detail.billed_input_tokens, Some(25));
    assert_eq!(resp.usage.detail.billed_output_tokens, Some(9));
    assert_eq!(resp.usage.detail.search_units, Some(3));
    assert_eq!(resp.usage.detail.billed_classifications, Some(1));
}

/// MONEY BUG regression (streaming path): the SAME float-encoded usage counts arrive on the
#[test]
fn test_stream_message_end_usage_float_token_counts_survive_as_integers() {
    let reader = CohereReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_MESSAGE_END,
            "delta": {
                "finish_reason": COHERE_FINISH_COMPLETE,
                "usage": {
                    "tokens": {"input_tokens": 27.0, "output_tokens": 9.0},
                    "billed_units": {
                        "input_tokens": 25.0,
                        "output_tokens": 9.0,
                        "search_units": 3.0,
                        "classifications": 1.0
                    }
                }
            }
        }),
        &mut state,
    );
    let crate::codec::ir::IrStreamEvent::MessageDelta { ref usage, .. } = &evs[0] else {
        panic!(
            "message-end must emit a MessageDelta first, got {:?}",
            evs[0]
        );
    };
    assert_eq!(
        usage.input_tokens, 27,
        "a float-encoded streamed input_tokens count must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.output_tokens, 9,
        "a float-encoded streamed output_tokens count must survive as the exact integer, not 0"
    );
    assert_eq!(usage.detail.billed_input_tokens, Some(25));
    assert_eq!(usage.detail.billed_output_tokens, Some(9));
    assert_eq!(usage.detail.search_units, Some(3));
    assert_eq!(usage.detail.billed_classifications, Some(1));
}

/// MONEY BUG regression (truncated-tail recovery path): a HEAD-truncated non-stream billing buffer
#[test]
fn test_recover_truncated_usage_float_token_counts_survive_as_integers() {
    let reader = CohereReader;
    let tail = br#"{"usage":{"tokens":{"input_tokens":27.0,"output_tokens":9.0}}}"#;
    let usage = reader
        .recover_truncated_usage(tail)
        .expect("usage tail must be recoverable");
    assert_eq!(
        usage.input, 27,
        "a float-encoded input_tokens count must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.output, 9,
        "a float-encoded output_tokens count must survive as the exact integer, not 0"
    );
}

/// `write_response` must nest `tool_calls` INSIDE the `message`
#[test]
fn test_write_response_tool_calls_nested_and_roundtrip() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![
            crate::codec::ir::IrBlock::text("calling tools".to_string()),
            crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "t1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
                cache_control: None,
            },
            crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "t2".to_string(),
                name: "get_time".to_string(),
                input: serde_json::json!({"tz": "PST"}),
                cache_control: None,
            },
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 4,
            output_tokens: 6,
            ..Default::default()
        },
        id: Some("resp-1".to_string()),
        ..Default::default()
    };

    let writer = CohereWriter;
    let body = writer.write_response(crate::codec::test_host::HOST, &resp);

    assert!(
        body.get("tool_calls").is_none(),
        "tool_calls must NOT be at the top level"
    );
    let nested = body
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
        .expect("tool_calls must be nested under message");
    assert_eq!(nested.len(), 2, "both parallel tool calls survive");

    let back = CohereReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response of self-written body");
    let tool_uses: Vec<(&str, &str)> = back
        .content
        .iter()
        .filter_map(|b| {
            if let crate::codec::ir::IrBlock::ToolUse { id, name, .. } = b {
                Some((id.as_str(), name.as_str()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        tool_uses,
        [("t1", "get_weather"), ("t2", "get_time")],
        "Cohere -> Cohere tool-call passthrough must preserve every call"
    );
    assert_eq!(
        back.stop_reason,
        Some(crate::codec::ir::IrStopReason::ToolUse)
    );
}

/// The streaming `content-delta` frame must carry text at
#[test]
fn test_write_response_event_content_delta_is_object() {
    let ev = IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::TextDelta("chunk".to_string()),
    };
    let writer = CohereWriter;
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("content-delta must serialize");
    let content = frame
        .get("delta")
        .and_then(|d| d.get("message"))
        .and_then(|m| m.get("content"))
        .expect("content present");
    assert!(
        content.is_object(),
        "content-delta content must be an object, got {content}"
    );
    assert_eq!(
        content.get("type"),
        None,
        "a real content-delta's content object never carries a type key (only content-start does)"
    );
    assert_eq!(content.get("text").and_then(|t| t.as_str()), Some("chunk"));
}

/// The content-delta WRITER emits `delta.message.content` as a
#[test]
fn test_content_delta_writer_reader_roundtrip_object_shape() {
    let writer = CohereWriter;
    let (_, frame) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
            },
        )
        .expect("content-delta must serialize");

    assert!(
        frame
            .pointer("/delta/message/content")
            .is_some_and(|c| c.is_object()),
        "writer must emit object-shaped content: {frame}"
    );

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let evs =
        CohereReader.read_response_events(crate::codec::test_host::HOST, "", &frame, &mut state);
    let decoded_text: Option<String> = evs.iter().find_map(|e| match e {
        IrStreamEvent::BlockDelta {
            delta: crate::codec::ir::IrDelta::TextDelta(t),
            ..
        } => Some(t.clone()),
        _ => None,
    });
    assert_eq!(
        decoded_text.as_deref(),
        Some("hi"),
        "object-shaped content-delta must round-trip to the original text, got events: {evs:?}"
    );
}

/// REAL Cohere v2 content-delta carries `delta.message.content = {"text": …}` with NO `type`
#[test]
fn test_content_delta_real_cohere_shape_no_type_field() {
    let frame: serde_json::Value = serde_json::from_str(
        r#"{"type":"content-delta","index":0,"delta":{"message":{"content":{"text":"hi"}}}}"#,
    )
    .unwrap();
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let evs =
        CohereReader.read_response_events(crate::codec::test_host::HOST, "", &frame, &mut state);
    let decoded: Option<String> = evs.iter().find_map(|e| match e {
        IrStreamEvent::BlockDelta {
            delta: crate::codec::ir::IrDelta::TextDelta(t),
            ..
        } => Some(t.clone()),
        _ => None,
    });
    assert_eq!(
        decoded.as_deref(),
        Some("hi"),
        "the real (type-less) content-delta shape must decode, got events: {evs:?}"
    );
}

/// A streaming tool call (tool-call-start / tool-call-delta /
#[test]
fn test_stream_tool_call_events_mapped() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": 0,
            "delta": {"message": {"tool_calls": {
                "id": "call_1",
                "type": "function",
                "function": {"name": "get_weather", "arguments": ""}
            }}}
        }),
        &mut state,
    );
    assert_eq!(evs.len(), 1, "tool-call-start must emit a BlockStart");
    match &evs[0] {
        crate::codec::ir::IrStreamEvent::BlockStart {
            index,
            block: crate::codec::ir::IrBlockMeta::ToolUse { id, name },
            refusal: _,
        } => {
            assert_eq!(*index, 0);
            assert_eq!(id, "call_1");
            assert_eq!(name, "get_weather");
        }
        other => panic!("expected BlockStart ToolUse, got {other:?}"),
    }

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_DELTA,
            "index": 0,
            "delta": {"message": {"tool_calls": {"function": {"arguments": "{\"city\":"}}}}
        }),
        &mut state,
    );
    assert_eq!(evs.len(), 1);
    match &evs[0] {
        crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::InputJsonDelta(args),
        } => assert_eq!(args, "{\"city\":"),
        other => panic!("expected InputJsonDelta, got {other:?}"),
    }

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_TOOL_CALL_END, "index": 0}),
        &mut state,
    );
    assert_eq!(evs.len(), 1);
    assert!(matches!(
        evs[0],
        crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
    ));

    assert_eq!(
        state.tool_ir_index.get(&0).copied(),
        Some(0),
        "the closed tool's assigned IR index is retained to keep later tool indices stable"
    );
}

/// A `tool-call-start` that arrives BEFORE any text content frame
#[test]
fn test_stream_tool_before_text_no_index_collision() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": 0,
            "delta": {"message": {"tool_calls": {
                "id": "call_1",
                "type": "function",
                "function": {"name": "lookup", "arguments": ""}
            }}}
        }),
        &mut state,
    );
    assert_eq!(evs.len(), 1, "tool-call-start emits one BlockStart");
    match &evs[0] {
        crate::codec::ir::IrStreamEvent::BlockStart {
            index,
            block: crate::codec::ir::IrBlockMeta::ToolUse { id, name },
            refusal: _,
        } => {
            assert_eq!(*index, 0, "the first-arriving tool claims IR index 0");
            assert_eq!(id, "call_1");
            assert_eq!(name, "lookup");
        }
        other => panic!("expected tool BlockStart, got {other:?}"),
    }

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_CONTENT_START, "index": 0}),
        &mut state,
    );
    assert_eq!(evs.len(), 1, "content-start emits one text BlockStart");
    let text_idx = match &evs[0] {
        crate::codec::ir::IrStreamEvent::BlockStart {
            index,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: _,
        } => *index,
        other => panic!("expected text BlockStart, got {other:?}"),
    };
    assert_eq!(
        text_idx, 1,
        "the text block must claim index 1 (off the tool-claimed 0), not collide on 0"
    );
    assert_ne!(
        text_idx, 0,
        "text must never reuse the tool-claimed IR index 0"
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_CONTENT_DELTA,
            "index": 0,
            "delta": {"message": {"content": {"type": "text", "text": "hi"}}}
        }),
        &mut state,
    );
    assert_eq!(evs.len(), 1, "content-delta emits one text delta");
    match &evs[0] {
        crate::codec::ir::IrStreamEvent::BlockDelta {
            index,
            delta: crate::codec::ir::IrDelta::TextDelta(t),
        } => {
            assert_eq!(*index, 1, "the text delta rides the assigned index 1");
            assert_eq!(t, "hi");
        }
        other => panic!("expected text BlockDelta, got {other:?}"),
    }

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_CONTENT_END, "index": 0}),
        &mut state,
    );
    assert_eq!(evs.len(), 1, "content-end emits one BlockStop");
    assert!(
        matches!(
            evs[0],
            crate::codec::ir::IrStreamEvent::BlockStop { index: 1 }
        ),
        "the text block closes at its assigned index 1, not a hardcoded 0; got {:?}",
        evs[0]
    );

    assert_eq!(
        state.tool_ir_index.get(&0).copied(),
        Some(0),
        "the tool retains its distinct IR index 0"
    );

    let mut state2 = crate::codec::ir::StreamDecodeState::default();
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_CONTENT_START, "index": 0}),
        &mut state2,
    );
    assert!(matches!(
        evs[0],
        crate::codec::ir::IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: _,
        }
    ));
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": 0,
            "delta": {"message": {"tool_calls": {
                "id": "c2", "type": "function",
                "function": {"name": "f", "arguments": ""}
            }}}
        }),
        &mut state2,
    );
    assert!(
        matches!(
            &evs[0],
            crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
        ),
        "the still-open text block must close at index 0 before the tool opens, got {evs:?}"
    );
    match &evs[1] {
        crate::codec::ir::IrStreamEvent::BlockStart { index, .. } => assert_eq!(
            *index, 1,
            "text-before-tool: text keeps 0, tool takes 1 (no regression)"
        ),
        other => panic!("expected tool BlockStart, got {other:?}"),
    }
}

/// Once the leading tool-plan text block is CLOSED by the
#[test]
fn test_stream_text_not_reopened_after_close() {
    let reader = CohereReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_PLAN_DELTA,
            "delta": {"message": {"tool_plan": "think"}}
        }),
        &mut state,
    );
    assert!(
        matches!(
            evs[0],
            crate::codec::ir::IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                refusal: _,
            }
        ),
        "tool-plan opens the leading reasoning block at index 0, got {evs:?}"
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": 0,
            "delta": {"message": {"tool_calls": {
                "id": "c1", "type": "function",
                "function": {"name": "f", "arguments": ""}
            }}}
        }),
        &mut state,
    );
    assert!(
        matches!(
            &evs[0],
            crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
        ),
        "tool-call-start closes the plan text block at index 0, got {evs:?}"
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_PLAN_DELTA,
            "delta": {"message": {"tool_plan": "more"}}
        }),
        &mut state,
    );
    assert!(
        evs.is_empty(),
        "a tool-plan-delta after the text block closed must be dropped, got {evs:?}"
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_CONTENT_DELTA,
            "delta": {"message": {"content": "resumed"}}
        }),
        &mut state,
    );
    assert!(
        evs.is_empty(),
        "a content-delta after the text block closed must be dropped, got {evs:?}"
    );
}

/// A stream that ends with the leading tool-plan text block
#[test]
fn test_stream_message_end_closes_dangling_text_block() {
    let reader = CohereReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_PLAN_DELTA,
            "delta": {"message": {"tool_plan": "thinking"}}
        }),
        &mut state,
    );
    assert!(
        matches!(
            evs[0],
            crate::codec::ir::IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                refusal: _,
            }
        ),
        "tool-plan opens the reasoning block, got {evs:?}"
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_MESSAGE_END, "delta": {"finish_reason": "COMPLETE"}}),
        &mut state,
    );
    assert!(
        matches!(
            evs[0],
            crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
        ),
        "message-end must force-close the dangling text block first, got {evs:?}"
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, crate::codec::ir::IrStreamEvent::MessageStop)),
        "message-end still emits MessageStop, got {evs:?}"
    );
}

/// An unknown Cohere stream event type is a documented no-op (no events, no panic) — the named
#[test]
fn test_stream_unknown_event_is_noop() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let evs = CohereReader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "debug", "index": 0}),
        &mut state,
    );
    assert!(evs.is_empty(), "unknown event types produce no IR events");
}

#[test]
fn test_stream_citations_match_non_stream() {
    let citation = serde_json::json!({
        "start": 0, "end": 5, "text": "Paris", "type": "TEXT_CONTENT",
        "sources": [{"type": "document", "id": "doc:0",
                     "document": {"title": "Capitals", "url": "https://example.com/capitals"}}]
    });
    let second = serde_json::json!({
        "start": 9, "end": 16, "text": "capital", "type": "TEXT_CONTENT",
        "sources": [{"type": "document", "id": "doc:1", "document": {"title": "Glossary"}}]
    });

    let buffered = CohereReader
        .read_response(
            crate::codec::test_host::HOST,
            &serde_json::json!({
                "id": "c1", "finish_reason": "COMPLETE",
                "message": {"role": "assistant",
                            "content": [{"type": "text", "text": "Paris is capital."}],
                            "citations": [citation.clone(), second.clone()]},
                "usage": {"tokens": {"input_tokens": 3, "output_tokens": 4}}
            }),
        )
        .expect("buffered read");
    let buffered_cits = buffered
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Text { citations, .. } => Some(citations.clone()),
            _ => None,
        })
        .expect("text block");
    assert_eq!(
        buffered_cits.len(),
        2,
        "buffered read carries both citations"
    );

    let frames = [
        serde_json::json!({"type": "message-start", "id": "c1",
                           "delta": {"message": {"role": "assistant"}}}),
        serde_json::json!({"type": "content-start", "index": 0,
                           "delta": {"message": {"content": {"type": "text", "text": ""}}}}),
        serde_json::json!({"type": "content-delta", "index": 0,
                           "delta": {"message": {"content": {"text": "Paris is "}}}}),
        serde_json::json!({"type": "content-delta", "index": 0,
                           "delta": {"message": {"content": {"text": "capital."}}}}),
        serde_json::json!({"type": "citation-start", "index": 0,
                           "delta": {"message": {"citations": citation}}}),
        serde_json::json!({"type": "citation-end", "index": 0}),
        serde_json::json!({"type": "citation-start", "index": 1,
                           "delta": {"message": {"citations": second}}}),
        serde_json::json!({"type": "citation-end", "index": 1}),
        serde_json::json!({"type": "content-end", "index": 0}),
        serde_json::json!({"type": "message-end",
                           "delta": {"finish_reason": "COMPLETE",
                                     "usage": {"tokens": {"input_tokens": 3, "output_tokens": 4}}}}),
    ];
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events: Vec<crate::codec::ir::IrStreamEvent> = frames
        .iter()
        .flat_map(|f| {
            CohereReader.read_response_events(crate::codec::test_host::HOST, "", f, &mut state)
        })
        .collect();

    let text_idx = events
        .iter()
        .find_map(|e| match e {
            crate::codec::ir::IrStreamEvent::BlockStart {
                index,
                block: crate::codec::ir::IrBlockMeta::Text,
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .expect("text block opened");
    let mut streamed_cits = Vec::new();
    for e in &events {
        if let crate::codec::ir::IrStreamEvent::BlockDelta {
            index,
            delta: crate::codec::ir::IrDelta::CitationsDelta(c),
        } = e
        {
            assert_eq!(*index, text_idx, "citation rides the text block's index");
            streamed_cits.extend(c.iter().cloned());
        }
    }
    assert_eq!(
        streamed_cits, buffered_cits,
        "stream and non-stream reads of the same Cohere answer must carry the same citations"
    );

    let stop_pos = events
        .iter()
        .position(
            |e| matches!(e, crate::codec::ir::IrStreamEvent::BlockStop { index } if *index == text_idx),
        )
        .expect("text block closed");
    let last_cit = events
        .iter()
        .rposition(|e| {
            matches!(
                e,
                crate::codec::ir::IrStreamEvent::BlockDelta {
                    delta: crate::codec::ir::IrDelta::CitationsDelta(_),
                    ..
                }
            )
        })
        .expect("citation delta present");
    assert!(last_cit < stop_pos, "citations precede the text block stop");
}

#[test]
fn test_stream_citation_after_content_end_is_dropped() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    for f in [
        serde_json::json!({"type": "content-delta", "index": 0,
                           "delta": {"message": {"content": {"text": "hi"}}}}),
        serde_json::json!({"type": "content-end", "index": 0}),
    ] {
        CohereReader.read_response_events(crate::codec::test_host::HOST, "", &f, &mut state);
    }
    let evs = CohereReader.read_response_events(crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "citation-start", "index": 0,
                            "delta": {"message": {"citations": {"start": 0, "end": 2, "text": "hi"}}}}),
        &mut state,
    );
    assert!(evs.is_empty(), "no delta into a stopped block, got {evs:?}");
}

/// CF6 (wire-conformance): a streamed citation egressing into the Cohere v2 dialect must serialize
#[test]
fn test_write_response_event_citation_start_is_single_object() {
    let writer = CohereWriter;
    writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("quoted".into()),
        },
    );
    let ev = IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::CitationsDelta(vec![crate::codec::ir::IrCitation {
            domain: None,
            kind: Some("web_search_result_location".to_string()),
            cited_text: Some("quoted".to_string()),
            title: Some("T".to_string()),
            url: Some("https://x".to_string()),
            document_index: None,
            start_index: Some(0),
            end_index: Some(6),
            encrypted_index: None,
            raw: None,
            ..Default::default()
        }]),
    };
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("a citation delta emits a citation-start frame");
    assert_eq!(
        frame.get("type").and_then(|t| t.as_str()),
        Some("citation-start")
    );
    let citations = frame
        .pointer("/delta/message/citations")
        .expect("citation-start carries delta.message.citations");
    assert!(
        citations.is_object(),
        "delta.message.citations must be a single Citation OBJECT; got {citations}"
    );
    assert!(
        !citations.is_array(),
        "delta.message.citations must NOT be a JSON array; got {citations}"
    );
    assert_eq!(
        citations.pointer("/start").and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(citations.pointer("/end").and_then(|v| v.as_u64()), Some(6));
    assert_eq!(
        citations.pointer("/text").and_then(|v| v.as_str()),
        Some("quoted")
    );
    assert_eq!(
        citations
            .pointer("/sources/0/document/url")
            .and_then(|v| v.as_str()),
        Some("https://x"),
        "the single Citation's source url must survive; got {citations}"
    );
}

/// `extract_error` derives both fields from a SINGLE parse.
#[test]
fn test_extract_error_single_parse_both_fields() {
    let body = br#"{"message": "boom", "error_type": "invalid_request"}"#;
    let err = CohereReader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(err.provider_code.as_deref(), Some("boom"));
    assert_eq!(err.structured_type.as_deref(), Some("invalid_request"));
    assert_eq!(err.http_status, 400);
}

/// Production `extract_error` must synthesize the
#[test]
fn test_extract_error_synthesizes_context_length_in_production() {
    let reader = CohereReader;

    let bodies: &[&[u8]] = &[
            br#"{"message": "too many tokens: the request exceeds the model's context window", "error_type": "invalid_request_error"}"#,
            br#"{"message": "the input is too long for the requested model; please reduce the prompt"}"#,
            br#"{"message": "requested 200000 tokens but the maximum is 128000 tokens for this model"}"#,
            br#"{"message": "prompt exceeds the maximum context length"}"#,
        ];

    for body in bodies {
        let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
        assert_eq!(
                raw.provider_code.as_deref(),
                Some("context_length_exceeded"),
                "production extract_error must synthesize the canonical context-length code for body {}",
                String::from_utf8_lossy(body)
            );

        let signal = crate::codec::test_host::classified(&raw);
        assert_eq!(
            signal.class,
            busbar_contract::upstream::StatusClass::ContextLength,
            "breaker must map the synthesized code to ContextLength for body {}",
            String::from_utf8_lossy(body)
        );
    }
}

/// A non-context-length Cohere error body must NOT be misclassified as context-length: the raw
#[test]
fn test_extract_error_non_context_length_message_preserved() {
    let reader = CohereReader;
    let body = br#"{"message": "invalid api key", "error_type": "invalid_request_error"}"#;
    let raw = reader.extract_error(StatusCode::UNAUTHORIZED, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("invalid api key"),
        "a non-context-length message must be carried verbatim"
    );
    let signal = crate::codec::test_host::classified(&raw);
    assert_ne!(
        signal.class,
        busbar_contract::upstream::StatusClass::ContextLength,
        "a non-context-length error must not be classified as ContextLength"
    );
}

/// The `too long` arm of `body_signals_context_length` must be
#[test]
fn test_too_long_only_classifies_context_length_when_qualified() {
    let reader = CohereReader;

    let non_context: &[&[u8]] = &[
        br#"{"message": "the requested URL is too long", "error_type": "invalid_request_error"}"#,
        br#"{"message": "value too long for column name", "error_type": "invalid_request_error"}"#,
        br#"{"message": "the password you provided is too long"}"#,
    ];
    for body in non_context {
        let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
        assert_ne!(
            raw.provider_code.as_deref(),
            Some("context_length_exceeded"),
            "a generic 'too long' message must not synthesize the context-length code: {}",
            String::from_utf8_lossy(body)
        );
        let signal = crate::codec::test_host::classified(&raw);
        assert_ne!(
            signal.class,
            busbar_contract::upstream::StatusClass::ContextLength,
            "a generic 'too long' message must not classify as ContextLength: {}",
            String::from_utf8_lossy(body)
        );
    }

    let context: &[&[u8]] = &[
        br#"{"message": "the input is too long for the requested model"}"#,
        br#"{"message": "your prompt is too long: it exceeds the model context window"}"#,
        br#"{"message": "message too long, too many tokens"}"#,
    ];
    for body in context {
        let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
        assert_eq!(
            raw.provider_code.as_deref(),
            Some("context_length_exceeded"),
            "a qualified 'too long' (context) message must synthesize the context-length code: {}",
            String::from_utf8_lossy(body)
        );
        let signal = crate::codec::test_host::classified(&raw);
        assert_eq!(
            signal.class,
            busbar_contract::upstream::StatusClass::ContextLength,
            "a qualified 'too long' (context) message must classify as ContextLength: {}",
            String::from_utf8_lossy(body)
        );
    }
}

/// A non-streaming request must OMIT the `stream` key entirely
#[test]
fn test_write_request_stream_field_conditional() {
    let base = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        ..Default::default()
    };

    let writer = CohereWriter;
    let non_streaming = writer.write_request(crate::codec::test_host::HOST, &base);
    assert!(
        non_streaming.get("stream").is_none(),
        "non-streaming request must omit the `stream` key, got {non_streaming}"
    );

    let streaming = writer.write_request(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrRequest {
            stream: true,
            ..base
        },
    );
    assert_eq!(
        streaming.get("stream"),
        Some(&serde_json::json!(true)),
        "streaming request must emit `\"stream\": true`"
    );
}

/// A non-streaming Cohere -> Cohere passthrough must NOT GAIN a
#[test]
fn test_stream_field_roundtrip_omitted() {
    let native = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let ir = CohereReader
        .read_request(&native)
        .expect("read_request should succeed");
    assert!(!ir.stream, "absent `stream` reads as false");
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        out.get("stream").is_none(),
        "round-trip must not inject a `stream` field, got {out}"
    );
}

/// The streaming `message-end` frame must carry token usage at
#[test]
fn test_write_response_event_message_end_carries_usage() {
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 42,
            output_tokens: 7,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        stop_detail: None,
    };
    let writer = CohereWriter;
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("message-end must serialize");
    assert_eq!(
        frame.get("type").and_then(|t| t.as_str()),
        Some("message-end") // golden wire-contract literal (kept bare on purpose)
    );
    let tokens = frame
        .get("delta")
        .and_then(|d| d.get("usage"))
        .and_then(|u| u.get("tokens"))
        .expect("delta.usage.tokens must be present");
    assert_eq!(
        tokens.get("input_tokens").and_then(|v| v.as_u64()),
        Some(42)
    );
    assert_eq!(
        tokens.get("output_tokens").and_then(|v| v.as_u64()),
        Some(7)
    );
}

/// When upstream usage is zero (no data), the message-end frame
#[test]
fn test_write_response_event_message_end_zero_usage_present() {
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: None,
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage::default(),
        stop_detail: None,
    };
    let writer = CohereWriter;
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("message-end must serialize");
    let tokens = frame
        .get("delta")
        .and_then(|d| d.get("usage"))
        .and_then(|u| u.get("tokens"))
        .expect("delta.usage.tokens must be present even with zero usage");
    assert_eq!(tokens.get("input_tokens").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(
        tokens.get("output_tokens").and_then(|v| v.as_u64()),
        Some(0)
    );
}

/// The message-end stream frame round-trips usage through this protocol's own reader: the usage
#[test]
fn test_message_end_usage_stream_roundtrip() {
    let usage = crate::codec::ir::IrUsage {
        input_tokens: 11,
        output_tokens: 3,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        detail: crate::codec::ir::IrUsageDetail::default(),
    };
    let writer = CohereWriter;
    let (_, frame) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: usage.clone(),
                stop_detail: None,
            },
        )
        .expect("message-end must serialize");

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let evs =
        CohereReader.read_response_events(crate::codec::test_host::HOST, "", &frame, &mut state);
    let back = evs
        .iter()
        .find_map(|e| {
            if let IrStreamEvent::MessageDelta { usage, .. } = e {
                Some(usage.clone())
            } else {
                None
            }
        })
        .expect("a MessageDelta must come back");
    assert_eq!(back.input_tokens, 11);
    assert_eq!(back.output_tokens, 3);
}

/// A Tool-role message carrying plain text ALONGSIDE a ToolResult
#[test]
fn test_tool_role_text_alongside_result_not_dropped() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Tool,
            content: vec![
                crate::codec::ir::IrBlock::text("note".to_string()),
                crate::codec::ir::IrBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: vec![crate::codec::ir::IrBlock::text("result".to_string())],
                    is_error: false,
                    cache_control: None,
                },
            ],
        }],
        ..Default::default()
    };
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let msgs = out.get("messages").unwrap().as_array().unwrap();
    assert_eq!(msgs.len(), 1, "one tool message emitted");
    let content = msgs[0].get("content").and_then(|c| c.as_str()).unwrap();
    assert!(
        content.contains("note") && content.contains("result"),
        "both the message-level text and the tool result text must survive, got {content}"
    );
    assert_eq!(
        msgs[0].get("tool_call_id").and_then(|v| v.as_str()),
        Some("t1")
    );
}

/// A ToolResult whose content is SPLIT across multiple text blocks
#[test]
fn test_tool_result_multi_block_content_joins_without_space() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Tool,
            content: vec![crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: vec![
                    crate::codec::ir::IrBlock::text("foo".to_string()),
                    crate::codec::ir::IrBlock::text("bar".to_string()),
                ],
                is_error: false,
                cache_control: None,
            }],
        }],
        ..Default::default()
    };
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let msgs = out.get("messages").unwrap().as_array().unwrap();
    let content = msgs[0].get("content").and_then(|c| c.as_str()).unwrap();
    assert_eq!(
        content, "foobar",
        "multi-block ToolResult content must join with NO separator (a phantom space corrupts \
             split payloads on round-trip), got {content:?}"
    );
}

/// A degenerate Tool-role message with text but NO ToolResult
#[test]
fn test_tool_role_text_without_result_not_dropped() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Tool,
            content: vec![crate::codec::ir::IrBlock::text(
                "orphan tool text".to_string(),
            )],
        }],
        ..Default::default()
    };
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let msgs = out.get("messages").unwrap().as_array().unwrap();
    assert_eq!(
        msgs.len(),
        1,
        "a Tool turn with text but no ToolResult must still emit a message"
    );
    assert_eq!(msgs[0].get("role").and_then(|r| r.as_str()), Some("tool"));
    assert_eq!(
        msgs[0].get("content").and_then(|c| c.as_str()),
        Some("orphan tool text")
    );
}

/// A degenerate Tool-role message with MULTIPLE text blocks
#[test]
fn test_tool_role_multi_text_without_result_is_string() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Tool,
            content: vec![
                crate::codec::ir::IrBlock::text("a".to_string()),
                crate::codec::ir::IrBlock::text("b".to_string()),
            ],
        }],
        ..Default::default()
    };
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let msgs = out.get("messages").unwrap().as_array().unwrap();
    assert_eq!(
        msgs.len(),
        1,
        "a Tool turn with multiple text blocks but no ToolResult must emit one message"
    );
    assert_eq!(msgs[0].get("role").and_then(|r| r.as_str()), Some("tool"));
    assert_eq!(
        msgs[0].get("content").and_then(|c| c.as_str()),
        Some("ab"),
        "multi-block degenerate Tool content must be a joined string (NO separator, matching \
             read_request's `.join(\"\")` — a phantom space would corrupt split payloads on a \
             round-trip), not a JSON array"
    );
}

/// A Cohere v2 `tool`-role message whose `content` is the
#[test]
fn test_read_request_tool_content_object_array_preserved() {
    let body = serde_json::json!({
        "model": "command-r",
        "messages": [
            {
                "role": "tool",
                "tool_call_id": "call_1",
                "content": [
                    {"type": "text", "text": "first part"},
                    {"type": "text", "text": "second part"},
                    {"type": "document", "document": {"id": "d1", "data": "doc body"}}
                ]
            }
        ]
    });
    let ir = CohereReader
        .read_request(&body)
        .expect("read_request should succeed");
    let tool_msg = ir
        .messages
        .iter()
        .find(|m| m.role == crate::codec::ir::IrRole::Tool)
        .expect("tool message present");
    let tool_result = tool_msg
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .expect("ToolResult block present");
    let text = match tool_result.first() {
        Some(crate::codec::ir::IrBlock::Text { text, .. }) => text.clone(),
        other => panic!("expected text block in tool result, got {other:?}"),
    };
    assert!(
        text.contains("first part"),
        "text block text must be preserved: {text}"
    );
    assert!(
        text.contains("second part"),
        "all text blocks must be joined: {text}"
    );

    assert!(
        !text.contains("doc body"),
        "the document must not be stringified into the tool-result text: {text}"
    );
    assert!(
        tool_result.iter().skip(1).any(|b| matches!(
            b,
            crate::codec::ir::IrBlock::Text { text, .. } if text == "doc body"
        )),
        "the document's data must be preserved as its own block: {tool_result:?}"
    );
}

/// Fidelity: the Cohere reader must concatenate tool-content text blocks with NO
#[test]
fn test_read_request_tool_text_blocks_join_without_space() {
    let body = serde_json::json!({
        "model": "command-r",
        "messages": [
            {
                "role": "tool",
                "tool_call_id": "call_1",
                "content": [
                    {"type": "text", "text": "foo"},
                    {"type": "text", "text": "bar"}
                ]
            }
        ]
    });
    let ir = CohereReader
        .read_request(&body)
        .expect("read_request should succeed");
    let tool_result = ir
        .messages
        .iter()
        .find(|m| m.role == crate::codec::ir::IrRole::Tool)
        .and_then(|m| {
            m.content.iter().find_map(|b| match b {
                crate::codec::ir::IrBlock::ToolResult { content, .. } => Some(content),
                _ => None,
            })
        })
        .expect("ToolResult block present");
    let text = match tool_result.first() {
        Some(crate::codec::ir::IrBlock::Text { text, .. }) => text.clone(),
        other => panic!("expected text block in tool result, got {other:?}"),
    };
    assert_eq!(
        text, "foobar",
        "adjacent tool-content text blocks must join with no inserted space"
    );
}

/// The bare-string tool-content array shape must keep working
#[test]
fn test_read_request_tool_content_string_array_still_works() {
    let body = serde_json::json!({
        "model": "command-r",
        "messages": [
            {
                "role": "tool",
                "tool_call_id": "call_2",
                "content": ["alpha", "beta"]
            }
        ]
    });
    let ir = CohereReader
        .read_request(&body)
        .expect("read_request should succeed");
    let tool_msg = ir
        .messages
        .iter()
        .find(|m| m.role == crate::codec::ir::IrRole::Tool)
        .expect("tool message present");
    let tool_result = tool_msg
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .expect("ToolResult block present");
    let text = match tool_result.first() {
        Some(crate::codec::ir::IrBlock::Text { text, .. }) => text.clone(),
        other => panic!("expected text block in tool result, got {other:?}"),
    };

    assert_eq!(text, "alphabeta");
}

/// Cohere v2 streams each tool call as a complete
#[test]
fn test_stream_two_sequential_tool_calls_get_distinct_indices() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": 0,
            "delta": {"message": {"tool_calls": {
                "id": "call_a",
                "type": "function",
                "function": {"name": "get_weather", "arguments": ""}
            }}}
        }),
        &mut state,
    );
    let idx1 = match &evs[0] {
        crate::codec::ir::IrStreamEvent::BlockStart {
            index,
            block: crate::codec::ir::IrBlockMeta::ToolUse { id, .. },
            refusal: _,
        } => {
            assert_eq!(id, "call_a");
            *index
        }
        other => panic!("expected BlockStart ToolUse, got {other:?}"),
    };

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_DELTA,
            "index": 0,
            "delta": {"message": {"tool_calls": {"function": {"arguments": "{\"a\":1}"}}}}
        }),
        &mut state,
    );
    assert!(matches!(
        &evs[0],
        crate::codec::ir::IrStreamEvent::BlockDelta { index, .. } if *index == idx1
    ));
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_TOOL_CALL_END, "index": 0}),
        &mut state,
    );
    assert!(matches!(
        &evs[0],
        crate::codec::ir::IrStreamEvent::BlockStop { index } if *index == idx1
    ));

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": 1,
            "delta": {"message": {"tool_calls": {
                "id": "call_b",
                "type": "function",
                "function": {"name": "get_time", "arguments": ""}
            }}}
        }),
        &mut state,
    );
    let idx2 = match &evs[0] {
        crate::codec::ir::IrStreamEvent::BlockStart {
            index,
            block: crate::codec::ir::IrBlockMeta::ToolUse { id, .. },
            refusal: _,
        } => {
            assert_eq!(id, "call_b");
            *index
        }
        other => panic!("expected BlockStart ToolUse, got {other:?}"),
    };

    assert_ne!(
        idx1, idx2,
        "two sequential streamed tool calls must get distinct IR block indices"
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_DELTA,
            "index": 1,
            "delta": {"message": {"tool_calls": {"function": {"arguments": "{\"b\":2}"}}}}
        }),
        &mut state,
    );
    assert!(matches!(
        &evs[0],
        crate::codec::ir::IrStreamEvent::BlockDelta { index, .. } if *index == idx2
    ));
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_TOOL_CALL_END, "index": 1}),
        &mut state,
    );
    assert!(matches!(
        &evs[0],
        crate::codec::ir::IrStreamEvent::BlockStop { index } if *index == idx2
    ));
}

/// A tool call's IR block index is ASSIGNED at `tool-call-start` and is
#[test]
fn test_stream_tool_ir_index_stable_under_non_monotonic_frame_indices() {
    let reader = CohereReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let start = |wire: u64, id: &str| {
        serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": wire,
            "delta": {"message": {"tool_calls": {
                "id": id,
                "type": "function",
                "function": {"name": "f", "arguments": ""}
            }}}
        })
    };
    let start_idx = |evs: &[IrStreamEvent]| match evs.first() {
        Some(IrStreamEvent::BlockStart { index, .. }) => *index,
        other => panic!("expected a BlockStart, got {other:?}"),
    };

    let a_idx = start_idx(&reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &start(10, "call_a"),
        &mut state,
    ));
    let b_idx = start_idx(&reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &start(5, "call_b"),
        &mut state,
    ));
    let c_idx = start_idx(&reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &start(2, "call_c"),
        &mut state,
    ));

    assert_eq!(
        (a_idx, b_idx, c_idx),
        (0, 1, 2),
        "tool IR indices must be assigned by insertion order, not wire-index rank"
    );

    for (wire, expected) in [(10u64, a_idx), (5, b_idx), (2, c_idx)] {
        let delta = serde_json::json!({
            "type": ET_TOOL_CALL_DELTA,
            "index": wire,
            "delta": {"message": {"tool_calls": {"function": {"arguments": "{}"}}}}
        });
        let devs =
            reader.read_response_events(crate::codec::test_host::HOST, "", &delta, &mut state);
        assert!(
            matches!(devs.first(), Some(IrStreamEvent::BlockDelta { index, .. }) if *index == expected),
            "delta for wire {wire} must resolve to its assigned IR index {expected}, got {devs:?}"
        );

        let end = serde_json::json!({ "type": ET_TOOL_CALL_END, "index": wire });
        let eevs = reader.read_response_events(crate::codec::test_host::HOST, "", &end, &mut state);
        assert!(
            matches!(eevs.first(), Some(IrStreamEvent::BlockStop { index }) if *index == expected),
            "end for wire {wire} must close its assigned IR index {expected}, got {eevs:?}"
        );
    }
}

/// A leading text block must push tool blocks to IR index 1+ while keeping each tool distinct.
#[test]
fn test_stream_tool_indices_offset_by_open_text_block() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;

    reader.read_response_events(crate::codec::test_host::HOST,
            "",
            &serde_json::json!({"type": ET_CONTENT_START, "index": 0, "delta": {"message": {"content": {"type": "text", "text": ""}}}}),
            &mut state,
        );

    let evs = reader.read_response_events(crate::codec::test_host::HOST,
            "",
            &serde_json::json!({
                "type": ET_TOOL_CALL_START,
                "index": 0,
                "delta": {"message": {"tool_calls": {"id": "c1", "type": "function", "function": {"name": "f1", "arguments": ""}}}}
            }),
            &mut state,
        );

    assert!(
        matches!(
            &evs[0],
            crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
        ),
        "the open text block must close at index 0 before the first tool opens, got {evs:?}"
    );
    let idx1 = match &evs[1] {
        crate::codec::ir::IrStreamEvent::BlockStart { index, .. } => *index,
        other => panic!("expected BlockStart, got {other:?}"),
    };
    assert_eq!(idx1, 1, "first tool follows the open text block at index 0");

    reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_TOOL_CALL_END, "index": 0}),
        &mut state,
    );

    let evs = reader.read_response_events(crate::codec::test_host::HOST,
            "",
            &serde_json::json!({
                "type": ET_TOOL_CALL_START,
                "index": 1,
                "delta": {"message": {"tool_calls": {"id": "c2", "type": "function", "function": {"name": "f2", "arguments": ""}}}}
            }),
            &mut state,
        );
    let idx2 = match &evs[0] {
        crate::codec::ir::IrStreamEvent::BlockStart { index, .. } => *index,
        other => panic!("expected BlockStart, got {other:?}"),
    };
    assert_eq!(
        idx2, 2,
        "second tool gets the next distinct index after the first"
    );
    assert_ne!(idx1, idx2);
}

/// A text content block that has CLOSED before the first
#[test]
fn test_stream_tool_after_closed_text_block_does_not_reuse_index_zero() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;

    let evs = reader.read_response_events(crate::codec::test_host::HOST,
            "",
            &serde_json::json!({"type": ET_CONTENT_START, "index": 0, "delta": {"message": {"content": {"type": "text", "text": ""}}}}),
            &mut state,
        );
    assert!(matches!(
        &evs[0],
        crate::codec::ir::IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: _,
        }
    ));

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_CONTENT_END, "index": 0}),
        &mut state,
    );
    assert!(matches!(
        &evs[0],
        crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
    ));
    assert!(
        !state.text_block_open,
        "content-end must clear the live text_block_open flag"
    );

    let evs = reader.read_response_events(crate::codec::test_host::HOST,
            "",
            &serde_json::json!({
                "type": ET_TOOL_CALL_START,
                "index": 0,
                "delta": {"message": {"tool_calls": {"id": "call_a", "type": "function", "function": {"name": "f", "arguments": ""}}}}
            }),
            &mut state,
        );
    let tool_idx = match &evs[0] {
        crate::codec::ir::IrStreamEvent::BlockStart {
            index,
            block: crate::codec::ir::IrBlockMeta::ToolUse { .. },
            refusal: _,
        } => *index,
        other => panic!("expected BlockStart ToolUse, got {other:?}"),
    };
    assert_eq!(
        tool_idx, 1,
        "a tool following a CLOSED text block must not reuse the text block's IR index 0"
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_TOOL_CALL_END, "index": 0}),
        &mut state,
    );
    assert!(matches!(
        &evs[0],
        crate::codec::ir::IrStreamEvent::BlockStop { index: 1 }
    ));
}

/// The modelled keys are the mapping file's top-level rows — so request fields like those stay
#[test]
fn test_modeled_keys_are_the_mapping_rows() {
    let a = |k: &str| crate::codec::carry::models(super::map::REQUEST, k);
    for k in [
        "model",
        "messages",
        "tools",
        "max_tokens",
        "temperature",
        "stream",
    ] {
        assert!(a(k), "{k} must be a modeled key");
    }

    assert!(!a("unknown_passthrough_key"));
}

/// A cross-protocol stream delivering tool calls to a
#[test]
fn test_write_response_event_emits_tool_call_frames() {
    let writer = CohereWriter;

    let start = IrStreamEvent::BlockStart {
        index: 2,
        block: crate::codec::ir::IrBlockMeta::ToolUse {
            id: "call_x".to_string(),
            name: "get_weather".to_string(),
        },
        refusal: false,
    };
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &start)
        .expect("BlockStart ToolUse must emit a frame");
    assert_eq!(
        frame.get("type").and_then(|t| t.as_str()),
        Some("tool-call-start") // golden wire-contract literal (kept bare on purpose)
    );
    assert_eq!(frame.get("index").and_then(|i| i.as_u64()), Some(2));
    let tc = frame
        .get("delta")
        .and_then(|d| d.get("message"))
        .and_then(|m| m.get("tool_calls"))
        .expect("tool_calls present");
    assert_eq!(tc.get("id").and_then(|v| v.as_str()), Some("call_x"));
    assert_eq!(tc.get("type").and_then(|v| v.as_str()), Some("function"));
    assert_eq!(
        tc.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str()),
        Some("get_weather")
    );
    assert_eq!(
        tc.get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str()),
        Some(""),
        "the reader accumulates argument deltas onto this opening empty string"
    );

    let delta = IrStreamEvent::BlockDelta {
        index: 2,
        delta: crate::codec::ir::IrDelta::InputJsonDelta("{\"city\":\"SF\"}".to_string()),
    };
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &delta)
        .expect("BlockDelta InputJsonDelta must emit a frame");
    assert_eq!(
        frame.get("type").and_then(|t| t.as_str()),
        Some("tool-call-delta") // golden wire-contract literal (kept bare on purpose)
    );
    assert_eq!(frame.get("index").and_then(|i| i.as_u64()), Some(2));
    assert_eq!(
        frame
            .get("delta")
            .and_then(|d| d.get("message"))
            .and_then(|m| m.get("tool_calls"))
            .and_then(|t| t.get("function"))
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str()),
        Some("{\"city\":\"SF\"}")
    );
}

/// The writer's emitted tool-call-start/delta frames round-trip through this protocol's OWN
#[test]
fn test_writer_tool_call_frames_roundtrip_through_reader() {
    let writer = CohereWriter;
    let (_, start_frame) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::ToolUse {
                    id: "call_z".to_string(),
                    name: "lookup".to_string(),
                },
                refusal: false,
            },
        )
        .expect("start frame");
    let (_, delta_frame) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::InputJsonDelta("{\"q\":1}".to_string()),
            },
        )
        .expect("delta frame");

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;
    let start_evs =
        reader.read_response_events(crate::codec::test_host::HOST, "", &start_frame, &mut state);
    match &start_evs[0] {
        crate::codec::ir::IrStreamEvent::BlockStart {
            block: crate::codec::ir::IrBlockMeta::ToolUse { id, name },
            ..
        } => {
            assert_eq!(id, "call_z");
            assert_eq!(name, "lookup");
        }
        other => panic!("expected BlockStart ToolUse, got {other:?}"),
    }
    let delta_evs =
        reader.read_response_events(crate::codec::test_host::HOST, "", &delta_frame, &mut state);
    match &delta_evs[0] {
        crate::codec::ir::IrStreamEvent::BlockDelta {
            delta: crate::codec::ir::IrDelta::InputJsonDelta(args),
            ..
        } => assert_eq!(args, "{\"q\":1}"),
        other => panic!("expected InputJsonDelta, got {other:?}"),
    }
}

/// A redacted-thinking/Image stream BLOCK-START has no native Cohere v2 opening frame, so the writer
#[test]
fn test_write_response_event_redacted_thinking_and_image_blocks_suppressed() {
    let writer = CohereWriter;
    assert!(writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::RedactedThinking,
                refusal: false,
            }
        )
        .is_none());
    assert!(writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Image,
                refusal: false,
            }
        )
        .is_none());

    let (_, start) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                refusal: false,
            },
        )
        .expect("a reasoning block has a native Cohere opening frame");
    assert_eq!(start["type"], ET_CONTENT_START);
    assert_eq!(start["delta"]["message"]["content"]["type"], "thinking");

    let (_, frame) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::ThinkingDelta("x".to_string()),
            },
        )
        .expect("a reasoning delta has a native Cohere frame");
    assert_eq!(frame["type"], ET_CONTENT_DELTA);
    assert_eq!(frame["delta"]["message"]["content"]["thinking"], "x");
}

/// A Cohere `tool`-role message's `content` must be decoded
#[test]
fn test_read_request_tool_content_not_double_decoded() {
    let body = serde_json::json!({
        "model": "command-r",
        "messages": [
            {
                "role": "tool",
                "tool_call_id": "call_1",
                "content": [{"type": "text", "text": "the result"}]
            }
        ]
    });
    let ir = CohereReader
        .read_request(&body)
        .expect("read_request should succeed");
    let tool_msg = ir
        .messages
        .iter()
        .find(|m| m.role == crate::codec::ir::IrRole::Tool)
        .expect("tool message present");

    let stray_text = tool_msg
        .content
        .iter()
        .any(|b| matches!(b, crate::codec::ir::IrBlock::Text { .. }));
    assert!(
        !stray_text,
        "tool message must NOT carry a top-level Text block (content belongs to the ToolResult)"
    );

    let tool_results: Vec<&Vec<crate::codec::ir::IrBlock>> = tool_msg
        .content
        .iter()
        .filter_map(|b| match b {
            crate::codec::ir::IrBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1, "exactly one ToolResult block");
    let inner = match tool_results[0].first() {
        Some(crate::codec::ir::IrBlock::Text { text, .. }) => text.clone(),
        other => panic!("expected text in tool result, got {other:?}"),
    };
    assert_eq!(inner, "the result");
}

/// A Cohere -> Cohere round-trip of a tool message must NOT
#[test]
fn test_tool_message_roundtrip_no_duplicate_text() {
    let body = serde_json::json!({
        "model": "command-r",
        "messages": [
            {
                "role": "tool",
                "tool_call_id": "call_1",
                "content": [{"type": "text", "text": "UNIQUEMARKER"}]
            }
        ]
    });
    let ir = CohereReader
        .read_request(&body)
        .expect("read_request should succeed");
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let msgs = out.get("messages").unwrap().as_array().unwrap();
    let tool_msg = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .expect("a tool message must be emitted");
    let content = tool_msg
        .get("content")
        .and_then(|c| c.as_str())
        .expect("tool content string");
    assert_eq!(
        content.matches("UNIQUEMARKER").count(),
        1,
        "tool-result text must appear exactly once (no double-decode duplication), got {content}"
    );
    assert_eq!(
        tool_msg.get("tool_call_id").and_then(|v| v.as_str()),
        Some("call_1")
    );
}

/// `max_tokens` must be narrowed with `u32::try_from`, NOT a
#[test]
fn test_read_request_unrepresentable_max_tokens_refuses_without_removing_cap() {
    let reader = CohereReader;

    let over = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": (u32::MAX as i64) + 1
    });
    assert!(reader.read_request(&over).is_err());

    let huge = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": i64::MAX
    });
    assert!(reader.read_request(&huge).is_err());

    let max_in_range = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": u32::MAX as i64
    });
    let ir = reader
        .read_request(&max_in_range)
        .expect("read_request should succeed");
    assert_eq!(ir.max_tokens, Some(u32::MAX));

    let normal = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024
    });
    let ir = reader
        .read_request(&normal)
        .expect("read_request should succeed");
    assert_eq!(ir.max_tokens, Some(1024));

    let zero = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 0
    });
    assert!(reader.read_request(&zero).is_err());
    let neg = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": -5
    });
    assert!(reader.read_request(&neg).is_err());
}

/// `k` (top_k) must be narrowed with `u32::try_from`, NOT a
#[test]
fn test_read_request_top_k_out_of_range_drops_to_none() {
    let reader = CohereReader;

    let over = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "k": (u32::MAX as u64) + 1
    });
    assert_eq!(
        reader.read_request(&over).expect("ok").top_k,
        None,
        "an out-of-range top_k must drop to None, not wrap under `as u32`"
    );

    let over2 = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "k": (u32::MAX as u64) + 2
    });
    assert_eq!(reader.read_request(&over2).expect("ok").top_k, None);

    let huge = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "k": u64::MAX
    });
    assert_eq!(reader.read_request(&huge).expect("ok").top_k, None);

    let max_in_range = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "k": u32::MAX as u64
    });
    assert_eq!(
        reader.read_request(&max_in_range).expect("ok").top_k,
        Some(u32::MAX)
    );

    let normal = serde_json::json!({
        "model": "command",
        "messages": [{"role": "user", "content": "hi"}],
        "k": 40
    });
    assert_eq!(reader.read_request(&normal).expect("ok").top_k, Some(40));
}

/// `state.open_tools` is never shrunk, so an upstream streaming an
#[test]
fn test_open_tools_growth_is_capped() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;
    for frame_idx in 0..(MAX_TRACKED_TOOL_FRAMES + 50) {
        reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &serde_json::json!({
                "type": ET_TOOL_CALL_START,
                "index": frame_idx,
                "delta": {"message": {"tool_calls": {
                    "id": format!("call_{frame_idx}"),
                    "type": "function",
                    "function": {"name": "f", "arguments": ""}
                }}}
            }),
            &mut state,
        );
    }
    assert!(
        state.open_tools.len() <= MAX_TRACKED_TOOL_FRAMES,
        "open_tools must be capped at MAX_TRACKED_TOOL_FRAMES, got {}",
        state.open_tools.len()
    );
}

/// Performance: the `state.tool_ir_index` lookup and `cohere_assign_tool_ir_index`
#[test]
fn test_stream_tool_ir_index_o1_lookup_matches_insertion_order_at_scale() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    assert_eq!(cohere_assign_tool_ir_index(&mut state, 10), Some(0));
    assert_eq!(cohere_assign_tool_ir_index(&mut state, 5), Some(1));
    assert_eq!(cohere_assign_tool_ir_index(&mut state, 2), Some(2));
    assert_eq!(state.tool_ir_index.get(&10).copied(), Some(0));
    assert_eq!(state.tool_ir_index.get(&5).copied(), Some(1));
    assert_eq!(state.tool_ir_index.get(&2).copied(), Some(2));

    assert_eq!(cohere_assign_tool_ir_index(&mut state, 10), None);
    assert_eq!(state.tool_ir_index.get(&10).copied(), Some(0));

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let n = MAX_TRACKED_TOOL_FRAMES;
    for i in 0..n {
        let wire = n - 1 - i;
        assert_eq!(
            cohere_assign_tool_ir_index(&mut state, wire),
            Some(i),
            "tool #{i} (wire {wire}) must be assigned IR index {i} by insertion order"
        );
    }
    assert_eq!(state.open_tools.len(), n);
    assert_eq!(state.tool_ir_index.len(), n);

    for i in 0..n {
        let wire = n - 1 - i;
        assert_eq!(
            state.tool_ir_index.get(&wire).copied(),
            Some(i),
            "tool #{i} (wire {wire}) must still resolve to its assigned IR index {i} after all {n} tools are open"
        );
    }

    assert_eq!(
        cohere_tracked_tool_count(&state),
        MAX_TRACKED_TOOL_FRAMES,
        "tracked tool count must equal the cap after opening exactly MAX_TRACKED_TOOL_FRAMES tools"
    );

    assert_eq!(
        cohere_assign_tool_ir_index(&mut state, MAX_TOOL_FRAME_INDEX as usize),
        None,
        "a fresh frame past MAX_TRACKED_TOOL_FRAMES must be rejected, not assigned a new IR index"
    );
    assert_eq!(
        state.tool_ir_index.len(),
        n,
        "a rejected over-cap frame must not grow tool_ir_index"
    );
}

/// A `BlockStop` that closes a TOOL-CALL block (one opened by a
#[test]
fn test_block_stop_closes_tool_block_with_tool_call_end() {
    let writer = CohereWriter;

    let (_, start) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::ToolUse {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                },
                refusal: false,
            },
        )
        .expect("tool-call-start must emit");
    assert_eq!(
        start.get("type").and_then(|t| t.as_str()),
        Some("tool-call-start") // golden wire-contract literal (kept bare on purpose)
    );

    let (_, stop) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 0 },
        )
        .expect("tool block stop must emit");
    assert_eq!(
        stop.get("type").and_then(|t| t.as_str()),
        Some("tool-call-end"), // golden wire-contract literal (kept bare on purpose)
        "a tool-call block must close with tool-call-end, not content-end"
    );
    assert_eq!(stop.get("index").and_then(|i| i.as_u64()), Some(0));
}

/// A `BlockStop` that closes a TEXT block (one opened by a
#[test]
fn test_block_stop_closes_text_block_with_content_end() {
    let writer = CohereWriter;

    let (_, start) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Text,
                refusal: false,
            },
        )
        .expect("content-start must emit");
    assert_eq!(
        start.get("type").and_then(|t| t.as_str()),
        Some("content-start") // golden wire-contract literal (kept bare on purpose)
    );
    let (_, stop) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 0 },
        )
        .expect("text block stop must emit");
    assert_eq!(
        stop.get("type").and_then(|t| t.as_str()),
        Some("content-end"), // golden wire-contract literal (kept bare on purpose)
        "a text block must close with content-end"
    );
    assert_eq!(stop.get("index").and_then(|i| i.as_u64()), Some(0));
}

/// A mixed stream (text block at index 0, then a tool-call block
#[test]
fn test_block_stop_mixed_text_and_tool_close_events() {
    let writer = CohereWriter;

    writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Text,
                refusal: false,
            },
        )
        .expect("text start");

    writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 1,
                block: crate::codec::ir::IrBlockMeta::ToolUse {
                    id: "call_2".to_string(),
                    name: "lookup".to_string(),
                },
                refusal: false,
            },
        )
        .expect("tool start");

    let (_, stop_text) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 0 },
        )
        .expect("text stop");
    assert_eq!(
        stop_text.get("type").and_then(|t| t.as_str()),
        Some("content-end"), // golden wire-contract literal (kept bare on purpose)
        "index 0 (text) must close with content-end"
    );

    let (_, stop_tool) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 1 },
        )
        .expect("tool stop");
    assert_eq!(
        stop_tool.get("type").and_then(|t| t.as_str()),
        Some("tool-call-end"), // golden wire-contract literal (kept bare on purpose)
        "index 1 (tool) must close with tool-call-end"
    );
}

/// The writer's tool open/close pair round-trips through this protocol's OWN reader: a
#[test]
fn test_tool_block_open_close_roundtrip_through_reader() {
    let writer = CohereWriter;
    let (_, start_frame) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::ToolUse {
                    id: "call_z".to_string(),
                    name: "lookup".to_string(),
                },
                refusal: false,
            },
        )
        .expect("start frame");
    let (_, stop_frame) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 0 },
        )
        .expect("stop frame");
    assert_eq!(
        stop_frame.get("type").and_then(|t| t.as_str()),
        Some("tool-call-end") // golden wire-contract literal (kept bare on purpose)
    );

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;
    let start_evs =
        reader.read_response_events(crate::codec::test_host::HOST, "", &start_frame, &mut state);
    assert!(matches!(
        &start_evs[0],
        crate::codec::ir::IrStreamEvent::BlockStart {
            block: crate::codec::ir::IrBlockMeta::ToolUse { .. },
            ..
        }
    ));
    let stop_evs =
        reader.read_response_events(crate::codec::test_host::HOST, "", &stop_frame, &mut state);
    assert!(
        matches!(
            &stop_evs[0],
            crate::codec::ir::IrStreamEvent::BlockStop { .. }
        ),
        "tool-call-end must map back to a BlockStop, got {stop_evs:?}"
    );
}

/// A tool index is consumed on close: a second `BlockStop` at the same index (an over-eager or
#[test]
fn test_block_stop_tool_index_consumed_on_close() {
    let writer = CohereWriter;
    writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 3,
                block: crate::codec::ir::IrBlockMeta::ToolUse {
                    id: "c".to_string(),
                    name: "f".to_string(),
                },
                refusal: false,
            },
        )
        .expect("start");
    let (_, first) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 3 },
        )
        .expect("first stop");
    assert_eq!(
        first.get("type").and_then(|t| t.as_str()),
        Some("tool-call-end") // golden wire-contract literal (kept bare on purpose)
    );

    let second = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 3 },
    );
    assert!(
            second.is_none(),
            "a tool index consumed on first close must emit no frame on a duplicate close, got {second:?}"
        );
}

/// The non-streaming `write_response` path must write IR
#[test]
fn test_write_response_stop_sequence_maps_to_stop_sequence() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::StopSequence),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        id: Some("c14c80c3-18eb-4519-9460-6c92edd8cfb4".to_string()),
        ..Default::default()
    };
    let writer = CohereWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out.get("finish_reason").and_then(|r| r.as_str()),
        Some("STOP_SEQUENCE"), // golden wire-contract literal (kept bare on purpose)
        "IR stop_sequence must serialize as native STOP_SEQUENCE, not COMPLETE"
    );
}

/// Companion: IR `end_turn` still maps to `COMPLETE` (the split must not regress the normal
#[test]
fn test_write_response_end_turn_maps_to_complete() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        id: Some("c14c80c3-18eb-4519-9460-6c92edd8cfb4".to_string()),
        ..Default::default()
    };
    let writer = CohereWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out.get("finish_reason").and_then(|r| r.as_str()),
        Some("COMPLETE") // golden wire-contract literal (kept bare on purpose)
    );
}

/// The streaming `MessageDelta` path must likewise write IR
#[test]
fn test_stream_message_delta_stop_sequence_maps_to_stop_sequence() {
    let writer = CohereWriter;
    let (_, frame) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::StopSequence),
                stop_sequence: None,
                usage: crate::codec::ir::IrUsage {
                    input_tokens: 2,
                    output_tokens: 3,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    detail: crate::codec::ir::IrUsageDetail::default(),
                },
                stop_detail: None,
            },
        )
        .expect("message-end frame");
    assert_eq!(
        frame
            .get("delta")
            .and_then(|d| d.get("finish_reason"))
            .and_then(|r| r.as_str()),
        Some("STOP_SEQUENCE"), // golden wire-contract literal (kept bare on purpose)
        "streamed IR stop_sequence must serialize as native STOP_SEQUENCE, not COMPLETE"
    );
}

/// Full round-trip: a native Cohere `STOP_SEQUENCE` read into IR must write back as
#[test]
fn test_stop_sequence_roundtrips_symmetrically() {
    let reader = CohereReader;
    let native = serde_json::json!({
        "id": "c14c80c3-18eb-4519-9460-6c92edd8cfb4",
        "finish_reason": COHERE_FINISH_STOP_SEQUENCE,
        "message": { "role": "assistant", "content": [{ "type": "text", "text": "x" }] },
        "usage": { "tokens": { "input_tokens": 1, "output_tokens": 1 } }
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &native)
        .expect("read native response");
    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::StopSequence)
    );

    let writer = CohereWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("finish_reason").and_then(|r| r.as_str()),
        Some("STOP_SEQUENCE"), // golden wire-contract literal (kept bare on purpose)
        "STOP_SEQUENCE must survive a Cohere -> IR -> Cohere round-trip unchanged"
    );
}

/// Test helper: count `BlockStart` events whose IR index equals `idx`.
fn count_block_starts_at(evs: &[IrStreamEvent], idx: usize) -> usize {
    evs.iter()
        .filter(|e| matches!(e, IrStreamEvent::BlockStart { index, .. } if *index == idx))
        .count()
}

/// A DUPLICATE `tool-call-start` for a frame index that is already open
#[test]
fn test_duplicate_tool_call_start_is_noop() {
    let reader = CohereReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let start = serde_json::json!({
        "type": ET_TOOL_CALL_START,
        "index": 0,
        "delta": { "message": { "tool_calls": {
            "id": "call_1",
            "type": "function",
            "function": { "name": "get_weather", "arguments": "" }
        }}}
    });

    let first = reader.read_response_events(crate::codec::test_host::HOST, "", &start, &mut state);

    assert_eq!(
        count_block_starts_at(&first, 0),
        1,
        "first tool-call-start must open the block once"
    );

    let second = reader.read_response_events(crate::codec::test_host::HOST, "", &start, &mut state);
    assert_eq!(
        count_block_starts_at(&second, 0),
        0,
        "a duplicate tool-call-start for an already-open frame must emit no BlockStart"
    );
    assert!(
        second.is_empty(),
        "a duplicate tool-call-start must be a complete no-op, got {second:?}"
    );
}

/// A `tool-call-start` for a frame BEYOND `MAX_TRACKED_TOOL_FRAMES` must
#[test]
fn test_over_cap_tool_call_start_emits_no_block() {
    let reader = CohereReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    for f in 0..MAX_TRACKED_TOOL_FRAMES {
        let start = serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": f,
            "delta": { "message": { "tool_calls": {
                "id": format!("call_{f}"),
                "type": "function",
                "function": { "name": "f", "arguments": "" }
            }}}
        });
        let _ = reader.read_response_events(crate::codec::test_host::HOST, "", &start, &mut state);
    }
    assert_eq!(state.open_tools.len(), MAX_TRACKED_TOOL_FRAMES);

    let over = serde_json::json!({
        "type": ET_TOOL_CALL_START,
        "index": MAX_TRACKED_TOOL_FRAMES + 5,
        "delta": { "message": { "tool_calls": {
            "id": "call_over",
            "type": "function",
            "function": { "name": "f", "arguments": "abc" }
        }}}
    });
    let evs = reader.read_response_events(crate::codec::test_host::HOST, "", &over, &mut state);
    assert!(
        evs.is_empty(),
        "an over-cap tool-call-start must emit no block events, got {evs:?}"
    );
    assert_eq!(
        count_block_starts_at(&evs, MAX_TRACKED_TOOL_FRAMES - 1),
        0,
        "an over-cap tool-call-start must not collide with the highest tracked tool's index"
    );
}

/// Content-start / content-delta / content-end must NORMALIZE the text
#[test]
fn test_text_block_normalized_to_ir_index_zero() {
    let reader = CohereReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let cs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_CONTENT_START,
            "index": 2,
            "delta": { "message": { "content": { "type": "text", "text": "" } } }
        }),
        &mut state,
    );
    match cs.as_slice() {
        [IrStreamEvent::BlockStart {
            index,
            block,
            refusal: _,
        }] => {
            assert_eq!(
                *index, 0,
                "text BlockStart must be normalized to IR index 0"
            );
            assert!(matches!(block, crate::codec::ir::IrBlockMeta::Text));
        }
        other => panic!("expected one text BlockStart, got {other:?}"),
    }

    let cd = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_CONTENT_DELTA,
            "index": 2,
            "delta": { "message": { "content": { "type": "text", "text": "hi" } } }
        }),
        &mut state,
    );
    match cd.as_slice() {
        [IrStreamEvent::BlockDelta { index, delta }] => {
            assert_eq!(
                *index, 0,
                "text BlockDelta must be normalized to IR index 0"
            );
            assert!(matches!(delta, crate::codec::ir::IrDelta::TextDelta(t) if t == "hi"));
        }
        other => panic!("expected one text BlockDelta, got {other:?}"),
    }

    let ce = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({ "type": ET_CONTENT_END, "index": 2 }),
        &mut state,
    );
    match ce.as_slice() {
        [IrStreamEvent::BlockStop { index }] => {
            assert_eq!(*index, 0, "text BlockStop must be normalized to IR index 0");
        }
        other => panic!("expected one text BlockStop, got {other:?}"),
    }

    let ts = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": 0,
            "delta": { "message": { "tool_calls": {
                "id": "t1",
                "type": "function",
                "function": { "name": "f", "arguments": "" }
            }}}
        }),
        &mut state,
    );
    assert_eq!(
        count_block_starts_at(&ts, 1),
        1,
        "the tool block must open at IR index 1 (after the text block at index 0)"
    );
    assert_eq!(
        count_block_starts_at(&ts, 2),
        0,
        "the tool block must NOT reuse the text block's non-zero wire index"
    );
}

/// A non-request-size status (e.g. 429) whose free-text body
#[test]
fn test_rate_limit_body_mentioning_tokens_is_not_context_length() {
    let reader = CohereReader;

    let body = br#"{"message":"You have exceeded the maximum number of tokens per minute for your trial key"}"#;
    let raw = reader.extract_error(StatusCode::TOO_MANY_REQUESTS, body);

    assert_ne!(
        raw.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "a 429 body mentioning tokens must not be reclassified as context_length_exceeded"
    );

    let sig = crate::codec::test_host::classified(&raw);
    assert_eq!(
            sig.class,
            StatusClass::RateLimit,
            "a 429 body mentioning tokens must normalize to RateLimit so the breaker penalizes the lane"
        );

    let signal = reader.classify(StatusCode::TOO_MANY_REQUESTS, body);
    assert_eq!(signal.class, StatusClass::RateLimit);
}

/// A genuine oversized-request failure (HTTP 400 with context-length phrasing) must still
#[test]
fn test_bad_request_body_mentioning_tokens_is_context_length() {
    let reader = CohereReader;
    let body = br#"{"message":"too many tokens: the request exceeds the maximum context length"}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "a 400 oversized-request body must still override to the canonical code"
    );
    let sig = crate::codec::test_host::classified(&raw);
    assert_eq!(sig.class, StatusClass::ContextLength);

    let signal = reader.classify(StatusCode::BAD_REQUEST, body);
    assert_eq!(signal.class, StatusClass::ContextLength);
}

/// A cross-protocol REDACTED-thinking block carries NO opening frame on the Cohere stream (its
#[test]
fn test_thinking_blockstop_emits_no_orphan_content_end() {
    let writer = CohereWriter;

    let start = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::RedactedThinking,
            refusal: false,
        },
    );
    assert!(
        start.is_none(),
        "a redacted-thinking BlockStart must not emit an opening frame"
    );

    let stop = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 0 },
    );
    assert!(
        stop.is_none(),
        "a redacted-thinking BlockStop must emit no frame (no orphan content-end)"
    );

    let start = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStart {
            index: 1,
            block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
            refusal: false,
        },
    );
    assert_eq!(start.expect("thinking opens").1["type"], ET_CONTENT_START);
    let stop = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 1 },
    );
    assert_eq!(stop.expect("thinking closes").1["type"], ET_CONTENT_END);
}

/// A normal Text block still emits a balanced `content-start` / `content-end` pair — the
#[test]
fn test_text_block_emits_balanced_content_start_and_end() {
    let writer = CohereWriter;

    let start = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
    );
    let (_, start_data) = start.expect("Text BlockStart must emit a content-start frame");
    assert_eq!(
        start_data.get("type").and_then(|t| t.as_str()),
        Some("content-start") // golden wire-contract literal (kept bare on purpose)
    );

    let stop = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 0 },
    );
    let (_, stop_data) = stop.expect("Text BlockStop must emit a content-end frame");
    assert_eq!(
        stop_data.get("type").and_then(|t| t.as_str()),
        Some("content-end") // golden wire-contract literal (kept bare on purpose)
    );
}

/// Minimal IR request carrying a single tool and an explicit `tool_choice`, for the
fn ir_with_tool_choice(tc: Option<crate::codec::ir::IrToolChoice>) -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        tools: vec![crate::codec::ir::IrTool {
            name: "get_weather".to_string(),
            description: None,
            input_schema: serde_json::json!({}),
            cache_control: None,

            hosted: None,

            strict: None,
        }],
        tool_choice: tc,
        ..Default::default()
    }
}

#[test]
fn test_cohere_tool_choice_required_roundtrips() {
    let body = serde_json::json!({
        "model": "command-r",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {}}}],
        "tool_choice": COHERE_TOOL_CHOICE_REQUIRED
    });
    let ir = CohereReader
        .read_request(&body)
        .expect("read_request should succeed");
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Required)
    );

    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("tool_choice").and_then(|v| v.as_str()),
        Some("REQUIRED") // golden wire-contract literal (kept bare on purpose)
    );
}

#[test]
fn test_cohere_tool_choice_none() {
    let body = serde_json::json!({
        "model": "command-r",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {}}}],
        "tool_choice": COHERE_TOOL_CHOICE_NONE
    });
    let ir = CohereReader
        .read_request(&body)
        .expect("read_request should succeed");
    assert_eq!(ir.tool_choice, Some(crate::codec::ir::IrToolChoice::None));

    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("tool_choice").and_then(|v| v.as_str()),
        Some("NONE") // golden wire-contract literal (kept bare on purpose)
    );
}

#[test]
fn test_cohere_tool_choice_specific_degrades_to_required() {
    let ir = ir_with_tool_choice(Some(crate::codec::ir::IrToolChoice::Tool {
        name: "get_weather".to_string(),
    }));
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("tool_choice").and_then(|v| v.as_str()),
        Some("REQUIRED"), // golden wire-contract literal (kept bare on purpose)
        "a forced specific tool must degrade to REQUIRED, never to auto"
    );
}

#[test]
fn test_cohere_tool_choice_auto_omitted() {
    let ir = ir_with_tool_choice(Some(crate::codec::ir::IrToolChoice::Auto));
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        out.get("tool_choice").is_none(),
        "Auto must omit tool_choice (it is Cohere's default)"
    );

    let body = serde_json::json!({
        "model": "command-r",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let ir = CohereReader
        .read_request(&body)
        .expect("read_request should succeed");
    assert_eq!(ir.tool_choice, None);
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(out.get("tool_choice").is_none());
}

#[test]
fn test_cohere_writer_clamps_temperature_above_one() {
    let mut ir = ir_with_tool_choice(None);
    ir.temperature = Some(1.8);
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("temperature").and_then(|v| v.as_f64()),
        Some(1.0),
        "an OpenAI-ingress temperature of 1.8 must clamp to 1.0 on the Cohere writer; got {out}"
    );
}

#[test]
fn test_clamp_temperature_for_cohere_signals_on_change() {
    assert_eq!(crate::codec::carry::clamp(1.8, 0.0, 1.0), (1.0, true));
    assert_eq!(crate::codec::carry::clamp(2.0, 0.0, 1.0), (1.0, true));

    assert_eq!(crate::codec::carry::clamp(-0.5, 0.0, 1.0), (0.0, true));

    assert_eq!(crate::codec::carry::clamp(0.7, 0.0, 1.0), (0.7, false));
    assert_eq!(crate::codec::carry::clamp(0.0, 0.0, 1.0), (0.0, false));
    assert_eq!(crate::codec::carry::clamp(1.0, 0.0, 1.0), (1.0, false));

    let (nan_out, nan_flag) = crate::codec::carry::clamp(f64::NAN, 0.0, 1.0);
    assert!(nan_out.is_nan() && !nan_flag);
}

#[test]
fn test_cohere_sampling_controls_survive_roundtrip() {
    let body = serde_json::json!({
        "model": "command-r",
        "messages": [{"role": "user", "content": "hi"}],
        "frequency_penalty": 0.5,
        "presence_penalty": 0.25,
        "seed": 42,
        "response_format": {"type": "json_object"}
    });
    let ir = CohereReader
        .read_request(&body)
        .expect("read_request should succeed");
    assert_eq!(ir.frequency_penalty, Some(0.5));
    assert_eq!(ir.presence_penalty, Some(0.25));
    assert_eq!(ir.seed, Some(42));
    assert_eq!(
        ir.response_format,
        Some(crate::codec::ir::IrResponseFormat {
            json: true,
            schema: None,
            name: None,
            strict: None,
            description: None,
        })
    );

    assert!(!ir.extra.contains_key("frequency_penalty"));
    assert!(!ir.extra.contains_key("presence_penalty"));
    assert!(!ir.extra.contains_key("seed"));
    assert!(!ir.extra.contains_key("response_format"));

    let out = {
        let __w = CohereWriter;
        __w.write_request(crate::codec::test_host::HOST, &ir)
    };
    assert_eq!(out.get("frequency_penalty"), Some(&serde_json::json!(0.5)));
    assert_eq!(out.get("presence_penalty"), Some(&serde_json::json!(0.25)));
    assert_eq!(out.get("seed"), Some(&serde_json::json!(42)));
    assert_eq!(
        out.get("response_format"),
        Some(&serde_json::json!({"type": "json_object"}))
    );
}

#[test]
fn test_cohere_image_content_part_read_write_roundtrip() {
    let data_uri = "data:image/png;base64,aGVsbG8=";
    let body = serde_json::json!({
        "model": "command-a-vision",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": data_uri}},
                {"type": "image_url", "image_url": {"url": "https://example.com/cat.jpg"}}
            ]
        }]
    });
    let ir = CohereReader
        .read_request(&body)
        .expect("read_request should succeed");
    let content = &ir.messages[0].content;

    assert!(matches!(content[0], crate::codec::ir::IrBlock::Text { .. }));
    match &content[1] {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Base64 { media_type, data },
            ..
        } => {
            assert_eq!(media_type, "image/png");
            assert_eq!(data, "aGVsbG8=");
        }
        other => panic!("expected base64 Image, got {other:?}"),
    }
    match &content[2] {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Url(url),
            ..
        } => {
            assert_eq!(url, "https://example.com/cat.jpg");
        }
        other => panic!("expected url Image, got {other:?}"),
    }

    let out = {
        let __w = CohereWriter;
        __w.write_request(crate::codec::test_host::HOST, &ir)
    };
    let msgs = out.get("messages").and_then(|m| m.as_array()).unwrap();
    let parts = msgs[0].get("content").and_then(|c| c.as_array()).unwrap();
    assert_eq!(parts[0].get("type").and_then(|t| t.as_str()), Some("text"));
    assert_eq!(
        parts[1].get("type").and_then(|t| t.as_str()),
        Some("image_url")
    );
    assert_eq!(
        parts[1]
            .get("image_url")
            .and_then(|iu| iu.get("url"))
            .and_then(|u| u.as_str()),
        Some(data_uri),
        "base64 image must reconstruct its original data URI"
    );
    assert_eq!(
        parts[2]
            .get("image_url")
            .and_then(|iu| iu.get("url"))
            .and_then(|u| u.as_str()),
        Some("https://example.com/cat.jpg"),
        "url image must emit its raw URL verbatim"
    );
}

/// Cohere's `finish_reason` codec: the generic infra-failure `ERROR` maps to `IrStopReason::Error`
#[test]
fn finish_reason_error_and_unknown_codec() {
    use crate::codec::ir::IrStopReason as S;

    assert_eq!(read_cohere_stop_reason(COHERE_FINISH_ERROR), S::Error);
    assert_eq!(
        read_cohere_stop_reason(COHERE_FINISH_ERROR_TOXIC),
        S::Safety
    );

    assert_eq!(write_cohere_stop_reason(S::Error), COHERE_FINISH_ERROR);

    assert_eq!(read_cohere_stop_reason("SOME_FUTURE_REASON"), S::Other);

    assert_eq!(write_cohere_stop_reason(S::Other), COHERE_FINISH_COMPLETE);
    assert_eq!(write_cohere_stop_reason(S::Refusal), COHERE_FINISH_COMPLETE);
    assert_eq!(
        write_cohere_stop_reason(S::PauseTurn),
        COHERE_FINISH_COMPLETE
    );
}

/// A full non-stream `read_response` carrying `finish_reason: "ERROR"` surfaces
#[test]
fn read_write_response_error_finish_reason_round_trips() {
    let body = serde_json::json!({
        "id": "c-123",
        "finish_reason": COHERE_FINISH_ERROR,
        "message": {"role": "assistant", "content": [{"type": "text", "text": ""}]},
        "usage": {"tokens": {"input_tokens": 4, "output_tokens": 0}}
    });
    let resp = CohereReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::Error)
    );
    let writer = CohereWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out.get("finish_reason").and_then(|v| v.as_str()),
        Some("ERROR") // golden wire-contract literal (kept bare on purpose)
    );
}

/// A `response_format` json_schema round-trips read→IR→write. Cohere's native shape puts the
#[test]
fn response_format_json_schema_round_trips_cohere_shape() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"]
    });
    let body = serde_json::json!({
        "model": "command-r",
        "messages": [{"role": "user", "content": "hi"}],
        "response_format": {"type": "json_object", "json_schema": schema}
    });
    let ir = CohereReader.read_request(&body).expect("read_request");
    let rf = ir
        .response_format
        .as_ref()
        .expect("response_format canonicalizes into the typed IR");
    assert!(rf.json, "json_object must set json=true");
    assert_eq!(
        rf.schema.as_ref(),
        Some(&schema),
        "the schema sits directly under json_schema in Cohere's shape"
    );

    assert!(!ir.extra.contains_key("response_format"));

    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/response_format/type")
            .and_then(|t| t.as_str()),
        Some("json_object")
    );
    assert_eq!(out.pointer("/response_format/json_schema"), Some(&schema));
}

/// Cohere v2 has NO `n`/`num_generations` parameter (it was a v1 field, removed in v2 — ir.rs:46).
#[test]
fn n_candidate_count_never_emitted_on_cohere() {
    let mut req = crate::codec::ir::IrRequest {
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        ..Default::default()
    };
    req.n = Some(3);
    let writer = CohereWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    assert!(
        out.get("n").is_none() && out.get("num_generations").is_none(),
        "Cohere v2 models no n/num_generations; it must never be emitted: {out}"
    );

    let body = serde_json::json!({
        "model": "command-r",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let ir = CohereReader.read_request(&body).expect("read_request");
    assert_eq!(ir.n, None, "the Cohere reader must never populate n");
}

/// Streaming symmetry: the STREAMING Cohere reader must carry `tool-plan-delta` the same way the
#[test]
fn test_stream_tool_plan_delta_becomes_leading_reasoning_before_tool_call() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let reader = CohereReader;

    reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_MESSAGE_START, "delta": {"message": {"role": "assistant"}}}),
        &mut state,
    );

    let evs = reader.read_response_events(crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_TOOL_PLAN_DELTA, "delta": {"message": {"tool_plan": "I will "}}}),
        &mut state,
    );
    assert!(
        matches!(
            &evs[0],
            crate::codec::ir::IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                refusal: _,
            }
        ),
        "first tool-plan-delta must open a leading reasoning block at index 0, got {evs:?}"
    );
    assert!(
        matches!(
            &evs[1],
            crate::codec::ir::IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::ThinkingDelta(t)
            } if t == "I will "
        ),
        "the plan token must be a ThinkingDelta, got {evs:?}"
    );

    let evs = reader.read_response_events(crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": ET_TOOL_PLAN_DELTA, "delta": {"message": {"tool_plan": "check the weather."}}}),
        &mut state,
    );
    assert_eq!(
        evs.len(),
        1,
        "a subsequent token emits only a ThinkingDelta"
    );
    assert!(matches!(
        &evs[0],
        crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::ThinkingDelta(t)
        } if t == "check the weather."
    ));

    let evs = reader.read_response_events(crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": 0,
            "delta": {"message": {"tool_calls": {"id": "t1", "type": "function", "function": {"name": "get_weather", "arguments": ""}}}}
        }),
        &mut state,
    );
    assert!(
        matches!(
            &evs[0],
            crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
        ),
        "the tool-call-start must close the open plan Text block at index 0 first, got {evs:?}"
    );
    assert!(
        matches!(
            &evs[1],
            crate::codec::ir::IrStreamEvent::BlockStart {
                index: 1,
                block: crate::codec::ir::IrBlockMeta::ToolUse { id, name }, refusal: _,
            } if id == "t1" && name == "get_weather"
        ),
        "the tool must open at index 1, after the leading plan Text at index 0, got {evs:?}"
    );
}

/// Known writer limitation: the IR carries a folded `tool_plan` as a plain
#[test]
fn test_write_response_reemits_folded_tool_plan_as_content_not_tool_plan() {
    let writer = CohereWriter;
    let resp = crate::codec::ir::IrResponse {
        content: vec![
            crate::codec::ir::IrBlock::text("First I will check the weather.".to_string()),
            crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "t1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
                cache_control: None,
            },
        ],
        ..Default::default()
    };
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    let message = out.get("message").expect("message");
    assert!(
        message.get("tool_plan").is_none(),
        "the IR cannot distinguish a folded tool_plan from a plain leading Text, so it is NOT \
         re-emitted as tool_plan: {out}"
    );
    let content = message
        .get("content")
        .and_then(|c| c.as_array())
        .expect("content array");
    assert!(
        content
            .iter()
            .any(|b| b.get("text").and_then(|t| t.as_str())
                == Some("First I will check the weather.")),
        "the reasoning text must survive as a content text block: {out}"
    );
}

/// Follow-up (MAJOR, balanced-stream): drive a REAL Cohere v2 tool-plan stream
#[test]
fn test_stream_tool_plan_to_anthropic_writer_is_balanced() {
    let reader = CohereReader;
    let writer = super::super::anthropic::AnthropicWriter;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let frames = vec![
        serde_json::json!({"type": ET_MESSAGE_START, "delta": {"message": {"role": "assistant"}}}),
        serde_json::json!({"type": ET_TOOL_PLAN_DELTA, "delta": {"message": {"tool_plan": "I will "}}}),
        serde_json::json!({"type": ET_TOOL_PLAN_DELTA, "delta": {"message": {"tool_plan": "check the weather."}}}),
        serde_json::json!({
            "type": ET_TOOL_CALL_START,
            "index": 0,
            "delta": {"message": {"tool_calls": {"id": "t1", "type": "function", "function": {"name": "get_weather", "arguments": ""}}}}
        }),
        serde_json::json!({
            "type": ET_TOOL_CALL_DELTA,
            "index": 0,
            "delta": {"message": {"tool_calls": {"function": {"arguments": "{\"city\":\"SF\"}"}}}}
        }),
        serde_json::json!({"type": ET_TOOL_CALL_END, "index": 0}),
        serde_json::json!({"type": ET_MESSAGE_END, "delta": {"finish_reason": "COMPLETE"}}),
    ];

    let mut starts = 0usize;
    let mut stops = 0usize;
    for frame in &frames {
        for ev in reader.read_response_events(crate::codec::test_host::HOST, "", frame, &mut state)
        {
            if let Some((event_type, _payload)) =
                writer.write_response_event(crate::codec::test_host::HOST, &ev)
            {
                if event_type == "content_block_start" {
                    starts += 1;
                } else if event_type == "content_block_stop" {
                    stops += 1;
                }
            }
        }
    }

    assert_eq!(
        starts, 2,
        "expected two content_block_start frames (plan text + tool_use)"
    );
    assert_eq!(
        starts, stops,
        "every content_block_start must have a matching content_block_stop (balanced stream); \
         got {starts} start(s) and {stops} stop(s)"
    );
}

/// REGRESSION: Anthropic and Gemini carry `tool_result` blocks on a USER-role
#[test]
fn test_anthropic_user_tool_result_survives_to_cohere() {
    let anthropic_body = serde_json::json!({
        "model": "x",
        "max_tokens": 10,
        "messages": [
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_01AAA", "name": "get_weather", "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01AAA",
                 "content": [{"type": "text", "text": "18C sunny"}]}
            ]}
        ]
    });
    let ir = super::super::anthropic::AnthropicReader
        .read_request(&anthropic_body)
        .expect("anthropic read_request");

    let tr_msg = ir
        .messages
        .iter()
        .find(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolResult { .. }))
        })
        .expect("IR must contain a ToolResult block");
    assert_eq!(
        tr_msg.role,
        crate::codec::ir::IrRole::User,
        "Anthropic carries tool_result on a User-role message in the IR"
    );

    let writer = CohereWriter;
    let cohere = writer.write_request(crate::codec::test_host::HOST, &ir);
    let msgs = cohere.get("messages").unwrap().as_array().unwrap();
    let tool_msg = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .expect("Cohere output MUST include a tool message for the tool_result");
    assert_eq!(
        tool_msg.get("tool_call_id").and_then(|c| c.as_str()),
        Some("toolu_01AAA"),
        "Cohere tool message must carry the originating tool_call_id"
    );
    assert_eq!(
        tool_msg.get("content").and_then(|c| c.as_str()),
        Some("18C sunny"),
        "Cohere tool message must carry the tool-result text"
    );
}

/// REGRESSION: an Anthropic USER-role message can bundle a `tool_result` block TOGETHER
#[test]
fn test_anthropic_user_tool_result_plus_text_splits_to_tool_and_user() {
    let anthropic_body = serde_json::json!({
        "model": "x",
        "max_tokens": 10,
        "messages": [
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_01AAA", "name": "get_weather", "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01AAA",
                 "content": [{"type": "text", "text": "18C sunny"}]},
                {"type": "text", "text": "and now also book me a table"}
            ]}
        ]
    });
    let ir = super::super::anthropic::AnthropicReader
        .read_request(&anthropic_body)
        .expect("anthropic read_request");

    let bundled = ir
        .messages
        .iter()
        .find(|m| {
            m.role == crate::codec::ir::IrRole::User
                && m.content
                    .iter()
                    .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolResult { .. }))
                && m.content
                    .iter()
                    .any(|b| matches!(b, crate::codec::ir::IrBlock::Text { .. }))
        })
        .expect("IR must carry a User message bundling a ToolResult AND follow-up Text");
    assert_eq!(bundled.role, crate::codec::ir::IrRole::User);

    let writer = CohereWriter;
    let cohere = writer.write_request(crate::codec::test_host::HOST, &ir);
    let msgs = cohere.get("messages").unwrap().as_array().unwrap();

    let tool_msg = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .expect("Cohere output MUST include a tool message for the tool_result");
    assert_eq!(
        tool_msg.get("tool_call_id").and_then(|c| c.as_str()),
        Some("toolu_01AAA"),
        "the tool message must carry the originating tool_call_id"
    );
    assert_eq!(
        tool_msg.get("content").and_then(|c| c.as_str()),
        Some("18C sunny"),
        "the tool message content must be ONLY the tool result, not the folded user text"
    );

    let follow_up = msgs
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .find(|m| m.get("content").and_then(|c| c.as_str()) == Some("and now also book me a table"))
        .expect("the follow-up user text MUST survive as its own Cohere user message");
    assert_eq!(
        follow_up.get("content").and_then(|c| c.as_str()),
        Some("and now also book me a table")
    );

    for m in msgs {
        if m.get("role").and_then(|r| r.as_str()) == Some("tool") {
            let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            assert!(
                !content.contains("book me a table"),
                "user text leaked into a tool message content: {content}"
            );
        }
    }
}

#[test]
fn cohere_empty_or_missing_stream_event_type_is_ignored_not_panic() {
    let reader = CohereReader;

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({ "type": "" }),
        &mut state,
    );
    assert!(
        evs.is_empty(),
        "an empty-type Cohere frame must be ignored (no IR events), got {evs:?}"
    );

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({ "index": 0 }),
        &mut state,
    );
    assert!(
        evs.is_empty(),
        "a type-less Cohere frame must be ignored (no IR events), got {evs:?}"
    );
}

/// A Cohere stream that terminates with `message-end` carrying the generic infra `finish_reason:
#[test]
fn test_stream_generic_error_finish_pushes_ir_error_event() {
    let reader = CohereReader;

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_MESSAGE_END,
            "delta": { "finish_reason": COHERE_FINISH_ERROR, "usage": { "tokens": {} } }
        }),
        &mut state,
    );
    let err = evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::Error(err) => Some(err),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("a terminal generic ERROR must push an IrStreamEvent::Error, got {evs:?}")
        });
    assert_eq!(
        err.class,
        StatusClass::ServerError,
        "the generic infra ERROR is a TRANSIENT upstream fault so the lane recovers via cooldown \
         rather than being permanently penalized: {evs:?}"
    );

    assert!(
        evs.iter().any(|e| matches!(
            e,
            IrStreamEvent::MessageDelta { stop_reason, .. }
                if stop_reason == &Some(crate::codec::ir::IrStopReason::Error)
        )),
        "the terminal MessageDelta must survive alongside the Error event: {evs:?}"
    );

    let mut toxic_state = crate::codec::ir::StreamDecodeState::default();
    let toxic_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": ET_MESSAGE_END,
            "delta": { "finish_reason": COHERE_FINISH_ERROR_TOXIC, "usage": { "tokens": {} } }
        }),
        &mut toxic_state,
    );
    assert!(
        !toxic_evs
            .iter()
            .any(|e| matches!(e, IrStreamEvent::Error(_))),
        "ERROR_TOXIC is a content-moderation stop, not an upstream fault — it must NOT push an \
         Error event (that would fail a lane for a safety refusal): {toxic_evs:?}"
    );
}

/// A two-citation delta driven straight at `write_response_events` must emit BOTH citations. The
#[test]
fn cohere_writer_emits_every_citation_in_a_multi_citation_delta() {
    let cit = |url: &str| crate::codec::ir::IrCitation {
        domain: None,
        kind: Some("web_search_result_location".to_string()),
        cited_text: Some("quoted".to_string()),
        title: Some("t".to_string()),
        url: Some(url.to_string()),
        document_index: None,
        start_index: Some(0),
        end_index: Some(6),
        encrypted_index: None,
        raw: None,
        ..Default::default()
    };
    let writer = CohereWriter;
    let frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::CitationsDelta(vec![
                cit("https://example.invalid/a"),
                cit("https://example.invalid/b"),
            ]),
        },
    );

    let starts: Vec<&serde_json::Value> = frames
        .iter()
        .filter(|(_, d)| d.get("type").and_then(|t| t.as_str()) == Some("citation-start"))
        .map(|(_, d)| d)
        .collect();
    assert_eq!(
        starts.len(),
        2,
        "both citations must reach the wire, got {frames:?}"
    );
    let urls: Vec<Option<&str>> = starts
        .iter()
        .map(|d| {
            d.pointer("/delta/message/citations/sources/0/document/url")
                .and_then(|v| v.as_str())
        })
        .collect();
    assert_ne!(
        urls[0], urls[1],
        "the two frames must carry the two DISTINCT citations, got {frames:?}"
    );
    assert_eq!(
        frames
            .iter()
            .filter(|(_, d)| d.get("type").and_then(|t| t.as_str()) == Some("citation-end"))
            .count(),
        2,
        "each citation-start keeps its paired citation-end, got {frames:?}"
    );
}

/// `raw_stop_reason` reads the top-level `finish_reason` off the RAW bytes and maps it exactly as
#[test]
fn raw_stop_reason_agrees_with_read_response() {
    for token in [
        "ERROR",
        "COMPLETE",
        "MAX_TOKENS",
        "TOOL_CALL",
        "ERROR_TOXIC",
        "NOVEL",
    ] {
        let body = serde_json::json!({
            "id": "c-1",
            "finish_reason": token,
            "message": {"role": "assistant", "content": [{"type": "text", "text": "x"}]},
            "usage": {"tokens": {"input_tokens": 10, "output_tokens": 5}}
        });
        let parsed = CohereReader
            .read_response(crate::codec::test_host::HOST, &body)
            .expect("read_response");
        assert_eq!(
            crate::codec::test_host::raw_stop_reason(&CohereReader, body.to_string().as_bytes()),
            parsed.stop_reason,
            "{token}"
        );
    }

    assert_eq!(
        crate::codec::test_host::raw_stop_reason(
            &CohereReader,
            b"{\"finish_reason\" :  \"ERROR\"}"
        ),
        Some(crate::codec::ir::IrStopReason::Error)
    );

    assert_eq!(
        crate::codec::test_host::raw_stop_reason(&CohereReader, b"{\"id\":\"c\"}"),
        None
    );
    assert_eq!(
        crate::codec::test_host::raw_stop_reason(&CohereReader, b"{\"finish_reason\":\"\"}"),
        None
    );
    assert_eq!(
        crate::codec::test_host::raw_stop_reason(&CohereReader, b"{\"finish_reason\":null}"),
        None
    );
}

/// The key spelled INSIDE delivered text is not the field: there its quotes are escaped. A body
#[test]
fn raw_stop_reason_ignores_the_key_inside_delivered_text() {
    let body = serde_json::json!({
        "id": "c-1",
        "message": {"role": "assistant",
            "content": [{"type": "text", "text": "{\"finish_reason\":\"ERROR\"}"}]},
        "finish_reason": "COMPLETE",
        "usage": {"tokens": {"input_tokens": 10, "output_tokens": 5}}
    });
    assert_eq!(
        crate::codec::test_host::raw_stop_reason(&CohereReader, body.to_string().as_bytes()),
        Some(crate::codec::ir::IrStopReason::EndTurn)
    );
}

#[test]
fn empty_writer_text_deltas_do_not_allocate_citation_anchors() {
    let writer = CohereWriter;
    for index in 0..1024 {
        writer.write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index,
                delta: crate::codec::ir::IrDelta::TextDelta(String::new()),
            },
        );
    }
    assert!(writer.stream.lock().citation_blocks.is_empty());
}
