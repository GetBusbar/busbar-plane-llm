use super::*;
use serde_json::json;

#[test]
fn creation_keeps_filtered_children_and_folded_system_origins_paired() {
    let request = AnthropicReader.read_request(&json!({
        "model": "fixture-model",
        "system": "same text",
        "messages": [
            {"role": "system", "content": "same text"},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "fixture-call", "content": [
                {"type": "future_fixture", "payload": true},
                {"type": "text", "text": "answer"},
                {"type": "document"}
            ]}]}
        ]
    })).expect("native request");
    let system_paths: Vec<_> = request
        .block_origins
        .system
        .iter()
        .map(|origin| origin.source.unwrap().path)
        .collect();
    assert_eq!(system_paths, vec![SYSTEM_STRING.path, MESSAGE_STRING.path]);
    assert_eq!(request.system.len(), system_paths.len());
    assert_eq!(
        request.system_folds,
        vec![crate::codec::ir::IrSystemFold {
            before: 0,
            role: crate::codec::ir::IrSystemRole::System,
            blocks: 1..2,
        }]
    );
    let result = request.messages.first().unwrap().content.first().unwrap();
    let crate::codec::ir::IrBlock::ToolResult { content, .. } = result else {
        panic!("fixture tool result")
    };
    let origin = request
        .block_origins
        .messages
        .first()
        .unwrap()
        .first()
        .unwrap();
    assert_eq!(origin.source.unwrap().path, MESSAGE_TOOL_RESULT.path);
    assert_eq!(content.len(), origin.children.len());
    let child_paths: Vec<_> = origin
        .children
        .iter()
        .map(|origin| origin.source.unwrap().path)
        .collect();
    assert_eq!(child_paths, vec![RESULT_BLOCK.path]);
}

#[test]
fn direct_response_reader_captures_native_reasoning_tag() {
    let response = AnthropicReader
        .read_response(
            crate::codec::test_host::HOST,
            &json!({
                "role": "assistant", "content": [
                    {"type": "thinking", "thinking": "fixture thought"},
                    {"type": "text", "text": "fixture answer"}
                ]
            }),
        )
        .expect("native response");
    let paths: Vec<_> = response
        .block_origins
        .response
        .iter()
        .map(|origin| origin.source.unwrap().path)
        .collect();
    assert_eq!(response.content.len(), paths.len());
    assert_eq!(paths, vec![RESPONSE_THINKING.path, RESPONSE_BLOCK.path]);
}
#[test]
fn native_stream_origins_follow_emitted_start_indices_and_suppression() {
    let host = crate::codec::test_host::HOST;
    for (block, source) in [
        (json!({"type": "text", "text": ""}), &STREAM_TEXT),
        (
            json!({"type": "thinking", "thinking": ""}),
            &STREAM_THINKING,
        ),
        (
            json!({"type": "redacted_thinking", "data": "fixture-opaque"}),
            &STREAM_REDACTED,
        ),
        (
            json!({"type": "tool_use", "id": "fixture-call", "name": "fixture_tool"}),
            &STREAM_TOOL_USE,
        ),
    ] {
        let mut state = crate::codec::ir::StreamDecodeState::default();
        let events = AnthropicReader.read_response_events(
            host,
            EVT_CONTENT_BLOCK_START,
            &json!({"index": 0, "content_block": block}),
            &mut state,
        );
        let index = events
            .iter()
            .find_map(|event| match event {
                IrStreamEvent::BlockStart { index, .. } => Some(index),
                _ => None,
            })
            .expect("native block start");
        assert_eq!(
            state.block_origins.get(index).unwrap().source.unwrap().path,
            source.path
        );
    }
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let unknown = AnthropicReader.read_response_events(
        host,
        EVT_CONTENT_BLOCK_START,
        &json!({"index": 0, "content_block": {"type": "future_fixture"}}),
        &mut state,
    );
    assert!(unknown.is_empty());
    assert!(state.block_origins.is_empty());
}
#[test]
fn absent_document_payload_is_reported_at_the_actual_native_decode_site() {
    let (_, paths) = crate::codec::drops::scope(
        crate::codec::drops::Seam {
            direction: crate::codec::drops::Direction::Request,
            ingress: "anthropic",
            egress: "openai",
        },
        || {
            AnthropicReader.read_request(&json!({
        "system": [{"type": "document"}],
        "messages": [{"role": "user", "content": [{
            "type": "tool_result", "tool_use_id": "fixture-call", "content": [{"type": "document"}]
        }]}]
    })).expect("missing document payload is omitted")
        },
    );
    assert_eq!(
        paths,
        [
            "system[].type=document",
            "messages[].content[].type=tool_result.content[].type=document"
        ]
    );
}
