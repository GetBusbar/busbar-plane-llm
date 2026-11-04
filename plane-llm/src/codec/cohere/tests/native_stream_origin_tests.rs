use super::*;
use serde_json::json;

#[test]
fn actual_stream_creation_sites_never_claim_buffered_answer_containers() {
    let host = crate::codec::test_host::HOST;
    for (frame, path) in [
        (
            json!({"type": "content-start", "delta": {"message": {"content": {"type": "thinking", "thinking": "thought"}}}}),
            "content-start.delta.message.content",
        ),
        (
            json!({"type": "content-delta", "delta": {"message": {"content": {"thinking": "thought"}}}}),
            "content-delta.delta.message.content",
        ),
        (json!({"type": "content-start"}), "content-start"),
        (json!({"type": "content-delta"}), "content-delta"),
        (json!({"type": "tool-plan-delta"}), "tool-plan-delta"),
        (
            json!({"type": "tool-call-start", "index": 0, "delta": {"message": {"tool_calls": {"id": "fixture-call", "function": {"name": "fixture_tool"}}}}}),
            "tool-call-start",
        ),
    ] {
        let mut state = crate::codec::ir::StreamDecodeState::default();
        let events = CohereReader.read_response_events(
            host,
            frame
                .get(keys::TYPE)
                .and_then(|value| value.as_str())
                .unwrap(),
            &frame,
            &mut state,
        );
        let index = events
            .iter()
            .find_map(|event| match event {
                IrStreamEvent::BlockStart { index, .. } => Some(index),
                _ => None,
            })
            .expect("existing native creation branch");
        let source = state.block_origins.get(index).unwrap().source.unwrap();
        assert_eq!(source.dialect, "cohere");
        assert_eq!(source.path, path);
    }
}

#[test]
fn missing_tool_content_keeps_synthesized_child_origin_explicitly_unknown() {
    let request = CohereReader
        .read_request(&json!({"messages": [
            {"role": "tool", "tool_call_id": "fixture-call"}
        ]}))
        .expect("native request");
    let origin = request
        .block_origins
        .messages
        .first()
        .unwrap()
        .first()
        .unwrap();
    assert!(origin.source.is_none());
    assert!(origin.children.first().unwrap().source.is_none());
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let ignored = CohereReader.read_response_events(
        crate::codec::test_host::HOST,
        "future_fixture",
        &json!({"type": "future_fixture"}),
        &mut state,
    );
    assert!(ignored.is_empty());
    assert!(state.block_origins.is_empty());
}
