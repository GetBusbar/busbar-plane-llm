// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors
use super::*;
use crate::codec::ir::{IrDelta, StreamDecodeState};

fn frame(state: &mut StreamDecodeState, kind: &str, data: serde_json::Value) -> Vec<IrStreamEvent> {
    ResponsesReader.read_response_events(crate::codec::test_host::HOST, kind, &data, state)
}

#[test]
fn multipart_identity_survives_done_annotations_and_other_items() {
    let mut state = StreamDecodeState::default();
    let first = frame(
        &mut state,
        EVT_OUTPUT_TEXT_DELTA,
        serde_json::json!({"output_index":0,"content_index":0,"delta":"first"}),
    );
    assert!(matches!(
        first[0],
        IrStreamEvent::BlockStart { index: 0, .. }
    ));
    let second = frame(
        &mut state,
        EVT_OUTPUT_TEXT_DELTA,
        serde_json::json!({"output_index":0,"content_index":1,"delta":"second"}),
    );
    let IrStreamEvent::BlockStart {
        index: second_index,
        ..
    } = second[0]
    else {
        panic!("missing start")
    };
    assert_ne!(second_index, 0);
    let stop = frame(
        &mut state,
        EVT_CONTENT_PART_DONE,
        serde_json::json!({"output_index":0,"content_index":0}),
    );
    assert!(matches!(
        stop.as_slice(),
        [IrStreamEvent::BlockStop { index: 0 }]
    ));
    let annotation = frame(
        &mut state,
        EVT_OUTPUT_TEXT_ANNOTATION_ADDED,
        serde_json::json!({"output_index":0,"content_index":1,"annotation":{"type":"url_citation","url":"https://example.org","title":"source","start_index":0,"end_index":6}}),
    );
    assert!(
        matches!(annotation.as_slice(), [IrStreamEvent::BlockDelta { index, delta: IrDelta::CitationsDelta(_) }] if *index == second_index)
    );
    let tool = frame(
        &mut state,
        EVT_OUTPUT_ITEM_ADDED,
        serde_json::json!({"output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"lookup"}}),
    );
    assert!(matches!(
        tool[0],
        IrStreamEvent::BlockStart { index: 1, .. }
    ));
    assert_ne!(second_index, 1);
    let stop = frame(
        &mut state,
        EVT_OUTPUT_ITEM_DONE,
        serde_json::json!({"output_index":0}),
    );
    assert!(
        matches!(stop.as_slice(), [IrStreamEvent::BlockStop { index }] if *index == second_index)
    );
    assert!(frame(
        &mut state,
        EVT_OUTPUT_TEXT_DELTA,
        serde_json::json!({"output_index":0,"content_index":1,"delta":"late"})
    )
    .is_empty());
    assert!(!frame(
        &mut state,
        EVT_FUNCTION_CALL_ARGS_DELTA,
        serde_json::json!({"output_index":1,"delta":"{}"})
    )
    .is_empty());
}

#[test]
fn terminal_refusal_snapshot_keeps_both_parts_and_deduplicates() {
    let mut state = StreamDecodeState::default();
    frame(
        &mut state,
        EVT_REFUSAL_DELTA,
        serde_json::json!({"output_index":0,"content_index":0,"delta":"first"}),
    );
    frame(
        &mut state,
        EVT_REFUSAL_DONE,
        serde_json::json!({"output_index":0,"content_index":0,"refusal":"first"}),
    );
    let events = frame(
        &mut state,
        EVT_RESPONSE_COMPLETED,
        serde_json::json!({"response":{"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"first"},{"type":"refusal","refusal":"second"}]}]}}),
    );
    let text: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockDelta {
                index,
                delta: IrDelta::TextDelta(text),
            } => Some((*index, text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(text.len(), 1);
    assert_ne!(text[0].0, 0);
    assert_eq!(text[0].1, "second");
}

fn assert_index_error(events: &[IrStreamEvent]) {
    assert!(
        events
            .iter()
            .any(|event| matches!(event, IrStreamEvent::Error(error)
        if error.provider_signal.as_deref() == Some("invalid_response_index"))),
        "{events:?}"
    );
}

#[test]
fn hostile_indices_fail_every_indexed_entry_branch() {
    for kind in [
        EVT_OUTPUT_ITEM_ADDED,
        EVT_OUTPUT_ITEM_DONE,
        EVT_CONTENT_PART_DONE,
        EVT_OUTPUT_TEXT_DELTA,
        EVT_OUTPUT_TEXT_ANNOTATION_ADDED,
        EVT_REFUSAL_DELTA,
        EVT_REFUSAL_DONE,
        EVT_FUNCTION_CALL_ARGS_DELTA,
        EVT_REASONING_TEXT_DELTA,
        EVT_REASONING_SUMMARY_TEXT_DELTA,
    ] {
        for key in ["output_index", "content_index"] {
            for value in [
                serde_json::json!(128),
                serde_json::json!(u64::MAX),
                serde_json::json!(-1),
                serde_json::json!("0"),
                serde_json::Value::Null,
                serde_json::json!(1.5),
            ] {
                let mut data = serde_json::json!({"delta":"x", "refusal":"no",
                    "item":{"type":"function_call","call_id":"c","name":"f"}});
                data[key] = value;
                assert_index_error(&frame(&mut StreamDecodeState::default(), kind, data));
            }
        }
    }
}

#[test]
fn mapping_cap_plus_one_fails_even_after_prior_parts_close() {
    let mut state = StreamDecodeState::default();
    for content in 0..128 {
        let data = serde_json::json!({"output_index":0,"content_index":content,"delta":"x"});
        assert!(!frame(&mut state, EVT_OUTPUT_TEXT_DELTA, data.clone())
            .iter()
            .any(|event| matches!(event, IrStreamEvent::Error(_))));
        frame(&mut state, EVT_CONTENT_PART_DONE, data);
    }
    for kind in [
        EVT_OUTPUT_TEXT_DELTA,
        EVT_REFUSAL_DELTA,
        EVT_REFUSAL_DONE,
        EVT_OUTPUT_TEXT_ANNOTATION_ADDED,
        EVT_CONTENT_PART_DONE,
    ] {
        assert_index_error(&frame(
            &mut state,
            kind,
            serde_json::json!({
            "output_index":1,"content_index":0,"delta":"x","refusal":"no"}),
        ));
    }
    assert_eq!(state.responses_parts.len(), MAX_OPEN_TOOLS);
    let events = frame(
        &mut state,
        EVT_RESPONSE_COMPLETED,
        serde_json::json!({
        "response":{"status":"completed","output":[{}, {"type":"message",
            "content":[{"type":"refusal","refusal":"no"}]}]}}),
    );
    assert_index_error(&events);
    assert!(!events
        .iter()
        .any(|event| matches!(event, IrStreamEvent::MessageStop)));
}

#[test]
fn absent_indices_still_default_to_first_text_part() {
    let events = frame(
        &mut StreamDecodeState::default(),
        EVT_OUTPUT_TEXT_DELTA,
        serde_json::json!({"delta":"hello"}),
    );
    assert!(matches!(
        events[0],
        IrStreamEvent::BlockStart { index: 0, .. }
    ));
}

#[test]
fn part_done_does_not_close_reasoning_and_item_done_carries_signature() {
    let mut state = StreamDecodeState::default();
    frame(
        &mut state,
        EVT_OUTPUT_ITEM_ADDED,
        serde_json::json!({"output_index":2,"item":{"type":"reasoning"}}),
    );
    assert!(frame(
        &mut state,
        EVT_CONTENT_PART_DONE,
        serde_json::json!({"output_index":2,"content_index":0})
    )
    .is_empty());
    let delta = frame(
        &mut state,
        EVT_REASONING_SUMMARY_TEXT_DELTA,
        serde_json::json!({"output_index":2,"delta":"thinking"}),
    );
    assert!(matches!(
        delta.as_slice(),
        [IrStreamEvent::BlockDelta {
            index: 2,
            delta: IrDelta::ThinkingDelta(_)
        }]
    ));
    let done = frame(
        &mut state,
        EVT_OUTPUT_ITEM_DONE,
        serde_json::json!({"output_index":2,"item":{"type":"reasoning","encrypted_content":"sig"}}),
    );
    assert!(matches!(
        done.as_slice(),
        [
            IrStreamEvent::BlockDelta {
                index: 2,
                delta: IrDelta::SignatureDelta(_)
            },
            IrStreamEvent::BlockStop { index: 2 }
        ]
    ));
}
