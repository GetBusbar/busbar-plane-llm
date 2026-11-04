use super::*;
use crate::codec::proto_codec::ProtocolReader;
use serde_json::json;

#[test]
fn actual_buffered_origins_follow_compacted_and_reordered_stream_indices() {
    let response = crate::codec::bedrock::BedrockReader
        .read_response(
            crate::codec::test_host::HOST,
            &json!({"output": {"message": {"role": "assistant", "content": [
                {"image": {"format": "png", "source": {"bytes": "YQ=="}}},
                {"text": "fixture answer"},
                {"reasoningContent": {"reasoningText": {"text": "fixture thought"}}}
            ]}}}),
        )
        .expect("native buffered response");
    let (events, origins) = response_to_ir_events_with_origins(&response);
    let opened: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            crate::codec::ir::IrStreamEvent::BlockStart { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(opened, origins.keys().copied().collect::<Vec<_>>());
    let sources: Vec<_> = opened
        .iter()
        .map(|index| {
            let source = origins.get(index).unwrap().source.unwrap();
            assert_eq!(source.dialect, "bedrock");
            source.path
        })
        .collect();
    assert_eq!(
        sources,
        [
            "output.message.content[].text",
            "output.message.content[].reasoningContent",
            "output.message.content[].image"
        ]
    );
}
