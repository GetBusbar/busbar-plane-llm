use super::*;
use crate::codec::ir::StreamDecodeState;
use serde_json::{json, Value};

fn read(state: &mut StreamDecodeState, frame: &Value) -> Vec<IrStreamEvent> {
    CohereReader.read_response_events(crate::codec::test_host::HOST, "", frame, state)
}

#[test]
fn invalid_wire_indices_fail_without_aliasing_or_closing_open_blocks() {
    for index in [
        json!(4097),
        json!(u64::MAX),
        json!("bad"),
        json!(-1),
        json!(null),
        json!(1.5),
        json!(false),
        json!([]),
        json!({}),
    ] {
        for kind in [ET_TOOL_CALL_START, ET_TOOL_CALL_DELTA, ET_TOOL_CALL_END] {
            let mut state = StreamDecodeState::default();
            read(&mut state, &json!({"type":"tool-call-start","index":0}));
            read(&mut state, &json!({"type":"tool-call-start","index":4096}));
            read(&mut state, &json!({"type":"content-start"}));
            let indices = state.tool_ir_index.clone();
            let open = state.open_tools.clone();
            let events = read(
                &mut state,
                &json!({"type":kind,"index":index,"delta":{"message":{"tool_calls":{"function":{"arguments":"poison"}}}}}),
            );
            assert!(matches!(events.as_slice(), [IrStreamEvent::Error(_)]));
            assert_eq!(state.tool_ir_index, indices);
            assert_eq!(state.open_tools, open);
            assert!(state.text_block_open);
        }
    }
}

#[test]
fn absent_index_and_duplicate_valid_start_preserve_original_identity() {
    let mut state = StreamDecodeState::default();
    assert!(read(&mut state, &json!({"type":"tool-call-start"}))
        .iter()
        .any(|e| matches!(e, IrStreamEvent::BlockStart { index: 0, .. })));
    assert!(read(&mut state, &json!({"type":"tool-call-start","index":0})).is_empty());
    let delta = read(
        &mut state,
        &json!({"type":"tool-call-delta","delta":{"message":{"tool_calls":{"function":{"arguments":"ok"}}}}}),
    );
    assert!(matches!(
        delta.as_slice(),
        [IrStreamEvent::BlockDelta { index: 0, .. }]
    ));
    assert!(matches!(
        read(&mut state, &json!({"type":"tool-call-end"})).as_slice(),
        [IrStreamEvent::BlockStop { index: 0 }]
    ));
}

#[test]
fn invalid_index_terminates_translator_before_later_success() {
    for kind in [ET_TOOL_CALL_START, ET_TOOL_CALL_DELTA, ET_TOOL_CALL_END] {
        let mut translator =
            crate::codec::proto_stream::StreamTranslate::new("openai", "cohere").unwrap();
        let mut output = Vec::new();
        for frame in [
            json!({"type":"tool-call-start","index":4096}),
            json!({"type":kind,"index":4097}),
            json!({"type":"message-end","delta":{"finish_reason":"COMPLETE"}}),
        ] {
            output.extend(translator.feed(
                crate::codec::test_host::HOST,
                format!("data: {frame}\n\n").as_bytes(),
            ));
        }
        output.extend(translator.finish(crate::codec::test_host::HOST));
        let body = String::from_utf8(output).unwrap();
        let frames: Vec<Value> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str(data.trim()).ok())
            .collect();
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.get("error").is_some())
                .count(),
            1
        );
        assert!(!frames
            .iter()
            .any(
                |frame| frame["choices"].as_array().is_some_and(|choices| choices
                    .iter()
                    .any(|choice| choice["finish_reason"] == "stop"))
            ));
    }
}
