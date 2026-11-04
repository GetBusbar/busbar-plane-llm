use super::*;
use serde_json::json;

#[test]
fn request_origins_pair_guarded_and_nested_blocks() {
    let body = json!({
        "system": [{"text": "top"}, {"guardContent": {"text": {"text": "guarded"}}}],
        "messages": [{"role": "user", "content": [
            {"cachePoint": {"type": "default"}},
            {"toolResult": {"toolUseId": "call", "content": [
                {"text": "answer"}, {"json": {"ok": true}},
                {"document": {"name": "doc", "format": "txt", "source": {"bytes": "YQ=="}}}
            ]}},
            {"reasoningContent": {"reasoningText": {"text": "thinking"}}}
        ]}]
    });
    let req = BedrockReader.read_request(&body).expect("native request");
    assert_eq!(req.system.len(), req.block_origins.system.len());
    assert_eq!(
        req.messages[0].content.len(),
        req.block_origins.messages[0].len()
    );
    assert_eq!(
        req.block_origins.system[1].source.unwrap().path,
        "system[].guardContent"
    );
    let result = &req.block_origins.messages[0][0];
    assert_eq!(
        result.source.unwrap().path,
        "messages[].content[].toolResult"
    );
    assert_eq!(result.children.len(), 3);
    assert_eq!(
        result.children[1].source.unwrap().path,
        "messages[].content[].toolResult.content[].json"
    );
    assert_eq!(
        req.block_origins.messages[0][1].source.unwrap().path,
        "messages[].content[].reasoningContent"
    );
}

#[test]
fn response_origins_follow_decodable_branches() {
    let body = json!({"output": {"message": {"role": "assistant", "content": [
        {"text": "answer"},
        {"reasoningContent": {"reasoningText": {"text": "thinking"}}},
        {"future": {"value": true}}
    ]}}});
    let resp = BedrockReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("native response");
    assert_eq!(resp.content.len(), resp.block_origins.response.len());
    assert_eq!(
        resp.block_origins.response[1].source.unwrap().path,
        "output.message.content[].reasoningContent"
    );
}
#[test]
fn absent_guard_projection_pushes_neither_block_nor_origin() {
    let request = BedrockReader
        .read_request(&json!({
            "system": [{"guardContent": {"future_fixture": true}}],
            "messages": [{"role": "user", "content": [{"guardContent": {"future_fixture": true}}]}]
        }))
        .expect("native request");
    assert!(request.system.is_empty());
    assert!(request.block_origins.system.is_empty());
    assert!(request.messages.first().unwrap().content.is_empty());
    assert!(request.block_origins.messages.first().unwrap().is_empty());
}

#[test]
fn native_stream_origins_follow_actual_start_and_lazy_delta_sites() {
    let host = crate::codec::test_host::HOST;
    for (frame, source) in [
        (
            json!({"type": "contentBlockStart", "contentBlockIndex": 0, "start": {"toolUse": {"toolUseId": "fixture-call", "name": "fixture_tool"}}}),
            &STREAM_TOOL_USE,
        ),
        (
            json!({"type": "contentBlockStart", "contentBlockIndex": 0, "start": {}}),
            &STREAM_TEXT_START,
        ),
        (
            json!({"type": "contentBlockStart", "contentBlockIndex": 0}),
            &STREAM_TEXT_EVENT,
        ),
        (
            json!({"type": "contentBlockDelta", "contentBlockIndex": 0, "delta": {"text": "fixture text"}}),
            &STREAM_TEXT_DELTA,
        ),
        (
            json!({"type": "contentBlockDelta", "contentBlockIndex": 0, "delta": {"citation": {"title": "fixture citation"}}}),
            &STREAM_CITATION_DELTA,
        ),
        (
            json!({"type": "contentBlockStart", "contentBlockIndex": 0, "start": {"reasoningContent": {}}}),
            &STREAM_REASONING_START,
        ),
        (
            json!({"type": "contentBlockDelta", "contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "fixture thought"}}}),
            &STREAM_REASONING_DELTA,
        ),
        (
            json!({"type": "contentBlockDelta", "contentBlockIndex": 0, "delta": {"reasoningContent": {"redactedContent": "fixture-opaque"}}}),
            &STREAM_REDACTED_DELTA,
        ),
    ] {
        let event_type = frame
            .get(keys::TYPE)
            .and_then(|value| value.as_str())
            .unwrap();
        let mut state = crate::codec::ir::StreamDecodeState::default();
        let before_message =
            BedrockReader.read_response_events(host, event_type, &frame, &mut state);
        assert!(!before_message
            .iter()
            .any(|event| matches!(event, IrStreamEvent::BlockStart { .. })));
        assert!(state.block_origins.is_empty());
        BedrockReader.read_response_events(
            host,
            ET_MESSAGE_START,
            &json!({"type": "messageStart"}),
            &mut state,
        );
        let events = BedrockReader.read_response_events(host, event_type, &frame, &mut state);
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
}
